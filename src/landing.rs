//! Plan an integration using immutable Git objects, then advance the destination.
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    process::Output,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    api::{Land, fault},
    git::{self, checked_oid, symbolic_ref},
    reconcile, recovery, registry,
};

/// The caller records this before applying it so a retry uses the same commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub target: String,
    pub old: String,
    pub new: String,
    pub tip: String,
}

fn git_failure(operation: &str, output: &Output) -> anyhow::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    anyhow::anyhow!("{operation} failed ({}): {detail}", output.status)
}

fn branch_ref(repo: &Path, branch: &str) -> Result<String> {
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.starts_with("refs/")
        || branch.starts_with("graft/")
        || branch == "HEAD"
    {
        bail!("invalid or graft-managed destination branch: {branch}");
    }
    let reference = format!("refs/heads/{branch}");
    git::bytes(repo, &["check-ref-format", &reference])
        .with_context(|| format!("invalid destination branch: {branch}"))?;
    Ok(reference)
}

fn branch_tip(repo: &Path, reference: &str) -> Result<String> {
    // Matching the actual ref name avoids bypassing worktree occupancy on
    // case-insensitive filesystems where a differently cased name may resolve.
    let refs = git::text(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
            reference,
        ],
    )?;
    for row in refs.lines() {
        if let Some((name, values)) = row.split_once('\0')
            && name == reference
        {
            let (oid, symbolic) = values
                .split_once('\0')
                .context("missing destination ref fields")?;
            if !symbolic.is_empty() {
                bail!("destination must be a direct local branch: {reference}");
            }
            return Ok(checked_oid(oid)?.to_owned());
        }
    }
    bail!("destination local branch does not exist: {reference}");
}

fn is_ancestor(repo: &Path, older: &str, newer: &str) -> Result<bool> {
    let output = git::raw(
        repo,
        &[
            "--no-replace-objects",
            "merge-base",
            "--is-ancestor",
            older,
            newer,
        ],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git_failure("check commit ancestry", &output)),
    }
}

fn dirty(path: &Path) -> Result<bool> {
    Ok(git::changes(path)?
        .iter()
        .any(|change| change.status != "!!"))
}

fn merged_tree(repo: &Path, state: &Path, old: &str, tip: &str, options: &Land) -> Result<String> {
    let output = git::raw(
        repo,
        &[
            "--no-replace-objects",
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            old,
            tip,
        ],
    )?;
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(git_failure("compute landing tree", &output));
    }
    let mut fields = output.stdout.split(|byte| *byte == 0);
    let tree = fields.next().context("missing landing tree")?;
    let tree = checked_oid(std::str::from_utf8(tree)?.trim_end_matches(['\n', '\r']))?;
    if output.status.success() {
        return Ok(tree.to_owned());
    }

    // An empty NUL record ends the index path list. Some conflicts, such as
    // directory renames, are only represented in the following message records.
    let mut raw_paths: BTreeSet<_> = fields
        .by_ref()
        .take_while(|field| !field.is_empty())
        .collect();
    let mut conflicts = Vec::new();
    let mut saw_content_conflict = false;
    let mut other_conflict = false;
    while let Some(count) = fields.next().filter(|field| !field.is_empty()) {
        let count: usize = std::str::from_utf8(count)?
            .parse()
            .context("invalid merge message path count")?;
        let affected: Vec<_> = fields.by_ref().take(count).collect();
        let kind = fields.next().context("missing merge message type")?;
        let message = fields.next().context("missing merge message text")?;
        if kind.starts_with(b"CONFLICT") {
            raw_paths.extend(affected.iter().copied());
            saw_content_conflict |= kind == b"CONFLICT (contents)";
            other_conflict |= kind != b"CONFLICT (contents)";
            let affected: Vec<_> = affected.into_iter().map(String::from_utf8_lossy).collect();
            conflicts.push(json!({
                "type": String::from_utf8_lossy(kind),
                "message": String::from_utf8_lossy(message).trim_end_matches(['\n', '\r']),
                "paths": affected,
            }));
        }
    }
    let paths: Vec<_> = raw_paths
        .iter()
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();
    let eligible = saw_content_conflict
        && !other_conflict
        && !raw_paths.is_empty()
        && raw_paths
            .iter()
            .all(|path| matches!(*path, b"package.json" | b"package-lock.json"));
    let mut details = json!({"target": options.into, "paths": paths, "conflicts": conflicts});
    if options.reconcile && eligible {
        match reconcile::resolve(repo, state, old, tip, tree, &paths) {
            Ok(resolved) => return Ok(resolved),
            Err(error) => details["reconcile_error"] = json!(format!("{error:#}")),
        }
    }
    Err(fault(
        "merge_conflict",
        format!("workspace conflicts with {}", options.into),
        details,
    ))
}

/// Stage and commit workspace changes only on explicit request, then plan from the two tips.
pub fn prepare(repo: &Path, worktree: &Path, state: &Path, options: &Land) -> Result<Plan> {
    let target = options.into.clone();
    let reference = branch_ref(repo, &target)?;
    if symbolic_ref(worktree, "HEAD")?.as_deref() == Some(reference.as_str()) {
        bail!("cannot land a workspace into its own branch");
    }
    let old = branch_tip(repo, &reference)?;
    git::check_idle(worktree)?;
    if dirty(worktree)? {
        let message = options
            .commit
            .as_deref()
            .context("workspace has uncommitted changes. Commit them first or pass --commit")?;
        if message.trim().is_empty() {
            bail!("--commit requires a nonempty commit message");
        }
        // Stage from the worktree root. Sparse workspaces retain their out-of-cone files.
        git::bytes(
            worktree,
            &["--no-replace-objects", "add", "--sparse", "-A", "--", ":/"],
        )?;
        let output = git::raw(worktree, &["--no-replace-objects", "commit", "-m", message])?;
        if !output.status.success() {
            return Err(git_failure("commit workspace changes", &output));
        }
        git::check_idle(worktree)?;
        if dirty(worktree)? {
            bail!("workspace still has uncommitted changes after commit");
        }
    }
    let tip = git::text(
        worktree,
        &[
            "--no-replace-objects",
            "rev-parse",
            "--verify",
            "HEAD^{commit}",
        ],
    )?;
    let new = if is_ancestor(repo, &tip, &old)? {
        old.clone()
    } else if !options.squash && is_ancestor(repo, &old, &tip)? {
        tip.clone()
    } else {
        let tree = merged_tree(repo, state, &old, &tip, options)?;
        let message = if options.squash {
            format!("Squash graft workspace {} into {target}", options.name)
        } else {
            format!("Merge graft workspace {} into {target}", options.name)
        };
        let mut args = vec!["--no-replace-objects", "commit-tree", &tree, "-p", &old];
        if !options.squash {
            args.extend(["-p", &tip]);
        }
        args.extend(["-m", &message]);
        git::text(repo, &args)?
    };
    Ok(Plan {
        target,
        old,
        new,
        tip,
    })
}

fn config(repo: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = git::raw(repo, args)?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8(output.stdout)?
                .trim_end_matches(['\n', '\r'])
                .into(),
        )),
        Some(1) if output.stderr.is_empty() => Ok(None),
        _ => Err(git_failure("read Git config", &output)),
    }
}

fn common_dir(repo: &Path) -> Result<PathBuf> {
    Ok(Path::new(&git::text(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)
    .canonicalize()?)
}

fn marker(admin: &Path, name: &str) -> Result<Option<String>> {
    match fs::read_to_string(admin.join(name)) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read Git marker {name}")),
    }
}

fn check_branch_operations(repo: &Path, reference: &str) -> Result<()> {
    // A rebasing/bisecting worktree can have a detached HEAD. Git still treats
    // its operation's branch (and rebase --update-refs destinations) as in use.
    let common = common_dir(repo)?;
    let mut admins = vec![common.clone()];
    match fs::read_dir(common.join("worktrees")) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    admins.push(entry.path());
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    for admin in admins {
        for name in [
            "rebase-merge/head-name",
            "rebase-apply/head-name",
            "BISECT_START",
        ] {
            if let Some(value) = marker(&admin, name)? {
                let value = value.trim_end_matches(['\n', '\r']);
                if value == reference || reference.strip_prefix("refs/heads/") == Some(value) {
                    bail!("destination is in use by an unfinished Git operation: {reference}");
                }
            }
        }
        if let Some(value) = marker(&admin, "rebase-merge/update-refs")?
            && value.lines().step_by(3).any(|line| line == reference)
        {
            bail!("destination is in use by an unfinished Git operation: {reference}");
        }
    }
    Ok(())
}

fn check_destination(repo: &Path, path: &Path) -> Result<()> {
    registry::owned_directory(path).context("registered destination was replaced")?;
    let root = git::text(path, &["rev-parse", "--show-toplevel"])?;
    if Path::new(&root).canonicalize()? != path.canonicalize()?
        || common_dir(path)? != common_dir(repo)?
    {
        bail!(
            "registered destination no longer points to this repository: {}",
            path.display()
        );
    }
    git::check_idle(path)?;
    if config(
        path,
        &["config", "--type=bool", "--get", "core.sparseCheckout"],
    )?
    .as_deref()
        == Some("true")
    {
        bail!(
            "refusing to change sparse destination worktree: {}",
            path.display()
        );
    }
    if dirty(path)? {
        bail!("destination worktree has local changes: {}", path.display());
    }
    Ok(())
}

fn apply_in_worktree(repo: &Path, path: &Path, reference: &str, plan: &Plan) -> Result<()> {
    check_destination(repo, path)?;
    let mut ref_lock = common_dir(repo)?.join(reference).into_os_string();
    ref_lock.push(".lock");
    if registry::exists(Path::new(&ref_lock))? {
        bail!("destination branch is locked: {}", plan.target);
    }
    let key = format!("branch.{}.mergeOptions", plan.target);
    let override_options = format!("{key}=");
    let mut args = vec![
        "--no-replace-objects",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "maintenance.auto=false",
    ];
    // Some branch options can override FF-only, or even request an abort/quit.
    // The first '=' in '-c name=value' is the separator, so use it only for safe keys.
    if !key.contains('=') {
        args.extend(["-c", &override_options]);
    } else if config(path, &["config", "--get", &key])?
        .is_some_and(|value| !value.trim().is_empty())
    {
        bail!(
            "cannot suppress custom merge options for destination branch {}",
            plan.target
        );
    }
    args.extend([
        "merge",
        "--ff-only",
        "--strategy=ort",
        "--no-overwrite-ignore",
        "--no-autostash",
        "--no-edit",
        "--",
        &plan.new,
    ]);

    // The caller serializes graft operations. External Git commands can still change
    // checkout occupancy or HEAD between these checks and the native Git commands.
    if symbolic_ref(path, "HEAD")?.as_deref() != Some(reference)
        || branch_tip(repo, reference)? != plan.old
    {
        bail!(
            "destination branch or tip changed after landing was planned: {}",
            plan.target
        );
    }
    let output = git::raw(path, &args)?;
    if !output.status.success() {
        // Git checks out the new tree before updating the ref. A late ref-lock
        // failure or rejection from a reference-transaction hook can leave the
        // destination files/index ahead of its ref. Leave both for inspection.
        return Err(git_failure("fast-forward destination worktree", &output))
            .with_context(|| format!("inspect Git status in {} before retrying", path.display()));
    }
    Ok(())
}

/// Apply a persisted plan, including retries after later commits on the destination.
pub fn apply(repo: &Path, plan: &Plan) -> Result<()> {
    let reference = branch_ref(repo, &plan.target)?;
    checked_oid(&plan.old)?;
    checked_oid(&plan.new)?;
    let current = branch_tip(repo, &reference)?;
    if !is_ancestor(repo, &plan.old, &plan.new)? {
        bail!("landing plan does not fast-forward {}", plan.target);
    }
    if is_ancestor(repo, &plan.new, &current)? {
        return Ok(());
    }
    if current != plan.old {
        bail!(
            "destination tip changed after landing was planned: {}",
            plan.target
        );
    }

    check_branch_operations(repo, &reference)?;
    let worktrees = git::worktrees(repo)?;
    let mut destinations = worktrees
        .iter()
        .filter(|tree| !tree.bare && tree.branch.as_deref() == Some(reference.as_str()));
    if let Some(destination) = destinations.next() {
        if destinations.next().is_some() {
            bail!(
                "destination is checked out in multiple worktrees: {}",
                plan.target
            );
        }
        if destination.locked {
            bail!(
                "destination worktree is locked: {}",
                destination.path.display()
            );
        }
        apply_in_worktree(repo, &destination.path, &reference, plan)?;
    } else {
        // A no-deref CAS can accept a symbolic ref whose referent has the
        // expected OID. Check the exact branch again under Git's prepared lock.
        let edits = format!(
            "option no-deref\nupdate {reference} {} {}\n",
            plan.new, plan.old
        );
        recovery::transaction(repo, &edits, Some("git graft land"), || {
            if branch_tip(repo, &reference)? != plan.old {
                bail!(
                    "destination tip changed after landing was planned: {}",
                    plan.target
                );
            }
            Ok(())
        })?;
    }
    let current = branch_tip(repo, &reference)?;
    if !is_ancestor(repo, &plan.new, &current)? {
        bail!(
            "destination did not retain the planned commit: {}",
            plan.target
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Fault;

    fn run(path: &Path, args: &[&str]) -> String {
        git::text(path, args).unwrap_or_else(|error| panic!("git {args:?}: {error:#}"))
    }

    fn commit(path: &Path, message: &str) -> String {
        run(path, &["add", "-A"]);
        run(path, &["commit", "-m", message]);
        run(path, &["rev-parse", "HEAD"])
    }

    fn write(path: &Path, file: &str, value: &str) {
        fs::write(path.join(file), value).unwrap();
    }

    struct Fixture {
        root: tempfile::TempDir,
        repo: PathBuf,
        source: PathBuf,
        base: String,
        options: Land,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let repo = root.path().join("repo");
            let source = root.path().join("workspace with spaces");
            fs::create_dir(&repo).unwrap();
            run(&repo, &["init", "-b", "main"]);
            for (key, value) in [
                ("user.name", "Test"),
                ("user.email", "test@example.invalid"),
                ("core.autocrlf", "false"),
                ("commit.gpgsign", "false"),
                (
                    "core.hooksPath",
                    root.path().join("hooks").to_str().unwrap(),
                ),
            ] {
                run(&repo, &["config", key, value]);
            }
            write(&repo, "base.txt", "base\n");
            let base = commit(&repo, "initial");
            run(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "graft/task",
                    source.to_str().unwrap(),
                ],
            );
            let options = Land {
                name: "task".into(),
                lease: "test".into(),
                into: "main".into(),
                squash: false,
                commit: None,
                discard_ignored: false,
                reconcile: false,
            };
            Self {
                root,
                repo,
                source,
                base,
                options,
            }
        }

        fn plan(&self) -> Result<Plan> {
            prepare(
                &self.repo,
                &self.source,
                &self.root.path().join("state"),
                &self.options,
            )
        }
    }

    #[test]
    fn applies_fast_forward_and_retries_as_noop() {
        let fixture = Fixture::new();
        let no_change = fixture.plan().unwrap();
        assert_eq!(no_change.old, no_change.new);
        apply(&fixture.repo, &no_change).unwrap();

        write(&fixture.source, "feature.txt", "feature\n");
        let tip = commit(&fixture.source, "feature");
        let plan = fixture.plan().unwrap();
        assert_eq!(plan.old, fixture.base);
        assert_eq!(plan.new, tip);
        assert_eq!(plan.tip, tip);
        assert!(!fixture.repo.join("feature.txt").exists());
        apply(&fixture.repo, &plan).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.repo.join("feature.txt")).unwrap(),
            "feature\n"
        );
        assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), tip);
        apply(&fixture.repo, &plan).unwrap();

        write(&fixture.repo, "later.txt", "later\n");
        let later = commit(&fixture.repo, "later");
        apply(&fixture.repo, &plan).unwrap();
        assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), later);
    }

    #[test]
    fn creates_real_merge_or_squash_and_resumes_persisted_plan() {
        for squash in [false, true] {
            let mut fixture = Fixture::new();
            fixture.options.squash = squash;
            write(&fixture.repo, "ours.txt", "ours\n");
            let old = commit(&fixture.repo, "ours");
            write(&fixture.source, "theirs.txt", "theirs\n");
            let tip = commit(&fixture.source, "theirs");
            let plan = fixture.plan().unwrap();
            assert_eq!(plan.old, old);
            assert_eq!(plan.tip, tip);
            let parents = run(&fixture.repo, &["show", "-s", "--format=%P", &plan.new]);
            assert_eq!(
                parents,
                if squash {
                    old.clone()
                } else {
                    format!("{old} {tip}")
                }
            );
            let persisted = serde_json::to_string(&plan).unwrap();
            let resumed: Plan = serde_json::from_str(&persisted).unwrap();
            apply(&fixture.source, &resumed).unwrap();
            apply(&fixture.source, &resumed).unwrap();
            assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), plan.new);
            for file in ["ours.txt", "theirs.txt"] {
                assert!(fixture.repo.join(file).is_file());
            }
            assert_eq!(run(&fixture.repo, &["status", "--porcelain"]), "");
        }
    }

    #[test]
    fn refuses_dirty_and_ignored_collisions_even_with_merge_config() {
        let fixture = Fixture::new();
        write(&fixture.source, "collision.txt", "landed\n");
        commit(&fixture.source, "feature");
        let plan = fixture.plan().unwrap();
        run(&fixture.repo, &["config", "merge.autostash", "true"]);
        run(&fixture.repo, &["config", "merge.ff", "false"]);
        run(
            &fixture.repo,
            &["config", "branch.main.mergeOptions", "--abort"],
        );

        write(&fixture.repo, "base.txt", "keep tracked edit\n");
        assert!(
            apply(&fixture.repo, &plan)
                .unwrap_err()
                .to_string()
                .contains("local changes")
        );
        assert_eq!(
            fs::read_to_string(fixture.repo.join("base.txt")).unwrap(),
            "keep tracked edit\n"
        );
        write(&fixture.repo, "base.txt", "base\n");

        write(&fixture.repo, "collision.txt", "keep untracked file\n");
        assert!(
            apply(&fixture.repo, &plan)
                .unwrap_err()
                .to_string()
                .contains("local changes")
        );
        write(
            &fixture.repo,
            ".git/info/exclude",
            "collision.txt\nnoncollision.txt\n",
        );
        let error = format!("{:#}", apply(&fixture.repo, &plan).unwrap_err());
        assert!(error.contains("collision.txt"), "{error}");
        assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), fixture.base);
        assert_eq!(
            fs::read_to_string(fixture.repo.join("collision.txt")).unwrap(),
            "keep untracked file\n"
        );

        fs::rename(
            fixture.repo.join("collision.txt"),
            fixture.repo.join("noncollision.txt"),
        )
        .unwrap();
        apply(&fixture.repo, &plan).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.repo.join("noncollision.txt")).unwrap(),
            "keep untracked file\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.repo.join("collision.txt")).unwrap(),
            "landed\n"
        );
    }

    #[test]
    fn finds_locked_sparse_and_busy_destination_in_linked_checkout() {
        let mut fixture = Fixture::new();
        fixture.options.into = "target".into();
        let target = fixture.root.path().join("destination with spaces");
        let target_arg = target.to_str().unwrap();
        run(
            &fixture.repo,
            &["worktree", "add", "-b", "target", target_arg],
        );
        write(&fixture.source, "feature.txt", "feature\n");
        commit(&fixture.source, "feature");
        let plan = fixture.plan().unwrap();

        run(&fixture.repo, &["worktree", "lock", target_arg]);
        assert!(
            apply(&fixture.repo, &plan)
                .unwrap_err()
                .to_string()
                .contains("locked")
        );
        run(&fixture.repo, &["worktree", "unlock", target_arg]);
        run(&target, &["sparse-checkout", "set", "subdirectory"]);
        assert!(
            apply(&fixture.repo, &plan)
                .unwrap_err()
                .to_string()
                .contains("sparse")
        );
        run(&target, &["sparse-checkout", "disable"]);

        let admin = PathBuf::from(run(&target, &["rev-parse", "--absolute-git-dir"]));
        write(&admin, "MERGE_HEAD", &plan.tip);
        let error = apply(&fixture.repo, &plan).unwrap_err();
        assert_eq!(error.downcast_ref::<Fault>().unwrap().code, "git_busy");
        fs::remove_file(admin.join("MERGE_HEAD")).unwrap();
        apply(&fixture.repo, &plan).unwrap();
        assert_eq!(run(&target, &["rev-parse", "HEAD"]), plan.new);
        assert!(target.join("feature.txt").is_file());
        assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), fixture.base);
    }

    #[test]
    fn refuses_unexpected_ref_for_unchecked_branch_and_retries_cas() {
        let mut fixture = Fixture::new();
        fixture.options.into = "target".into();
        run(&fixture.repo, &["branch", "target"]);
        write(&fixture.source, "feature.txt", "feature\n");
        commit(&fixture.source, "feature");
        let plan = fixture.plan().unwrap();

        write(&fixture.repo, "other.txt", "other\n");
        let other = commit(&fixture.repo, "other");
        run(
            &fixture.repo,
            &["update-ref", "refs/heads/target", &other, &plan.old],
        );
        assert!(
            apply(&fixture.source, &plan)
                .unwrap_err()
                .to_string()
                .contains("tip changed")
        );
        assert_eq!(
            run(&fixture.repo, &["rev-parse", "refs/heads/target"]),
            other
        );

        let revised = fixture.plan().unwrap();
        apply(&fixture.source, &revised).unwrap();
        apply(&fixture.source, &revised).unwrap();
        assert_eq!(
            run(&fixture.repo, &["rev-parse", "refs/heads/target"]),
            revised.new
        );
        assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), other);
        assert!(!fixture.repo.join("feature.txt").exists());
    }

    #[test]
    fn bare_invocation_updates_the_registered_checkout() {
        let fixture = Fixture::new();
        write(&fixture.source, "feature.txt", "feature\n");
        let tip = commit(&fixture.source, "feature");
        let bare = fixture.root.path().join("bare");
        run(
            fixture.root.path(),
            &[
                "clone",
                "--bare",
                fixture.repo.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let target = fixture.root.path().join("target");
        let source = fixture.root.path().join("source");
        run(
            &bare,
            &["worktree", "add", target.to_str().unwrap(), "main"],
        );
        run(
            &bare,
            &["worktree", "add", source.to_str().unwrap(), "graft/task"],
        );
        let plan = prepare(
            &bare,
            &source,
            &fixture.root.path().join("state"),
            &fixture.options,
        )
        .unwrap();
        apply(&bare, &plan).unwrap();
        assert_eq!(run(&target, &["rev-parse", "HEAD"]), tip);
        assert!(target.join("feature.txt").is_file());
        assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), fixture.base);
    }

    #[test]
    fn refuses_ref_updates_while_destination_is_detached_for_rebase() {
        let fixture = Fixture::new();
        write(&fixture.source, "feature.txt", "feature\n");
        commit(&fixture.source, "feature");
        let plan = fixture.plan().unwrap();
        let admin = PathBuf::from(run(&fixture.repo, &["rev-parse", "--absolute-git-dir"]));
        run(&fixture.repo, &["switch", "--detach"]);
        fs::create_dir(admin.join("rebase-merge")).unwrap();
        write(&admin, "rebase-merge/head-name", "refs/heads/main\n");
        assert!(
            apply(&fixture.source, &plan)
                .unwrap_err()
                .to_string()
                .contains("unfinished Git operation")
        );
        assert_eq!(
            run(&fixture.repo, &["rev-parse", "refs/heads/main"]),
            fixture.base
        );
        assert!(!fixture.repo.join("feature.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_repository_hooks_and_reports_late_git_rejection() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        write(&fixture.source, "feature.txt", "feature\n");
        let tip = commit(&fixture.source, "feature");
        let plan = fixture.plan().unwrap();
        let hooks = fixture.root.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("reference-transaction");
        fs::write(&hook, "#!/bin/sh\nif [ \"$1\" = prepared ]; then\n  while read -r old new ref; do\n    if [ \"$ref\" = refs/heads/main ]; then\n      echo 'landing fixture rejected the ref transaction' >&2\n      exit 1\n    fi\n  done\nfi\n").unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let error = format!("{:#}", apply(&fixture.repo, &plan).unwrap_err());
        assert!(error.contains("landing fixture rejected the ref transaction"));
        assert!(error.contains("inspect Git status"));
        assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), fixture.base);
        assert_eq!(run(&fixture.source, &["rev-parse", "HEAD"]), tip);
        assert!(fixture.repo.join("feature.txt").is_file());
        assert!(
            apply(&fixture.repo, &plan)
                .unwrap_err()
                .to_string()
                .contains("local changes")
        );
    }

    #[test]
    fn reports_conflict_paths_and_preserves_checkout_and_refs() {
        let fixture = Fixture::new();
        let file = "file with spaces.txt";
        write(&fixture.repo, file, "ours\n");
        let old = commit(&fixture.repo, "ours");
        write(&fixture.source, file, "theirs\n");
        let tip = commit(&fixture.source, "theirs");
        let error = fixture.plan().unwrap_err();
        let fault = error.downcast_ref::<Fault>().unwrap();
        assert_eq!(fault.code, "merge_conflict");
        assert_eq!(fault.details["paths"], json!([file]));
        let conflicts = fault.details["conflicts"].as_array().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0]["type"], "CONFLICT (contents)");
        assert_eq!(conflicts[0]["paths"], json!([file]));
        assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), old);
        assert_eq!(run(&fixture.source, &["rev-parse", "HEAD"]), tip);
        assert_eq!(
            fs::read_to_string(fixture.repo.join(file)).unwrap(),
            "ours\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join(file)).unwrap(),
            "theirs\n"
        );
    }

    #[test]
    fn commits_workspace_changes_only_on_explicit_request() {
        let mut fixture = Fixture::new();
        fs::remove_file(fixture.source.join("base.txt")).unwrap();
        write(&fixture.source, ".gitignore", "ignored.txt\n");
        write(&fixture.source, "ignored.txt", "local cache\n");
        write(&fixture.source, "new.txt", "new workspace file\n");
        assert!(
            fixture
                .plan()
                .unwrap_err()
                .to_string()
                .contains("Commit them first")
        );
        assert_eq!(run(&fixture.source, &["rev-parse", "HEAD"]), fixture.base);
        assert_eq!(
            run(&fixture.source, &["diff", "--cached", "--name-only"]),
            ""
        );

        fixture.options.commit = Some("Workspace changes".into());
        let plan = fixture.plan().unwrap();
        assert_eq!(
            run(&fixture.source, &["show", "-s", "--format=%s", &plan.tip]),
            "Workspace changes"
        );
        assert_eq!(
            run(&fixture.source, &["ls-tree", "--name-only", "HEAD"]),
            ".gitignore\nnew.txt"
        );
        assert_eq!(
            fs::read_to_string(fixture.source.join("ignored.txt")).unwrap(),
            "local cache\n"
        );
        assert_eq!(fixture.plan().unwrap().tip, plan.tip);
    }

    #[test]
    fn reports_modify_delete_and_rename_delete_without_reconciliation() {
        for rename in [false, true] {
            let mut fixture = Fixture::new();
            fixture.options.reconcile = true;
            let file = "package.json";
            let renamed = "renamed.json";
            write(
                &fixture.repo,
                file,
                "{\"name\":\"fixture\",\"version\":\"1.0.0\"}\n",
            );
            let base = commit(&fixture.repo, "add manifest");
            run(&fixture.source, &["merge", "--ff-only", &base]);
            fs::remove_file(fixture.repo.join(file)).unwrap();
            let old = commit(&fixture.repo, "remove manifest");
            let (kind, paths) = if rename {
                fs::rename(fixture.source.join(file), fixture.source.join(renamed)).unwrap();
                ("CONFLICT (rename/delete)", json!([file, renamed]))
            } else {
                write(
                    &fixture.source,
                    file,
                    "{\"name\":\"fixture\",\"version\":\"2.0.0\"}\n",
                );
                ("CONFLICT (modify/delete)", json!([file]))
            };
            let tip = commit(&fixture.source, "change manifest");

            let error = fixture.plan().unwrap_err();
            let fault = error.downcast_ref::<Fault>().unwrap();
            assert_eq!(fault.code, "merge_conflict");
            assert_eq!(fault.details["paths"], paths);
            let conflicts = fault.details["conflicts"].as_array().unwrap();
            assert_eq!(conflicts.len(), 1);
            assert_eq!(conflicts[0]["type"], kind);
            assert!(conflicts[0]["message"].as_str().unwrap().contains(file));
            assert!(fault.details.get("reconcile_error").is_none());
            assert!(!fixture.root.path().join("state").exists());
            assert_eq!(run(&fixture.repo, &["rev-parse", "HEAD"]), old);
            assert_eq!(run(&fixture.source, &["rev-parse", "HEAD"]), tip);
        }
    }

    #[test]
    fn reconcile_only_resolves_eligible_conflicts_when_requested() {
        fn write_packages(path: &Path, package: Option<&str>) {
            let mut manifest = json!({"name": "fixture", "version": "1.0.0"});
            let mut lock = json!({
                "name": "fixture", "version": "1.0.0", "lockfileVersion": 3, "requires": true,
                "packages": {"": {"name": "fixture", "version": "1.0.0"}}
            });
            if let Some(package) = package {
                let dependencies = json!({package: "1.0.0"});
                manifest["dependencies"] = dependencies.clone();
                lock["packages"][""]["dependencies"] = dependencies;
                lock["packages"][format!("node_modules/{package}")] = json!({
                    "version": "1.0.0", "resolved": format!("https://registry.npmjs.org/{package}/-/{package}-1.0.0.tgz"),
                    "integrity": "sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=="
                });
            }
            write(
                path,
                "package.json",
                &serde_json::to_string_pretty(&manifest).unwrap(),
            );
            write(
                path,
                "package-lock.json",
                &serde_json::to_string_pretty(&lock).unwrap(),
            );
        }

        let mut fixture = Fixture::new();
        write_packages(&fixture.repo, None);
        let base = commit(&fixture.repo, "npm base");
        run(&fixture.source, &["merge", "--ff-only", &base]);
        write_packages(&fixture.repo, Some("alpha"));
        write(&fixture.repo, "ours.txt", "ours\n");
        commit(&fixture.repo, "ours");
        write_packages(&fixture.source, Some("beta"));
        write(&fixture.source, "theirs.txt", "theirs\n");
        commit(&fixture.source, "theirs");

        let error = fixture.plan().unwrap_err();
        let fault = error.downcast_ref::<Fault>().unwrap();
        assert_eq!(fault.code, "merge_conflict");
        assert_eq!(
            fault.details["paths"],
            json!(["package-lock.json", "package.json"])
        );
        assert!(!fixture.root.path().join("state").exists());

        fixture.options.reconcile = true;
        let plan = fixture.plan().unwrap_or_else(|error| {
            panic!(
                "{error:#} {}",
                error
                    .downcast_ref::<Fault>()
                    .map(|fault| fault.details.to_string())
                    .unwrap_or_default()
            )
        });
        apply(&fixture.repo, &plan).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.repo.join("package.json")).unwrap()).unwrap();
        assert_eq!(
            manifest["dependencies"],
            json!({"alpha": "1.0.0", "beta": "1.0.0"})
        );
        for file in ["ours.txt", "theirs.txt"] {
            assert!(fixture.repo.join(file).is_file());
        }
    }
}
