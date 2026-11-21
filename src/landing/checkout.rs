//! Native index ownership and the durable transition for a checked-out branch.
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{
    api::fault,
    cache::{DirectoryIdentity, directory_identity},
    git, recovery, registry,
};

use super::{Plan, git_failure};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum Phase {
    Prepared,
    Updating,
    Indexed,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    plan: Plan,
    token: String,
    phase: Phase,
    directory: DirectoryIdentity,
}

fn read_record(directory: &Path) -> Result<Record> {
    let identity = directory_identity(directory)?;
    let record: Record =
        serde_json::from_slice(&registry::regular(&directory.join("record.json"))?)?;
    ensure!(
        Uuid::parse_str(&record.token)?.to_string() == record.token && record.directory == identity,
        "invalid checkout transition identity at {}",
        directory.display()
    );
    Ok(record)
}

pub(super) struct Update {
    directory: PathBuf,
    record: Record,
}

impl Update {
    pub(super) fn load(admin: &Path) -> Result<Option<Self>> {
        let directory = admin.join("graft").join("checkout-update");
        if !registry::exists(&directory)? {
            return Ok(None);
        }
        let record = read_record(&directory)?;
        Ok(Some(Self { directory, record }))
    }

    pub(super) fn begin(admin: &Path, plan: &Plan) -> Result<Self> {
        let state = admin.join("graft");
        registry::owned_directory(&state)?;
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        #[cfg(not(unix))]
        builder.prefix("checkout-");
        let staged = builder.tempdir_in(&state)?;
        let record = Record {
            plan: plan.clone(),
            token: Uuid::new_v4().to_string(),
            phase: Phase::Prepared,
            directory: directory_identity(staged.path())?,
        };
        registry::atomic(
            &staged.path().join("record.json"),
            &serde_json::to_vec(&record)?,
        )?;
        let directory = state.join("checkout-update");
        recovery::rename_exclusive(&staged.keep(), &directory)?;
        #[cfg(unix)]
        fs::File::open(&state)?.sync_all()?;
        Ok(Self { directory, record })
    }

    pub(super) fn matches(&self, plan: &Plan) -> Result<()> {
        ensure!(
            self.is_for(plan),
            self.recovery_required("another checked-out merge is still pending")
        );
        Ok(())
    }

    pub(super) fn is_for(&self, plan: &Plan) -> bool {
        self.record.plan == *plan
    }

    fn lock_marker(&self) -> String {
        format!("git-graft checkout {}\n", self.record.token)
    }

    pub(super) fn acquire_index(&self, admin: &Path, retrying: bool) -> Result<IndexLock> {
        let path = admin.join("index.lock");
        let marker = self.lock_marker();
        if retrying && registry::exists(&path)? && registry::regular(&path)? == marker.as_bytes() {
            if registry::exists(&self.directory.join("index.next.lock"))? {
                return Err(self.recovery_required(
                    "the interrupted Git checkout still has a staging index lock",
                ));
            }
            Ok(IndexLock { path, marker })
        } else {
            IndexLock::create(path, marker)
        }
    }

    pub(super) fn has_mutated_checkout(&self) -> bool {
        self.record.phase != Phase::Prepared
    }

    pub(super) fn finished_index(&self) -> bool {
        self.record.phase == Phase::Indexed
    }

    fn set_phase(&mut self, phase: Phase) -> Result<()> {
        self.record.phase = phase;
        registry::atomic(
            &self.directory.join("record.json"),
            &serde_json::to_vec(&self.record)?,
        )
    }

    pub(super) fn clear(&self, admin: &Path) -> Result<()> {
        ensure!(
            read_record(&self.directory)? == self.record,
            "checkout transition changed before cleanup at {}",
            self.directory.display()
        );
        let parent = self
            .directory
            .parent()
            .context("checkout transition has no parent")?;
        let retained = parent.join(format!(".checkout-finished-{}", self.record.token));
        let path = admin.join("index.lock");
        if registry::exists(&path)? && registry::regular(&path)? == self.lock_marker().as_bytes() {
            fs::remove_file(path)?;
        }
        // Never recursively delete the live journal. An interruption may
        // leave a partial cleanup in this private path, away from retries.
        recovery::rename_exclusive(&self.directory, &retained)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        ensure!(
            read_record(&retained)? == self.record,
            "unverified checkout transition was retained at {}",
            retained.display()
        );
        for entry in fs::read_dir(&retained)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            ensure!(
                metadata.is_file()
                    && !registry::is_reparse_point(&metadata)
                    && matches!(
                        entry.file_name().to_str(),
                        Some("record.json" | "index.before" | "index.next")
                    ),
                "unrecognized data from the checkout transition was retained at {}",
                retained.display()
            );
        }
        fs::remove_dir_all(retained)?;
        Ok(())
    }

    pub(super) fn recovery_required(&self, cause: impl std::fmt::Display) -> anyhow::Error {
        let plan = &self.record.plan;
        fault(
            "checkout_recovery_required",
            format!(
                "Checkout update is pending. Retry the same operation if the checkout is unchanged. If recovery still stops, save your files and inspect Git status and the saved index at {}. {cause}",
                self.directory.display()
            ),
            json!({"target":plan.target,"previous":plan.old,"commit":plan.new,"recovery_path":self.directory}),
        )
    }

    fn index_command(&self, path: &Path, objects: Option<&Path>) -> Result<std::process::Command> {
        let mut command = git::command(path);
        command
            .env("GIT_INDEX_FILE", self.directory.join("index.next"))
            .args([
                "--no-replace-objects",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.splitIndex=false",
                "-c",
                "core.untrackedCache=false",
                "-c",
                "core.fsync=index",
                "-c",
                "core.fsyncMethod=fsync",
            ]);
        if let Some(objects) = objects {
            recovery::alternate_objects(&mut command, objects)?;
        }
        Ok(command)
    }

    fn two_tree(&self, path: &Path, objects: Option<&Path>, dry_run: bool) -> Result<()> {
        let mut command = self.index_command(path, objects)?;
        command.args(["read-tree", "-m", "-u", "--no-recurse-submodules"]);
        if dry_run {
            command.arg("-n");
        }
        let output = command
            .args([&self.record.plan.old, &self.record.plan.new])
            .output()?;
        if !output.status.success() {
            return Err(git_failure("update destination checkout", &output));
        }
        Ok(())
    }

    fn protect_ignored(&self, path: &Path, objects: Option<&Path>) -> Result<()> {
        let ignored = git::bytes(
            path,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ],
        )?;
        if ignored.is_empty() {
            return Ok(());
        }
        let entries: Vec<&[u8]> = ignored
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .collect();
        let ignored: BTreeSet<_> = entries
            .iter()
            .map(|path| path.strip_suffix(b"/").unwrap_or(path).to_ascii_lowercase())
            .collect();
        let output = self
            .index_command(path, objects)?
            .args([
                "diff-tree",
                "-r",
                "--no-renames",
                "--name-only",
                "-z",
                &self.record.plan.old,
                &self.record.plan.new,
            ])
            .output()?;
        if !output.status.success() {
            return Err(git_failure("check ignored checkout paths", &output));
        }
        let changed: Vec<&[u8]> = output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .collect();
        // Byte comparison folds ASCII only. The filesystem may also fold other
        // case and Unicode normalization, so compare what each path resolves to.
        if let Some(changed) = crate::collision::ignored_collision(path, &entries, &changed)? {
            bail!(
                "an ignored path would be overwritten by checkout: {}",
                String::from_utf8_lossy(changed)
            );
        }
        for changed in changed {
            let mut folded = changed.to_ascii_lowercase();
            let ancestor = folded
                .iter()
                .enumerate()
                .any(|(index, byte)| *byte == b'/' && ignored.contains(&folded[..index]));
            let exact = ignored.contains(&folded);
            folded.push(b'/');
            let descendant = ignored
                .range(folded.clone()..)
                .next()
                .is_some_and(|path| path.starts_with(&folded));
            ensure!(
                !(ancestor || exact || descendant),
                "an ignored path would be overwritten by checkout: {}",
                String::from_utf8_lossy(changed)
            );
        }
        Ok(())
    }

    fn matches_result(&self, path: &Path, objects: Option<&Path>) -> Result<bool> {
        if !registry::exists(&self.directory.join("index.next"))? {
            return Ok(false);
        }
        for args in [
            &[
                "diff-index",
                "--cached",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                &self.record.plan.new,
                "--",
            ][..],
            &[
                "diff-files",
                "--quiet",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=none",
                "--",
            ][..],
        ] {
            let result = self.index_command(path, objects)?.args(args).output()?;
            match result.status.code() {
                Some(0) => (),
                Some(1) => return Ok(false),
                _ => return Err(git_failure("verify recovered checkout", &result)),
            }
        }
        Ok(true)
    }

    pub(super) fn update_index_and_files(
        &mut self,
        path: &Path,
        admin: &Path,
        objects: Option<&Path>,
        publish: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let index = admin.join("index");
        let before_path = self.directory.join("index.before");
        let next_path = self.directory.join("index.next");
        if registry::exists(&self.directory.join("index.next.lock"))? {
            bail!("the private Git index is still locked");
        }
        let current = index_bytes(&index)?;
        if self.has_mutated_checkout() {
            if self.matches_result(path, objects)? {
                let next = registry::regular(&next_path)?;
                ensure!(
                    current.as_deref() == Some(next.as_slice())
                        || current == index_bytes(&before_path)?,
                    "the destination index changed after the checkout update"
                );
                // Native maintenance may have pruned the still-unreferenced
                // publication since the interruption. Retain the new import
                // guard through the surrounding ref commit on every attempt.
                publish()?;
                if current.as_deref() != Some(next.as_slice()) {
                    registry::atomic(&index, &next)?;
                }
                return self.set_phase(Phase::Indexed);
            }
            ensure!(
                current == index_bytes(&before_path)?,
                "the destination index changed after the checkout update"
            );
        }

        ensure!(
            !super::dirty(path)?,
            "destination checkout has local changes: {}",
            path.display()
        );
        if let Some(current) = current {
            registry::atomic(&before_path, &current)?;
            registry::atomic(&next_path, &current)?;
        } else if registry::exists(&next_path)? {
            registry::regular(&next_path)?;
            fs::remove_file(&next_path)?;
        }
        self.protect_ignored(path, objects)?;
        self.two_tree(path, objects, true)?;
        publish()?;
        self.protect_ignored(path, objects)?;
        self.set_phase(Phase::Updating)?;
        self.two_tree(path, objects, false)?;
        let next = registry::regular(&next_path)?;
        registry::atomic(&index, &next)?;
        self.set_phase(Phase::Indexed)
    }
}

pub(super) fn index_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    registry::exists(path)?
        .then(|| registry::regular(path))
        .transpose()
}

pub(super) struct IndexLock {
    path: PathBuf,
    marker: String,
}

impl IndexLock {
    pub(super) fn create(path: PathBuf, marker: String) -> Result<Self> {
        let parent = path.parent().context("Git index lock has no parent")?;
        let mut pending = tempfile::NamedTempFile::new_in(parent)?;
        pending.write_all(marker.as_bytes())?;
        pending.as_file().sync_all()?;
        pending
            .persist_noclobber(&path)
            .with_context(|| format!("cannot lock checkout index at {}", path.display()))?;
        Ok(Self { path, marker })
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if registry::regular(&self.path).is_ok_and(|bytes| bytes == self.marker.as_bytes()) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Project;

    fn fixture(format: &str) -> Result<(tempfile::TempDir, Project, fs::File, Plan)> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repository");
        fs::create_dir(&repo)?;
        git::bytes(
            &repo,
            &[
                "init",
                "-q",
                "--template=",
                "-b",
                "main",
                &format!("--ref-format={format}"),
            ],
        )?;
        for (key, value) in [
            ("user.name", "Checkout Fixture"),
            ("user.email", "checkout@example.invalid"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "disabled-hooks"),
        ] {
            git::bytes(&repo, &["config", key, value])?;
        }
        git::bytes(&repo, &["commit", "-qm", "old", "--allow-empty"])?;
        let old = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["branch", "other"])?;
        git::bytes(&repo, &["switch", "-qc", "incoming"])?;
        fs::write(repo.join("added"), b"landed\n")?;
        git::bytes(&repo, &["add", "added"])?;
        git::bytes(&repo, &["commit", "-qm", "new"])?;
        let new = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["switch", "-q", "main"])?;
        let (p, lock) = Project::open(&repo)?;
        Ok((
            temp,
            p,
            lock,
            Plan {
                target: "main".into(),
                old,
                tip: new.clone(),
                new,
            },
        ))
    }

    fn apply(p: &Project, plan: &Plan, publish: impl FnOnce() -> Result<()>) -> Result<()> {
        super::super::apply_with_destination(
            &p.repo,
            plan,
            &p.repo,
            |reference, _| {
                Ok(
                    (git::symbolic_ref(&p.repo, "HEAD")?.as_deref() == Some(reference))
                        .then(|| p.repo.clone()),
                )
            },
            publish,
        )
    }

    #[test]
    fn locks_exact_head_branch_and_index_on_both_ref_backends() -> Result<()> {
        for format in ["files", "reftable"] {
            let (_temp, p, _lock, plan) = fixture(format)?;
            let repo = &p.repo;
            apply(&p, &plan, || {
                for args in [
                    &["symbolic-ref", "HEAD", "refs/heads/other"][..],
                    &["update-ref", "refs/heads/main", &plan.new, &plan.old][..],
                    &["read-tree", "HEAD"][..],
                ] {
                    assert!(
                        !git::raw(repo, args)?.status.success(),
                        "{format} allowed {args:?}"
                    );
                }
                assert_eq!(git::oid(repo, "HEAD")?, plan.old);
                assert!(!repo.join("added").exists());
                Ok(())
            })?;
            assert_eq!(git::oid(repo, "refs/heads/main")?, plan.new);
            assert_eq!(git::oid(repo, "refs/heads/other")?, plan.old);
            assert_eq!(
                git::symbolic_ref(repo, "HEAD")?.as_deref(),
                Some("refs/heads/main")
            );
            assert_eq!(fs::read(repo.join("added"))?, b"landed\n");
            assert!(git::changes(repo)?.is_empty());
            assert!(Update::load(&p.common)?.is_none());
        }
        Ok(())
    }

    #[test]
    fn refuses_when_head_switches_before_git_prepares_the_update() -> Result<()> {
        let (_temp, p, _lock, plan) = fixture("files")?;
        let previous_index = fs::read(p.common.join("index"))?;
        let error = super::super::apply_with_destination(
            &p.repo,
            &plan,
            &p.repo,
            |_, owns_head_lock| {
                if owns_head_lock {
                    return Ok(None);
                }
                git::bytes(&p.repo, &["symbolic-ref", "HEAD", "refs/heads/other"])?;
                Ok(Some(p.repo.clone()))
            },
            || panic!("the checkout changed before publication"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("destination checkout changed"));
        assert_eq!(git::oid(&p.repo, "refs/heads/main")?, plan.old);
        assert_eq!(git::oid(&p.repo, "refs/heads/other")?, plan.old);
        assert_eq!(fs::read(p.common.join("index"))?, previous_index);
        assert!(!p.repo.join("added").exists());
        assert!(Update::load(&p.common)?.is_none());
        Ok(())
    }

    #[test]
    fn locks_primary_head_while_advancing_an_unchecked_branch() -> Result<()> {
        for format in ["files", "reftable"] {
            for detached in [false, true] {
                let (_temp, p, _lock, mut plan) = fixture(format)?;
                let repo = &p.repo;
                git::bytes(repo, &["branch", "unchecked"])?;
                if detached {
                    git::bytes(repo, &["switch", "-q", "--detach"])?;
                }
                plan.target = "unchecked".into();
                apply(&p, &plan, || {
                    assert!(
                        !git::raw(repo, &["symbolic-ref", "HEAD", "refs/heads/unchecked"])?
                            .status
                            .success()
                    );
                    Ok(())
                })?;
                assert_eq!(git::oid(repo, "HEAD")?, plan.old);
                assert_eq!(git::oid(repo, "refs/heads/unchecked")?, plan.new);
                assert!(!repo.join("added").exists());
            }
        }
        Ok(())
    }

    #[test]
    fn resumes_a_finalized_index_after_native_gc_prunes_the_landing() -> Result<()> {
        let (temp, p, _lock, plan) = fixture("files")?;
        let private = temp.path().join("private objects");
        git::bytes(
            temp.path(),
            &[
                "clone",
                "--bare",
                "--no-local",
                git::path_str(&p.repo)?,
                git::path_str(&private)?,
            ],
        )?;
        let mut transition = Update::begin(&p.common, &plan)?;
        {
            let _index = transition.acquire_index(&p.common, false)?;
            transition.update_index_and_files(&p.repo, &p.common, None, || Ok(()))?;
        }
        assert!(transition.finished_index());
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
        assert_eq!(fs::read(p.repo.join("added"))?, b"landed\n");
        super::super::require_no_pending_checkout(&p.repo, &plan).unwrap_err();

        git::bytes(&p.repo, &["branch", "-D", "incoming"])?;
        git::bytes(
            &p.repo,
            &[
                "reflog",
                "expire",
                "--all",
                "--expire=now",
                "--expire-unreachable=now",
            ],
        )?;
        git::bytes(&p.repo, &["gc", "--prune=now"])?;
        assert!(git::oid(&p.repo, &plan.new).is_err());
        let mut published = false;
        super::super::apply_with_destination(
            &p.repo,
            &plan,
            &private,
            |reference, _| {
                Ok(
                    (git::symbolic_ref(&p.repo, "HEAD")?.as_deref() == Some(reference))
                        .then(|| p.repo.clone()),
                )
            },
            || {
                assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
                git::bytes(
                    &p.repo,
                    &[
                        "-c",
                        "protocol.file.allow=always",
                        "fetch",
                        "--no-write-fetch-head",
                        "--no-auto-maintenance",
                        "--no-tags",
                        git::path_str(&private)?,
                        &plan.new,
                    ],
                )?;
                published = true;
                Ok(())
            },
        )?;
        assert!(published);
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        git::bytes(&p.repo, &["fsck", "--full", "--no-dangling"])?;
        assert!(git::changes(&p.repo)?.is_empty());
        assert!(Update::load(&p.common)?.is_none());
        Ok(())
    }

    #[test]
    fn undo_replay_clears_only_its_own_committed_checkout_update() -> Result<()> {
        let (_temp, p, _lock, plan) = fixture("files")?;
        apply(&p, &plan, || Ok(()))?;
        let repo = &p.repo;
        let previous_tree = git::text(repo, &["rev-parse", &format!("{}^{{tree}}", plan.old)])?;
        let candidate = git::text(
            repo,
            &["commit-tree", &previous_tree, "-p", &plan.new, "-m", "undo"],
        )?;
        let pin = "refs/graft/test/undo";
        git::create_private_ref(repo, pin, &candidate)?;
        let undo = Plan {
            target: plan.target.clone(),
            old: plan.new.clone(),
            new: candidate.clone(),
            tip: plan.new.clone(),
        };
        let mut update = Update::begin(&p.common, &undo)?;
        {
            let _index = update.acquire_index(&p.common, false)?;
            update.update_index_and_files(repo, &p.common, None, || Ok(()))?;
            git::bytes(repo, &["update-ref", "HEAD", &candidate, &plan.new])?;
        }
        assert!(Update::load(&p.common)?.is_some());

        let replay = || {
            crate::undo::reverse(repo, repo, &plan, pin, |_| {
                panic!("undo replay must not apply or republish")
            })
        };
        assert_eq!(replay()?["commit"], candidate);
        assert!(Update::load(&p.common)?.is_none());
        assert!(git::changes(repo)?.is_empty());

        let unrelated = Update::begin(&p.common, &plan)?;
        assert_eq!(replay()?["commit"], candidate);
        assert!(Update::load(&p.common)?.is_some_and(|update| update.is_for(&plan)));
        unrelated.clear(&p.common)?;
        Ok(())
    }

    #[test]
    fn clears_an_indexed_update_after_the_destination_advances() -> Result<()> {
        let (_temp, p, _lock, plan) = fixture("files")?;
        let mut update = Update::begin(&p.common, &plan)?;
        {
            let _index = update.acquire_index(&p.common, false)?;
            update.update_index_and_files(&p.repo, &p.common, None, || Ok(()))?;
            git::bytes(&p.repo, &["update-ref", "HEAD", &plan.new, &plan.old])?;
        }
        git::bytes(&p.repo, &["commit", "-qm", "later", "--allow-empty"])?;
        let current = git::oid(&p.repo, "HEAD")?;
        fs::write(p.repo.join("added"), b"local changes")?;
        let index = fs::read(p.common.join("index"))?;

        super::super::finish_applied_checkout(&p.repo, &plan)?;
        assert!(Update::load(&p.common)?.is_none());
        assert_eq!(git::oid(&p.repo, "HEAD")?, current);
        assert_eq!(fs::read(p.repo.join("added"))?, b"local changes");
        assert_eq!(fs::read(p.common.join("index"))?, index);
        Ok(())
    }

    #[test]
    fn detaches_before_final_cleanup_and_retains_unrecognized_contents() -> Result<()> {
        let (_temp, p, _lock, plan) = fixture("files")?;
        let update = Update::begin(&p.common, &plan)?;
        let retained = p
            .state
            .join(format!(".checkout-finished-{}", update.record.token));
        fs::write(update.directory.join("extra"), b"retain")?;
        assert!(update.clear(&p.common).is_err());
        assert!(Update::load(&p.common)?.is_none());
        assert_eq!(fs::read(retained.join("extra"))?, b"retain");

        // A crash during cleanup can leave a detached directory without its
        // record. The next operation can still use the live transition name.
        fs::remove_file(retained.join("record.json"))?;
        let fresh = Update::begin(&p.common, &plan)?;
        fresh.clear(&p.common)?;
        assert_eq!(fs::read(retained.join("extra"))?, b"retain");
        Ok(())
    }
}
