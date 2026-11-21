//! Native index ownership and the durable transition for a checked-out branch.
//!
//! An update of the checked-out branch is journaled in three phases.
//! `Prepared` has changed nothing. `Updating` means native `read-tree` may
//! have rewritten some files. `Indexed` means the files and index hold the
//! merged commit and only the ref commit remains. A retry does not trust the
//! phase alone: it rebuilds a private index from the files on disk, then lets
//! Git finish the update or, for another merge's journal, return the files to
//! the previous commit. Each changed path must still hold one of the two
//! recorded versions, so neither direction replaces anyone's edits, and
//! Git's own checks keep untracked and ignored files in place.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{
    api::{Fault, fault},
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
    /// The last native Git command that locked the private index, such as
    /// the `read-tree` that writes files during `Updating`. A retry changes
    /// nothing while its lock remains and this process may still be running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker: Option<u32>,
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

/// An index entry in the form `update-index --index-info` reads. Mode
/// `000000` with the null object ID means the path is absent on that side.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    mode: String,
    oid: String,
}

impl Entry {
    fn present(&self) -> bool {
        self.mode != "000000"
    }
}

/// One path the update changes, with its entry before and after.
struct Change {
    path: Vec<u8>,
    old: Entry,
    new: Entry,
}

/// Name at most a screenful of paths in a message.
fn listed(paths: &[Vec<u8>]) -> String {
    let mut names: Vec<_> = paths
        .iter()
        .take(20)
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();
    if paths.len() > names.len() {
        names.push(format!("and {} more", paths.len() - names.len()));
    }
    names.join(", ")
}

/// Stage-zero entries of the index Git reads for `command`, by path.
fn staged_entries(mut command: Command) -> Result<BTreeMap<Vec<u8>, Entry>> {
    let output = command.args(["ls-files", "--stage", "-z"]).output()?;
    if !output.status.success() {
        return Err(git_failure("read checkout index", &output));
    }
    let mut entries = BTreeMap::new();
    for row in output.stdout.split(|byte| *byte == 0) {
        if row.is_empty() {
            continue;
        }
        let tab = row
            .iter()
            .position(|byte| *byte == b'\t')
            .context("malformed Git index record")?;
        let header = std::str::from_utf8(&row[..tab])?;
        let mut fields = header.split(' ');
        let (Some(mode), Some(oid), Some(stage), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            bail!("malformed Git index record");
        };
        ensure!(
            stage == "0",
            "the destination index has unresolved conflicts"
        );
        entries.insert(
            row[tab + 1..].to_vec(),
            Entry {
                mode: mode.into(),
                oid: oid.into(),
            },
        );
    }
    Ok(entries)
}

/// Whether a process that wrote a private lock has certainly exited.
#[cfg(unix)]
fn worker_exited(worker: Option<u32>) -> bool {
    let Some(pid) = worker
        .and_then(|pid| libc::pid_t::try_from(pid).ok())
        .filter(|pid| *pid > 0)
    else {
        return false;
    };
    // Signal 0 only asks whether the process exists. ESRCH proves it does
    // not; a reused ID merely keeps the lock, which is the safe outcome.
    let result = unsafe { libc::kill(pid, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
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
            worker: None,
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

    pub(super) fn plan(&self) -> &Plan {
        &self.record.plan
    }

    pub(super) fn is_for(&self, plan: &Plan) -> bool {
        self.record.plan == *plan
    }

    fn lock_marker(&self) -> String {
        format!("git-graft checkout {}\n", self.record.token)
    }

    /// Hold the primary index. The lock of an interrupted attempt carries
    /// this journal's token; the project lock proves its process has exited.
    pub(super) fn acquire_index(&self, admin: &Path, retrying: bool) -> Result<IndexLock> {
        let path = admin.join("index.lock");
        let marker = self.lock_marker();
        if retrying && registry::exists(&path)? && registry::regular(&path)? == marker.as_bytes() {
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

    fn save(&self) -> Result<()> {
        registry::atomic(
            &self.directory.join("record.json"),
            &serde_json::to_vec(&self.record)?,
        )
    }

    /// Record a new phase. Every earlier private Git command has exited.
    fn set_phase(&mut self, phase: Phase) -> Result<()> {
        self.record.phase = phase;
        self.record.worker = None;
        self.save()
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

    /// Stop tracking an update that can no longer finish, because its branch
    /// moved or is no longer checked out. The checkout is not touched. The
    /// journal, including the saved original index, stays for inspection.
    pub(super) fn set_aside(self, admin: &Path) -> Result<PathBuf> {
        self.settle_worker()?;
        ensure!(
            read_record(&self.directory)? == self.record,
            "checkout transition changed before it was set aside at {}",
            self.directory.display()
        );
        let parent = self
            .directory
            .parent()
            .context("checkout transition has no parent")?;
        let retained = parent.join(format!(".checkout-set-aside-{}", self.record.token));
        let path = admin.join("index.lock");
        if registry::exists(&path)? && registry::regular(&path)? == self.lock_marker().as_bytes() {
            fs::remove_file(path)?;
        }
        recovery::rename_exclusive(&self.directory, &retained)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(retained)
    }

    pub(super) fn recovery_required(&self, cause: impl std::fmt::Display) -> anyhow::Error {
        self.stopped(cause, &[])
    }

    /// Report why recovery stopped, naming any paths that need attention.
    fn stopped(&self, cause: impl std::fmt::Display, paths: &[Vec<u8>]) -> anyhow::Error {
        let plan = &self.record.plan;
        let paths: Vec<_> = paths
            .iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect();
        fault(
            "checkout_recovery_required",
            format!(
                "The checkout update of {} to {} was interrupted. Retry the merge or undo that started it, or any merge into this repository: Graft finishes or reverses the update from the files on disk and leaves every other change in place. Stopped because {cause}. The saved original index is kept in {}.",
                plan.target,
                plan.new,
                self.directory.display()
            ),
            json!({"target":plan.target,"previous":plan.old,"commit":plan.new,
                "graft_tip":plan.tip,"recovery_path":self.directory,"paths":paths}),
        )
    }

    /// Keep a recovery report as it is; wrap any other failure in one.
    pub(super) fn explain(&self, error: anyhow::Error) -> anyhow::Error {
        if error
            .downcast_ref::<Fault>()
            .is_some_and(|fault| fault.code == "checkout_recovery_required")
        {
            error
        } else {
            self.recovery_required(format!("{error:#}"))
        }
    }

    /// Wait out the native update of an interrupted attempt. Git keeps its
    /// private index lock from its first file write until it exits.
    fn settle_worker(&self) -> Result<()> {
        let lock = self.directory.join("index.next.lock");
        if !registry::exists(&lock)? {
            return Ok(());
        }
        #[cfg(unix)]
        let released = worker_exited(self.record.worker) && {
            fs::remove_file(&lock)?;
            true
        };
        // Windows refuses to delete a file that a running process holds
        // open. Every later step also verifies the files it relies on.
        #[cfg(not(unix))]
        let released = fs::remove_file(&lock).is_ok();
        if released {
            return Ok(());
        }
        Err(self.recovery_required(match self.record.worker {
            Some(pid) => format!(
                "Git process {pid} from the interrupted update may still be writing files. Wait for it to exit, then retry"
            ),
            None => "the interrupted Git update still holds its private index lock".into(),
        }))
    }

    fn index_command(&self, path: &Path, objects: Option<&Path>) -> Result<Command> {
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

    /// Run a Git command that locks the private index, recording its
    /// process ID so a retry can prove it has exited before it reclaims the
    /// lock or touches the same files. `input` goes to standard input.
    fn run_private(&mut self, mut command: Command, input: Option<&[u8]>) -> Result<Output> {
        if self.record.worker.is_some() {
            // Never leave an exited process on record for a live lock.
            self.record.worker = None;
            self.save()?;
        }
        let mut child = command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        self.record.worker = Some(child.id());
        let recorded = self.save();
        let written = match (child.stdin.take(), input) {
            (Some(mut stdin), Some(input)) => stdin.write_all(input),
            _ => Ok(()),
        };
        let output = child.wait_with_output();
        recorded?;
        let output = output?;
        if output.status.success() {
            written?;
        }
        Ok(output)
    }

    /// Move the files from one recorded commit to the other with Git's
    /// two-tree merge, which keeps untracked and ignored files.
    fn two_tree(
        &mut self,
        path: &Path,
        objects: Option<&Path>,
        forward: bool,
        dry_run: bool,
    ) -> Result<()> {
        let plan = &self.record.plan;
        let (from, to) = if forward {
            (plan.old.clone(), plan.new.clone())
        } else {
            (plan.new.clone(), plan.old.clone())
        };
        let mut command = self.index_command(path, objects)?;
        command.args(["read-tree", "-m", "-u", "--no-recurse-submodules"]);
        if dry_run {
            command.arg("-n");
        }
        command.args([&from, &to]);
        let output = self.run_private(command, None)?;
        if !output.status.success() {
            return Err(git_failure("update destination checkout", &output));
        }
        Ok(())
    }

    fn protect_ignored(&self, path: &Path, objects: Option<&Path>) -> Result<()> {
        // List ignored files against the private index, which already tracks
        // any path a resumed update wrote before its interruption.
        let ignored = self
            .index_command(path, objects)?
            .args([
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ])
            .output()?;
        if !ignored.status.success() {
            return Err(git_failure("list ignored checkout paths", &ignored));
        }
        let ignored = ignored.stdout;
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

    /// Every path the update changes, with both sides' index entries.
    fn changes(&self, path: &Path, objects: Option<&Path>) -> Result<Vec<Change>> {
        let plan = &self.record.plan;
        let output = self
            .index_command(path, objects)?
            .args([
                "diff-tree",
                "-r",
                "--no-renames",
                "--raw",
                "-z",
                &plan.old,
                &plan.new,
            ])
            .output()?;
        if !output.status.success() {
            return Err(git_failure("list checkout changes", &output));
        }
        let mut fields = output.stdout.split(|byte| *byte == 0);
        let mut changes = Vec::new();
        while let Some(header) = fields.next().filter(|field| !field.is_empty()) {
            let path = fields
                .next()
                .filter(|field| !field.is_empty())
                .context("malformed Git tree difference")?;
            let header = std::str::from_utf8(header)?
                .strip_prefix(':')
                .context("malformed Git tree difference")?;
            let mut parts = header.split(' ');
            let (Some(old_mode), Some(new_mode), Some(old_oid), Some(new_oid)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                bail!("malformed Git tree difference");
            };
            changes.push(Change {
                path: path.to_vec(),
                old: Entry {
                    mode: old_mode.into(),
                    oid: old_oid.into(),
                },
                new: Entry {
                    mode: new_mode.into(),
                    oid: new_oid.into(),
                },
            });
        }
        Ok(changes)
    }

    /// Start the private index from `index`, or empty when there is none.
    fn seed(&self, index: Option<&[u8]>) -> Result<()> {
        let next = self.directory.join("index.next");
        match index {
            Some(bytes) => registry::atomic(&next, bytes),
            None if registry::exists(&next)? => {
                registry::regular(&next)?;
                Ok(fs::remove_file(&next)?)
            }
            None => Ok(()),
        }
    }

    /// Write a private index: `index` with the given entry for each path,
    /// refreshed against the files on disk.
    fn write_view<'a>(
        &mut self,
        path: &Path,
        objects: Option<&Path>,
        index: Option<&[u8]>,
        entries: impl IntoIterator<Item = (&'a [u8], &'a Entry)>,
    ) -> Result<()> {
        self.seed(index)?;
        // Removals go first so a file can give way to a directory.
        let mut removals = Vec::new();
        let mut additions = Vec::new();
        for (name, entry) in entries {
            let list = if entry.present() {
                &mut additions
            } else {
                &mut removals
            };
            list.extend_from_slice(format!("{} {}\t", entry.mode, entry.oid).as_bytes());
            list.extend_from_slice(name);
            list.push(0);
        }
        removals.append(&mut additions);
        if !removals.is_empty() {
            let mut command = self.index_command(path, objects)?;
            command.args(["update-index", "-z", "--index-info"]);
            let output = self.run_private(command, Some(&removals))?;
            if !output.status.success() {
                return Err(git_failure("build recovery index", &output));
            }
        }
        let mut command = self.index_command(path, objects)?;
        command.args(["update-index", "-q", "--refresh"]);
        let output = self.run_private(command, None)?;
        if !output.status.success() {
            return Err(git_failure("refresh recovery index", &output));
        }
        Ok(())
    }

    /// Paths whose files differ from the private index, with Git's status
    /// letter. `D` means Git finds no file for the entry.
    fn differing(&self, path: &Path, objects: Option<&Path>) -> Result<BTreeMap<Vec<u8>, u8>> {
        let output = self
            .index_command(path, objects)?
            .args([
                "diff-files",
                "--name-status",
                "-z",
                "--ignore-submodules=none",
                "--",
            ])
            .output()?;
        if !output.status.success() {
            return Err(git_failure("compare checkout files", &output));
        }
        let mut fields = output.stdout.split(|byte| *byte == 0);
        let mut differing = BTreeMap::new();
        while let Some(status) = fields.next().filter(|field| !field.is_empty()) {
            let name = fields.next().context("malformed Git file difference")?;
            differing.insert(name.to_vec(), status[0]);
        }
        Ok(differing)
    }

    /// Rebuild the private index from the files on disk after an interrupted
    /// update, so that `read-tree` can finish it (`forward`) or reverse it.
    /// Each changed path takes the entry whose version its file holds. Git
    /// removes a file before it writes the replacement, so a missing file
    /// takes the entry of the side the update moves from, and is written
    /// again. Staged entries for other paths are kept. A changed path with
    /// any other content or staged version stops recovery before a file is
    /// written.
    fn rebuild(
        &mut self,
        path: &Path,
        admin: &Path,
        objects: Option<&Path>,
        forward: bool,
    ) -> Result<()> {
        let changes = self.changes(path, objects)?;
        let staged = staged_entries(git::command(path))?;
        let mut edited = Vec::new();
        for change in &changes {
            let entry = staged.get(&change.path);
            let holds = |side: &Entry| {
                if side.present() {
                    entry == Some(side)
                } else {
                    entry.is_none()
                }
            };
            if !holds(&change.old) && !holds(&change.new) {
                edited.push(change.path.clone());
            }
        }
        if !edited.is_empty() {
            return Err(self.stopped(
                format!(
                    "these paths have staged changes made after the interruption: {}. Graft changed nothing. Record the staged versions somewhere safe, unstage them, then retry",
                    listed(&edited)
                ),
                &edited,
            ));
        }

        let index = index_bytes(&admin.join("index"))?;
        let index = index.as_deref();
        let side = |old: bool| {
            changes.iter().map(move |change| {
                (
                    &change.path[..],
                    if old { &change.old } else { &change.new },
                )
            })
        };
        self.write_view(path, objects, index, side(true))?;
        let from_old = self.differing(path, objects)?;
        self.write_view(path, objects, index, side(false))?;
        let from_new = self.differing(path, objects)?;
        // A side holds when its file is unchanged or missing. A side without
        // the path holds when Git finds no file there.
        let holds = |entry: &Entry,
                     own: &BTreeMap<Vec<u8>, u8>,
                     other: &BTreeMap<Vec<u8>, u8>,
                     name: &[u8]| {
            if entry.present() {
                own.get(name).is_none_or(|status| *status == b'D')
            } else {
                other.get(name) == Some(&b'D')
            }
        };
        let mut chosen = Vec::with_capacity(changes.len());
        for change in &changes {
            let name = &change.path;
            let old = (&change.old, &from_old, &from_new);
            let new = (&change.new, &from_new, &from_old);
            let (first, second) = if forward { (old, new) } else { (new, old) };
            if let Some((entry, _, _)) = [first, second]
                .into_iter()
                .find(|(entry, own, other)| holds(entry, own, other, name))
            {
                chosen.push((&name[..], entry));
            } else {
                edited.push(name.clone());
            }
        }
        if !edited.is_empty() {
            return Err(self.stopped(
                format!(
                    "these files changed after the interruption, so Graft cannot tell which version to keep: {}. Graft left them as they are. Keep a copy of your edits, return each file to its version in {} or in the merged commit, then retry",
                    listed(&edited),
                    self.record.plan.target
                ),
                &edited,
            ));
        }
        self.write_view(path, objects, index, chosen)
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
        self.settle_worker()?;
        let current = index_bytes(&index)?;
        if self.has_mutated_checkout() {
            let next = index_bytes(&next_path)?;
            let unchanged = current == index_bytes(&before_path)? || current == next;
            if unchanged && self.matches_result(path, objects)? {
                let next = next.context("the finished checkout index is missing")?;
                // Native maintenance may have pruned the still-unreferenced
                // publication since the interruption. Retain the new import
                // guard through the surrounding ref commit on every attempt.
                publish()?;
                if current.as_deref() != Some(next.as_slice()) {
                    registry::atomic(&index, &next)?;
                }
                return self.set_phase(Phase::Indexed);
            }
            // The files can hold a mix of both versions, and someone may
            // have edited or staged other paths since. Git finishes the
            // update from a private index that matches the files.
            self.rebuild(path, admin, objects, true)?;
        } else {
            ensure!(
                !super::dirty(path)?,
                "destination checkout has local changes: {}",
                path.display()
            );
            if let Some(current) = &current {
                registry::atomic(&before_path, current)?;
            }
            self.seed(current.as_deref())?;
        }
        self.protect_ignored(path, objects)?;
        self.two_tree(path, objects, true, true)?;
        publish()?;
        self.protect_ignored(path, objects)?;
        self.set_phase(Phase::Updating)?;
        self.two_tree(path, objects, true, false)?;
        let next = registry::regular(&next_path)?;
        registry::atomic(&index, &next)?;
        self.set_phase(Phase::Indexed)
    }

    /// Return the files and index to the previous commit after another
    /// merge's update was interrupted. The caller holds the primary index
    /// and the destination ref locks, and has checked the branch is still at
    /// the previous commit. Afterwards the journal can be cleared.
    pub(super) fn restore_previous(&mut self, path: &Path, admin: &Path) -> Result<()> {
        self.settle_worker()?;
        if !self.has_mutated_checkout() {
            return Ok(());
        }
        self.rebuild(path, admin, None, false)?;
        self.protect_ignored(path, None)?;
        self.two_tree(path, None, false, true)?;
        // Until the restore finishes, files can again hold either version,
        // and any later attempt resumes from them in either direction.
        self.set_phase(Phase::Updating)?;
        self.two_tree(path, None, false, false)?;
        let next = registry::regular(&self.directory.join("index.next"))?;
        registry::atomic(&admin.join("index"), &next)
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

    /// A primary at `old` and a commit `new` that modifies, adds and
    /// deletes files and turns the file `swap` into a directory.
    fn partial_fixture() -> Result<(tempfile::TempDir, Project, fs::File, Plan)> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repository");
        fs::create_dir(&repo)?;
        git::bytes(&repo, &["init", "-q", "--template=", "-b", "main"])?;
        for (key, value) in [
            ("user.name", "Checkout Fixture"),
            ("user.email", "checkout@example.invalid"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "disabled-hooks"),
        ] {
            git::bytes(&repo, &["config", key, value])?;
        }
        for (name, content) in [
            (".gitignore", "ignored/\n"),
            ("a.txt", "a\n"),
            ("b.txt", "b\n"),
            ("gone.txt", "gone\n"),
            ("swap", "file\n"),
            ("unrelated.txt", "unrelated\n"),
        ] {
            fs::write(repo.join(name), content)?;
        }
        git::bytes(&repo, &["add", "-A"])?;
        git::bytes(&repo, &["commit", "-qm", "old"])?;
        let old = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["switch", "-qc", "incoming"])?;
        fs::write(repo.join("a.txt"), "A\n")?;
        fs::write(repo.join("b.txt"), "B\n")?;
        fs::write(repo.join("added.txt"), "added\n")?;
        fs::remove_file(repo.join("gone.txt"))?;
        fs::remove_file(repo.join("swap"))?;
        fs::create_dir(repo.join("swap"))?;
        fs::write(repo.join("swap").join("inner"), "inner\n")?;
        git::bytes(&repo, &["add", "-A"])?;
        git::bytes(&repo, &["commit", "-qm", "new"])?;
        let new = git::oid(&repo, "HEAD")?;
        git::bytes(&repo, &["switch", "-q", "main"])?;
        fs::create_dir(repo.join("ignored"))?;
        fs::write(repo.join("ignored").join("keep"), "ignored data\n")?;
        let (p, lock) = Project::open(&repo)?;
        let plan = Plan {
            target: "main".into(),
            old,
            tip: new.clone(),
            new,
        };
        Ok((temp, p, lock, plan))
    }

    /// Leave the journal and files as a `read-tree` stopped partway leaves
    /// them: the deletions and the first update are on disk, the file being
    /// replaced has been unlinked, the rest is not written, and the primary
    /// index is untouched.
    fn interrupt(p: &Project, plan: &Plan) -> Result<Update> {
        let mut update = Update::begin(&p.common, plan)?;
        let index = fs::read(p.common.join("index"))?;
        registry::atomic(&update.directory.join("index.before"), &index)?;
        registry::atomic(&update.directory.join("index.next"), &index)?;
        update.set_phase(Phase::Updating)?;
        let repo = &p.repo;
        fs::remove_file(repo.join("gone.txt"))?;
        fs::remove_file(repo.join("swap"))?;
        fs::write(repo.join("a.txt"), "A\n")?;
        fs::remove_file(repo.join("b.txt"))?;
        Ok(update)
    }

    fn read(p: &Project, name: &str) -> Result<String> {
        Ok(fs::read_to_string(p.repo.join(name))?)
    }

    fn code(error: &anyhow::Error) -> serde_json::Value {
        crate::api::error_json(error)["code"].clone()
    }

    /// Everything a person may have done in the primary after the crash.
    fn edit_primary(p: &Project) -> Result<()> {
        fs::write(p.repo.join("unrelated.txt"), "edited after the crash\n")?;
        fs::write(p.repo.join("staged.txt"), "staged after the crash\n")?;
        git::bytes(&p.repo, &["add", "staged.txt"])?;
        fs::write(p.repo.join("untracked.txt"), "untracked\n")?;
        fs::write(p.repo.join("ignored").join("later"), "later ignored\n")?;
        Ok(())
    }

    fn assert_primary_edits_kept(p: &Project) -> Result<()> {
        assert_eq!(read(p, "unrelated.txt")?, "edited after the crash\n");
        assert_eq!(read(p, "staged.txt")?, "staged after the crash\n");
        assert_eq!(read(p, "untracked.txt")?, "untracked\n");
        assert_eq!(read(p, "ignored/keep")?, "ignored data\n");
        assert_eq!(read(p, "ignored/later")?, "later ignored\n");
        let staged = git::text(&p.repo, &["diff", "--cached", "--name-only"])?;
        assert_eq!(staged, "staged.txt");
        Ok(())
    }

    #[test]
    fn retry_finishes_a_partial_update_and_keeps_later_primary_edits() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        drop(interrupt(&p, &plan)?);
        edit_primary(&p)?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "a.txt")?, "A\n");
        assert_eq!(read(&p, "b.txt")?, "B\n");
        assert_eq!(read(&p, "added.txt")?, "added\n");
        assert_eq!(read(&p, "swap/inner")?, "inner\n");
        assert!(!p.repo.join("gone.txt").exists());
        assert_primary_edits_kept(&p)?;
        let changed = git::text(&p.repo, &["diff", "--name-only"])?;
        assert_eq!(changed, "unrelated.txt");
        assert!(Update::load(&p.common)?.is_none());
        assert!(!p.common.join("index.lock").exists());
        Ok(())
    }

    #[test]
    fn retry_stops_without_writing_when_a_changed_file_was_edited() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        drop(interrupt(&p, &plan)?);
        fs::write(p.repo.join("b.txt"), "my own edit\n")?;
        fs::write(p.repo.join("added.txt"), "my new file\n")?;
        let index = fs::read(p.common.join("index"))?;
        let error = apply(&p, &plan, || Ok(())).unwrap_err();
        let details = crate::api::error_json(&error);
        assert_eq!(details["code"], "checkout_recovery_required");
        assert_eq!(
            details["details"]["paths"],
            json!(["added.txt", "b.txt"]),
            "{details}"
        );
        assert_eq!(read(&p, "b.txt")?, "my own edit\n");
        assert_eq!(read(&p, "added.txt")?, "my new file\n");
        assert_eq!(read(&p, "a.txt")?, "A\n");
        assert_eq!(fs::read(p.common.join("index"))?, index);
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);

        // Returning each file to either version lets the retry finish.
        fs::write(p.repo.join("b.txt"), "b\n")?;
        fs::write(p.repo.join("added.txt"), "added\n")?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "b.txt")?, "B\n");
        assert!(git::changes(&p.repo)?.iter().all(|c| c.status == "!!"));
        Ok(())
    }

    #[test]
    fn retry_stops_when_a_changed_path_was_staged_after_the_crash() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        drop(interrupt(&p, &plan)?);
        fs::write(p.repo.join("b.txt"), "staged version\n")?;
        git::bytes(&p.repo, &["add", "b.txt"])?;
        fs::write(p.repo.join("b.txt"), "b\n")?;
        let error = apply(&p, &plan, || Ok(())).unwrap_err();
        assert_eq!(code(&error), "checkout_recovery_required");
        assert!(format!("{error:#}").contains("staged changes"), "{error:#}");
        let staged = git::text(&p.repo, &["show", ":b.txt"])?;
        assert_eq!(staged, "staged version");
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.old);
        Ok(())
    }

    #[test]
    fn another_merge_restores_the_previous_commit_before_it_applies() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let repo = &p.repo;
        git::bytes(repo, &["switch", "-qc", "second", &plan.old])?;
        fs::write(repo.join("second.txt"), "second\n")?;
        git::bytes(repo, &["add", "second.txt"])?;
        git::bytes(repo, &["commit", "-qm", "second"])?;
        let second = git::oid(repo, "HEAD")?;
        git::bytes(repo, &["switch", "-q", "main"])?;
        let other = Plan {
            target: "main".into(),
            old: plan.old.clone(),
            tip: second.clone(),
            new: second.clone(),
        };
        drop(interrupt(&p, &plan)?);
        edit_primary(&p)?;
        // The interrupted update is reversed first. The new merge then sees
        // the person's edits and, as for any merge, leaves them alone.
        let error = apply(&p, &other, || Ok(())).unwrap_err();
        assert!(format!("{error:#}").contains("local changes"), "{error:#}");
        assert!(Update::load(&p.common)?.is_none());
        assert_eq!(git::oid(repo, "HEAD")?, plan.old);
        assert_eq!(read(&p, "a.txt")?, "a\n");
        assert_eq!(read(&p, "b.txt")?, "b\n");
        assert_eq!(read(&p, "gone.txt")?, "gone\n");
        assert_eq!(read(&p, "swap")?, "file\n");
        assert!(!repo.join("added.txt").exists());
        assert_primary_edits_kept(&p)?;
        let changed = git::text(repo, &["diff", "--name-only"])?;
        assert_eq!(changed, "unrelated.txt");

        git::bytes(repo, &["rm", "-q", "--cached", "staged.txt"])?;
        for name in ["staged.txt", "untracked.txt"] {
            fs::remove_file(repo.join(name))?;
        }
        fs::write(repo.join("unrelated.txt"), "unrelated\n")?;
        apply(&p, &other, || Ok(()))?;
        assert_eq!(git::oid(repo, "HEAD")?, second);
        assert_eq!(read(&p, "second.txt")?, "second\n");
        assert_eq!(read(&p, "ignored/later")?, "later ignored\n");
        Ok(())
    }

    #[test]
    fn a_moved_destination_sets_the_update_aside_without_touching_files() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let update = interrupt(&p, &plan)?;
        let token = update.record.token.clone();
        drop(update);
        git::bytes(&p.repo, &["commit", "-qm", "moved", "--allow-empty"])?;
        let moved = git::oid(&p.repo, "HEAD")?;
        let error = apply(&p, &plan, || Ok(())).unwrap_err();
        assert_eq!(code(&error), "checkout_update_set_aside");
        let retained = p.state.join(format!(".checkout-set-aside-{token}"));
        assert!(retained.join("index.before").is_file());
        assert!(Update::load(&p.common)?.is_none());
        assert_eq!(git::oid(&p.repo, "HEAD")?, moved);
        assert_eq!(read(&p, "a.txt")?, "A\n");
        assert!(!p.repo.join("b.txt").exists());
        assert!(!p.repo.join("gone.txt").exists());
        Ok(())
    }

    #[test]
    fn every_command_that_locks_the_private_index_is_recorded() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let mut update = Update::begin(&p.common, &plan)?;
        let index = fs::read(p.common.join("index"))?;
        update.seed(Some(&index))?;
        let recorded =
            |update: &Update| -> Result<Option<u32>> { Ok(read_record(&update.directory)?.worker) };
        assert_eq!(recorded(&update)?, None);
        update.two_tree(&p.repo, None, true, true)?;
        let dry_run = recorded(&update)?.context("the dry run was not recorded")?;
        let changes = update.changes(&p.repo, None)?;
        update.write_view(
            &p.repo,
            None,
            Some(&index),
            changes.iter().map(|c| (&c.path[..], &c.new)),
        )?;
        let view = recorded(&update)?.context("the recovery index was not recorded")?;
        assert_ne!(view, dry_run);
        update.set_phase(Phase::Updating)?;
        assert_eq!(recorded(&update)?, None);
        update.clear(&p.common)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn retry_waits_for_a_live_worker_and_reclaims_a_dead_one() -> Result<()> {
        let (_temp, p, _lock, plan) = partial_fixture()?;
        let mut update = interrupt(&p, &plan)?;
        let lock = update.directory.join("index.next.lock");
        fs::write(&lock, b"partial")?;
        update.record.worker = Some(std::process::id());
        update.save()?;
        let error = apply(&p, &plan, || Ok(())).unwrap_err();
        assert_eq!(code(&error), "checkout_recovery_required");
        assert!(
            format!("{error:#}").contains("may still be writing"),
            "{error:#}"
        );
        assert!(lock.exists());
        assert!(!p.repo.join("b.txt").exists());

        update.record.worker = None;
        update.save()?;
        assert!(apply(&p, &plan, || Ok(())).is_err());
        assert!(lock.exists());

        let mut exited = std::process::Command::new("true").spawn()?;
        exited.wait()?;
        update.record.worker = Some(exited.id());
        update.save()?;
        apply(&p, &plan, || Ok(()))?;
        assert_eq!(git::oid(&p.repo, "HEAD")?, plan.new);
        assert_eq!(read(&p, "b.txt")?, "B\n");
        assert!(Update::load(&p.common)?.is_none());
        Ok(())
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
