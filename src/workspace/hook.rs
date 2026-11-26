//! Commands a project's configuration runs when a graft is added, is about
//! to be archived, or is restored. They are defined in `.graftconfig` and
//! `.graftconfig.local` at the root of the primary checkout (see
//! [`crate::config`]); no MCP tool defines one. A hook the repository
//! carries runs only once the person has trusted it; see [`trust`].

use super::registry::{self as grafts, Record, Status};
use crate::{
    api,
    config::{Config, Hook},
    registry::{Held, Project},
    runtime::Runtime,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::File,
    path::{Component, Path},
    process::Command,
};

pub(crate) mod trust;

const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 4096;

/// A point in a graft's life at which a configured command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Event {
    /// After `add` has created a graft.
    PostAdd,
    /// Before `remove`, `merge` or `prune --apply` archives a graft.
    PreArchive,
    /// After `restore` has reopened an archived graft.
    PostRestore,
    /// After `turn take` has granted a holder a turn it did not hold, or
    /// one whose grafts it changed.
    PostTake,
}

impl Event {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::PostAdd => "post-add",
            Self::PreArchive => "pre-archive",
            Self::PostRestore => "post-restore",
            Self::PostTake => "post-take",
        }
    }
}

/// Whether the caller may run hooks. The command line always may. An MCP
/// host may only when it was started with `--allow-exec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Run,
    Skip,
}

/// A hook is a program and its arguments, started without a shell.
pub(crate) fn validate(argv: &[String]) -> Result<()> {
    ensure!(
        !argv.is_empty() && argv.len() <= MAX_ARGUMENTS,
        "a hook is a program and up to {} arguments",
        MAX_ARGUMENTS - 1
    );
    ensure!(!argv[0].is_empty(), "the hook program must not be empty");
    ensure!(
        is_bare_name(&argv[0]) || Path::new(&argv[0]).is_absolute(),
        "the hook program {:?} is a relative path; give an absolute path, or a bare name to look up in PATH. \
         A relative path would name a program inside the graft on Unix and one in the caller's directory on Windows",
        argv[0]
    );
    ensure!(
        argv.iter()
            .all(|arg| arg.len() <= MAX_ARGUMENT_BYTES && !arg.contains('\0')),
        "each hook argument must be at most {MAX_ARGUMENT_BYTES} bytes and contain no NUL character"
    );
    Ok(())
}

/// Whether `program` is a bare name, which the system looks up in `PATH`,
/// rather than a path: one plain component with no separator, `/` on every
/// platform and `\\` on Windows, and on Windows no drive such as `C:`.
fn is_bare_name(program: &str) -> bool {
    let separated = program.contains('/') || cfg!(windows) && program.contains('\\');
    let mut components = Path::new(program).components();
    !separated
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
}

/// The effective hooks of the project whose primary checkout is `primary`,
/// in the order they run, with the file each came from and whether it may
/// run as trust stands in the Graft state directory `state`, which need not
/// exist. Keys under `repositories` that name no repository are reported in
/// `missing_repositories`, when there are any.
pub(crate) fn list(primary: &Path, state: &Path) -> Result<Value> {
    let config = Config::read(primary)?;
    let gate = trust::Gate::of(&config)?;
    let trusted = trust::is_trusted(state, &gate.hooks(&config))?;
    let hooks: Vec<_> = config
        .hooks
        .iter()
        .map(|hook| {
            let mut described = hook.describe();
            described["trusted"] = json!(trusted || !gate.needs_trust(hook));
            described
        })
        .collect();
    let mut listed = json!({ "hooks": hooks });
    if !config.missing_repositories.is_empty() {
        listed["missing_repositories"] = json!(config.missing_repositories);
    }
    // The remotes a new graft gets need trust as hooks do, recorded apart.
    if !config.remotes.is_empty() {
        let trusted = trust::remotes_trusted(state, &gate.remotes(&config))?;
        listed["remotes"] = config
            .remotes
            .iter()
            .map(|name| {
                json!({"name": name, "trusted": trusted || !gate.remote_needs_trust(&config, name)})
            })
            .collect();
    }
    Ok(listed)
}

/// The graft a hook runs in, as the hook's environment describes it.
struct Graft<'a> {
    name: &'a str,
    path: &'a Path,
    primary: &'a Path,
    branch: Option<&'a str>,
    base: Option<&'a str>,
    port: u16,
}

/// The process for a hook: the program started directly in `dir`, with the
/// caller's environment. Like `exec`, it drops the lease and the variables
/// that point Git at another repository.
fn base_command(hook: &Hook, dir: &Path) -> Result<Command> {
    let (program, args) = hook.argv.split_first().context("the hook has no program")?;
    let mut command = Command::new(hook.program.as_deref().unwrap_or(Path::new(program)));
    crate::git::clear_location(&mut command);
    command
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_GRAFT_LEASE")
        .env("GIT_GRAFT_HOOK", hook.event.name())
        .env("GIT_GRAFT_HOOK_NAME", &hook.name)
        .env(
            "GIT_GRAFT_CONFIG_DIR",
            hook.dir.as_deref().unwrap_or(Path::new("")),
        );
    Ok(command)
}

/// The process for a hook of a graft's life: started in the graft, with the
/// graft's description.
fn command(hook: &Hook, graft: &Graft<'_>) -> Result<Command> {
    let mut command = base_command(hook, graft.path)?;
    command
        .env("GIT_GRAFT_NAME", graft.name)
        .env("GIT_GRAFT_PATH", graft.path)
        .env("GIT_GRAFT_PRIMARY", graft.primary)
        .env("GIT_GRAFT_BRANCH", graft.branch.unwrap_or_default())
        .env("GIT_GRAFT_BASE", graft.base.unwrap_or_default())
        .env("GIT_GRAFT_PORT", graft.port.to_string());
    Ok(command)
}

/// The entry a command's `hooks` array carries for one hook.
fn entry(hook: &Hook, status: &str, output: &Value) -> Value {
    json!({
        "name": hook.name,
        "event": hook.event,
        "file": hook.file,
        "status": status,
        "exit_code": output["exit_code"],
        "stdout": output["stdout"].as_str().unwrap_or_default(),
        "stderr": output["stderr"].as_str().unwrap_or_default(),
        "stdout_truncated": output["stdout_truncated"] == true,
        "stderr_truncated": output["stderr_truncated"] == true,
    })
}

/// Run `hook` in `graft` and wait for it, without a time limit. A hook that
/// cannot be started is a failed hook, with the cause as its `reason`, so
/// the caller decides what a failure means.
fn run(hook: &Hook, graft: &Graft<'_>) -> Value {
    finish(hook, command(hook, graft))
}

/// Start the prepared `command` for `hook`, wait for it and report it.
fn finish(hook: &Hook, command: Result<Command>) -> Value {
    let started = command.and_then(|command| Runtime::capture_command(command, &hook.argv[0]));
    match started {
        Ok(output) => {
            let status = if output["success"] == true {
                "ok"
            } else {
                "failed"
            };
            entry(hook, status, &output)
        }
        Err(error) => {
            let mut failed = entry(hook, "failed", &Value::Null);
            failed["reason"] = json!(format!("The hook could not be started: {error:#}"));
            failed
        }
    }
}

/// The entry for a hook that was not run, and why.
fn skipped(hook: &Hook, reason: &str) -> Value {
    let mut skipped = entry(hook, "skipped", &Value::Null);
    skipped["reason"] = json!(reason);
    skipped
}

const NOT_ALLOWED: &str = "The MCP host was not started with --allow-exec, so the hook was not run";

/// A turn a `post-take` hook is told about.
pub(crate) struct Taken<'a> {
    /// The checkout of the project that keeps the turn, where the hook runs.
    pub primary: &'a Path,
    pub resource: &'a str,
    pub holder: &'a str,
    /// The paths of the grafts the holder named.
    pub grafts: &'a [std::path::PathBuf],
}

/// Run the `post-take` `hooks` for the turn `taken`, in order, and report
/// them: all skipped when the caller may not run hooks, and once one fails,
/// the rest skipped. The caller holds no project lock.
pub(crate) fn after_take(policy: Policy, hooks: &[Hook], taken: &Taken<'_>) -> Vec<Value> {
    let mut entries: Vec<Value> = Vec::with_capacity(hooks.len());
    for hook in hooks {
        if policy == Policy::Skip {
            entries.push(skipped(hook, NOT_ALLOWED));
        } else if let Some(failed) = entries.iter().find(|entry| entry["status"] == "failed") {
            let reason = format!(
                "The post-take hook {} failed first, so this one was not run",
                failed["name"]
            );
            entries.push(skipped(hook, &reason));
        } else {
            let command = base_command(hook, taken.primary).map(|mut command| {
                let grafts: Vec<String> = taken
                    .grafts
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                command
                    .env("GIT_GRAFT_PRIMARY", taken.primary)
                    .env("GIT_GRAFT_TURN_RESOURCE", taken.resource)
                    .env("GIT_GRAFT_TURN_HOLDER", taken.holder)
                    .env("GIT_GRAFT_TURN_GRAFTS", grafts.join("\n"));
                command
            });
            entries.push(finish(hook, command));
        }
    }
    entries
}

/// The refusal of a `turn take` whose `post-take` hook failed: the turn was
/// given up again. `entries` are the reports of the hooks it ran.
pub(crate) fn take_failed(resource: &str, holder: &str, entries: Vec<Value>) -> anyhow::Error {
    let failed = entries
        .iter()
        .find(|entry| entry["status"] == "failed")
        .cloned()
        .unwrap_or_default();
    let name = &failed["name"];
    let cause = match (failed["reason"].as_str(), failed["exit_code"].as_i64()) {
        (Some(reason), _) => format!("The post-take hook {name} failed: {reason}"),
        (None, Some(code)) => format!("The post-take hook {name} exited with code {code}"),
        (None, None) => format!("The post-take hook {name} was ended by a signal"),
    };
    api::fault(
        "hook_failed",
        format!(
            "{cause}, so {holder} did not keep the turn on {resource}; it was given up. Fix what the hook reports and take the turn again"
        ),
        json!({"resource": resource, "holder": holder, "hooks": entries}),
    )
}

/// The project locks could not be taken again after a hook ran. The command
/// no longer holds what protects the project's records, so it must stop.
#[derive(Debug)]
pub(crate) struct Unlocked;

impl std::fmt::Display for Unlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not lock the project again after a hook")
    }
}

/// Runs a command's hooks and collects their reports. It holds the project
/// lock and the build lock the command works under, and releases both while
/// hooks run, so a slow hook does not stall other grafts. The command keeps
/// the graft's own lock throughout, which keeps every other command off the
/// graft and its record meanwhile.
pub(crate) struct Runner<'a> {
    policy: Policy,
    project: &'a File,
    builds: Option<Held>,
    released: bool,
    config: Option<Config>,
    entries: Vec<Value>,
}

impl<'a> Runner<'a> {
    /// `project` is the held project lock and `builds` the held build lock,
    /// if the command has one.
    pub(crate) fn new(policy: Policy, project: &'a File, builds: Option<Held>) -> Self {
        Self {
            policy,
            project,
            builds,
            released: false,
            config: None,
            entries: Vec::new(),
        }
    }

    /// The hooks configured for `event`. The configuration is read once per
    /// command, so every graft a prune archives runs the same hooks. Refuses
    /// with `hooks_untrusted` when the caller would run a hook that needs
    /// trust and has not been trusted.
    pub(crate) fn hooks(&mut self, p: &Project, event: Event) -> Result<Vec<Hook>> {
        if self.config.is_none() {
            self.config = Some(Config::read(&p.repo)?);
        }
        let config = self
            .config
            .as_ref()
            .context("the configuration was not read")?;
        trust::require(p, config, event, self.policy)?;
        Ok(config.hooks_for(event))
    }

    /// Add the collected reports to a command's result as `hooks`. A result
    /// is unchanged when no hook is configured for the command's event.
    pub(crate) fn report(self, mut value: Value) -> Value {
        if !self.entries.is_empty() {
            value["hooks"] = json!(self.entries);
        }
        value
    }

    /// The reports of `hooks`, run in order in the graft `r`: all skipped
    /// when the caller may not run hooks, otherwise run with the project
    /// locks released. Once one fails, the rest are skipped. The locks stay
    /// released; see [`Self::resume`].
    fn run(&mut self, p: &Project, hooks: &[Hook], r: &Record) -> Result<Vec<Value>> {
        if self.policy == Policy::Skip {
            return Ok(hooks
                .iter()
                .map(|hook| skipped(hook, NOT_ALLOWED))
                .collect());
        }
        if hooks.is_empty() {
            return Ok(Vec::new());
        }
        if !self.released {
            self.builds = None;
            self.project.unlock().context("release the project lock")?;
            self.released = true;
        }
        let graft = Graft {
            name: &r.name,
            path: &r.path,
            primary: &p.repo,
            branch: r.branch.as_deref(),
            base: r.base.as_deref(),
            port: r.port,
        };
        let mut entries: Vec<Value> = Vec::with_capacity(hooks.len());
        for hook in hooks {
            if let Some(failed) = entries.iter().find(|entry| entry["status"] == "failed") {
                let reason = format!(
                    "The {} hook {} failed first, so this one was not run",
                    hook.event.name(),
                    failed["name"]
                );
                entries.push(skipped(hook, &reason));
            } else {
                entries.push(run(hook, &graft));
            }
        }
        Ok(entries)
    }

    /// Take the project locks again after hooks ran, in the order
    /// [`Project::lock_builds`] gives. Returns whether any hook ran, in
    /// which case the caller reads its records again. A failure is an
    /// [`Unlocked`] error.
    fn resume(&mut self, p: &Project) -> Result<bool> {
        if !self.released {
            return Ok(false);
        }
        self.project
            .lock()
            .map_err(anyhow::Error::from)
            .and_then(|()| p.lock_builds(false))
            .map(|builds| self.builds = Some(builds))
            .context(Unlocked)?;
        self.released = false;
        Ok(true)
    }

    /// Run `hooks` once `add` or `restore` has made the graft `r` usable. A
    /// failure is reported and nothing else: the graft stays. The project
    /// locks are not taken again, so the caller must have finished its work
    /// under them.
    pub(crate) fn after(&mut self, p: &Project, hooks: &[Hook], r: &Record) -> Result<()> {
        let entries = self.run(p, hooks, r)?;
        self.entries.extend(entries);
        Ok(())
    }

    /// Run the `pre-archive` hooks, if any are configured, before `remove`
    /// or `merge` inspects the active graft `r` for work it must not
    /// archive. The caller has changed nothing yet. A hook that fails
    /// refuses the command with `hook_failed`. After hooks ran, `r` is read
    /// and checked again, since the project lock was released meanwhile.
    pub(crate) fn pre_archive(&mut self, p: &Project, r: &mut Record) -> Result<()> {
        let hooks = self.hooks(p, Event::PreArchive)?;
        if hooks.is_empty() {
            return Ok(());
        }
        let entries = self.run(p, &hooks, r)?;
        if entries.iter().any(|entry| entry["status"] == "failed") {
            return Err(hook_failed(r, entries));
        }
        if self.resume(p)? {
            *r = grafts::authenticate(p, &r.name, &r.lease)?;
            ensure!(
                r.status == Status::Active && r.retiring_to().is_none(),
                "graft changed while its pre-archive hooks ran"
            );
            grafts::preflight_retire(p, r)?;
        }
        self.entries.extend(entries);
        Ok(())
    }

    /// Run the `pre-archive` hooks, if any are configured, for the graft `r`
    /// that `prune --apply` is about to archive, and return their reports.
    /// Prune goes on to other grafts whatever the hooks did, so the project
    /// locks are held again when this returns.
    pub(crate) fn before_prune(&mut self, p: &Project, r: &Record) -> Result<Vec<Value>> {
        let hooks = self.hooks(p, Event::PreArchive)?;
        let entries = self.run(p, &hooks, r)?;
        self.resume(p)?;
        Ok(entries)
    }
}

/// The refusal of a command whose `pre-archive` hook failed: `entries` are
/// the reports of the hooks it ran, with their exit codes and output.
fn hook_failed(r: &Record, entries: Vec<Value>) -> anyhow::Error {
    let failed = entries
        .iter()
        .find(|entry| entry["status"] == "failed")
        .cloned()
        .unwrap_or_default();
    let name = &failed["name"];
    let cause = match (failed["reason"].as_str(), failed["exit_code"].as_i64()) {
        (Some(reason), _) => format!("The pre-archive hook {name} failed: {reason}"),
        (None, Some(code)) => format!("The pre-archive hook {name} exited with code {code}"),
        (None, None) => format!("The pre-archive hook {name} was ended by a signal"),
    };
    api::fault(
        "hook_failed",
        format!(
            "{cause}, so graft {} was not archived. Graft changed nothing itself. Fix what the hook reports and retry; the hooks will run again",
            r.name
        ),
        json!({"name": r.name, "hooks": entries}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: Event, argv: &[&str]) -> Hook {
        Hook {
            name: "setup".into(),
            event,
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            file: crate::config::SHARED.to_owned(),
            local: false,
            program: None,
            dir: Some(Path::new("configured").to_owned()),
        }
    }

    #[test]
    fn event_names_match_in_the_configuration_and_in_reports() {
        for event in [Event::PostAdd, Event::PreArchive, Event::PostRestore] {
            let name = event.name();
            assert_eq!(serde_json::to_value(event).unwrap(), json!(name));
            assert_eq!(serde_json::from_value::<Event>(json!(name)).unwrap(), event);
        }
        for unknown in ["", "post_add", "Post-Add", "pre-remove", "post-merge"] {
            assert!(serde_json::from_value::<Event>(json!(unknown)).is_err());
        }
    }

    #[test]
    fn a_hook_is_a_bounded_program_and_arguments() {
        let argv = |parts: &[&str]| -> Vec<String> {
            parts.iter().map(|part| (*part).to_owned()).collect()
        };
        let absolute = if cfg!(windows) {
            r"C:\Program Files\prepare.exe"
        } else {
            "/usr/local/bin/prepare"
        };
        for accepted in [
            argv(&[absolute]),
            argv(&[absolute, "./relative/argument"]),
            argv(&[
                "node",
                "script with spaces.js",
                "",
                "--flag=a b",
                "line\nbreak",
            ]),
            vec!["x".repeat(MAX_ARGUMENT_BYTES); MAX_ARGUMENTS],
        ] {
            assert!(validate(&accepted).is_ok(), "{accepted:?}");
        }
        for refused in [
            argv(&[]),
            argv(&[""]),
            argv(&["", "argument"]),
            argv(&["program", "nul\0byte"]),
            argv(&["program", &"x".repeat(MAX_ARGUMENT_BYTES + 1)]),
            vec!["x".to_owned(); MAX_ARGUMENTS + 1],
        ] {
            assert!(validate(&refused).is_err(), "{refused:?}");
        }
        // A program with a directory in it must be absolute: Unix would run
        // the graft's copy and Windows the one in the caller's directory.
        let mut relative = vec![
            "./scripts/setup",
            "scripts/setup",
            "../bin/setup",
            "bin/",
            ".",
            "..",
        ];
        if cfg!(windows) {
            relative.extend([
                r"scripts\setup",
                r".\setup.exe",
                r"\bin\setup.exe",
                "C:setup.exe",
            ]);
        }
        for program in relative {
            let error = validate(&argv(&[program])).expect_err(program);
            assert!(format!("{error}").contains("relative path"), "{error}");
        }
        // Outside Windows a backslash is part of a file name, not a separator.
        if !cfg!(windows) {
            assert!(validate(&argv(&[r"odd\name"])).is_ok());
        }
    }

    #[test]
    fn hooks_are_listed_from_the_configuration() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        crate::git::bytes(tmp.path(), &["init", "-q", "--template="])?;
        let state = tmp.path().join("no-state");
        assert_eq!(list(tmp.path(), &state)?, json!({"hooks": []}));
        std::fs::write(
            tmp.path().join(crate::config::SHARED),
            r#"{"hooks": {"db": {"event": "post-add", "argv": ["create-db", "--name"]}}}"#,
        )?;
        std::fs::write(
            tmp.path().join(crate::config::LOCAL),
            r#"{"hooks": {"notify": {"event": "pre-archive", "argv": ["notify"]}}}"#,
        )?;
        assert_eq!(
            list(tmp.path(), &state)?,
            json!({"hooks": [
                {"name": "db", "event": "post-add", "argv": ["create-db", "--name"], "file": ".graftconfig", "trusted": false},
                {"name": "notify", "event": "pre-archive", "argv": ["notify"], "file": ".graftconfig.local", "trusted": true},
            ]})
        );
        std::fs::write(tmp.path().join(crate::config::LOCAL), "{")?;
        let error = list(tmp.path(), &state).unwrap_err();
        assert_eq!(api::error_json(&error)["code"], "invalid_config");
        Ok(())
    }

    #[test]
    fn a_hook_starts_in_the_graft_without_the_lease_or_another_repository() -> Result<()> {
        let graft = Graft {
            name: "fix-parser",
            path: Path::new("grafts/fix-parser"),
            primary: Path::new("primary"),
            branch: None,
            base: Some("0123456789abcdef"),
            port: 30123,
        };
        let command = command(
            &hook(Event::PreArchive, &["program", "an argument"]),
            &graft,
        )?;
        assert_eq!(command.get_program(), "program");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["an argument"]);
        assert_eq!(command.get_current_dir(), Some(graft.path));
        let envs: std::collections::HashMap<_, _> = command.get_envs().collect();
        let env = |name: &str| envs.get(std::ffi::OsStr::new(name)).copied();
        for removed in [
            "GIT_GRAFT_LEASE",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
        ] {
            assert_eq!(env(removed), Some(None), "{removed}");
        }
        for (name, value) in [
            ("GIT_GRAFT_HOOK", "pre-archive"),
            ("GIT_GRAFT_HOOK_NAME", "setup"),
            ("GIT_GRAFT_NAME", "fix-parser"),
            ("GIT_GRAFT_PATH", "grafts/fix-parser"),
            ("GIT_GRAFT_PRIMARY", "primary"),
            ("GIT_GRAFT_CONFIG_DIR", "configured"),
            ("GIT_GRAFT_BRANCH", ""),
            ("GIT_GRAFT_BASE", "0123456789abcdef"),
            ("GIT_GRAFT_PORT", "30123"),
        ] {
            assert_eq!(env(name), Some(Some(std::ffi::OsStr::new(value))), "{name}");
        }
        Ok(())
    }

    #[test]
    fn a_hook_that_cannot_start_is_a_failed_hook() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let graft = Graft {
            name: "missing",
            path: tmp.path(),
            primary: tmp.path(),
            branch: Some("graft/missing"),
            base: None,
            port: 30000,
        };
        let program = tmp.path().join("no-such-program");
        let failed = run(
            &hook(Event::PostAdd, &[program.to_string_lossy().as_ref()]),
            &graft,
        );
        assert_eq!(failed["name"], "setup");
        assert_eq!(failed["event"], "post-add");
        assert_eq!(failed["file"], ".graftconfig");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["exit_code"], Value::Null);
        assert_eq!(failed["stdout"], "");
        assert!(
            failed["reason"]
                .as_str()
                .is_some_and(|reason| reason.starts_with("The hook could not be started")),
            "{failed}"
        );
        let skipped = skipped(&hook(Event::PostRestore, &["x"]), NOT_ALLOWED);
        assert_eq!(skipped["status"], "skipped");
        assert!(
            skipped["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("--allow-exec")),
            "{skipped}"
        );
        Ok(())
    }
}
