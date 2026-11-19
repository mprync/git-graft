//! Plan an integration using immutable Git objects, then advance the destination.
mod checkout;
mod installed;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    process::Output,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    api::{Land, fault},
    git::{self, checked_oid, common_dir, symbolic_ref},
    reconcile, recovery, registry,
};

/// The caller records this before applying it so a retry uses the same commit.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
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

fn partially_staged(paths: impl IntoIterator<Item = impl AsRef<[u8]>>) -> anyhow::Error {
    let paths: Vec<_> = paths
        .into_iter()
        .map(|path| String::from_utf8_lossy(path.as_ref()).into_owned())
        .collect();
    fault(
        "partially_staged",
        "Commit or unstage the existing staged version before using land --commit",
        json!({"paths": paths}),
    )
}

fn reject_partially_staged(workspace: &Path) -> Result<BTreeSet<Vec<u8>>> {
    let status = git::bytes(
        workspace,
        &[
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=none",
        ],
    )?;
    let mut staged = BTreeSet::new();
    let mut untracked = Vec::new();
    let mut conflicts = BTreeSet::new();
    for entry in status
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
    {
        let [index, worktree, b' ', path @ ..] = entry else {
            bail!("malformed Git status record");
        };
        match [*index, *worktree] {
            [b'?', b'?'] => untracked.push(path),
            [b'!', b'!'] | [b' ', _] => (),
            [_, b' '] => {
                staged.insert(path);
            }
            _ => {
                conflicts.insert(path);
            }
        }
    }
    for path in untracked {
        if staged.contains(path) {
            conflicts.insert(path);
            continue;
        }
        for (offset, _) in path.iter().enumerate().filter(|(_, byte)| **byte == b'/') {
            if staged.contains(&path[..offset]) {
                conflicts.insert(&path[..offset]);
            }
        }
    }
    if !conflicts.is_empty() {
        return Err(partially_staged(conflicts));
    }
    Ok(staged.into_iter().map(<[u8]>::to_vec).collect())
}

fn index_entries(bytes: &[u8]) -> Result<BTreeMap<&[u8], &[u8]>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
        .map(|row| {
            let tab = row
                .iter()
                .position(|byte| *byte == b'\t')
                .context("malformed Git index record")?;
            Ok((&row[tab + 1..], &row[..tab]))
        })
        .collect()
}

fn stage_workspace(workspace: &Path, installations: &[&str]) -> Result<()> {
    let admin = git::absolute_git_dir(workspace)?;
    let _lock = checkout::IndexLock::create(
        admin.join("index.lock"),
        format!("git-graft staging {}\n", uuid::Uuid::new_v4()),
    )?;
    git::check_idle_with_locks(workspace, true, false)?;
    // Ask Git about the recorded directories, respecting the current checkout's
    // ignore rules. Refuse before writing an index or importing package data.
    let staging_git = || {
        let mut command = git::command(workspace);
        for key in [
            "GIT_LITERAL_PATHSPECS",
            "GIT_GLOB_PATHSPECS",
            "GIT_NOGLOB_PATHSPECS",
            "GIT_ICASE_PATHSPECS",
        ] {
            command.env_remove(key);
        }
        command
    };
    let protected = installed::spellings(workspace, installations)?;
    let literal = |path: &String| format!(":(top,literal,icase){path}");
    if !protected.is_empty() {
        let changes = staging_git()
            .args([
                "--no-optional-locks",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--no-renames",
                "--ignore-submodules=none",
                "--",
            ])
            .args(protected.iter().map(literal))
            .output()?;
        if !changes.status.success() {
            return Err(git_failure("inspect installed dependency paths", &changes));
        }
        if !changes.stdout.is_empty() {
            return Err(installed::exposed(installations));
        }
    }
    let staged = reject_partially_staged(workspace)?;
    let index = admin.join("index");
    let temporary = tempfile::tempdir_in(&admin)?;
    let next = temporary.path().join("index");
    if let Some(original) = checkout::index_bytes(&index)? {
        registry::atomic(&next, &original)?;
    }
    let before = if staged.is_empty() {
        Vec::new()
    } else {
        git::bytes(workspace, &["ls-files", "--stage", "-z"])?
    };
    let private_index = || {
        let mut command = staging_git();
        command.env("GIT_INDEX_FILE", &next).args([
            "--no-replace-objects",
            "-c",
            "core.splitIndex=false",
        ]);
        command
    };
    let output = private_index()
        .args(["add", "--sparse", "-A", "--", ":/"])
        // An ignore rule can change after the status check. Exclude installed
        // directories from the staging command itself as well.
        .args(protected.iter().flat_map(|p| installed::exclusions(p)))
        .output()?;
    if !output.status.success() {
        return Err(git_failure("stage workspace changes", &output));
    }
    if !protected.is_empty() {
        if installed::spellings(workspace, installations)? != protected {
            return Err(installed::exposed(installations));
        }
        let changes = private_index()
            .args([
                "diff",
                "--cached",
                "--quiet",
                "--no-ext-diff",
                "--no-renames",
                "HEAD",
                "--",
            ])
            .args(protected.iter().map(literal))
            .output()?;
        match changes.status.code() {
            Some(0) => (),
            Some(1) => return Err(installed::exposed(installations)),
            _ => return Err(git_failure("verify staged dependency exclusions", &changes)),
        }
    }
    if !staged.is_empty() {
        let original = index_entries(&before)?;
        let output = private_index()
            .args(["ls-files", "--stage", "-z"])
            .output()?;
        if !output.status.success() {
            return Err(git_failure(
                "verify previously staged workspace paths",
                &output,
            ));
        }
        let updated = index_entries(&output.stdout)?;
        let changed: Vec<_> = staged
            .iter()
            .filter(|path| original.get(path.as_slice()) != updated.get(path.as_slice()))
            .collect();
        if !changed.is_empty() {
            return Err(partially_staged(changed));
        }
    }
    registry::atomic(&index, &registry::regular(&next)?)
}

fn merged_tree(
    repo: &Path,
    objects: &Path,
    state: &Path,
    old: &str,
    tip: &str,
    options: &Land,
) -> Result<String> {
    let mut command = git::command(repo);
    git::use_private_objects(&mut command, objects);
    let output = command
        .args([
            "--no-replace-objects",
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            old,
            tip,
        ])
        .output()?;
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
        match reconcile::resolve(repo, Some(objects), state, old, tip, tree, &paths) {
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

/// Prepare the exact landing result in a private object database.
pub(crate) fn prepare_with_import(
    repo: &Path,
    workspace: &Path,
    state: &Path,
    options: &Land,
    objects: &Path,
    installations: &[&str],
    import: impl FnOnce(&str, &str) -> Result<()>,
) -> Result<Plan> {
    let target = options.into.clone();
    let reference = branch_ref(repo, &target)?;
    let old = branch_tip(repo, &reference)?;
    git::check_idle(workspace)?;
    if dirty(workspace)? {
        let message = options
            .commit
            .as_deref()
            .context("workspace has uncommitted changes. Commit them first or pass --commit")?;
        if message.trim().is_empty() {
            bail!("--commit requires a nonempty commit message");
        }
        stage_workspace(workspace, installations)?;
        let output = git::raw(
            workspace,
            &["--no-replace-objects", "commit", "-m", message],
        )?;
        if !output.status.success() {
            return Err(git_failure("commit workspace changes", &output));
        }
        git::check_idle(workspace)?;
        if dirty(workspace)? {
            bail!("workspace still has uncommitted changes after commit");
        }
    }
    let tip = git::text(
        workspace,
        &[
            "--no-replace-objects",
            "rev-parse",
            "--verify",
            "HEAD^{commit}",
        ],
    )?;
    checked_oid(&tip)?;
    import(&old, &tip)?;
    let new = if is_ancestor(objects, &tip, &old)? {
        old.clone()
    } else if !options.squash && is_ancestor(objects, &old, &tip)? {
        tip.clone()
    } else {
        let tree = merged_tree(repo, objects, state, &old, &tip, options)?;
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
        let mut command = git::command(repo);
        git::use_private_objects(&mut command, objects);
        let output = command.args(args).output()?;
        if !output.status.success() {
            return Err(git_failure("create landing commit", &output));
        }
        let oid = String::from_utf8(output.stdout)?;
        oid.trim_end().to_owned()
    };
    checked_oid(&new)?;
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

fn marker(admin: &Path, name: &str) -> Result<Option<String>> {
    match fs::read_to_string(admin.join(name)) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read Git marker {name}")),
    }
}

// Rebase and bisect can hold a branch even while the checkout has a detached HEAD.
pub(crate) fn check_admin_branch_operations(admin: &Path, reference: &str) -> Result<()> {
    for name in [
        "rebase-merge/head-name",
        "rebase-apply/head-name",
        "BISECT_START",
    ] {
        if let Some(value) = marker(admin, name)?
            && {
                let value = value.trim_end_matches(['\n', '\r']);
                value.eq_ignore_ascii_case(reference)
                    || reference
                        .strip_prefix("refs/heads/")
                        .is_some_and(|branch| value.eq_ignore_ascii_case(branch))
            }
        {
            bail!("destination is in use by an unfinished Git operation: {reference}");
        }
    }
    if let Some(value) = marker(admin, "rebase-merge/update-refs")?
        && value
            .lines()
            .step_by(3)
            .any(|line| line.eq_ignore_ascii_case(reference))
    {
        bail!("destination is in use by an unfinished Git operation: {reference}");
    }
    Ok(())
}

fn check_destination(admin: &Path, path: &Path, locks_held: bool) -> Result<()> {
    registry::owned_directory(path).context("registered destination was replaced")?;
    let root = git::text(path, &["rev-parse", "--show-toplevel"])?;
    if dunce::canonicalize(&root)? != path || git::absolute_git_dir(path)? != admin {
        bail!(
            "registered destination no longer points to this repository: {}",
            path.display()
        );
    }
    git::check_idle_with_locks(path, locks_held, locks_held)?;
    if config(
        path,
        &["config", "--type=bool", "--get", "core.sparseCheckout"],
    )?
    .as_deref()
        == Some("true")
    {
        bail!(
            "refusing to change sparse destination checkout: {}",
            path.display()
        );
    }
    ensure!(
        git::text(path, &["rev-parse", "--shared-index-path"])?.is_empty(),
        "switch away from the destination branch or disable its split index before retrying"
    );
    Ok(())
}

fn verify_ref_locks(admin: &Path, reference: &str, format: &str) -> Result<()> {
    let lock_is_file = |path: &Path| -> Result<bool> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(metadata.is_file() && !registry::is_reparse_point(&metadata))
    };
    let held = match format {
        "files" => {
            let mut branch = admin.join(reference).into_os_string();
            branch.push(".lock");
            lock_is_file(&admin.join("HEAD.lock"))? && lock_is_file(Path::new(&branch))?
        }
        "reftable" => lock_is_file(&admin.join("reftable").join("tables.list.lock"))?,
        _ => false,
    };
    ensure!(
        held,
        "Git did not hold the required checkout reference locks"
    );
    Ok(())
}

#[derive(PartialEq, Eq)]
enum PrimaryHead {
    Symbolic(String),
    Detached(String),
}

impl PrimaryHead {
    fn read(repo: &Path) -> Result<Option<Self>> {
        match git::text(repo, &["rev-parse", "--is-bare-repository"])?.as_str() {
            "true" => Ok(None),
            "false" => match symbolic_ref(repo, "HEAD")? {
                Some(reference) => {
                    git::bytes(repo, &["check-ref-format", &reference])?;
                    Ok(Some(Self::Symbolic(reference)))
                }
                None => Ok(Some(Self::Detached(git::oid(repo, "HEAD")?))),
            },
            _ => bail!("cannot determine whether the destination repository is bare"),
        }
    }

    fn verification(&self) -> String {
        match self {
            Self::Symbolic(reference) => {
                format!("option no-deref\nsymref-verify HEAD {reference}\n")
            }
            Self::Detached(oid) => format!("option no-deref\nverify HEAD {oid}\n"),
        }
    }
}

fn apply_in_checkout(
    path: &Path,
    admin: &Path,
    plan: &Plan,
    objects: Option<&Path>,
    pending: Option<checkout::Update>,
    check_occupancy: impl Fn() -> Result<()>,
    publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let reference = format!("refs/heads/{}", plan.target);
    let format = git::text(path, &["rev-parse", "--show-ref-format"])?;
    if !matches!(format.as_str(), "files" | "reftable") {
        return Err(fault(
            "checkout_update_unsupported",
            "Switch away from the destination branch and retry the operation",
            json!({"target":plan.target,"ref_format":format}),
        ));
    }

    let retrying = pending.is_some();
    if !retrying {
        check_destination(admin, path, false)?;
    }
    let mut pending = match pending {
        Some(update) => update,
        None => checkout::Update::begin(admin, plan)?,
    };
    let result = (|| -> Result<()> {
        let _index = pending.acquire_index(admin, retrying)?;
        // Updating through the symbolic reference makes Git hold both HEAD
        // and its referent. A separate symref-verify and branch update is
        // rejected as two updates to HEAD by both native ref backends.
        let edits = format!("update HEAD {} {}\n", plan.new, plan.old);
        recovery::transaction_with_objects(path, &edits, Some("git graft land"), objects, || {
            check_occupancy()?;
            if symbolic_ref(path, "HEAD")?.as_deref() != Some(reference.as_str())
                || branch_tip(path, &reference)? != plan.old
            {
                bail!(
                    "destination branch or tip changed after landing was planned: {}",
                    plan.target
                );
            }
            verify_ref_locks(admin, &reference, &format)?;
            check_destination(admin, path, true)?;
            pending.update_index_and_files(path, admin, objects, || {
                publish()?;
                check_occupancy()
            })?;
            check_occupancy()
        })
    })();
    if let Err(error) = result {
        if pending.has_mutated_checkout() {
            return Err(pending.recovery_required(format!("{error:#}")));
        }
        pending.clear(admin)?;
        return Err(error);
    }
    pending.clear(admin)?;

    // A native fast-forward invokes post-merge after the index and ref are in
    // place. As with Git, this hook cannot reject or reverse an applied update.
    let _ = git::raw(
        path,
        &["hook", "run", "--ignore-missing", "post-merge", "--", "0"],
    );
    Ok(())
}

pub(crate) fn apply_with_destination(
    repo: &Path,
    plan: &Plan,
    object_repo: &Path,
    destination: impl Fn(&str, bool) -> Result<Option<PathBuf>>,
    publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let reference = branch_ref(repo, &plan.target)?;
    checked_oid(&plan.old)?;
    checked_oid(&plan.new)?;
    let admin = common_dir(repo)?;
    let pending = checkout::Update::load(&admin)?;
    if let Some(update) = &pending {
        update.matches(plan)?;
    }
    let current = branch_tip(repo, &reference)?;
    if !is_ancestor(object_repo, &plan.old, &plan.new)? {
        bail!("landing plan does not fast-forward {}", plan.target);
    }
    if plan.new == plan.old
        || (current != plan.old
            && git::oid(repo, &plan.new).is_ok()
            && is_ancestor(repo, &plan.new, &current)?)
    {
        if let Some(update) = pending {
            ensure!(
                update.finished_index(),
                update.recovery_required("the checkout index was not finalized")
            );
            update.clear(&admin)?;
        }
        return Ok(());
    }
    if current != plan.old {
        if let Some(update) = pending {
            return Err(
                update.recovery_required("the destination branch moved during checkout recovery")
            );
        }
        bail!(
            "destination tip changed after landing was planned: {}",
            plan.target
        );
    }

    let objects = if dunce::canonicalize(object_repo)? == dunce::canonicalize(repo)? {
        None
    } else {
        Some(dunce::canonicalize(git::text(
            object_repo,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "objects",
            ],
        )?)?)
    };
    if let Some(path) = destination(&reference, false)? {
        apply_in_checkout(
            &path,
            &admin,
            plan,
            objects.as_deref(),
            pending,
            || {
                ensure!(
                    destination(&reference, true)?.as_ref() == Some(&path),
                    "destination checkout changed after landing was planned: {}",
                    plan.target
                );
                Ok(())
            },
            publish,
        )?;
    } else {
        if let Some(update) = pending {
            return Err(
                update.recovery_required("the destination is no longer checked out in the primary")
            );
        }
        // Hold the primary HEAD as well as the exact destination branch.
        // Otherwise an unchecked destination could become the primary's
        // checked-out branch just before the ref-only update commits.
        let head = PrimaryHead::read(repo)?;
        let format = if head.is_some() {
            let format = git::text(repo, &["rev-parse", "--show-ref-format"])?;
            ensure!(
                matches!(format.as_str(), "files" | "reftable"),
                "cannot lock the primary checkout with ref format {format}; retry with current Git"
            );
            Some(format)
        } else {
            None
        };
        let verify = head
            .as_ref()
            .map_or_else(String::new, PrimaryHead::verification);
        let edits = format!(
            "{verify}option no-deref\nupdate {reference} {} {}\n",
            plan.new, plan.old
        );
        recovery::transaction_with_objects(
            repo,
            &edits,
            Some("git graft land"),
            objects.as_deref(),
            || {
                if let Some(format) = &format {
                    verify_ref_locks(&admin, &reference, format)?;
                    ensure!(
                        PrimaryHead::read(repo)? == head,
                        "primary checkout HEAD changed after landing was planned"
                    );
                }
                if branch_tip(repo, &reference)? != plan.old {
                    bail!(
                        "destination tip changed after landing was planned: {}",
                        plan.target
                    );
                }
                ensure!(
                    destination(&reference, head.is_some())?.is_none(),
                    "destination was checked out after landing was planned: {}",
                    plan.target
                );
                publish()?;
                ensure!(
                    destination(&reference, head.is_some())?.is_none(),
                    "destination was checked out while preparing the landing: {}",
                    plan.target
                );
                Ok(())
            },
        )?;
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

pub(crate) fn require_no_pending_checkout(repo: &Path, plan: &Plan) -> Result<()> {
    if let Some(update) = checkout::Update::load(&common_dir(repo)?)?
        && update.is_for(plan)
    {
        return Err(
            update.recovery_required("resume the checkout update before dropping the workspace")
        );
    }
    Ok(())
}

/// Clear only an indexed journal whose result is already in the branch history.
pub(crate) fn finish_applied_checkout(repo: &Path, plan: &Plan) -> Result<()> {
    let admin = common_dir(repo)?;
    if let Some(update) = checkout::Update::load(&admin)?
        && update.is_for(plan)
    {
        let reference = branch_ref(repo, &plan.target)?;
        checked_oid(&plan.new)?;
        let current = branch_tip(repo, &reference)?;
        ensure!(
            update.finished_index() && is_ancestor(repo, &plan.new, &current)?,
            update.recovery_required("the checkout index or destination has not finalized")
        );
        update.clear(&admin)?;
    }
    Ok(())
}
