//! Scoped execution and package-manager caches for managed worktrees.
//!
//! Callers must serialize commands within a worktree and require explicit trust
//! before invoking code. Package caches are shared; installed files and build
//! outputs are private to a worktree. No install is performed by `env` or `exec`.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

const OUTPUT_LIMIT: usize = 256 * 1024;

struct Runtime {
    worktree: PathBuf,
    cache: PathBuf,
    private: PathBuf,
    port: u16,
}

impl Runtime {
    fn new(worktree: &Path, state: &Path, port: u16) -> Result<Self> {
        let worktree = worktree.canonicalize().context("resolve worktree")?;
        if !worktree.is_dir() {
            bail!("worktree is not a directory");
        }
        let slug = worktree.file_name().context("worktree has no name")?;
        let state = if state.is_absolute() {
            state.to_path_buf()
        } else {
            std::env::current_dir()?.join(state)
        };
        Ok(Self {
            private: state.join("runtime").join(slug),
            cache: state.join("cache"),
            worktree,
            port,
        })
    }

    fn configure(&self, command: &mut Command) -> Result<()> {
        crate::git::clear_location(command);
        command.env_remove("GIT_GRAFT_LEASE");
        std::fs::create_dir_all(&self.private).context("create private runtime directory")?;
        std::fs::create_dir_all(&self.cache).context("create package cache directory")?;
        let venv = self.private.join("venv");
        let bin = if cfg!(windows) { "Scripts" } else { "bin" };
        let mut paths = vec![
            self.worktree.join("node_modules").join(".bin"),
            venv.join(bin),
        ];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        command
            .current_dir(&self.worktree)
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

    fn run(&self, program: &str, args: &[String]) -> Result<Value> {
        let mut command = Command::new(program);
        command.args(args);
        self.run_command(command, program)
    }

    fn run_command(&self, mut command: Command, program: &str) -> Result<Value> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        self.configure(&mut command)?;
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
        Ok(Command::new("npm"))
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

/// Report only values managed here, never the inherited process environment.
pub fn env(worktree: &Path, state: &Path, port: u16) -> Result<Value> {
    let runtime = Runtime::new(worktree, state, port)?;
    Ok(json!({"worktree": runtime.worktree, "port": port,
        "cache_dir": runtime.cache, "runtime_dir": runtime.private,
        "node_modules": runtime.worktree.join("node_modules"),
        "cargo_target_dir": runtime.private.join("cargo-target"),
        "python_environment": runtime.private.join("venv")}))
}

/// Execute an argv vector directly in the worktree; no shell interpretation occurs.
pub fn exec(worktree: &Path, state: &Path, port: u16, argv: &[String]) -> Result<Value> {
    let (program, args) = argv.split_first().context("command must not be empty")?;
    if program.is_empty() {
        bail!("command must not be empty");
    }
    Runtime::new(worktree, state, port)?.run(program, args)
}

/// Install locked dependencies, or add the supplied packages, using native caches.
pub fn dependency(
    worktree: &Path,
    state: &Path,
    port: u16,
    manager: &str,
    packages: &[String],
    offline: bool,
) -> Result<Value> {
    if packages.iter().any(|p| p.is_empty() || p.starts_with('-')) {
        bail!("package names must be nonempty and cannot begin with '-'");
    }
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
    let runtime = Runtime::new(worktree, state, port)?;
    if manager == "bun" && offline {
        // Older Bun versions predate strict offline operation. Fail closed if
        // the installed executable does not advertise the option.
        let help = runtime.run("bun", &["install".into(), "--help".into()])?;
        if help["success"] != true || !help["stdout"].as_str().unwrap_or("").contains("--offline") {
            bail!("installed Bun does not advertise strict --offline support");
        }
    }
    #[cfg(windows)]
    {
        if manager == "pnpm" {
            let paths = std::env::var_os("PATH").context("PATH is unavailable")?;
            let binary = std::env::split_paths(&paths)
                .map(|dir| dir.join("pnpm.exe"))
                .find(|path| path.is_file())
                .context(
                    "pnpm requires a native pnpm.exe on PATH; batch wrappers are unsupported",
                )?;
            let mut command = Command::new(binary);
            command.args(&args);
            return runtime.run_command(command, "pnpm");
        }
    }
    if manager == "npm" {
        let mut command = npm_command()?;
        command.args(&args);
        return runtime.run_command(command, "npm");
    }
    runtime.run(manager, &args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn configuration_scrubs_git_location_and_lease_and_copies_uv_files() -> Result<()> {
        let root = tempdir()?;
        let worktree = root.path().join("worktree");
        std::fs::create_dir(&worktree)?;
        let runtime = Runtime::new(&worktree, &root.path().join("state"), 1000)?;
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
        let report_a = env(&a, &state, 1234)?;
        let report_b = env(&b, &state, 1235)?;
        assert_ne!(report_a["cargo_target_dir"], report_b["cargo_target_dir"]);
        assert_ne!(
            report_a["python_environment"],
            report_b["python_environment"]
        );
        let result = exec(&a, &state, 1234, &["sh".into(), "-c".into(),
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
        assert!(!state.join("runtime").join("b").exists());
        Ok(())
    }

    #[test]
    fn pnpm_uses_the_managed_store() -> Result<()> {
        if !on_path("pnpm") {
            eprintln!("pnpm unavailable; skipping fixture");
            return Ok(());
        }
        let root = tempdir()?;
        let worktree = root.path().join("worktree");
        let state = root.path().join("state");
        fs::create_dir_all(&worktree)?;
        let result = exec(
            &worktree,
            &state,
            1000,
            &["pnpm".into(), "store".into(), "path".into()],
        )?;
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
    fn npm_cache_enables_offline_install_into_independent_worktrees() -> Result<()> {
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
        let populate = exec(
            &a,
            &state,
            1001,
            &[
                "npm".into(),
                "cache".into(),
                "add".into(),
                tarball.to_string_lossy().into_owned(),
            ],
        )?;
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
        let failed = dependency(
            &cold,
            &root.path().join("empty-state"),
            1000,
            "npm",
            &[],
            true,
        )?;
        assert_eq!(failed["success"], false, "cold cache must fail offline");
        for (dir, port) in [(&a, 1001), (&b, 1002)] {
            let installed = dependency(dir, &state, port, "npm", &[], true)?;
            assert_eq!(installed["success"], true, "{installed}");
        }
        let a_file = a.join("node_modules/graft-fixture/index.js");
        let b_file = b.join("node_modules/graft-fixture/index.js");
        fs::write(&a_file, "modified")?;
        assert_eq!(fs::read_to_string(&b_file)?, "module.exports = 42;\n");
        Ok(())
    }
}
