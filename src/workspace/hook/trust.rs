//! Consent for hooks that come from a file the repository carries. A hook in
//! `.graftconfig`, or in `.graftconfig.local` once Git tracks it, arrives with
//! a clone, a checked-out branch or a merged graft, so it runs only after the
//! person has run `git graft hook trust`. That command records a digest of
//! every hook those files define, by event, name and program with arguments,
//! in the project's Graft state. A program named by a path relative to the
//! configuration's directory is part of what is trusted, contents included.
//! A command that would run one of them refuses with `hooks_untrusted` until
//! the digest matches, and again after any change. What an untracked
//! `.graftconfig.local` adds, replaces or turns off is the person's own and
//! needs no trust, except one in a directory above the checkout on Windows,
//! where Graft cannot check that the person owns it. No MCP tool records
//! trust, so an agent cannot approve its own hook.
//!
//! The `remotes` those files list need trust by the same rule: a remote they
//! name gives every graft a URL that `git fetch` and `git push` use. Trust
//! covers the remotes a graft would get from them, each with the file that
//! brings it, so the same entries arranged to bring another remote need
//! trust again. `git graft hook trust` records that digest beside the hooks'
//! digest, which stays what it was, and the two are compared
//! apart, so a change to the remotes does not stop a hook and a change to a
//! hook does not hold back a remote. Until they are trusted, `add` and
//! `restore` leave them out of the graft and report them as skipped.

use crate::{
    api,
    config::{Config, Hook, LOCAL, OWNER_CHECKED},
    registry::{self, Project},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io, path::Path};

use super::{Event, Policy};

/// The file in the project's Graft state that records the trusted hooks.
const FILE: &str = "trusted-hooks.json";
/// The command that records trust, as the refusal names it.
pub(crate) const COMMAND: &str = "git graft hook trust";

/// What `git graft hook trust` recorded.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    sha256: String,
    hooks: Vec<Value>,
    /// The digest of the remotes that need trust, when there are any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remotes_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    remotes: Vec<String>,
}

/// Which of the project's configuration files have hooks and remotes that
/// take effect only once trusted.
pub(crate) struct Gate {
    local: bool,
}

impl Gate {
    /// `.graftconfig` always does, and `.graftconfig.local` when Git tracks
    /// it in the repository whose working tree holds it, or when it is in a
    /// directory above the checkout and Graft does not check who owns it.
    /// Git is asked only when the local file defines a hook or lists a
    /// remote.
    pub(crate) fn of(config: &Config) -> Result<Self> {
        Self::with_owner_check(config, OWNER_CHECKED)
    }

    /// [`Self::of`], as on a platform that checks the owner of a file above
    /// the checkout when `owner_checked` is set.
    fn with_owner_check(config: &Config, owner_checked: bool) -> Result<Self> {
        let local = match &config.dir {
            Some(dir)
                if config.defined.iter().any(|hook| hook.local)
                    || config.listed_remotes.iter().any(|remote| remote.local) =>
            {
                (config.above && !owner_checked) || tracked(dir, LOCAL)?
            }
            _ => false,
        };
        Ok(Self { local })
    }

    /// The remotes a graft gets that need trust, in the order it gets them,
    /// each as a pair of a file that needs trust and lists it and its name:
    /// one pair for each such file that lists it. This is what trust covers,
    /// so a change to what a graft would get, or to which file brings it,
    /// needs trust again.
    pub(crate) fn remotes<'a>(&self, config: &'a Config) -> Vec<(&'a str, &'a str)> {
        config
            .remotes
            .iter()
            .filter(|name| self.remote_needs_trust(config, name))
            .flat_map(|name| {
                config
                    .listed_remotes
                    .iter()
                    .filter(move |remote| remote.name == *name && (!remote.local || self.local))
                    .map(|remote| (remote.file.as_str(), remote.name.as_str()))
            })
            .collect()
    }

    /// Whether the effective remote `name` is set up only once trusted: it
    /// is, unless the person's own untracked local file lists it too.
    pub(crate) fn remote_needs_trust(&self, config: &Config, name: &str) -> bool {
        self.local
            || !config
                .listed_remotes
                .iter()
                .any(|remote| remote.local && remote.name == name)
    }

    /// Whether `hook` runs only once trusted.
    pub(crate) fn needs_trust(&self, hook: &Hook) -> bool {
        !hook.local || self.local
    }

    /// The hooks that run only once trusted, as the files define them.
    pub(crate) fn hooks<'a>(&self, config: &'a Config) -> Vec<&'a Hook> {
        config
            .defined
            .iter()
            .filter(|hook| self.needs_trust(hook))
            .collect()
    }
}

/// Whether Git tracks the file `name` in `dir`, in any case: on a
/// case-insensitive filesystem Graft reads a tracked `.GraftConfig.local` as
/// `.graftconfig.local`, and the index may spell `dir` itself in another case
/// than the path Graft found it by, so both must be gated as one. The
/// pathspec names `dir` by its path from the top of the working tree, because
/// `icase` does not apply to the prefix Git takes from the current directory.
/// A directory outside any working tree tracks nothing.
fn tracked(dir: &Path, name: &str) -> Result<bool> {
    let found = crate::git::raw(
        dir,
        &["rev-parse", "--is-inside-work-tree", "--show-prefix"],
    )
    .with_context(|| format!("check whether Git tracks {name}"))?;
    let mut lines = found.stdout.split(|byte| *byte == b'\n');
    if !found.status.success() || lines.next() != Some(b"true".as_slice()) {
        return Ok(false);
    }
    // A prefix that is not UTF-8 cannot be put in a pathspec here, so the
    // file is treated as tracked: it then needs trust.
    let Ok(prefix) = std::str::from_utf8(lines.next().unwrap_or_default()) else {
        return Ok(true);
    };
    let pathspec = format!(":(top,icase,literal){prefix}{name}");
    let listed = crate::git::bytes(dir, &["ls-files", "--cached", "-z", "--", &pathspec])
        .with_context(|| format!("check whether Git tracks {name}"))?;
    Ok(!listed.is_empty())
}

/// The SHA-256 of the file at `path`, or `None` when it cannot be read.
fn file_digest(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match io::Read::read(&mut file, &mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The digest of `hooks`: their events, names and arguments, in order. A
/// hook whose program is a path relative to its configuration's directory
/// also counts that path and the program's contents, since the file comes
/// with the repository that holds the configuration as the hook does.
fn digest(hooks: &[&Hook]) -> String {
    let parts: Vec<Value> = hooks
        .iter()
        .map(|hook| match &hook.program {
            None => json!([hook.event.name(), hook.name, hook.argv]),
            Some(program) => json!([
                hook.event.name(),
                hook.name,
                hook.argv,
                program,
                file_digest(program)
            ]),
        })
        .collect();
    let bytes = serde_json::to_vec(&parts).expect("hook descriptions serialize");
    hex(&Sha256::digest(&bytes))
}

/// The record in the Graft state directory `state`, if trust was recorded.
fn recorded(state: &Path) -> Result<Option<Record>> {
    let path = state.join(FILE);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        Ok(_) => {}
    }
    let bytes = registry::regular(&path)?;
    let record = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a record of trusted hooks", path.display()))?;
    Ok(Some(record))
}

/// Whether the hooks that need trust, `hooks`, are the ones the record in
/// `state` trusts. With none, there is nothing to trust. `state` need not exist.
pub(crate) fn is_trusted(state: &Path, hooks: &[&Hook]) -> Result<bool> {
    if hooks.is_empty() {
        return Ok(true);
    }
    Ok(recorded(state)?.is_some_and(|record| record.sha256 == digest(hooks)))
}

/// The digest of the remotes that need trust, `remotes`, as
/// [`Gate::remotes`] gives them: (file, name) pairs, in order. It is recorded
/// apart from the hooks' digest, so a change to one leaves the trust of the
/// other as it was.
fn remotes_digest(remotes: &[(&str, &str)]) -> String {
    let bytes = serde_json::to_vec(remotes).expect("remote names serialize");
    hex(&Sha256::digest(&bytes))
}

/// The names in `remotes`, each once, in order.
fn remote_names<'a>(remotes: &[(&'a str, &'a str)]) -> Vec<&'a str> {
    let mut names: Vec<&str> = Vec::new();
    for (_, name) in remotes {
        if !names.contains(name) {
            names.push(name);
        }
    }
    names
}

/// Whether the remotes that need trust, `remotes`, are the ones the record
/// in `state` trusts. With none, there is nothing to trust. `state` need not
/// exist.
pub(crate) fn remotes_trusted(state: &Path, remotes: &[(&str, &str)]) -> Result<bool> {
    if remotes.is_empty() {
        return Ok(true);
    }
    let sha256 = remotes_digest(remotes);
    Ok(recorded(state)?.is_some_and(|record| record.remotes_sha256.as_ref() == Some(&sha256)))
}

/// The remotes of `config` a graft must not get yet: those a file the
/// repository carries lists, while the remotes that need trust are not the
/// ones last trusted. A graft gets every other remote. Unlike a hook, a
/// remote a graft can do without, so it is left out rather than refused.
pub(crate) fn untrusted_remotes(p: &Project, config: &Config) -> Result<Vec<String>> {
    if config.remotes.is_empty() {
        return Ok(Vec::new());
    }
    let gate = Gate::of(config)?;
    let due = gate.remotes(config);
    if remotes_trusted(&p.state, &due)? {
        return Ok(Vec::new());
    }
    Ok(remote_names(&due).into_iter().map(str::to_owned).collect())
}

/// Refuse with `hooks_untrusted` when a caller that runs hooks would run one
/// for `event` that needs trust, and the hooks that need trust are not the
/// ones last trusted. Nothing has run or changed when this refuses.
pub(crate) fn require(p: &Project, config: &Config, event: Event, policy: Policy) -> Result<()> {
    if policy == Policy::Skip {
        return Ok(());
    }
    let due = config.hooks_for(event);
    if due.is_empty() {
        return Ok(());
    }
    let gate = Gate::of(config)?;
    if !due.iter().any(|hook| gate.needs_trust(hook)) {
        return Ok(());
    }
    let gated = gate.hooks(config);
    let record = recorded(&p.state)?;
    if record
        .as_ref()
        .is_some_and(|record| record.sha256 == digest(&gated))
    {
        return Ok(());
    }
    let mut files: Vec<&str> = gated.iter().map(|hook| hook.file.as_str()).collect();
    files.dedup();
    let state = if record.is_some() {
        "have changed since they were last trusted"
    } else {
        "have not been trusted"
    };
    Err(api::fault(
        "hooks_untrusted",
        format!(
            "The hooks in {} {state}, so Graft ran nothing and changed nothing. They come with the repository and run with your permissions. \
             Read them in details.hooks or with `git graft hook list`, then run `{COMMAND}` in a terminal to let them run. No MCP tool can trust hooks",
            files.join(" and ")
        ),
        json!({
            "event": event,
            "hooks": gated.iter().map(|hook| hook.describe()).collect::<Vec<_>>(),
            "trusted": record.map(|record| record.hooks),
            "command": COMMAND,
        }),
    ))
}

/// Record the hooks and remotes that need trust, as the files define them
/// now, as trusted. Returns them with their digests.
pub(crate) fn trust(p: &Project) -> Result<Value> {
    let config = Config::read(&p.repo)?;
    let gate = Gate::of(&config)?;
    let gated = gate.hooks(&config);
    let due = gate.remotes(&config);
    let names = remote_names(&due);
    let hooks: Vec<Value> = gated.iter().map(|hook| hook.describe()).collect();
    let record = Record {
        sha256: digest(&gated),
        hooks: hooks.clone(),
        remotes_sha256: (!due.is_empty()).then(|| remotes_digest(&due)),
        remotes: names.iter().map(|name| (*name).to_owned()).collect(),
    };
    registry::atomic(&p.state.join(FILE), &serde_json::to_vec_pretty(&record)?)?;
    let mut reply = json!({"trusted": hooks, "sha256": record.sha256});
    if let Some(sha256) = &record.remotes_sha256 {
        reply["trusted_remotes"] = json!(record.remotes);
        reply["remotes_sha256"] = json!(sha256);
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SHARED;

    fn hook(name: &str, event: Event, argv: &[&str], file: &'static str) -> Hook {
        Hook {
            name: name.into(),
            event,
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            file: file.to_owned(),
            local: file == LOCAL,
            program: None,
            dir: None,
        }
    }

    #[test]
    fn the_digest_covers_event_name_arguments_and_order_but_not_the_file() {
        let a = hook("db", Event::PostAdd, &["create-db"], SHARED);
        let b = hook("clean", Event::PreArchive, &["drop-db"], SHARED);
        let base = digest(&[&a, &b]);
        assert_eq!(base.len(), 64);
        assert_eq!(
            base,
            digest(&[&a, &hook("clean", Event::PreArchive, &["drop-db"], LOCAL)])
        );
        for changed in [
            digest(&[&b, &a]),
            digest(&[&a]),
            digest(&[&hook("db", Event::PostRestore, &["create-db"], SHARED), &b]),
            digest(&[&hook("db2", Event::PostAdd, &["create-db"], SHARED), &b]),
            digest(&[
                &hook("db", Event::PostAdd, &["create-db", "--x"], SHARED),
                &b,
            ]),
            // Arguments are not joined, so moving a boundary changes the digest.
            digest(&[&hook("db", Event::PostAdd, &["create", "-db"], SHARED), &b]),
        ] {
            assert_ne!(changed, base);
        }
    }

    #[test]
    fn shared_hooks_and_those_of_a_tracked_local_file_need_trust_as_written() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let primary = tmp.path();
        crate::git::bytes(primary, &["init", "-q", "--template="])?;
        fs::write(
            primary.join(SHARED),
            r#"{"hooks": {"db": {"event": "post-add", "argv": ["create-db"]},
                          "notify": {"event": "post-add", "argv": ["notify"]},
                          "drop": {"event": "pre-archive", "argv": ["drop-db"]}}}"#,
        )?;
        let shared_only = Config::read(primary)?;
        fs::write(
            primary.join(LOCAL),
            r#"{"hooks": {"notify": {"event": "post-add", "argv": ["notify", "--quiet"]},
                          "drop": null,
                          "mine": {"event": "pre-archive", "argv": ["mine"]}}}"#,
        )?;
        let config = Config::read(primary)?;
        let names = |hooks: Vec<&Hook>| {
            hooks
                .iter()
                .map(|hook| format!("{}:{}", hook.file, hook.name))
                .collect::<Vec<_>>()
        };
        // What an untracked local file replaces or turns off leaves the
        // shared definitions, and so their trust, as they were.
        let untracked = Gate::of(&config)?;
        assert!(!untracked.local);
        // The checkout's own file needs no owner check, on any platform.
        assert!(!config.above);
        assert!(!Gate::with_owner_check(&config, false)?.local);
        assert_eq!(
            names(untracked.hooks(&config)),
            [
                ".graftconfig:db",
                ".graftconfig:notify",
                ".graftconfig:drop"
            ]
        );
        assert_eq!(
            digest(&untracked.hooks(&config)),
            digest(&Gate::of(&shared_only)?.hooks(&shared_only))
        );
        crate::git::bytes(primary, &["add", "--", LOCAL])?;
        let tracked = Gate::of(&config)?;
        assert!(tracked.local);
        assert_eq!(
            names(tracked.hooks(&config)),
            [
                ".graftconfig:db",
                ".graftconfig:notify",
                ".graftconfig:drop",
                ".graftconfig.local:notify",
                ".graftconfig.local:mine"
            ]
        );

        let state = primary.join("state");
        assert!(is_trusted(&state, &[])?);
        let gated = tracked.hooks(&config);
        assert!(!is_trusted(&state, &gated)?);
        fs::create_dir(&state)?;
        let record = Record {
            sha256: digest(&gated),
            hooks: Vec::new(),
            remotes_sha256: None,
            remotes: Vec::new(),
        };
        fs::write(state.join(FILE), serde_json::to_vec(&record)?)?;
        assert!(is_trusted(&state, &gated)?);
        assert!(!is_trusted(&state, &gated[..1])?);
        fs::write(state.join(FILE), "{")?;
        assert!(is_trusted(&state, &gated).is_err());
        Ok(())
    }

    #[test]
    fn a_local_file_tracked_in_another_case_counts_as_tracked() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let primary = tmp.path();
        crate::git::bytes(primary, &["init", "-q", "--template="])?;
        assert!(!tracked(primary, LOCAL)?);
        // Added to the index directly, so the test needs no particular
        // filesystem to hold a name in another case.
        let blob = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        crate::git::bytes(
            primary,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},.GraftConfig.local"),
            ],
        )?;
        assert!(tracked(primary, LOCAL)?);
        Ok(())
    }

    #[test]
    fn a_local_file_in_a_directory_the_index_spells_in_another_case_counts_as_tracked() -> Result<()>
    {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path();
        crate::git::bytes(root, &["init", "-q", "--template="])?;
        // As a clone on a case-insensitive filesystem leaves it: the index
        // holds `sub/`, and the directory on disk is `Sub/`.
        let dir = root.join("Sub");
        fs::create_dir(&dir)?;
        assert!(!tracked(&dir, LOCAL)?);
        let blob = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        crate::git::bytes(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},sub/.GraftConfig.local"),
            ],
        )?;
        assert!(tracked(&dir, LOCAL)?);
        // The file in another directory is not this one.
        let other = root.join("other");
        fs::create_dir(&other)?;
        assert!(!tracked(&other, LOCAL)?);
        Ok(())
    }

    #[test]
    fn a_file_above_needs_trust_when_tracked_and_a_relative_program_counts_by_contents()
    -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = dunce::canonicalize(tmp.path())?;
        let app = root.join("app");
        fs::create_dir(&app)?;
        crate::git::bytes(&app, &["init", "-q", "--template="])?;
        fs::create_dir(root.join("scripts"))?;
        fs::write(root.join("scripts").join("setup"), "#!/bin/sh\n")?;
        let local = r#"{"repositories": {"app": {"hooks": {"setup": {"event": "post-add", "argv": ["./scripts/setup"]}}}}}"#;
        fs::write(root.join(LOCAL), local)?;

        // Outside any working tree, the local file is the person's own,
        // where Graft checks that the person owns it.
        let config = Config::read(&app)?;
        assert!(config.above);
        assert!(!Gate::with_owner_check(&config, true)?.local);
        assert!(
            Gate::with_owner_check(&config, true)?
                .hooks(&config)
                .is_empty()
        );
        // Where it does not, as on Windows, the hook needs trust.
        assert!(Gate::with_owner_check(&config, false)?.local);
        assert_eq!(
            Gate::with_owner_check(&config, false)?.hooks(&config).len(),
            1
        );
        assert_eq!(Gate::of(&config)?.local, !OWNER_CHECKED);

        // Once the directory's own repository tracks it, it needs trust.
        crate::git::bytes(&root, &["init", "-q", "--template="])?;
        assert!(!Gate::with_owner_check(&config, true)?.local);
        crate::git::bytes(&root, &["add", "--", LOCAL])?;
        assert!(Gate::with_owner_check(&config, true)?.local);
        let gate = Gate::of(&config)?;
        assert!(gate.local);
        let gated = gate.hooks(&config);
        assert_eq!(gated.len(), 1);
        let before = digest(&gated);

        // Changing the program changes what was trusted; restoring it does not.
        fs::write(root.join("scripts").join("setup"), "#!/bin/sh\nrm -rf ~\n")?;
        assert_ne!(digest(&gated), before);
        fs::write(root.join("scripts").join("setup"), "#!/bin/sh\n")?;
        assert_eq!(digest(&gated), before);
        fs::remove_file(root.join("scripts").join("setup"))?;
        assert_ne!(digest(&gated), before);
        Ok(())
    }
    #[test]
    fn remotes_have_a_digest_of_their_own_and_leave_the_hooks_digest_as_it_was() {
        let a = hook("db", Event::PostAdd, &["create-db"], SHARED);
        // The digest of this hook before remotes could be listed.
        let before = "20929a4647d06e48cf9bd23c16711e8ea45e95d49a9ef68bfe255aa7bf59cbd0";
        assert_eq!(digest(&[&a]), before);
        let origin = remotes_digest(&[(SHARED, "origin")]);
        assert_eq!(origin.len(), 64);
        for changed in [
            remotes_digest(&[(SHARED, "upstream")]),
            remotes_digest(&[(LOCAL, "origin")]),
            remotes_digest(&[(SHARED, "origin"), (LOCAL, "origin")]),
            remotes_digest(&[(SHARED, "origin"), (SHARED, "upstream")]),
            remotes_digest(&[(SHARED, "upstream"), (SHARED, "origin")]),
            remotes_digest(&[]),
        ] {
            assert_ne!(changed, origin);
        }
    }

    #[test]
    fn remotes_a_tracked_file_lists_need_trust_and_the_person_s_own_do_not() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let primary = tmp.path();
        crate::git::bytes(primary, &["init", "-q", "--template="])?;
        fs::write(primary.join(SHARED), r#"{"remotes": ["origin"]}"#)?;
        fs::write(primary.join(LOCAL), r#"{"remotes": ["fork", "!origin"]}"#)?;
        let config = Config::read(primary)?;
        assert_eq!(config.remotes, ["fork"]);
        let names = |gate: &Gate| remote_names(&gate.remotes(&config)).join(" ");

        // An untracked local file is the person's own: what it lists needs
        // no trust, and what it removes no graft gets, so nothing needs it.
        let untracked = Gate::of(&config)?;
        assert!(!untracked.local);
        assert_eq!(names(&untracked), "");
        assert!(!untracked.remote_needs_trust(&config, "fork"));
        assert!(untracked.remote_needs_trust(&config, "origin"));

        crate::git::bytes(primary, &["add", "--", LOCAL])?;
        let tracked = Gate::of(&config)?;
        assert!(tracked.local);
        assert_eq!(names(&tracked), "fork");
        assert_eq!(tracked.remotes(&config), [(LOCAL, "fork")]);
        assert!(tracked.remote_needs_trust(&config, "fork"));

        // The remotes' trust is recorded and compared apart from the hooks'.
        let state = primary.join("state");
        fs::create_dir(&state)?;
        let remotes = tracked.remotes(&config);
        assert!(remotes_trusted(&state, &[])?);
        assert!(!remotes_trusted(&state, &remotes)?);
        let record = Record {
            sha256: digest(&[]),
            hooks: Vec::new(),
            remotes_sha256: Some(remotes_digest(&remotes)),
            remotes: Vec::new(),
        };
        fs::write(state.join(FILE), serde_json::to_vec(&record)?)?;
        assert!(remotes_trusted(&state, &remotes)?);
        assert!(!remotes_trusted(&state, &[(SHARED, "fork")])?);
        assert!(is_trusted(&state, &[])?);
        // A record made before remotes could be listed trusts none.
        fs::write(state.join(FILE), r#"{"sha256": "x", "hooks": []}"#)?;
        assert!(!remotes_trusted(&state, &remotes)?);
        Ok(())
    }

    #[test]
    fn remotes_an_untracked_local_file_above_lists_need_trust_where_its_owner_is_not_checked()
    -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = dunce::canonicalize(tmp.path())?;
        let app = root.join("app");
        fs::create_dir(&app)?;
        crate::git::bytes(&app, &["init", "-q", "--template="])?;
        // The local file lists a remote and defines no hook.
        fs::write(
            root.join(LOCAL),
            r#"{"repositories": {"app": {"remotes": ["origin"]}}}"#,
        )?;
        let config = Config::read(&app)?;
        assert!(config.above);
        assert_eq!(config.remotes, ["origin"]);

        // Where Graft checks its owner, it is the person's own.
        let checked = Gate::with_owner_check(&config, true)?;
        assert!(!checked.local);
        assert!(checked.remotes(&config).is_empty());
        assert!(!checked.remote_needs_trust(&config, "origin"));

        // Where it does not, as on Windows, its remotes need trust as its hooks would.
        let unchecked = Gate::with_owner_check(&config, false)?;
        assert!(unchecked.local);
        assert_eq!(remote_names(&unchecked.remotes(&config)), ["origin"]);
        assert!(unchecked.remote_needs_trust(&config, "origin"));
        Ok(())
    }

    #[test]
    fn a_change_to_what_a_graft_gets_or_to_the_file_that_brings_it_needs_trust_again() -> Result<()>
    {
        let tmp = tempfile::tempdir()?;
        let primary = tmp.path();
        crate::git::bytes(primary, &["init", "-q", "--template="])?;
        let due = |shared: &str, local: &str| -> Result<(Vec<String>, String)> {
            fs::write(primary.join(SHARED), shared)?;
            fs::write(primary.join(LOCAL), local)?;
            crate::git::bytes(primary, &["add", "--", SHARED, LOCAL])?;
            let config = Config::read(primary)?;
            let remotes = Gate::of(&config)?.remotes(&config);
            Ok((config.remotes.clone(), remotes_digest(&remotes)))
        };
        // The review's case: `a` removed by the tracked local file, then
        // listed again once the `!a` moves.
        let (remotes, trusted) = due(r#"{"remotes": ["a"]}"#, r#"{"remotes": ["!a", "b"]}"#)?;
        assert_eq!(remotes, ["b"]);
        // `!` in .graftconfig is refused, so the change as the review made
        // it reads as an invalid configuration.
        fs::write(primary.join(SHARED), r#"{"remotes": ["a", "!a"]}"#)?;
        let error = Config::read(primary).unwrap_err();
        assert_eq!(crate::api::error_json(&error)["code"], "invalid_config");
        // Every way the same entries can bring `a` back changes the digest.
        for (shared, local) in [
            (r#"{"remotes": ["a"]}"#, r#"{"remotes": ["b"]}"#),
            (r#"{"remotes": []}"#, r#"{"remotes": ["a", "b"]}"#),
        ] {
            let (remotes, changed) = due(shared, local)?;
            assert!(remotes.contains(&"a".to_owned()), "{shared} {local}");
            assert_ne!(changed, trusted, "{shared} {local}");
        }
        // So does moving `b` from one file to the other.
        let (remotes, moved) = due(r#"{"remotes": ["b"]}"#, r#"{"remotes": []}"#)?;
        assert_eq!(remotes, ["b"]);
        assert_ne!(moved, trusted);
        // And the same remotes from the same files keep it.
        assert_eq!(
            due(r#"{"remotes": ["a"]}"#, r#"{"remotes": ["!a", "b"]}"#)?.1,
            trusted
        );
        Ok(())
    }
}
