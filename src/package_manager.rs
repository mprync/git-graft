//! Declarative, project-local cache routing and installed dependency reuse.
//! Registrations contain configuration only. A process starts only on explicit execution.

use crate::{
    api::RegisterManager,
    cache, layer,
    registry::{self, Project},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
};

const REGISTRY_FILE: &str = "registered-managers-v1.json";
const MAX_BYTES: usize = 1024 * 1024;
const MAX_MANAGERS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstallSpec {
    pub(crate) manager: String,
    pub(crate) directory: String,
    pub(crate) inputs: Vec<String>,
    pub(crate) key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    cache_env: Option<String>,
    cache_args: Vec<String>,
    install: Option<InstallSpec>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Managers {
    version: u8,
    project: String,
    managers: BTreeMap<String, Registration>,
}

fn identifier(name: &str) -> Result<String> {
    ensure!(
        name.len() <= 128
            && name
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c)),
        "manager must be a portable identifier of up to 128 letters, digits, dots, underscores, plus signs or hyphens"
    );
    Ok(name.to_ascii_lowercase())
}

fn device_name(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_lowercase();
    if matches!(
        stem.as_str(),
        "aux" | "con" | "nul" | "prn" | "conin$" | "conout$"
    ) {
        return true;
    }
    stem.strip_prefix("com")
        .or_else(|| stem.strip_prefix("lpt"))
        .is_some_and(|digit| {
            matches!(
                digit,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

pub(crate) fn validate_relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 4096,
        "invalid length for checkout-relative path"
    );
    for part in path.split('/') {
        let lower = part.to_ascii_lowercase();
        let short_alias = part
            .split_once('~')
            .is_some_and(|(_, tail)| tail.starts_with(|c: char| c.is_ascii_digit()));
        let invalid_character = part.chars().any(|c| {
            c.is_control() || "\\:<>\"|?*".contains(c)
                || matches!(c, '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{206f}' | '\u{feff}')
        });
        ensure!(
            !part.is_empty()
                && part.len() <= 255
                && !part.ends_with([' ', '.'])
                && lower != ".git"
                && !device_name(part)
                && !short_alias
                && !invalid_character,
            "expected a portable checkout-relative path with ordinary components"
        );
    }
    Ok(())
}

fn validate_environment(name: &str) -> Result<()> {
    ensure!(
        name.len() <= 128
            && name
                .as_bytes()
                .first()
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
        "invalid package cache environment variable"
    );
    let upper = name.to_ascii_uppercase();
    ensure!(
        !["GIT_", "GRAFT_", "LD_", "DYLD_"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
            && !matches!(
                upper.as_str(),
                "PATH"
                    | "PATHEXT"
                    | "HOME"
                    | "USERPROFILE"
                    | "HOMEDRIVE"
                    | "HOMEPATH"
                    | "PWD"
                    | "TMP"
                    | "TEMP"
                    | "TMPDIR"
                    | "SHELL"
                    | "COMSPEC"
                    | "SYSTEMROOT"
                    | "WINDIR"
                    | "PORT"
                    | "ENV"
                    | "BASH_ENV"
                    | "CARGO_TARGET_DIR"
                    | "CARGO_BUILD_TARGET_DIR"
                    | "CARGO_BUILD_BUILD_DIR"
                    | "RUSTC_WRAPPER"
                    | "UV_PROJECT_ENVIRONMENT"
                    | "UV_LINK_MODE"
                    | "NPM_CONFIG_PACKAGE_IMPORT_METHOD"
                    | "PNPM_CONFIG_PACKAGE_IMPORT_METHOD"
            ),
        "this environment variable controls process execution or private workspace outputs"
    );
    Ok(())
}

fn validate(manager: &str, config: &Registration) -> Result<()> {
    ensure!(
        identifier(manager)? == manager,
        "invalid stored manager name"
    );
    if let Some(env) = &config.cache_env {
        validate_environment(env)?;
    }
    ensure!(
        config.cache_args.len() <= 32
            && config.cache_args.iter().all(|arg| !arg.is_empty()
                && arg.len() <= 4096
                && !arg.chars().any(char::is_control)),
        "invalid cache arguments"
    );
    ensure!(
        config.cache_args.is_empty() || config.cache_args.iter().any(|arg| arg.contains("{cache}")),
        "cache arguments must include a {{cache}} placeholder"
    );
    if let Some(spec) = &config.install {
        ensure!(
            spec.manager == manager,
            "installed dependency manager name does not match"
        );
        validate_relative(&spec.directory)?;
        ensure!(
            !spec.key.trim().is_empty()
                && spec.key.len() <= 1024
                && !spec.key.chars().any(char::is_control),
            "a compatibility key of 1 to 1024 bytes is required for install reuse"
        );
        ensure!(
            !spec.inputs.is_empty() && spec.inputs.len() <= 128,
            "declare between 1 and 128 tracked install inputs"
        );
        let directory = spec.directory.to_lowercase();
        let mut paths = BTreeSet::new();
        for input in &spec.inputs {
            validate_relative(input)?;
            let input = input.to_lowercase();
            ensure!(paths.insert(input.clone()), "duplicate install input");
            ensure!(
                !Path::new(&input).starts_with(&directory)
                    && !Path::new(&directory).starts_with(&input),
                "install inputs and installed dependencies must have independent paths"
            );
        }
    }
    Ok(())
}

impl Managers {
    fn read(project: &Project) -> Result<(Self, Option<Vec<u8>>)> {
        project.require_private_state()?;
        let path = project.state.join(REGISTRY_FILE);
        if !registry::exists(&path)? {
            return Ok((
                Self {
                    version: 1,
                    project: project.id().into(),
                    managers: BTreeMap::new(),
                },
                None,
            ));
        }
        let bytes = cache::read_private_file(&path, MAX_BYTES as u64)?;
        let state: Self =
            serde_json::from_slice(&bytes).context("read package manager registry")?;
        ensure!(
            state.version == 1
                && state.project == project.id()
                && state.managers.len() <= MAX_MANAGERS,
            "invalid project package manager registry"
        );
        for (name, entry) in &state.managers {
            validate(name, entry)?;
        }
        Ok((state, Some(bytes)))
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            self.managers.len() <= MAX_MANAGERS && bytes.len() <= MAX_BYTES,
            "project package manager registry is full"
        );
        Ok(bytes)
    }

    // The project lock serializes Graft writers. Recheck the source before
    // replacing it, since validating an install may take appreciable time.
    fn publish(project: &Project, previous: Option<Vec<u8>>, bytes: &[u8]) -> Result<()> {
        let path = project.state.join(REGISTRY_FILE);
        if let Some(previous) = previous {
            ensure!(
                cache::read_private_file(&path, MAX_BYTES as u64)? == previous,
                "package manager registry changed"
            );
            if previous == bytes {
                return Ok(());
            }
        } else {
            ensure!(
                !registry::exists(&path)?,
                "package manager registry appeared during registration"
            );
        }
        registry::atomic(&path, bytes)
    }
}

fn description(project: &Project, manager: &str, entry: &Registration) -> Value {
    json!({
        "manager": manager, "cache_dir": cache::registered_cache_path(project, manager),
        "cache_env": entry.cache_env, "cache_args": entry.cache_args,
        "install_dir": entry.install.as_ref().map(|spec| &spec.directory),
        "inputs": entry.install.as_ref().map_or(&[][..], |spec| &spec.inputs),
        "key": entry.install.as_ref().map(|spec| &spec.key)
    })
}

pub(crate) fn list(project: &Project) -> Result<Value> {
    let (state, _) = Managers::read(project)?;
    let managers: Vec<_> = state
        .managers
        .iter()
        .map(|(name, entry)| description(project, name, entry))
        .collect();
    Ok(json!({ "managers": managers }))
}

pub(crate) fn register(
    project: &Project,
    checkout: &Path,
    args: &RegisterManager,
) -> Result<Value> {
    let name = identifier(&args.manager)?;
    let install = if let Some(directory) = &args.install_dir {
        let mut inputs = args.inputs.clone();
        inputs.sort();
        Some(InstallSpec {
            manager: name.clone(),
            directory: directory.clone(),
            inputs,
            key: args
                .key
                .as_deref()
                .context("an install directory requires a compatibility key")?
                .into(),
        })
    } else {
        ensure!(
            args.inputs.is_empty() && args.key.is_none(),
            "install inputs and compatibility key require an install directory"
        );
        None
    };
    let entry = Registration {
        cache_env: args.cache_env.clone(),
        cache_args: args.cache_args.clone(),
        install,
    };
    validate(&name, &entry)?;
    let (mut state, previous) = Managers::read(project)?;
    state.managers.insert(name.clone(), entry.clone());
    let bytes = state.encode()?;
    cache::prepare_registered_cache(project, &name)?;
    let mut result = description(project, &name, &entry);
    if let Some(spec) = &entry.install {
        result["install"] = layer::seed_registered(project, checkout, spec)?;
    }
    Managers::publish(project, previous, &bytes)?;
    Ok(result)
}

pub(crate) fn unregister(project: &Project, manager: &str) -> Result<Value> {
    let name = identifier(manager)?;
    let (mut state, previous) = Managers::read(project)?;
    if state.managers.remove(&name).is_some() {
        Managers::publish(project, previous, &state.encode()?)?;
    }
    Ok(json!({"manager":name, "registered":false, "cache_retained":true}))
}

pub(crate) fn attach(project: &Project, checkout: &Path) -> Result<Vec<Value>> {
    let (state, _) = Managers::read(project)?;
    state
        .managers
        .values()
        .filter_map(|entry| entry.install.as_ref())
        .map(|spec| layer::attach_registered(project, checkout, spec))
        .collect()
}

pub(crate) struct ExecutionProfile {
    cache: PathBuf,
    env: Option<String>,
    args: Vec<String>,
}

impl ExecutionProfile {
    pub(crate) fn configure(&self, command: &mut Command) -> Result<()> {
        let cache = dunce::simplified(&self.cache);
        command.env("GRAFT_PACKAGE_CACHE", cache);
        if let Some(env) = &self.env {
            command.env(env, cache);
        }
        if !self.args.is_empty() {
            let path = crate::git::path_str(cache)?;
            command.args(self.args.iter().map(|arg| arg.replace("{cache}", path)));
        }
        Ok(())
    }
}

pub(crate) fn execution(
    project: &Project,
    selection: Option<&str>,
    program: &str,
) -> Result<Option<ExecutionProfile>> {
    let name = if let Some(selection) = selection {
        identifier(selection)?
    } else {
        let Some(file) = Path::new(program)
            .file_name()
            .and_then(|file| file.to_str())
        else {
            return Ok(None);
        };
        let mut file = file.to_ascii_lowercase();
        if cfg!(windows)
            && let Some((stem, "exe" | "cmd" | "bat")) = file.rsplit_once('.')
        {
            file.truncate(stem.len());
        }
        if identifier(&file).is_err() {
            return Ok(None);
        }
        file
    };
    let (mut state, _) = Managers::read(project)?;
    let Some(entry) = state.managers.remove(&name) else {
        ensure!(
            selection.is_none(),
            "package manager {name} is not registered in this project"
        );
        return Ok(None);
    };
    Ok(Some(ExecutionProfile {
        cache: cache::prepare_registered_cache(project, &name)?,
        env: entry.cache_env,
        args: entry.cache_args,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_paths_are_relative_and_portable() {
        for path in ["vendor/packages", "ruby gems/工具", "nested/manifest.lock"] {
            assert!(validate_relative(path).is_ok(), "{path}");
        }
        for path in [
            "",
            ".",
            "..",
            "../bin",
            "/a",
            "a//b",
            "cache/",
            ".GiT/objects",
            "C:drive",
            "vendor\\lib",
            "AUX.txt",
            "LPT²",
            "git~1",
            "a./b",
            ".g\u{200d}it/config",
        ] {
            assert!(validate_relative(path).is_err(), "{path}");
        }
    }

    #[test]
    fn cache_environment_preserves_workspace_locations() {
        for name in [
            "NUGET_PACKAGES",
            "_MY_CACHE_ROOT",
            "my_cache42",
            "CARGO_HOME",
        ] {
            assert!(validate_environment(name).is_ok(), "{name}");
        }
        for name in [
            "9CACHE",
            "BAD=VALUE",
            "pAth",
            "Git_Dir",
            "CARGO_TARGET_DIR",
            "GIT_GRAFT_LEASE",
            "LD_PRELOAD",
        ] {
            assert!(validate_environment(name).is_err(), "{name}");
        }
    }

    #[test]
    fn foreign_registry_survives_registration_and_removal() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        crate::git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let (project, _lock) = Project::open(tmp.path())?;
        let foreign = br#"{"version":1,"project":"other-project","managers":{}}"#;
        let path = project.state.join(REGISTRY_FILE);
        registry::atomic(&path, foreign)?;
        let args = serde_json::from_value(json!({"manager":"example"}))?;
        assert!(register(&project, tmp.path(), &args).is_err());
        assert!(unregister(&project, "example").is_err());
        assert_eq!(std::fs::read(path)?, foreign);
        assert!(!cache::registered_cache_path(&project, "example").exists());
        Ok(())
    }
}
