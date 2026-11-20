//! Prove where a package manager will write before running an install.

use super::Runtime;
use crate::registry::is_reparse_point;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Resolve against the host PATH before Runtime prepends local script directories.
pub(super) fn command(manager: &str) -> Result<Command> {
    let search = std::env::var_os("PATH").context("PATH is unavailable")?;
    let executable = super::executable(manager);
    let binary = std::env::split_paths(&search)
        .filter(|dir| dir.is_absolute())
        .find_map(|dir| {
            let path = dir.join(&executable);
            let metadata = fs::metadata(&path).ok()?;
            if !metadata.is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 == 0 {
                    return None;
                }
            }
            Some(path)
        })
        .with_context(|| format!("install {executable} on the host PATH"))?;
    Ok(Command::new(binary))
}

/// Keep discovery and install on the same project even when the caller sets overrides.
pub(super) fn scope(runtime: &Runtime, manager: &str, command: &mut Command) {
    let workspace = dunce::simplified(&runtime.workspace);
    match manager {
        "cargo" => {
            command
                .arg("--manifest-path")
                .arg(workspace.join("Cargo.toml"));
        }
        "pnpm" => {
            let adding = command.get_args().next().is_some_and(|arg| arg == "add");
            command
                .args(["--global=false", "--dir"])
                .arg(workspace)
                .env_remove("NPM_CONFIG_WORKSPACE_DIR")
                .env_remove("npm_config_workspace_dir");
            if adding && workspace.join("pnpm-workspace.yaml").is_file() {
                command.arg("--workspace-root");
            }
        }
        "uv" => {
            command
                .arg("--directory")
                .arg(workspace)
                .arg("--project")
                .arg(workspace);
        }
        _ => {}
    }
}

pub(super) fn validate(runtime: &Runtime, manager: &str) -> Result<()> {
    let root = dunce::canonicalize(&runtime.workspace).context("locate managed workspace")?;
    let check = match manager {
        "npm" => npm(runtime, &root),
        "cargo" => cargo(runtime, &root),
        "pnpm" => pnpm(runtime, &root),
        "bun" => bun(runtime, &root),
        "uv" => uv(runtime, &root),
        _ => bail!("unsupported package manager: {manager}"),
    };
    check.with_context(|| format!("cannot prove {manager} writes stay in {}", root.display()))
}

fn discovery(runtime: &Runtime, manager: &str, args: &[&str]) -> Result<Output> {
    let (subcommand, flags) = args.split_first().context("missing discovery command")?;
    let mut probe = command(manager)?;
    probe.arg(subcommand).args(flags).stdin(Stdio::null());
    scope(runtime, manager, &mut probe);
    if manager == "cargo" {
        probe.env("RUSTUP_AUTO_INSTALL", "0");
    }
    if manager == "pnpm" {
        probe.env("COREPACK_ENABLE_NETWORK", "0");
    }
    runtime.configure_manager(&mut probe)?;
    probe
        .output()
        .with_context(|| format!("run {manager} {subcommand}"))
}

fn success(output: &Output, action: &str) -> Result<()> {
    ensure!(
        output.status.success(),
        "{action} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end()
    );
    Ok(())
}

fn npm(runtime: &Runtime, root: &Path) -> Result<()> {
    // The actual invocation pins npm's CLI prefix and local mode, so global
    // and project configuration cannot redirect Arborist's output root.
    npm_outputs(root)?;
    let manifest = root.join("package.json");
    if !endpoint(&manifest, false)? {
        return Ok(());
    }
    let bytes = fs::read(&manifest)?;
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
    let package: Value = serde_json::from_slice(bytes).context("read npm package.json")?;
    if package.get("workspaces").is_none() {
        return Ok(());
    }

    let npm = super::npm_command()?;
    let cli = if cfg!(windows) {
        Path::new(
            npm.get_args()
                .next()
                .context("npm CLI path is unavailable")?,
        )
    } else {
        Path::new(npm.get_program())
    };
    let cli = dunce::canonicalize(cli).context("locate npm workspace mapper")?;
    ensure!(
        cli.file_name().is_some_and(|name| name == "npm-cli.js"),
        "cannot locate npm's workspace mapper through {}. Use the npm executable from a Node.js installation",
        cli.display()
    );
    let mut probe = command("node")?;
    runtime.configure_manager(&mut probe)?;
    probe
        .args(["-e", NPM_MEMBERS, "--"])
        .arg(cli)
        .arg(root)
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH")
        .stdin(Stdio::null());
    let output = probe.output().context("discover npm workspace packages")?;
    success(&output, "discover npm workspace packages")?;
    let projects: Vec<PathBuf> =
        serde_json::from_slice(&output.stdout).context("read npm workspace packages")?;
    for project in projects {
        let project = confined_path(root, &project)?;
        required_manifest(project.join("package.json"))?;
        npm_outputs(&project)?;
    }
    Ok(())
}

fn npm_outputs(project: &Path) -> Result<()> {
    files(project, &["package-lock.json", "npm-shrinkwrap.json"])?;
    let modules = project.join("node_modules");
    node_modules(&modules)?;
    files(&modules, &[".package-lock.json"])?;
    endpoint(&modules.join(".store"), true)?;
    Ok(())
}

const NPM_MEMBERS: &str = r#"
const [cli, cwd] = process.argv.slice(1);
const { createRequire } = require("node:module");
const fromNpm = createRequire(cli);
const fromArborist = createRequire(fromNpm.resolve("@npmcli/arborist"));
const mapWorkspaces = fromArborist("@npmcli/map-workspaces");
const pkg = JSON.parse(require("node:fs").readFileSync(cwd + "/package.json", "utf8").replace(/^\uFEFF/, ""));
mapWorkspaces({cwd, pkg}).then(result => {
  console.log(JSON.stringify([...result.values()]));
}).catch(error => {
  console.error(error.message);
  process.exitCode = 1;
});
"#;

fn cargo(runtime: &Runtime, root: &Path) -> Result<()> {
    required_manifest(root.join("Cargo.toml"))?;
    let output = discovery(
        runtime,
        "cargo",
        &["locate-project", "--workspace", "--offline"],
    )?;
    success(&output, "cargo locate-project --workspace")?;
    let manifest: Value =
        serde_json::from_slice(&output.stdout).context("read Cargo workspace location")?;
    let path = manifest["root"]
        .as_str()
        .context("Cargo did not report a workspace manifest")?;
    let path = Path::new(path);
    let parent = path.parent().context("Cargo manifest has no parent")?;
    let workspace = confined_path(root, parent)?;
    ensure!(
        path.file_name().is_some_and(|name| name == "Cargo.toml"),
        "Cargo reported an unexpected workspace manifest at {}",
        path.display()
    );
    required_manifest(workspace.join("Cargo.toml"))?;
    files(&workspace, &["Cargo.lock"])?;
    // Cargo's build and target paths are assigned by Runtime::configure.
    endpoint(&runtime.private.join("cargo-target"), true)?;
    endpoint(&runtime.private.join("cargo-build"), true)?;
    Ok(())
}

fn pnpm(runtime: &Runtime, root: &Path) -> Result<()> {
    let local_workspace = endpoint(&root.join("pnpm-workspace.yaml"), false)?;
    pnpm_manifest(root, !local_workspace)?;
    if !local_workspace {
        for ancestor in root.ancestors().skip(1) {
            let path = ancestor.join("pnpm-workspace.yaml");
            ensure!(
                !endpoint(&path, false)?,
                "pnpm would use the enclosing workspace at {}. Run from a graft with its own pnpm workspace root",
                path.display()
            );
        }
    }
    let raw = discovery(runtime, "pnpm", &["config", "list", "--json"])?;
    success(&raw, "pnpm config list")?;
    let config: Value =
        serde_json::from_slice(&raw.stdout).context("read effective pnpm configuration")?;
    ensure!(
        config.is_object(),
        "pnpm did not report effective configuration"
    );
    ensure!(
        pnpm_setting(&config, "symlinkedlockfiletargetdirs")
            .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty)),
        "pnpm symlinkedLockfileTargetDirs can redirect lockfiles. Remove that setting before using add-dep"
    );
    if let Some(settings) = pnpm_setting(&config, "packageconfigs") {
        validate_pnpm_package_configs(settings)?;
    }
    let lock_root = configured_path(root, root, &config, "lockfiledir", ".")?;
    let mut projects = vec![root.to_path_buf()];
    if local_workspace {
        let output = discovery(runtime, "pnpm", &["list", "-r", "--depth", "-1", "--json"])?;
        success(&output, "pnpm list workspace projects")?;
        let members: Vec<Value> =
            serde_json::from_slice(&output.stdout).context("read pnpm workspace projects")?;
        ensure!(
            !members.is_empty(),
            "pnpm did not report any workspace projects"
        );
        for member in members {
            let path = member["path"]
                .as_str()
                .context("pnpm workspace project has no path")?;
            let project = confined_path(root, Path::new(path))?;
            pnpm_manifest(&project, project != root)?;
            if project != root {
                projects.push(project);
            }
        }
    }
    // A shared lockfile lives at lockfileDir. A workspace with independent
    // project locks uses the project paths as well.
    for project in projects.iter().chain(std::iter::once(&lock_root)) {
        if project.is_dir() {
            for entry in fs::read_dir(project)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("pnpm-lock.") && name.ends_with(".yaml") {
                    endpoint(&entry.path(), false)?;
                }
            }
        }
        files(
            project,
            &[
                "pnpm-lock.yaml",
                ".pnp.cjs",
                ".pnp.loader.mjs",
                ".pnp.data.json",
            ],
        )?;
        let modules = configured_path(root, project, &config, "modulesdir", "node_modules")?;
        node_modules(&modules)?;
        files(&modules, &[".modules.yaml"])?;
        let store = modules.join(".pnpm");
        endpoint(&store, true)?;
        files(&store, &["lock.yaml"])?;
        node_modules(&store.join("node_modules"))?;
        for name in ["virtualstoredir", "globalvirtualstoredir", "bin"] {
            if pnpm_setting(&config, name).is_some() {
                let path = configured_path(root, project, &config, name, ".")?;
                if name == "bin" {
                    endpoint(&path, true)?;
                } else {
                    files(&path, &["lock.yaml"])?;
                    node_modules(&path.join("node_modules"))?;
                }
            }
        }
    }
    Ok(())
}

fn pnpm_manifest(root: &Path, required: bool) -> Result<()> {
    let names = ["package.json", "package.yaml", "package.json5"];
    let mut found = false;
    for name in names {
        found |= endpoint(&root.join(name), false)?;
    }
    ensure!(
        found || !required,
        "create a pnpm package manifest in {}",
        root.display()
    );
    Ok(())
}

fn pnpm_key(key: &str) -> String {
    key.replace('-', "").to_ascii_lowercase()
}

fn pnpm_setting<'a>(config: &'a Value, name: &str) -> Option<&'a Value> {
    config
        .as_object()?
        .iter()
        .find_map(|(key, value)| (pnpm_key(key) == name && !value.is_null()).then_some(value))
}

fn validate_pnpm_package_configs(value: &Value) -> Result<()> {
    if let Some(settings) = value.as_object() {
        for (key, setting) in settings {
            ensure!(
                !matches!(
                    pnpm_key(key).as_str(),
                    "lockfiledir"
                        | "modulesdir"
                        | "virtualstoredir"
                        | "globalvirtualstoredir"
                        | "bin"
                ),
                "pnpm packageConfigs contains {key}. Move path settings to the workspace configuration so add-dep can verify them"
            );
            validate_pnpm_package_configs(setting)?;
        }
    } else if let Some(values) = value.as_array() {
        for setting in values {
            validate_pnpm_package_configs(setting)?;
        }
    }
    Ok(())
}

fn configured_path(
    root: &Path,
    project: &Path,
    config: &Value,
    name: &str,
    default: &str,
) -> Result<PathBuf> {
    let configured = match pnpm_setting(config, name) {
        Some(value) => value
            .as_str()
            .with_context(|| format!("invalid pnpm path setting {name}"))?,
        None => default,
    };
    let path = project.join(configured);
    confined_path(root, &path).with_context(|| format!("check pnpm path setting {name}"))
}

fn bun(runtime: &Runtime, root: &Path) -> Result<()> {
    required_manifest(root.join("package.json"))?;
    let output = discovery(runtime, "bun", &["pm", "bin"])?;
    success(&output, "bun pm bin")?;
    let bin = std::str::from_utf8(&output.stdout)?.trim_end_matches(['\r', '\n']);
    let bin = confined_path(root, Path::new(bin))
        .context("Bun resolved an unconfined node_modules directory")?;
    ensure!(
        bin == root.join("node_modules/.bin"),
        "Bun reported an unexpected bin directory at {}",
        bin.display()
    );
    bun_outputs(root)?;
    for project in bun_members(runtime, root)? {
        let project = confined_path(root, &root.join(project))?;
        required_manifest(project.join("package.json"))?;
        bun_outputs(&project)?;
    }
    Ok(())
}

fn bun_outputs(project: &Path) -> Result<()> {
    files(project, &["bun.lock", "bun.lockb", "yarn.lock"])?;
    let modules = project.join("node_modules");
    node_modules(&modules)?;
    endpoint(&modules.join(".bun"), true)?;
    endpoint(&modules.join(".cache"), true)?;
    Ok(())
}

fn bun_members(runtime: &Runtime, root: &Path) -> Result<Vec<PathBuf>> {
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("package.json"))?)
        .context("read Bun package.json")?;
    let migrates = !endpoint(&root.join("bun.lock"), false)?
        && endpoint(&root.join("pnpm-workspace.yaml"), false)?;
    if manifest["workspaces"].is_null() && !migrates {
        return Ok(Vec::new());
    }
    // Use Bun's own glob syntax. An explicit empty config keeps the discovery
    // script independent of the repository's runtime preloads.
    let config = tempfile::Builder::new().suffix(".toml").tempfile()?;
    let mut config_arg = std::ffi::OsString::from("--config=");
    config_arg.push(config.path());
    let mut probe = command("bun")?;
    runtime.configure_manager(&mut probe)?;
    probe
        .arg(config_arg)
        .args(["--no-install", "-e", BUN_MEMBERS, "--"])
        .arg(root)
        .arg(if migrates { "migrate" } else { "native" })
        .stdin(Stdio::null());
    let output = probe.output().context("discover Bun workspace packages")?;
    success(&output, "discover Bun workspace packages")?;
    serde_json::from_slice(&output.stdout).context("read Bun workspace packages")
}

const BUN_MEMBERS: &str = r#"
const [root, mode] = process.argv.slice(1);
const pkg = await Bun.file(root + "/package.json").json();
const workspaces = Array.isArray(pkg.workspaces) ? pkg.workspaces : pkg.workspaces?.packages;
const migrating = mode === "migrate"
  ? Bun.YAML.parse(await Bun.file(root + "/pnpm-workspace.yaml").text())?.packages : [];
const members = new Set();
for (const pattern of [...workspaces ?? [], ...migrating ?? []]) {
  if (typeof pattern !== "string") throw Error("Invalid Bun workspace pattern");
  // Exclusions can only subtract from the conservative set we inspect.
  if (pattern.startsWith("!")) continue;
  if (/(^|[{,])\//.test(pattern) || pattern.includes("\\") || pattern.includes(":") ||
      pattern.split(/[/{,}]/).some(part => part === "..")) {
    throw Error("Bun workspace pattern can leave the managed directory: " + pattern);
  }
  const base = pattern.replace(/^\.\//, "").replace(/\/$/, "");
  if (!base || base === ".") continue;
  const selectsNodeModules = base.split(/[/{,}]/).includes("node_modules");
  for (const match of new Bun.Glob(base + "/package.json").scanSync({
    cwd: root, dot: true, followSymlinks: true,
  })) {
    // Bun skips node_modules in broad glob walks, but explicit package paths
    // there can still be workspaces.
    if (!selectsNodeModules && match.split("/").includes("node_modules")) continue;
    members.add(match.slice(0, -"/package.json".length));
  }
}
console.log(JSON.stringify([...members]));
"#;

fn uv(runtime: &Runtime, root: &Path) -> Result<()> {
    let manifest = root.join("pyproject.toml");
    required_manifest(manifest.clone())?;
    let project = read_pyproject(&manifest)?;
    // uv's project option sets the discovery start, but uv still checks for
    // workspace membership in the first enclosing pyproject.toml.
    if let Some(workspace) = uv_workspace(&project) {
        let excludes = uv_patterns(root, workspace, "exclude")?;
        for pattern in uv_patterns(root, workspace, "members")? {
            for member in glob::glob(pattern.as_str())? {
                let member = member?;
                if excludes.iter().any(|pattern| pattern.matches_path(&member))
                    || !fs::metadata(&member)?.is_dir()
                {
                    continue;
                }
                let member = confined_path(root, &member)?;
                endpoint(&member.join("pyproject.toml"), false)?;
            }
        }
    } else {
        for ancestor in root.ancestors().skip(1) {
            let manifest = ancestor.join("pyproject.toml");
            if !endpoint(&manifest, false)? {
                continue;
            }
            let project = read_pyproject(&manifest)?;
            if let Some(workspace) = uv_workspace(&project) {
                let membership = uv_patterns(ancestor, workspace, "members")?;
                let excludes = uv_patterns(ancestor, workspace, "exclude")?;
                ensure!(
                    !membership.iter().any(|pattern| pattern.matches_path(root))
                        || excludes.iter().any(|pattern| pattern.matches_path(root)),
                    "uv would write the lockfile in the enclosing workspace declared at {}. Use a graft with its own uv workspace root",
                    manifest.display()
                );
            }
            break;
        }
    }
    files(root, &["uv.lock"])?;
    uv_environment(&runtime.private.join("venv"))
}

fn read_pyproject(path: &Path) -> Result<toml::Value> {
    let contents = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("parse {}", path.display()))
}

fn uv_workspace(project: &toml::Value) -> Option<&toml::Value> {
    project.get("tool")?.get("uv")?.get("workspace")
}

fn uv_patterns(root: &Path, workspace: &toml::Value, key: &str) -> Result<Vec<glob::Pattern>> {
    let Some(values) = workspace.get(key) else {
        return Ok(Vec::new());
    };
    let values = values
        .as_array()
        .with_context(|| format!("uv workspace {key} must be an array"))?;
    let root = root.to_str().context("uv workspace path is not UTF-8")?;
    let escaped = PathBuf::from(glob::Pattern::escape(root));
    values
        .iter()
        .map(|value| {
            let pattern = value
                .as_str()
                .with_context(|| format!("uv workspace {key} must contain paths"))?;
            let joined = escaped.join(pattern);
            let mut path = PathBuf::new();
            for component in joined.components() {
                match component {
                    Component::CurDir => {}
                    Component::ParentDir => {
                        path.pop();
                    }
                    _ => path.push(component.as_os_str()),
                }
            }
            glob::Pattern::new(path.to_str().context("uv workspace pattern is not UTF-8")?)
                .with_context(|| format!("invalid uv workspace {key} pattern {pattern}"))
        })
        .collect()
}

fn uv_environment(venv: &Path) -> Result<()> {
    endpoint(venv, true)?;
    for relative in [
        "bin",
        "Scripts",
        "Lib",
        "Lib/site-packages",
        "include",
        "Include",
        "share",
    ] {
        endpoint(&venv.join(relative), true)?;
    }
    files(venv, &["pyvenv.cfg"])?;
    let lib = venv.join("lib");
    if endpoint(&lib, true)? {
        for dir in fs::read_dir(&lib)? {
            let dir = dir?;
            if dir.file_name().to_string_lossy().starts_with("python") {
                endpoint(&dir.path(), true)?;
                endpoint(&dir.path().join("site-packages"), true)?;
            }
        }
    }
    Ok(())
}

/// Refuse redirects at each existing component below the canonical workspace.
fn confined_path(root: &Path, path: &Path) -> Result<PathBuf> {
    let path = dunce::simplified(path);
    let relative = path.strip_prefix(root).with_context(|| {
        format!(
            "write location {} is outside the managed workspace",
            path.display()
        )
    })?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            bail!(
                "write location has an unresolved path component: {}",
                path.display()
            );
        };
        current.push(part);
        endpoint(&current, true)?;
    }
    Ok(current)
}

fn node_modules(path: &Path) -> Result<()> {
    endpoint(path, true)?;
    endpoint(&path.join(".bin"), true)?;
    Ok(())
}

fn required_manifest(path: PathBuf) -> Result<()> {
    ensure!(
        endpoint(&path, false)?,
        "create {} in the managed workspace before using add-dep",
        path.display()
    );
    Ok(())
}

fn files(directory: &Path, names: &[&str]) -> Result<()> {
    for name in names {
        endpoint(&directory.join(name), false)?;
    }
    Ok(())
}

fn endpoint(path: &Path, directory: bool) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    let (expected_type, kind) = if directory {
        (metadata.is_dir(), "directory")
    } else {
        (metadata.is_file(), "regular file")
    };
    ensure!(
        !is_reparse_point(&metadata) && expected_type,
        "dependency write location {} must be a private {kind} without symlinks or reparse points",
        path.display(),
    );
    if !directory {
        ensure!(
            single_link(path, &metadata)?,
            "dependency write location {} is shared by a hardlink. Replace it with a private copy",
            path.display()
        );
    }
    Ok(true)
}

#[cfg(unix)]
fn single_link(_: &Path, metadata: &fs::Metadata) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(metadata.nlink() == 1)
}

#[cfg(windows)]
fn single_link(path: &Path, _: &fs::Metadata) -> Result<bool> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // The handle remains open during the query. Windows initializes the buffer on success.
    let status =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    ensure!(
        status != 0,
        "inspect hardlinks for {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    Ok(unsafe { information.assume_init() }.nNumberOfLinks == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const JSON_PROJECT: &str = r#"{"name":"fixture","version":"1.0.0"}"#;
    const UV_PROJECT: &str =
        "[project]\nname = 'fixture'\nversion = '1.0.0'\nrequires-python = '>=3.10'\n";

    fn fixture() -> Result<(TempDir, Runtime)> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("parent/graft");
        let state = fixture.path().join("state");
        fs::create_dir_all(&root)?;
        fs::create_dir_all(state.join("workspaces"))?;
        fs::create_dir(state.join("cache"))?;
        let runtime = Runtime::new(&root, &state, 1, &uuid::Uuid::nil().to_string())?;
        runtime.prepare_private_directory()?;
        Ok((fixture, runtime))
    }

    fn crate_manifest(root: &Path, name: &str, extra: &str) -> Result<()> {
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/lib.rs"), "")?;
        fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname = '{name}'\nversion = '1.0.0'\nedition = '2021'\n{extra}"),
        )?;
        Ok(())
    }

    fn offline_add(runtime: &Runtime, manager: &str, arguments: &[&str]) -> Result<()> {
        validate(runtime, manager)?;
        let mut add = command(manager)?;
        add.arg("add");
        scope(runtime, manager, &mut add);
        runtime.configure_manager(&mut add)?;
        add.arg("--offline")
            .args(arguments)
            .env("COREPACK_ENABLE_NETWORK", "0")
            .stdin(Stdio::null());
        let output = add.output()?;
        success(&output, &format!("{manager} offline add"))
    }

    #[test]
    fn empty_graft_cannot_inherit_enclosing_manifest() -> Result<()> {
        let (_fixture, runtime) = fixture()?;
        let parent = runtime.workspace.parent().unwrap();
        for (manager, manifest) in [
            ("cargo", "Cargo.toml"),
            ("pnpm", "package.json"),
            ("bun", "package.json"),
            ("uv", "pyproject.toml"),
        ] {
            let sentinel = parent.join(manifest);
            fs::write(&sentinel, "outside sentinel")?;
            let error = validate(&runtime, manager).unwrap_err();
            assert!(format!("{error:#}").contains("create"), "{error:#}");
            assert_eq!(fs::read_to_string(sentinel)?, "outside sentinel");
        }
        validate(&runtime, "npm")?;
        Ok(())
    }

    #[test]
    fn npm_declared_members_must_be_private() -> Result<()> {
        if super::super::npm_command().is_err() || command("node").is_err() {
            return Ok(());
        }
        let (_fixture, runtime) = fixture()?;
        let root = &runtime.workspace;
        let child = root.join("packages/child");
        fs::create_dir_all(&child)?;
        fs::write(child.join("package.json"), JSON_PROJECT)?;
        fs::write(
            root.join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        )?;
        validate(&runtime, "npm")?;

        let outside = root.parent().unwrap().join("outside");
        fs::create_dir(&outside)?;
        fs::write(outside.join("package.json"), JSON_PROJECT)?;
        fs::hard_link(
            outside.join("package.json"),
            child.join("package-lock.json"),
        )?;
        let error = validate(&runtime, "npm").unwrap_err();
        assert!(format!("{error:#}").contains("hardlink"), "{error:#}");
        fs::write(
            root.join("package.json"),
            r#"{"name":"root","workspaces":["../outside"]}"#,
        )?;
        let error = validate(&runtime, "npm").unwrap_err();
        assert!(
            format!("{error:#}").contains("outside the managed workspace"),
            "{error:#}"
        );
        Ok(())
    }

    #[test]
    fn cargo_workspace_root_must_be_private() -> Result<()> {
        if command("cargo").is_err() {
            return Ok(());
        }
        let (_fixture, runtime) = fixture()?;
        let root = &runtime.workspace;
        let parent = root.parent().unwrap();
        fs::write(
            parent.join("Cargo.toml"),
            "[workspace]\nmembers = ['graft']\n",
        )?;
        fs::write(parent.join("Cargo.lock"), "outside sentinel")?;
        crate_manifest(root, "fixture", "workspace = '..'\n")?;
        let error = validate(&runtime, "cargo").unwrap_err();
        assert!(
            format!("{error:#}").contains("outside the managed workspace"),
            "{error:#}"
        );
        assert_eq!(
            fs::read_to_string(parent.join("Cargo.lock"))?,
            "outside sentinel"
        );

        crate_manifest(root, "fixture", "\n[workspace]\nmembers = ['dep']\n")?;
        crate_manifest(&root.join("dep"), "managed-dep", "")?;
        offline_add(&runtime, "cargo", &["managed-dep"])?;
        assert!(fs::read_to_string(root.join("Cargo.toml"))?.contains("managed-dep"));
        assert!(root.join("Cargo.lock").is_file());
        assert_eq!(
            fs::read_to_string(parent.join("Cargo.lock"))?,
            "outside sentinel"
        );
        Ok(())
    }

    #[test]
    fn pnpm_workspace_root_and_configured_paths_must_be_private() -> Result<()> {
        if command("pnpm").is_err() {
            return Ok(());
        }
        let (_fixture, runtime) = fixture()?;
        let root = &runtime.workspace;
        let parent = root.parent().unwrap();
        fs::write(parent.join("pnpm-workspace.yaml"), "packages: ['graft']\n")?;
        fs::write(parent.join("pnpm-lock.yaml"), "outside sentinel")?;
        fs::write(root.join("package.json"), JSON_PROJECT)?;
        let error = validate(&runtime, "pnpm").unwrap_err();
        assert!(
            format!("{error:#}").contains("enclosing workspace"),
            "{error:#}"
        );

        let dep = root.join("dep");
        fs::create_dir(&dep)?;
        fs::write(
            dep.join("package.json"),
            r#"{"name":"managed-dep","version":"1.0.0"}"#,
        )?;
        fs::write(root.join("pnpm-workspace.yaml"), "packages: ['dep']\n")?;
        fs::write(root.join(".npmrc"), "lockfile-dir=..\n")?;
        fs::write(
            root.join("pnpm-workspace.yaml"),
            "packages: ['dep']\nlockfileDir: '..'\n",
        )?;
        let error = validate(&runtime, "pnpm").unwrap_err();
        assert!(format!("{error:#}").contains("lockfiledir"), "{error:#}");
        fs::write(root.join(".npmrc"), "")?;
        fs::write(root.join("pnpm-workspace.yaml"), "packages: ['dep']\n")?;
        offline_add(&runtime, "pnpm", &["file:./dep"])?;
        assert!(root.join("pnpm-lock.yaml").is_file());
        assert_eq!(
            fs::read_to_string(parent.join("pnpm-lock.yaml"))?,
            "outside sentinel"
        );
        Ok(())
    }

    #[test]
    fn bun_workspace_root_and_members_must_be_private() -> Result<()> {
        if command("bun").is_err() {
            return Ok(());
        }
        let (_fixture, runtime) = fixture()?;
        let root = &runtime.workspace;
        let parent = root.parent().unwrap();
        let outside = parent.join("outside");
        fs::create_dir(&outside)?;
        fs::write(outside.join("package.json"), JSON_PROJECT)?;
        fs::write(
            parent.join("package.json"),
            r#"{"name":"outside","workspaces":["graft"]}"#,
        )?;
        fs::write(root.join("package.json"), JSON_PROJECT)?;
        let error = validate(&runtime, "bun").unwrap_err();
        assert!(
            format!("{error:#}").contains("unconfined node_modules"),
            "{error:#}"
        );

        fs::write(parent.join("package.json"), r#"{"name":"outside"}"#)?;
        fs::write(
            root.join("package.json"),
            r#"{"name":"fixture","workspaces":["../outside"]}"#,
        )?;
        let error = validate(&runtime, "bun").unwrap_err();
        assert!(
            format!("{error:#}").contains("workspace pattern can leave"),
            "{error:#}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let modules = root.join("node_modules");
            fs::create_dir(&modules)?;
            symlink(&outside, modules.join("dep"))?;
            fs::write(
                root.join("package.json"),
                r#"{"name":"fixture","workspaces":["node_modules/dep"]}"#,
            )?;
            let error = validate(&runtime, "bun").unwrap_err();
            assert!(
                format!("{error:#}").contains("without symlinks"),
                "{error:#}"
            );
            fs::remove_file(modules.join("dep"))?;
        }

        fs::write(
            root.join("package.json"),
            r#"{"name":"fixture","workspaces":["dep*"]}"#,
        )?;
        let dep = root.join("dep");
        fs::create_dir(&dep)?;
        fs::write(
            dep.join("package.json"),
            r#"{"name":"managed-dep","version":"1.0.0"}"#,
        )?;
        // A direct local directory requires no registry request, including with
        // older Bun versions whose install help does not advertise --offline.
        let mut add = command("bun")?;
        add.args(["add", "--backend=copyfile", "./dep"]);
        validate(&runtime, "bun")?;
        runtime.configure_manager(&mut add)?;
        success(&add.output()?, "Bun local add")?;
        assert!(fs::read_to_string(root.join("package.json"))?.contains("managed-dep"));
        assert_eq!(
            fs::read_to_string(outside.join("package.json"))?,
            JSON_PROJECT
        );
        Ok(())
    }

    #[test]
    fn uv_workspace_root_must_be_private() -> Result<()> {
        if command("uv").is_err() {
            return Ok(());
        }
        let (_fixture, runtime) = fixture()?;
        let root = &runtime.workspace;
        let parent = root.parent().unwrap();
        fs::write(root.join("pyproject.toml"), UV_PROJECT)?;
        validate(&runtime, "uv")?;
        fs::write(
            parent.join("pyproject.toml"),
            "[tool.uv.workspace]\nmembers = ['graft']\n",
        )?;
        fs::write(parent.join("uv.lock"), "outside sentinel")?;
        let error = validate(&runtime, "uv").unwrap_err();
        assert!(
            format!("{error:#}").contains("enclosing workspace"),
            "{error:#}"
        );

        fs::write(
            root.join("pyproject.toml"),
            format!("{UV_PROJECT}\n[tool.uv.workspace]\nmembers = []\n"),
        )?;
        // An impossible marker records a dependency without retrieving a wheel.
        offline_add(
            &runtime,
            "uv",
            &[
                "--no-sync",
                "--no-python-downloads",
                "managed-dep ; python_version < '1.0'",
            ],
        )?;
        assert!(root.join("uv.lock").is_file());
        assert_eq!(
            fs::read_to_string(parent.join("uv.lock"))?,
            "outside sentinel"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn dependency_write_endpoints_reject_symlinks_and_hardlinks() -> Result<()> {
        use std::os::unix::fs::symlink;

        let (_fixture, runtime) = fixture()?;
        let root = &runtime.workspace;
        let external = root.parent().unwrap().join("outside");
        fs::create_dir(&external)?;
        symlink(&external, root.join("node_modules"))?;
        assert!(format!("{:#}", validate(&runtime, "npm").unwrap_err()).contains("node_modules"));
        fs::remove_file(root.join("node_modules"))?;
        fs::write(external.join("package.json"), JSON_PROJECT)?;
        fs::hard_link(external.join("package.json"), root.join("package.json"))?;
        let error = validate(&runtime, "npm").unwrap_err();
        assert!(format!("{error:#}").contains("hardlink"), "{error:#}");
        Ok(())
    }
}
