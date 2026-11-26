//! Publish a graft's commits to a remote of the primary repository.
//!
//! This needs no remote in the graft, which has none unless the project's
//! configuration names some (see `remotes`). The graft's commits are copied
//! into the primary's object store under a private ref, and the primary's own
//! Git pushes them, so the remote's URL, credentials and hooks are the ones
//! the user already set up. A plain `git push` through a remote the graft
//! got from the configuration does not come here, so the check against the
//! remote's default branch below does not cover it.
use std::{path::Path, process::Stdio};

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use super::{land, objects, registry::Record};
use crate::{
    api, git, recovery,
    registry::{Held, Project},
    runtime::Runtime,
};

/// Most bytes of Git's output that a refusal carries.
const LISTED_OUTPUT: usize = 8 * 1024;

/// How Git ended a push of one ref.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Pushed,
    /// The update was refused, for the reason Git or the remote gave.
    Rejected(String),
    Failed,
}

/// Refuse a remote the primary does not configure. The name is matched
/// exactly, so a caller cannot pass a URL or a path where `git push` would
/// accept one.
fn require_remote(primary: &Path, name: &str) -> Result<()> {
    let listed = git::text(primary, &["remote"])?;
    let remotes: Vec<&str> = listed.lines().filter(|line| !line.is_empty()).collect();
    if remotes.contains(&name) {
        return Ok(());
    }
    Err(api::fault(
        "remote_not_found",
        format!(
            "The primary repository has no remote named {name:?}. Nothing was pushed. Configure it there with git remote add, or choose one of its remotes"
        ),
        json!({"remote": name, "remotes": remotes}),
    ))
}

/// The commit to push and the remote branch to update: the graft's `HEAD`,
/// and `to` or else the branch `HEAD` is on.
fn source(r: &Record, to: Option<&str>) -> Result<(String, String)> {
    if super::unborn(&r.path)? {
        return Err(api::fault(
            "nothing_to_push",
            format!(
                "Graft {} has no commits yet. Commit in the graft, then push again",
                r.name
            ),
            json!({"name": r.name}),
        ));
    }
    let commit = git::oid(&r.path, "HEAD")?;
    let current = git::symbolic_ref(&r.path, "HEAD")?
        .and_then(|head| head.strip_prefix("refs/heads/").map(str::to_owned));
    match to.map(str::to_owned).or(current) {
        Some(branch) => Ok((commit, branch)),
        None => Err(api::fault(
            "detached_head",
            format!(
                "Graft {} has a detached HEAD, so there is no branch name to push. Name the remote branch with --to (to over MCP). Nothing was pushed",
                r.name
            ),
            json!({"name": r.name, "commit": commit}),
        )),
    }
}

/// The remote's default branch as the primary last recorded it in
/// `refs/remotes/<remote>/HEAD`, which `git clone` and `git remote set-head`
/// write. `None` when the primary has no such record.
fn default_branch(primary: &Path, remote: &str) -> Result<Option<String>> {
    let prefix = format!("refs/remotes/{remote}/");
    Ok(git::symbolic_ref(primary, &format!("{prefix}HEAD"))?
        .and_then(|target| target.strip_prefix(&prefix).map(str::to_owned)))
}

/// Refuse to push the graft's own branch name to the remote's default
/// branch. A graft added with `--branch main` would otherwise update the
/// remote's `main` on a plain `push`, which is meant for review branches.
/// Naming the branch with `to` is taken as the intent to update it.
fn require_review_branch(
    primary: &Path,
    name: &str,
    remote: &str,
    branch: &str,
    to: Option<&str>,
) -> Result<()> {
    if to.is_some() || default_branch(primary, remote)?.as_deref() != Some(branch) {
        return Ok(());
    }
    Err(api::fault(
        "default_branch",
        format!(
            "Graft {name} is on branch {branch}, which is the default branch of remote {remote}. Nothing was pushed. To update it, name it with --to {branch} (to over MCP); otherwise push to another branch with --to"
        ),
        json!({"name": name, "remote": remote, "branch": branch}),
    ))
}

/// Let Git decide whether `name` is a branch name. Git also expands
/// `@{-1}` here; a name it rewrites is not the name that was asked for.
/// `refs/heads/x` is a valid name for the branch `refs/heads/refs/heads/x`,
/// which nobody means, so a full ref is refused as `add --branch` does.
fn check_branch(primary: &Path, name: &str) -> Result<()> {
    let checked = git::raw(primary, &["check-ref-format", "--branch", name])?;
    let accepted = String::from_utf8_lossy(&checked.stdout);
    ensure!(
        checked.status.success()
            && accepted.trim_end_matches(['\n', '\r']) == name
            && !name.starts_with("refs/"),
        "invalid branch name: {name:?}"
    );
    Ok(())
}

/// The private ref in the primary that holds a graft's commit while it is
/// pushed. One name per graft: a push that was killed leaves at most this
/// ref, and the graft's next push removes it.
fn pin(id: &str) -> String {
    format!(
        "refs/graft/push/{}",
        land::short_key(blake3::hash(id.as_bytes()))
    )
}

/// Copy `commit` from the graft into the primary's object store and pin it
/// under `pin`. The copy is the transfer a merge uses to bring its result
/// into the primary: it reads no user configuration and runs no hook, and
/// neither does writing the pin. No branch of the primary moves.
fn import(primary: &Path, graft: &Path, commit: &str, pin: &str) -> Result<()> {
    // A push that was killed can have left the pin on an earlier commit.
    if let Some(stale) = recovery::direct_ref(primary, pin)? {
        objects::remove_import_pin(primary, pin, &stale)?;
    }
    // The new packs are held against garbage collection until the pin exists.
    let _packs = objects::transfer_result(primary, graft, commit)?;
    git::create_private_ref(primary, pin, commit)
}

/// Remove the user-info part of every URL in `text`, so
/// `https://user:secret@host/path` reads `https://host/path`. Git's messages,
/// and those of remote helpers and hooks, can quote the remote's URL.
///
/// As Git does, this takes the authority to end at the first `/`, `?` or
/// `#`. It also ends at whitespace, so an address later on the line is left
/// alone.
fn redact(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme) = rest.find("://") {
        let (before, after) = rest.split_at(scheme + 3);
        clean.push_str(before);
        let end = after
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace() || c.is_control())
            .unwrap_or(after.len());
        rest = match after[..end].rfind('@') {
            Some(at) => &after[at + 1..],
            None => after,
        };
    }
    clean.push_str(rest);
    clean
}

/// Git's messages for a refusal: redacted, and cut to the last
/// [`LISTED_OUTPUT`] bytes. `cut_short` says the capture stopped early, in
/// which case its last line may be half a URL and is dropped.
fn listed_output(stderr: &str, cut_short: bool) -> String {
    let complete = match stderr.rfind('\n') {
        Some(end) if cut_short => &stderr[..end],
        _ => stderr,
    };
    // Redact first: a cut through a URL could otherwise keep its secret.
    let clean = redact(complete.trim_end());
    let mut start = clean.len().saturating_sub(LISTED_OUTPUT);
    while !clean.is_char_boundary(start) {
        start += 1;
    }
    clean[start..].to_owned()
}

/// Read the outcome for `branch` from `git push --porcelain`. Each ref gets
/// one line, `<flag>\t<from>:<to>\t<summary>`, and the flag `!` marks a ref
/// that was not updated. The flag and the bracketed summary are part of the
/// porcelain format and are not translated, unlike Git's messages.
fn classify(success: bool, porcelain: &str, branch: &str) -> Outcome {
    if success {
        return Outcome::Pushed;
    }
    let destination = format!("refs/heads/{branch}");
    for line in porcelain.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some("!"), Some(refs), Some(summary)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if refs.rsplit_once(':').map(|(_, to)| to) != Some(destination.as_str()) {
            continue;
        }
        // The connection ended before the remote said what it did.
        if summary.starts_with("[remote failure]") {
            return Outcome::Failed;
        }
        let reason = summary
            .split_once("] (")
            .and_then(|(_, reason)| reason.strip_suffix(')'))
            .unwrap_or(summary);
        return Outcome::Rejected(reason.to_owned());
    }
    // No line for the branch: Git never reached the remote's answer.
    Outcome::Failed
}

/// The SSH command for a push when the user has not chosen one: OpenSSH in
/// batch mode, so a passphrase, password or host-key question fails the push
/// instead of waiting for an answer nobody can give. `None` when
/// `GIT_SSH_COMMAND`, `GIT_SSH` or `core.sshCommand` names a command, which
/// is then used as it is.
fn batch_ssh(primary: &Path) -> Result<Option<&'static str>> {
    if ["GIT_SSH_COMMAND", "GIT_SSH"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
    {
        return Ok(None);
    }
    let mut read = git::user_command(primary);
    read.args(["config", "--get", "core.sshCommand"]);
    let configured = read.stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    // Exit 1 means the key is not set; any other failure keeps Git's default.
    Ok((configured.code() == Some(1)).then_some("ssh -o BatchMode=yes"))
}

/// Run the push in the primary with the user's own Git configuration and
/// environment. The commit is named by its ID and the destination in full,
/// so no local branch, upstream setting or configured refspec takes part.
/// Nothing is forced. Tags and submodules are left out even if the user's
/// configuration would add them: one ref on one remote is all that changes.
fn send(primary: &Path, remote: &str, commit: &str, branch: &str) -> Result<()> {
    let mut push = git::user_command(primary);
    push.args([
        "push",
        "--porcelain",
        "--no-follow-tags",
        "--recurse-submodules=no",
        "--end-of-options",
        remote,
        &format!("{commit}:refs/heads/{branch}"),
    ]);
    if let Some(ssh) = batch_ssh(primary)? {
        push.env("GIT_SSH_COMMAND", ssh);
    }
    let pushed = Runtime::capture_command(push, "git")?;
    let outcome = classify(
        pushed["success"] == true,
        pushed["stdout"].as_str().unwrap_or_default(),
        branch,
    );
    if outcome == Outcome::Pushed {
        return Ok(());
    }
    let stderr = listed_output(
        pushed["stderr"].as_str().unwrap_or_default(),
        pushed["stderr_truncated"] == true,
    );
    let mut details = json!({"remote": remote, "branch": branch, "stderr": stderr});
    Err(match outcome {
        Outcome::Rejected(reason) => {
            let reason = redact(&reason);
            details["reason"] = json!(reason);
            api::fault(
                "push_rejected",
                format!(
                    "Remote {remote} refused to update branch {branch}: {reason}. Nothing was pushed, and the graft is unchanged. If the remote branch has commits the graft lacks, merge them in the graft and push again, or push to another branch with --to"
                ),
                details,
            )
        }
        _ => {
            let cause = stderr
                .lines()
                .find(|line| line.starts_with("fatal: ") || line.starts_with("error: "))
                .map(|line| format!(": {}", line.trim_end_matches('.')))
                .unwrap_or_default();
            api::fault(
                "push_failed",
                format!(
                    "The push to remote {remote} did not complete{cause}. The graft and the primary's branches are unchanged; details.stderr has Git's output"
                ),
                details,
            )
        }
    })
}

/// Push the `HEAD` commit of the active graft `r`. The caller holds the
/// graft's lock, and passes the project and build locks as `locks`: they
/// cover the import into the primary and are released before Git talks to
/// the remote, which can take as long as the network does.
pub(super) fn run(
    p: &Project,
    r: &Record,
    args: &api::Push,
    locks: (std::fs::File, Option<Held>),
) -> Result<Value> {
    require_remote(&p.repo, &args.remote)?;
    let (commit, branch) = source(r, args.to.as_deref())?;
    check_branch(&p.repo, &branch)?;
    require_review_branch(&p.repo, &r.name, &args.remote, &branch, args.to.as_deref())?;
    let receipt = super::with_attempts(
        p,
        r,
        json!({"action": "push", "name": r.name, "remote": args.remote, "branch": branch,
            "commit": commit, "uncommitted_changes": git::edits(&r.path)?.len()}),
    )?;
    let pin = pin(&r.id);
    import(&p.repo, &r.path, &commit, &pin)?;
    drop(locks);
    let sent = send(&p.repo, &args.remote, &commit, &branch);
    // The pin goes whether or not the push went through.
    let unpinned = objects::remove_import_pin(&p.repo, &pin, &commit);
    match (sent, unpinned) {
        (Ok(()), Ok(())) => Ok(receipt),
        // The remote has the commit, so this is not a failed push.
        (Ok(()), Err(error)) => {
            let mut receipt = receipt;
            receipt["private_ref"] = json!(pin);
            receipt["message"] = json!(format!(
                "The push succeeded, but the private ref {pin} could not be removed from the primary: {error:#}. The graft's next push removes it"
            ));
            Ok(receipt)
        }
        (Err(error), _) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_removes_user_info_and_nothing_else() {
        for (text, clean) in [
            (
                "fatal: unable to access 'https://user:secret@example.com/team/repo.git/': failed",
                "fatal: unable to access 'https://example.com/team/repo.git/': failed",
            ),
            ("https://token@example.com", "https://example.com"),
            (
                "ssh://git@example.com:2222/repo.git",
                "ssh://example.com:2222/repo.git",
            ),
            // Git ends the authority at the first slash, so a later @ is a path.
            (
                "https://example.com/a@b and https://u:p@h/x@y",
                "https://example.com/a@b and https://h/x@y",
            ),
            // A query or fragment also ends the authority.
            ("https://example.com?a@b", "https://example.com?a@b"),
            ("https://example.com#a@b", "https://example.com#a@b"),
            ("https://u:p@example.com?a@b", "https://example.com?a@b"),
            ("https://u:p@example.com#x", "https://example.com#x"),
            // The last @ before the path ends the user-info.
            ("https://user:p@ss@example.com/x", "https://example.com/x"),
            // An address after the URL is not part of it.
            (
                "see https://example.com or mail admin@example.com",
                "see https://example.com or mail admin@example.com",
            ),
            (
                "To https://a:b@one/x\nTo https://c:d@two/y\n",
                "To https://one/x\nTo https://two/y\n",
            ),
            (
                "git@example.com:team/repo.git",
                "git@example.com:team/repo.git",
            ),
            ("no URL here", "no URL here"),
            ("ends with ://", "ends with ://"),
            ("https://ü:ß@example.com/é", "https://example.com/é"),
        ] {
            assert_eq!(redact(text), clean, "{text}");
        }
    }

    #[test]
    fn listed_output_is_redacted_before_it_is_cut() {
        let secret = "https://user:secret@example.com/repo.git";
        // A cut at the byte limit would otherwise start inside the user-info.
        let long = format!("{secret}{}", "x".repeat(LISTED_OUTPUT - 30));
        let listed = listed_output(&long, false);
        assert!(listed.len() <= LISTED_OUTPUT);
        assert!(
            !listed.contains("secret") && !listed.contains("user"),
            "{listed}"
        );
        // A capture that stopped early may end inside a URL: drop that line.
        let listed = listed_output("fatal: first\nremote: https://user:sec", true);
        assert_eq!(listed, "fatal: first");
        assert_eq!(listed_output("fatal: whole\n", false), "fatal: whole");
        // The cut never splits a character.
        let wide = "é".repeat(LISTED_OUTPUT);
        assert!(listed_output(&wide, false).len() <= LISTED_OUTPUT);
    }

    #[test]
    fn push_outcome_follows_the_porcelain_flags() {
        let oid = "6474a0824464c6e99c27d23698228a0870057271";
        let line = |flag: &str, summary: &str| {
            format!("To ../remote.git\n{flag}\t{oid}:refs/heads/task\t{summary}\nDone\n")
        };
        for accepted in [
            line("*", "[new branch]"),
            line(" ", "6474a08..ed8aa61"),
            line("=", "[up to date]"),
        ] {
            assert_eq!(classify(true, &accepted, "task"), Outcome::Pushed);
        }
        for (summary, reason) in [
            ("[rejected] (non-fast-forward)", "non-fast-forward"),
            ("[rejected] (fetch first)", "fetch first"),
            (
                "[remote rejected] (pre-receive hook declined)",
                "pre-receive hook declined",
            ),
            (
                "[remote rejected] (protected branch hook declined)",
                "protected branch hook declined",
            ),
            ("[rejected]", "[rejected]"),
        ] {
            assert_eq!(
                classify(false, &line("!", summary), "task"),
                Outcome::Rejected(reason.to_owned()),
                "{summary}"
            );
        }
        // Windows line endings do not change the reading.
        assert_eq!(
            classify(
                false,
                &line("!", "[rejected] (non-fast-forward)").replace('\n', "\r\n"),
                "task"
            ),
            Outcome::Rejected("non-fast-forward".to_owned())
        );
        // The remote never answered, or Git never reached it.
        for failed in [
            line("!", "[remote failure] (remote failed to report status)"),
            String::new(),
            "To ../remote.git\nDone\n".to_owned(),
            // A refusal of another ref says nothing about this branch.
            format!("!\t{oid}:refs/heads/other\t[rejected] (non-fast-forward)\n"),
            // An exit failure with an accepted line is still a failure.
            line("*", "[new branch]"),
        ] {
            assert_eq!(
                classify(false, &failed, "task"),
                Outcome::Failed,
                "{failed}"
            );
        }
    }

    #[test]
    fn each_graft_has_one_private_pin_name() {
        let first = pin("0d0c7f0e-3c44-4c53-9a43-1f6f5d0a7f11");
        assert_eq!(first, pin("0d0c7f0e-3c44-4c53-9a43-1f6f5d0a7f11"));
        assert_ne!(first, pin("0d0c7f0e-3c44-4c53-9a43-1f6f5d0a7f12"));
        assert!(first.starts_with("refs/graft/push/"));
    }

    #[test]
    fn the_push_runs_git_with_the_users_environment_but_never_the_lease() {
        let dir = Path::new("primary");
        let set = |command: &std::process::Command, key: &str| {
            command
                .get_envs()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.map(|value| value.to_owned()))
        };
        let push = git::user_command(dir);
        // Removed whatever the caller's environment holds.
        for key in [
            "GIT_GRAFT_LEASE",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
        ] {
            assert_eq!(set(&push, key), Some(None), "{key}");
        }
        assert_eq!(set(&push, "GIT_TERMINAL_PROMPT"), Some(Some("0".into())));
        // Command-line configuration reaches the push as the caller gave it.
        for key in ["GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT"] {
            assert_eq!(set(&push, key), Some(std::env::var_os(key)), "{key}");
        }
        // Everything else is inherited: no private configuration, no hook
        // override and no ban on fetching a partial clone's missing objects.
        for key in [
            "GIT_NO_LAZY_FETCH",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_NOSYSTEM",
        ] {
            assert_eq!(set(&push, key), None, "{key}");
        }
        assert!(
            !push
                .get_args()
                .any(|arg| arg.to_string_lossy().contains("hooksPath"))
        );
        assert_eq!(
            set(&git::command(dir), "GIT_NO_LAZY_FETCH"),
            Some(Some("1".into()))
        );
    }

    fn repository(path: &Path) -> Result<()> {
        std::fs::create_dir(path)?;
        git::bytes(path, &["init", "-q", "--template=", "-b", "main"])?;
        for (key, value) in [
            ("user.name", "Push Fixture"),
            ("user.email", "push@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "disabled-hooks"),
        ] {
            git::bytes(path, &["config", key, value])?;
        }
        Ok(())
    }

    fn commit(path: &Path, file: &str) -> Result<String> {
        std::fs::write(path.join(file), file)?;
        git::bytes(path, &["add", "--", file])?;
        git::bytes(path, &["commit", "-qm", file])?;
        git::oid(path, "HEAD")
    }

    #[test]
    fn import_pins_the_commit_and_moves_no_branch_of_the_primary() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let (primary, graft) = (temp.path().join("primary"), temp.path().join("graft"));
        repository(&primary)?;
        repository(&graft)?;
        let base = commit(&primary, "base")?;
        let first = commit(&graft, "first")?;
        let second = commit(&graft, "second")?;
        let pin = pin("fixture");
        let refs =
            |repo: &Path| git::text(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]);
        let branches = refs(&primary)?;

        import(&primary, &graft, &first, &pin)?;
        assert_eq!(recovery::direct_ref(&primary, &pin)?, Some(first.clone()));
        // A killed push left its pin behind: the next one replaces it.
        import(&primary, &graft, &second, &pin)?;
        assert_eq!(recovery::direct_ref(&primary, &pin)?, Some(second.clone()));
        assert_eq!(git::oid(&primary, &second)?, second);
        assert_eq!(git::oid(&primary, "HEAD")?, base);

        // Only the commit that was pinned is released.
        assert!(objects::remove_import_pin(&primary, &pin, &first).is_err());
        objects::remove_import_pin(&primary, &pin, &second)?;
        assert_eq!(refs(&primary)?, branches);
        // Removing a pin that is already gone is not an error.
        objects::remove_import_pin(&primary, &pin, &second)?;
        Ok(())
    }

    #[test]
    fn the_remote_default_branch_is_pushed_only_when_named() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        repository(&repo)?;
        let base = commit(&repo, "base")?;
        git::bytes(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/repo.git",
            ],
        )?;
        // No record of the remote's default branch: nothing is refused.
        assert_eq!(default_branch(&repo, "origin")?, None);
        require_review_branch(&repo, "task", "origin", "main", None)?;

        git::bytes(&repo, &["update-ref", "refs/remotes/origin/main", &base])?;
        git::bytes(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        )?;
        assert_eq!(default_branch(&repo, "origin")?.as_deref(), Some("main"));
        assert_eq!(default_branch(&repo, "fork")?, None);
        let error = api::error_json(
            &require_review_branch(&repo, "task", "origin", "main", None).unwrap_err(),
        );
        assert_eq!(error["code"], "default_branch", "{error}");
        assert_eq!(
            error["details"],
            json!({"name": "task", "remote": "origin", "branch": "main"})
        );
        // Named with --to, another branch, or another remote: allowed.
        require_review_branch(&repo, "task", "origin", "main", Some("main"))?;
        require_review_branch(&repo, "task", "origin", "graft/task", None)?;
        require_review_branch(&repo, "task", "fork", "main", None)?;
        Ok(())
    }

    #[test]
    fn ssh_runs_in_batch_mode_unless_the_user_chose_a_command() -> Result<()> {
        // The caller's own SSH command always wins; nothing to check here.
        if ["GIT_SSH_COMMAND", "GIT_SSH"]
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
        {
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        repository(&repo)?;
        let global = git::user_command(&repo)
            .args(["config", "--get", "core.sshCommand"])
            .output()?;
        if global.status.code() == Some(1) {
            assert_eq!(batch_ssh(&repo)?, Some("ssh -o BatchMode=yes"));
        }
        git::bytes(&repo, &["config", "core.sshCommand", "ssh -i key"])?;
        assert_eq!(batch_ssh(&repo)?, None);
        Ok(())
    }

    #[test]
    fn branch_names_are_checked_by_git() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        repository(&repo)?;
        git::bytes(&repo, &["commit", "-q", "--allow-empty", "-m", "base"])?;
        git::bytes(&repo, &["checkout", "-q", "-b", "other"])?;
        git::bytes(&repo, &["checkout", "-q", "main"])?;
        // A leading + is part of the name. The refspec starts with the
        // commit, so it cannot turn the push into a forced one.
        for valid in ["task", "graft/task", "feature/a.b-c_d", "v1.0", "+task"] {
            check_branch(&repo, valid)?;
        }
        for invalid in [
            "",
            "-force",
            "--delete",
            "HEAD",
            "a..b",
            "a b",
            "a:b",
            "task.lock",
            "task/",
            "refs/heads/task",
            // Git would expand this to the branch checked out before.
            "@{-1}",
            "a\nb",
        ] {
            assert!(check_branch(&repo, invalid).is_err(), "{invalid:?}");
        }
        Ok(())
    }

    #[test]
    fn only_a_remote_the_primary_configures_is_accepted() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        repository(&repo)?;
        let bare = temp.path().join("remote.git");
        git::bytes(
            temp.path(),
            &["init", "-q", "--bare", "--template=", git::path_str(&bare)?],
        )?;
        for name in ["origin", "fork"] {
            git::bytes(
                &repo,
                &[
                    "remote",
                    "add",
                    name,
                    "https://user:secret@example.invalid/repo.git",
                ],
            )?;
        }
        require_remote(&repo, "origin")?;
        require_remote(&repo, "fork")?;
        // A path or URL that `git push` itself would accept is not a name.
        for unknown in [
            "upstream",
            "",
            git::path_str(&bare)?,
            "https://example.invalid/x",
        ] {
            let error = api::error_json(&require_remote(&repo, unknown).unwrap_err());
            assert_eq!(error["code"], "remote_not_found", "{error}");
            assert_eq!(error["details"]["remotes"], json!(["fork", "origin"]));
            assert!(!error.to_string().contains("secret"), "{error}");
        }
        Ok(())
    }
}
