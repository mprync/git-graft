//! Run programs with workspace-private build outputs and project-wide package caches.

mod managers;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

const OUTPUT_LIMIT: usize = 256 * 1024;

pub(crate) struct Runtime {
    workspace: PathBuf,
    cache: PathBuf,
    private: PathBuf,
    port: u16,
}

impl Runtime {
    /// A workspace keeps its build outputs across moves, archival and restoration.
    pub(crate) fn new(workspace: &Path, state: &Path, port: u16, id: &str) -> Result<Self> {
        let id = uuid::Uuid::parse_str(id)?.to_string();
        let parent = state.join("workspaces").join("runtime");
        let private = parent.join(id);
        Ok(Self {
            workspace: workspace.canonicalize()?,
            private,
            cache: state.join("cache"),
            port,
        })
    }

    pub(crate) fn prepare_private_directory(&self) -> Result<()> {
        let parent = self
            .private
            .parent()
            .context("private runtime path has no parent")?;
        crate::registry::dir(parent)?;
        crate::registry::dir(&self.private)
    }

    fn configure(&self, command: &mut Command) -> Result<()> {
        crate::git::clear_location(command);
        command.env_remove("GIT_GRAFT_LEASE");
        command.env_remove("GRAFT_PACKAGE_CACHE");
        let venv = self.private.join("venv");
        let bin = if cfg!(windows) { "Scripts" } else { "bin" };
        let mut paths = vec![
            self.workspace.join("node_modules").join(".bin"),
            venv.join(bin),
        ];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        command
            .current_dir(&self.workspace)
            .env(
                "PATH",
                std::env::join_paths(paths).context("construct executable PATH")?,
            )
            .env("PORT", self.port.to_string())
            .env("CARGO_HOME", self.cache.join("cargo"))
            .env("CARGO_TARGET_DIR", self.private.join("cargo-target"))
            .env("CARGO_BUILD_BUILD_DIR", self.private.join("cargo-build"))
            .env("npm_config_cache", self.cache.join("npm"))
            .env("npm_config_store_dir", self.cache.join("pnpm"))
            .env("PNPM_CONFIG_STORE_DIR", self.cache.join("pnpm"))
            .env("PNPM_CONFIG_PACKAGE_IMPORT_METHOD", "clone-or-copy")
            .env("npm_config_package_import_method", "clone-or-copy")
            .env("BUN_INSTALL_CACHE_DIR", self.cache.join("bun"))
            .env("UV_CACHE_DIR", self.cache.join("uv"))
            // uv can fall back from clone to hardlinks on non-CoW volumes.
            .env("UV_LINK_MODE", "copy")
            .env("UV_PROJECT_ENVIRONMENT", venv);
        // Respect an explicitly configured wrapper. sccache's cache is shared,
        // while Cargo's mutable target artifacts remain workspace-private.
        if std::env::var_os("RUSTC_WRAPPER").is_none() && on_path("sccache") {
            command
                .env("RUSTC_WRAPPER", "sccache")
                .env("CARGO_INCREMENTAL", "0")
                .env("SCCACHE_DIR", self.cache.join("sccache"));
        }
        Ok(())
    }

    fn configure_manager(&self, command: &mut Command) -> Result<()> {
        self.configure(command)?;
        // Resolve interpreters used by host package managers on the host PATH.
        // Package managers supply their own local PATH when running package scripts.
        command.env(
            "PATH",
            std::env::var_os("PATH").context("PATH is unavailable")?,
        );
        Ok(())
    }

    fn run_manager(&self, mut command: Command, program: &str) -> Result<Value> {
        self.configure_manager(&mut command)?;
        Self::capture_command(command, program)
    }

    fn capture_command(mut command: Command, program: &str) -> Result<Value> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("start {program}"))?;
        // Drain both pipes concurrently, even after the capture limit, to avoid
        // deadlocks when a child writes large amounts to either stream.
        let stdout = child.stdout.take().context("capture stdout")?;
        let stderr = child.stderr.take().context("capture stderr")?;
        let out_thread = thread::spawn(move || capture(stdout));
        let err_thread = thread::spawn(move || capture(stderr));
        let status = child.wait().context("wait for child")?;
        let (stdout, stdout_truncated) = out_thread
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader failed"))??;
        let (stderr, stderr_truncated) = err_thread
            .join()
            .map_err(|_| anyhow::anyhow!("stderr reader failed"))??;
        Ok(
            json!({"success": status.success(), "exit_code": status.code(),
            "stdout": stdout, "stderr": stderr,
            "stdout_truncated": stdout_truncated, "stderr_truncated": stderr_truncated}),
        )
    }
}

fn capture(mut reader: impl Read) -> io::Result<(String, bool)> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let keep = n.min(OUTPUT_LIMIT.saturating_sub(retained.len()));
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < n;
    }
    Ok((String::from_utf8_lossy(&retained).into_owned(), truncated))
}

fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let executable = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    std::env::split_paths(&path).any(|dir| dir.join(&executable).is_file())
}

/// Construct npm without relying on Windows batch-file command parsing.
pub(crate) fn npm_command() -> Result<Command> {
    #[cfg(not(windows))]
    {
        managers::command("npm")
    }
    #[cfg(windows)]
    {
        let paths = std::env::var_os("PATH").context("PATH is unavailable")?;
        let (node, cli) = std::env::split_paths(&paths).find_map(|dir| {
            let node = dir.join("node.exe");
            let cli = dir.join("node_modules/npm/bin/npm-cli.js");
            (node.is_file() && cli.is_file()).then_some((node, cli))
        }).context("npm requires node.exe and node_modules/npm/bin/npm-cli.js in the same PATH directory")?;
        let mut command = Command::new(node);
        command.arg(cli);
        Ok(command)
    }
}

impl Runtime {
    fn scoped_npm(&self, attached: bool) -> Result<Command> {
        let root = dunce::simplified(&self.workspace);
        let path = crate::git::path_str(root)?;
        // npm normalizes --prefix values as configuration. Let it discover
        // these particular roots by their native cwd to preserve the spelling.
        let discover = path.trim_end() != path || path.contains("${");
        let manifest = root.join("package.json");
        if discover && !manifest.exists() && !root.join("node_modules").exists() {
            crate::registry::dir(&root.join("node_modules"))?;
        }
        let root_has_workspaces = discover && manifest.is_file() && {
            let contents: Value = serde_json::from_slice(&std::fs::read(manifest)?)?;
            !contents["workspaces"].is_null()
        };
        let command = || -> Result<Command> {
            let mut cmd = npm_command()?;
            cmd.args(["--global=false", "--location=project"])
                .env_remove("npm_config_prefix")
                .env_remove("NPM_CONFIG_PREFIX");
            if !discover {
                cmd.arg("--prefix").arg(root);
            }
            if attached || (discover && !root_has_workspaces) {
                cmd.arg("--workspaces=false");
            }
            Ok(cmd)
        };
        if discover {
            let mut probe = command()?;
            probe.arg("prefix");
            self.configure_manager(&mut probe)?;
            let report = probe
                .stdin(Stdio::null())
                .output()
                .context("locate npm project")?;
            ensure!(
                report.status.success(),
                "locate npm project: {}",
                String::from_utf8_lossy(&report.stderr).trim_end()
            );
            let printed = std::str::from_utf8(&report.stdout)?
                .strip_suffix('\n')
                .context("npm did not return a project directory")?;
            #[cfg(windows)]
            let printed = printed.strip_suffix('\r').unwrap_or(printed);
            ensure!(
                dunce::simplified(Path::new(printed)) == root,
                "npm selected a different project directory"
            );
        }
        command()
    }

    pub(crate) fn environment(&self) -> Value {
        json!({"path": self.workspace, "port": self.port,
            "cache_dir": self.cache, "runtime_dir": self.private,
            "node_modules": self.workspace.join("node_modules"),
            "cargo_target_dir": self.private.join("cargo-target"),
            "python_environment": self.private.join("venv")})
    }

    pub(crate) fn execute_registered(
        &self,
        argv: &[String],
        profile: Option<&crate::package_manager::ExecutionProfile>,
    ) -> Result<Value> {
        let (program, args) = argv.split_first().context("command must not be empty")?;
        ensure!(!program.is_empty(), "command must not be empty");
        let mut command = Command::new(program);
        self.configure(&mut command)?;
        if let Some(profile) = profile {
            profile.configure(&mut command)?;
        }
        command.args(args);
        Self::capture_command(command, program)
    }

    #[cfg(test)]
    fn execute(&self, argv: &[String]) -> Result<Value> {
        self.execute_registered(argv, None)
    }

    pub(crate) fn install(
        &self,
        manager: &str,
        packages: &[String],
        offline: bool,
        attached: bool,
    ) -> Result<Value> {
        if packages.iter().any(|p| p.is_empty() || p.starts_with('-')) {
            bail!("package names must be nonempty and cannot begin with '-'");
        }
        managers::validate(self, manager)?;
        let adding = !packages.is_empty();
        let mut args: Vec<String> = match (manager, adding) {
            ("npm", false) => vec!["ci".into()],
            ("npm", true) => vec!["install".into()],
            ("pnpm", false) => vec![
                "install".into(),
                "--frozen-lockfile".into(),
                "--package-import-method=clone-or-copy".into(),
            ],
            ("pnpm", true) => vec!["add".into(), "--package-import-method=clone-or-copy".into()],
            ("bun", false) => vec![
                "install".into(),
                "--frozen-lockfile".into(),
                "--backend=copyfile".into(),
            ],
            ("bun", true) => vec!["add".into(), "--backend=copyfile".into()],
            ("uv", false) => vec!["sync".into(), "--locked".into()],
            ("uv", true) => vec!["add".into()],
            ("cargo", false) => vec!["fetch".into(), "--locked".into()],
            ("cargo", true) => vec!["add".into()],
            _ => bail!("unsupported package manager: {manager}"),
        };
        if offline {
            args.push("--offline".into());
        }
        args.extend_from_slice(packages);
        if manager == "bun" && offline {
            // Older Bun versions predate strict offline operation. Fail closed if
            // the installed executable does not advertise the option.
            let mut cmd = managers::command("bun")?;
            cmd.args(["install", "--help"]);
            let help = self.run_manager(cmd, "bun")?;
            if help["success"] != true
                || !help["stdout"].as_str().unwrap_or("").contains("--offline")
            {
                bail!("installed Bun does not advertise strict --offline support");
            }
        }
        if manager == "npm" {
            let mut command = self.scoped_npm(attached)?;
            command.args(&args);
            return self.run_manager(command, "npm");
        }
        let mut cmd = managers::command(manager)?;
        cmd.arg(&args[0]);
        managers::scope(self, manager, &mut cmd);
        cmd.args(&args[1..]);
        self.run_manager(cmd, manager)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn runtime(path: &Path, state: &Path, port: u16) -> Result<Runtime> {
        std::fs::create_dir_all(state.join("workspaces"))?;
        crate::registry::dir(&state.join("cache"))?;
        // Distinct identities model two workspaces using the same project cache.
        Runtime::new(
            path,
            state,
            port,
            &uuid::Uuid::from_u128(port.into()).to_string(),
        )
    }

    #[test]
    fn configuration_scrubs_git_location_and_lease_and_copies_uv_files() -> Result<()> {
        let root = tempdir()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let runtime = runtime(&workspace, &root.path().join("state"), 1000)?;
        runtime.prepare_private_directory()?;
        let mut cmd = Command::new("test-program");
        cmd.env("GIT_DIR", "wrong")
            .env("GIT_WORK_TREE", "wrong")
            .env("GIT_GRAFT_LEASE", "secret");
        runtime.configure(&mut cmd)?;
        let envs: std::collections::HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(envs.get(std::ffi::OsStr::new("GIT_DIR")), Some(&None));
        assert_eq!(envs.get(std::ffi::OsStr::new("GIT_WORK_TREE")), Some(&None));
        assert_eq!(
            envs.get(std::ffi::OsStr::new("GIT_GRAFT_LEASE")),
            Some(&None)
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("UV_LINK_MODE")),
            Some(&Some(std::ffi::OsStr::new("copy")))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn execution_is_scoped_and_drains_both_streams() -> Result<()> {
        let root = tempdir()?;
        let a = root.path().join("a");
        let b = root.path().join("b");
        let state = root.path().join("state");
        fs::create_dir_all(&a)?;
        fs::create_dir_all(&b)?;
        let first = runtime(&a, &state, 1234)?;
        let second = runtime(&b, &state, 1235)?;
        let report_a = first.environment();
        let report_b = second.environment();
        first.prepare_private_directory()?;
        assert_ne!(report_a["cargo_target_dir"], report_b["cargo_target_dir"]);
        assert_ne!(
            report_a["python_environment"],
            report_b["python_environment"]
        );
        let result = first.execute(&["sh".into(), "-c".into(),
            "printf '%s\\n%s\\n%s\\n' \"$PWD\" \"$PORT\" \"$CARGO_TARGET_DIR\"; head -c 300000 /dev/zero; head -c 300000 /dev/zero >&2; exit 23".into()])?;
        assert_eq!(result["exit_code"], 23);
        assert_eq!(result["success"], false);
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], true);
        let output = result["stdout"].as_str().unwrap();
        assert!(output.contains(a.canonicalize()?.to_str().unwrap()));
        assert!(output.contains("1234"));
        assert!(output.contains(report_a["cargo_target_dir"].as_str().unwrap()));
        assert!(!report_a.to_string().contains("RUSTC_WRAPPER"));
        assert!(!second.private.exists());
        Ok(())
    }

    #[test]
    fn pnpm_uses_the_managed_store() -> Result<()> {
        if !on_path("pnpm") {
            eprintln!("pnpm unavailable; skipping fixture");
            return Ok(());
        }
        let root = tempdir()?;
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        fs::create_dir_all(&workspace)?;
        let workspace = runtime(&workspace, &state, 1000)?;
        workspace.prepare_private_directory()?;
        let result = workspace.execute(&["pnpm".into(), "store".into(), "path".into()])?;
        assert_eq!(result["success"], true, "{result}");
        assert!(
            result["stdout"]
                .as_str()
                .unwrap()
                .contains(state.join("cache/pnpm").to_str().unwrap())
        );
        Ok(())
    }

    #[test]
    fn npm_cache_enables_offline_install_into_independent_workspaces() -> Result<()> {
        if !on_path("npm") {
            eprintln!("npm unavailable; skipping fixture");
            return Ok(());
        }
        let root = tempdir()?;
        let package = root.path().join("package");
        let a = root.path().join("a");
        let b = root.path().join("b");
        let cold = root.path().join("cold");
        let state = root.path().join("state");
        for dir in [&package, &a, &b, &cold] {
            fs::create_dir_all(dir)?;
        }
        fs::write(
            package.join("package.json"),
            r#"{"name":"graft-fixture","version":"1.0.0"}"#,
        )?;
        fs::write(package.join("index.js"), "module.exports = 42;\n")?;
        let packed = Command::new("npm")
            .args(["pack", "--json", "--ignore-scripts"])
            .current_dir(&package)
            .env("npm_config_cache", root.path().join("pack-cache"))
            .output()?;
        assert!(
            packed.status.success(),
            "{}",
            String::from_utf8_lossy(&packed.stderr)
        );
        let meta: Value = serde_json::from_slice(&packed.stdout)?;
        let integrity = meta[0]["integrity"].as_str().context("pack integrity")?;
        let tarball = package.join(meta[0]["filename"].as_str().context("pack filename")?);
        let first = runtime(&a, &state, 1001)?;
        first.prepare_private_directory()?;
        let populate = first.execute(&[
            "npm".into(),
            "cache".into(),
            "add".into(),
            tarball.to_string_lossy().into_owned(),
        ])?;
        assert_eq!(populate["success"], true, "{populate}");
        // The URL is deliberately unreachable: npm must use the content keyed
        // by integrity, not the fixture tarball or a network registry.
        let manifest =
            json!({"name":"consumer", "version":"1.0.0", "dependencies":{"graft-fixture":"1.0.0"}});
        let lock = json!({"name":"consumer", "version":"1.0.0", "lockfileVersion":3,
            "packages":{"":{"name":"consumer", "version":"1.0.0", "dependencies":{"graft-fixture":"1.0.0"}},
            "node_modules/graft-fixture":{"version":"1.0.0", "resolved":"https://127.0.0.1:1/graft-fixture.tgz", "integrity":integrity}}});
        for dir in [&a, &b, &cold] {
            fs::write(dir.join("package.json"), manifest.to_string())?;
            fs::write(dir.join("package-lock.json"), lock.to_string())?;
        }
        let empty = runtime(&cold, &root.path().join("empty-state"), 1000)?;
        empty.prepare_private_directory()?;
        let failed = empty.install("npm", &[], true, false)?;
        assert_eq!(failed["success"], false, "cold cache must fail offline");
        for (dir, port) in [(&a, 1001), (&b, 1002)] {
            let workspace = runtime(dir, &state, port)?;
            workspace.prepare_private_directory()?;
            let installed = workspace.install("npm", &[], true, false)?;
            assert_eq!(installed["success"], true, "{installed}");
        }
        let a_file = a.join("node_modules/graft-fixture/index.js");
        let b_file = b.join("node_modules/graft-fixture/index.js");
        fs::write(&a_file, "modified")?;
        assert_eq!(fs::read_to_string(&b_file)?, "module.exports = 42;\n");
        Ok(())
    }
}
