//! A retirement keeps its Git objects reachable independently of the active
//! branch and of expiring command receipts. Keep the original checkout and
//! index whenever reconstructing them would discard data.
use crate::{
    api::fault,
    git,
    landing::Plan,
    registry::{self, Phase, Project, Record, exists},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Stdio,
};
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
enum State {
    Saving,
    Retired,
    Restoring { lease: String, port: u16 },
    Restored { result: Value },
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
enum Checkout {
    Original,
    Reclaiming,
    Commit,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    record: Record,
    tip: String,
    checkout: Checkout,
    checkout_identity: Option<Identity>,
    #[serde(default)]
    rebuild_attempt: u32,
    runtime: Option<Identity>,
    state: State,
}

fn directory(p: &Project) -> PathBuf {
    p.state.join("recovery")
}

fn recovery_id(lease: &str) -> String {
    let id = blake3::Hasher::new_derive_key("git-graft recovery identity v1")
        .update(lease.as_bytes())
        .finalize()
        .to_hex();
    id[..32].to_owned()
}

fn manifest(p: &Project, lease: Uuid) -> PathBuf {
    directory(p).join(format!("{}.json", recovery_id(&lease.to_string())))
}

fn load(p: &Project, lease: Uuid) -> Result<Option<Saved>> {
    let path = manifest(p, lease);
    if !exists(&directory(p))? {
        return Ok(None);
    }
    registry::owned_directory(&directory(p))?;
    if !exists(&path)? {
        return Ok(None);
    }
    let saved = read_manifest(&path)?;
    ensure!(
        saved.record.lease == lease.to_string(),
        "recovery lease mismatch"
    );
    Ok(Some(saved))
}

fn read_manifest(path: &Path) -> Result<Saved> {
    ensure!(regular_file(path)?, "invalid recovery manifest");
    let saved: Saved = serde_json::from_slice(&fs::read(path)?)?;
    let id = recovery_id(&Uuid::parse_str(&saved.record.lease)?.to_string());
    ensure!(
        path.file_stem().is_some_and(|name| name == id.as_str()),
        "recovery identity mismatch"
    );
    registry::validate_name(&saved.record.name)?;
    ensure!(valid_oid(&saved.tip), "invalid recovery commit");
    Ok(saved)
}

fn save(p: &Project, saved: &Saved) -> Result<()> {
    let dir = directory(p);
    registry::dir(&dir)?;
    registry::owned_directory(&dir)?;
    sync_directory(&p.state)?;
    registry::atomic(
        &manifest(p, Uuid::parse_str(&saved.record.lease)?),
        &serde_json::to_vec(saved)?,
    )
}

pub fn lease_used(p: &Project, lease: &str) -> Result<bool> {
    Ok(load(p, Uuid::parse_str(lease)?)?.is_some())
}

pub fn supported(p: &Project) -> Result<bool> {
    let format = git::text(&p.repo, &["rev-parse", "--show-object-format"])?;
    let digits = match format.as_str() {
        "sha1" => 40,
        "sha256" => 64,
        _ => return Ok(false),
    };
    // Parse the same operation restore uses, then abort before taking any
    // ref locks. This also works in an empty repository with an unborn HEAD.
    let mut input = tempfile::tempfile()?;
    write!(
        input,
        "start\noption no-deref\nsymref-update HEAD refs/heads/graft/probe oid {}\nabort\n",
        "0".repeat(digits)
    )?;
    input.rewind()?;
    let output = git::command(&p.repo)
        .args(["update-ref", "--stdin"])
        .stdin(input)
        .output()?;
    Ok(output.status.success()
        && String::from_utf8(output.stdout)?
            .lines()
            .eq(["start: ok", "abort: ok"]))
}

pub fn ensure_supported(p: &Project) -> Result<()> {
    if !supported(p)? {
        return Err(fault(
            "recovery_unsupported",
            "Recovery requires transactional symbolic ref support from Git 2.46 or newer",
            Value::Null,
        ));
    }
    Ok(())
}

pub(crate) fn undo_ref(lease: &str) -> Result<String> {
    let lease = Uuid::parse_str(lease)?.to_string();
    Ok(format!("refs/graft/recovery/{}/undo", recovery_id(&lease)))
}

fn landing_plan(saved: &Saved) -> Option<&Plan> {
    match &saved.record.state {
        Phase::Removing { landed, .. } => landed.as_ref(),
        _ => None,
    }
}

pub(crate) fn retired_landing(p: &Project, name: &str, lease: &str) -> Result<Plan> {
    registry::validate_name(name)?;
    let outcome = load(p, Uuid::parse_str(lease)?)?
        .filter(|saved| saved.record.name == name && !matches!(saved.state, State::Saving));
    let (saved, plan) = outcome
        .as_ref()
        .and_then(|saved| landing_plan(saved).map(|plan| (saved, plan)))
        .ok_or_else(|| {
            fault(
                "landing_not_found",
                "No completed landing matches this name and lease",
                json!({"name":name}),
            )
        })?;
    git::checked_oid(&plan.old)?;
    git::checked_oid(&plan.new)?;
    ensure!(plan.tip == saved.tip, "landing and recovery tips differ");
    ensure!(
        direct_ref(&p.repo, &marker_ref(&saved.record, "landed"))?.as_deref() == Some(&plan.new),
        "integration recovery ref is missing or changed"
    );
    Ok(plan.clone())
}

pub fn list(p: &Project) -> Result<Value> {
    let mut entries = Vec::new();
    let dir = directory(p);
    if exists(&dir)? {
        registry::owned_directory(&dir)?;
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let saved = read_manifest(&path)?;
            if matches!(saved.state, State::Retired | State::Restoring { .. }) {
                entries.push(json!({"name":saved.record.name,"tip":saved.tip,
                    "checkout_preserved":saved.checkout != Checkout::Commit,
                    "runtime_preserved":saved.runtime.is_some()}));
            }
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    }
    Ok(json!(entries))
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn archived_path(p: &Project, lease: &str) -> PathBuf {
    // A sibling at the same depth also keeps relative .git pointers valid.
    p.root
        .join(format!(".graft-recovery-{}", recovery_id(lease)))
}

fn rebuild_path(p: &Project, saved: &Saved) -> PathBuf {
    let id = recovery_id(&saved.record.lease);
    let attempt = saved.rebuild_attempt;
    p.root.join(format!(".graft-rebuild-{id}-{attempt}"))
}

fn runtime_paths(p: &Project, r: &Record) -> (PathBuf, PathBuf) {
    let root = p.state.join("runtime");
    (
        root.join(&r.name),
        root.join(format!(".graft-recovery-{}", recovery_id(&r.lease))),
    )
}

fn tip_ref(r: &Record) -> String {
    marker_ref(r, "tip")
}

fn marker_ref(r: &Record, stage: &str) -> String {
    format!("refs/graft/recovery/{}/{stage}", recovery_id(&r.lease))
}

fn lock_reason(r: &Record) -> String {
    format!("git-graft recovery {}", recovery_id(&r.lease))
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Identity(u64, u64);

fn identity(path: &Path) -> Result<Identity> {
    let metadata = registry::owned_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Identity(metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            GetFileInformationByHandle,
        };
        let _ = metadata;
        let file = fs::OpenOptions::new()
            .access_mode(0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // Keep the opened directory handle and output buffer alive.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Identity(
            info.dwVolumeSerialNumber.into(),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }
    #[cfg(not(any(unix, windows)))]
    bail!("cannot verify directory identity on this platform");
}

fn rename(source: &Path, destination: &Path, expected: Identity) -> Result<()> {
    ensure!(
        identity(source)? == expected,
        "move source was replaced: {}",
        source.display()
    );
    let result = rename_exclusive(source, destination);
    result.with_context(|| format!("move {} to {}", source.display(), destination.display()))?;
    if identity(destination).ok() != Some(expected) {
        rename_exclusive(destination, source)
            .context("move source changed and the replacement could not be returned to its original location")?;
        bail!("move source was replaced: {}", source.display());
    }
    let source_parent = source.parent().context("missing source parent")?;
    let destination_parent = destination.parent().context("missing destination parent")?;
    sync_directory(destination_parent)?;
    if source_parent != destination_parent {
        sync_directory(source_parent)?;
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn rename_exclusive(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // Both terminated paths live through the syscall. Exclusive rename cannot
    // replace even an empty directory created after our preflight checks.
    #[cfg(target_os = "macos")]
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn rename_exclusive(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<_> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // No REPLACE_EXISTING or COPY_ALLOWED: move the directory on its volume.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn rename_exclusive(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

pub(crate) fn direct_ref(dir: &Path, reference: &str) -> Result<Option<String>> {
    let output = git::raw(dir, &["show-ref", "--verify", "--hash", reference])?;
    // show-ref --verify exits 128 both for absence and a broken ref. rev-parse
    // distinguishes ordinary absence without hiding diagnostics.
    if !output.status.success() {
        let verify = git::raw(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                reference,
            ],
        )?;
        ensure!(
            verify.status.code() == Some(1) && verify.stderr.is_empty(),
            "cannot resolve {reference}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            git::symbolic_ref(dir, reference)?.is_none(),
            "{reference} is symbolic"
        );
        return Ok(None);
    }
    ensure!(
        git::symbolic_ref(dir, reference)?.is_none(),
        "{reference} is symbolic"
    );
    let references = git::text(dir, &["for-each-ref", "--format=%(refname)", reference])?;
    ensure!(
        references.lines().any(|candidate| candidate == reference),
        "Git resolved a differently named ref for {reference}"
    );
    let oid = String::from_utf8(output.stdout)?.trim_end().to_owned();
    ensure!(valid_oid(&oid), "invalid object ID for {reference}");
    Ok(Some(oid))
}

pub(crate) fn transaction(
    dir: &Path,
    edits: &str,
    message: Option<&str>,
    check: impl FnOnce() -> Result<()>,
) -> Result<()> {
    // Reference-transaction hooks may produce more than a pipe can hold
    // while we wait for Git's prepare acknowledgement on stdout.
    let mut diagnostics = tempfile::tempfile()?;
    let mut command = git::command(dir);
    command.args([
        "-c",
        "core.fsync=reference",
        "-c",
        "core.fsyncMethod=fsync",
        "update-ref",
        "--stdin",
    ]);
    if let Some(message) = message {
        command.args(["-m", message]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(diagnostics.try_clone()?)
        .spawn()?;
    let attempt = (|| -> Result<()> {
        let mut input = child
            .stdin
            .take()
            .context("missing ref transaction input")?;
        let mut reply = BufReader::new(
            child
                .stdout
                .take()
                .context("missing ref transaction output")?,
        );
        write!(input, "start\n{edits}prepare\n")?;
        input.flush()?;
        let mut line = String::new();
        reply.read_line(&mut line)?;
        ensure!(
            line.trim_end() == "start: ok",
            "Git could not start a ref transaction"
        );
        line.clear();
        reply.read_line(&mut line)?;
        ensure!(
            line.trim_end() == "prepare: ok",
            "Git could not lock the recovery refs"
        );
        // no-deref + an old OID alone also permits a symbolic ref resolving to
        // that OID. Check the type while Git holds the prepared ref lock.
        check()?;
        input.write_all(b"commit\n")?;
        input.flush()?;
        line.clear();
        reply.read_line(&mut line)?;
        ensure!(
            line.trim_end() == "commit: ok",
            "Git could not commit the recovery refs"
        );
        Ok(())
    })();
    let status = child.wait()?;
    if !status.success() {
        let length = diagnostics.metadata()?.len();
        if length > 0 {
            diagnostics.seek(SeekFrom::Start(length.saturating_sub(16 * 1024)))?;
            let mut tail = Vec::new();
            diagnostics.take(16 * 1024).read_to_end(&mut tail)?;
            bail!("{}", String::from_utf8_lossy(&tail).trim_end());
        }
        ensure!(
            attempt.is_err(),
            "Git recovery ref transaction failed: {status}"
        );
    }
    attempt
}

fn pin(p: &Project, saved: &Saved, history_dir: Option<&Path>) -> Result<()> {
    let r = &saved.record;
    let reference = tip_ref(r);
    let mut edits = String::new();
    if let Some(plan) = landing_plan(saved) {
        git::checked_oid(&plan.old)?;
        git::checked_oid(&plan.new)?;
        ensure!(plan.tip == saved.tip, "landing and recovery tips differ");
        let integrated = marker_ref(r, "landed");
        match direct_ref(&p.repo, &integrated)? {
            Some(oid) => ensure!(oid == plan.new, "landing recovery ref changed"),
            None => edits.push_str(&format!("create {integrated} {}\n", plan.new)),
        }
    }
    if let Some(oid) = direct_ref(&p.repo, &reference)? {
        ensure!(oid == saved.tip, "recovery ref changed");
        return if edits.is_empty() {
            Ok(())
        } else {
            transaction(&p.repo, &edits, None, || Ok(()))
        };
    }
    edits.push_str(&format!("create {reference} {}\n", saved.tip));
    if let Some(dir) = history_dir {
        let mut history = BTreeSet::new();
        for reference in [r.branch_ref().as_str(), "HEAD"] {
            let reflog = git::text(dir, &["reflog", "show", "--format=%H", reference])?;
            history.extend(
                reflog
                    .lines()
                    .filter(|oid| valid_oid(oid))
                    .map(str::to_owned),
            );
        }
        for oid in history {
            if oid != saved.tip {
                edits.push_str(&format!("create {reference}-history/{oid} {oid}\n"));
            }
        }
    }
    transaction(&p.repo, &edits, None, || Ok(()))
}

fn regular_file(path: &Path) -> Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(metadata.is_file() && !registry::is_reparse_point(&metadata))
}

fn git_admin(p: &Project, path: &Path) -> Result<PathBuf> {
    registry::owned_directory(path)?;
    ensure!(
        regular_file(&path.join(".git"))?,
        "missing regular worktree .git file"
    );
    let admin = dunce::canonicalize(git::text(path, &["rev-parse", "--absolute-git-dir"])?)?;
    ensure!(
        admin.parent() == Some(p.common.join("worktrees").as_path()),
        "foreign worktree administrative directory"
    );
    registry::owned_directory(&p.common.join("worktrees"))?;
    registry::owned_directory(&admin)?;
    let common_file = admin.join("commondir");
    ensure!(
        regular_file(&common_file)?,
        "invalid Git common-directory pointer"
    );
    let common = fs::read_to_string(common_file)?;
    let common = admin.join(common.trim_end_matches(['\r', '\n']));
    ensure!(
        dunce::canonicalize(common)? == p.common,
        "worktree belongs to a different repository"
    );
    Ok(admin)
}

fn gitfile_target(admin: &Path) -> Result<PathBuf> {
    let pointer = admin.join("gitdir");
    ensure!(
        regular_file(&pointer)?,
        "missing regular Git worktree pointer"
    );
    let contents = fs::read_to_string(pointer)?;
    let path = Path::new(contents.trim_end_matches(['\r', '\n']));
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        admin.join(path)
    };
    Ok(std::path::absolute(absolute)?)
}

fn points_to(admin: &Path, path: &Path) -> Result<bool> {
    let current = gitfile_target(admin)?;
    let expected = path.join(".git");
    if current == expected {
        return Ok(true);
    }
    // Relative Git pointers may contain .. and Windows Git may write a
    // different case. Canonicalize the existing parents without following
    // the checkout directory or the pointer file itself.
    let parent_matches = |candidate: &Path, expected: &Path| {
        dunce::canonicalize(candidate).ok() == Some(expected.to_path_buf())
    };
    Ok(current.file_name() == expected.file_name()
        && current
            .parent()
            .zip(expected.parent())
            .is_some_and(|(current, expected)| {
                current.file_name() == expected.file_name()
                    && current
                        .parent()
                        .zip(expected.parent())
                        .is_some_and(|(current, expected)| parent_matches(current, expected))
            }))
}

fn owner_is(admin: &Path, lease: &str) -> Result<bool> {
    let mark = admin.join("graft-owner");
    Ok(exists(&mark)? && regular_file(&mark)? && fs::read(mark)? == lease.as_bytes())
}

fn owns_lock(admin: &Path, r: &Record) -> Result<bool> {
    let path = admin.join("locked");
    Ok(exists(&path)?
        && regular_file(&path)?
        && fs::read_to_string(path)?.trim_end_matches(['\r', '\n']) == lock_reason(r))
}

fn lock(admin: &Path, r: &Record) -> Result<()> {
    if !owns_lock(admin, r)? {
        let mut file = tempfile::NamedTempFile::new_in(admin)?;
        writeln!(file, "{}", lock_reason(r))?;
        file.as_file().sync_all()?;
        file.persist_noclobber(admin.join("locked"))
            .context("Git worktree is already locked")?;
        sync_directory(admin)?;
    }
    Ok(())
}

fn unlock(admin: &Path, r: &Record) -> Result<()> {
    if exists(&admin.join("locked"))? {
        ensure!(owns_lock(admin, r)?, "Git worktree is locked by its owner");
        fs::remove_file(admin.join("locked"))?;
        sync_directory(admin)?;
    }
    Ok(())
}

fn relink(admin: &Path, from: &Path, to: &Path) -> Result<()> {
    if !points_to(admin, to)? {
        ensure!(
            points_to(admin, from)?,
            "Git registration was moved outside Graft"
        );
        let gitfile = to.join(".git");
        let gitfile = git::path_str(&gitfile)?;
        ensure!(
            !gitfile.ends_with(['\r', '\n']),
            "unsupported worktree path"
        );
        registry::atomic(&admin.join("gitdir"), format!("{gitfile}\n").as_bytes())?;
    }
    Ok(())
}

fn move_checkout(
    p: &Project,
    saved: &Saved,
    from: &Path,
    to: &Path,
    restore_lease: Option<&str>,
) -> Result<PathBuf> {
    let r = &saved.record;
    let expected = saved
        .checkout_identity
        .context("missing checkout identity")?;
    ensure!(
        !(exists(from)? && exists(to)?),
        "workspace destination is occupied: {}",
        to.display()
    );
    let source = if exists(from)? { from } else { to };
    ensure!(
        identity(source)? == expected,
        "workspace directory was replaced"
    );
    let admin = git_admin(p, source)?;
    if saved.checkout != Checkout::Commit {
        ensure!(
            r.admin.as_ref() == Some(&admin),
            "workspace registration changed"
        );
    }
    ensure!(
        owner_is(&admin, &r.lease)?
            || restore_lease
                .map(|lease| owner_is(&admin, lease))
                .transpose()?
                .unwrap_or(false),
        "workspace registration has a different owner"
    );
    ensure!(
        points_to(&admin, from)? || points_to(&admin, to)?,
        "Git worktree registration changed"
    );
    if source == from {
        lock(&admin, r)?;
        ensure!(
            git_admin(p, from)? == admin && points_to(&admin, from)?,
            "worktree registration changed before move"
        );
        rename(from, to, expected)?;
    }
    ensure!(
        git_admin(p, to)? == admin,
        "worktree identity changed during move"
    );
    relink(&admin, from, to)?;
    Ok(admin)
}

fn move_runtime(from: &Path, to: &Path, expected: Option<Identity>) -> Result<()> {
    registry::owned_directory(from.parent().context("missing runtime directory")?)?;
    if let Some(expected) = expected {
        if exists(from)? {
            rename(from, to, expected)?;
        } else {
            ensure!(
                identity(to)? == expected,
                "private runtime directory was replaced"
            );
        }
    } else {
        ensure!(
            !exists(from)? && !exists(to)?,
            "unexpected runtime directory"
        );
    }
    Ok(())
}

fn detach(p: &Project, saved: &Saved, archive: &Path) -> Result<()> {
    let branch = saved.record.branch_ref();
    ensure!(
        git::oid(archive, "HEAD")? == saved.tip,
        "workspace HEAD changed during retirement"
    );
    if let Some(target) = git::symbolic_ref(archive, "HEAD")? {
        ensure!(
            target == branch,
            "workspace switched branches during retirement"
        );
        let edits = format!("option no-deref\nupdate HEAD {} {}\n", saved.tip, saved.tip);
        transaction(archive, &edits, None, || {
            ensure!(
                git::symbolic_ref(archive, "HEAD")?.as_deref() == Some(&branch),
                "workspace HEAD changed during retirement"
            );
            ensure!(
                direct_ref(&p.repo, &branch)?.as_deref() == Some(&saved.tip),
                "workspace branch changed during retirement"
            );
            Ok(())
        })?;
    }
    Ok(())
}

fn release_branch(p: &Project, saved: &Saved) -> Result<()> {
    let branch = saved.record.branch_ref();
    let marker = marker_ref(&saved.record, "retired");
    if let Some(oid) = direct_ref(&p.repo, &marker)? {
        ensure!(oid == saved.tip, "retirement marker changed");
        return Ok(());
    }
    let mut edits = format!("create {marker} {}\n", saved.tip);
    if let Some(oid) = direct_ref(&p.repo, &branch)? {
        ensure!(
            oid == saved.tip,
            "workspace branch changed during retirement"
        );
        edits.push_str(&format!("option no-deref\ndelete {branch} {}\n", saved.tip));
    } else {
        edits.push_str(&format!("option no-deref\nverify {branch}\n"));
    }
    transaction(&p.repo, &edits, None, || {
        ensure!(
            !git::worktrees(&p.repo)?
                .iter()
                .any(|w| w.branch.as_deref() == Some(&branch)),
            "workspace branch is checked out elsewhere"
        );
        // This rejects a new symbolic ref with the same referent commit.
        direct_ref(&p.repo, &branch)?;
        Ok(())
    })
}

// Confirm the whole physical checkout is reproducible from the pinned Git
// tree. Git status by itself can miss empty directories, modes, and bytes
// hidden by the stat cache or a clean filter.
fn reconstructible(path: &Path, admin: &Path, r: &Record, tip: &str) -> Result<bool> {
    if !r.sparse.is_empty()
        || git::check_idle(path).is_err()
        || !git::changes(path)?.is_empty()
        || !git::bytes(path, &["ls-files", "--resolve-undo", "-z"])?.is_empty()
        || registry::check_owned_tree(path, true).is_err()
    {
        return Ok(false);
    }
    if !unadorned_git_metadata(admin) {
        return Ok(false);
    }
    for entry in fs::read_dir(admin)? {
        let entry = entry?;
        if !unadorned_git_metadata(&entry.path()) {
            return Ok(false);
        }
        let generated = match entry.file_name().to_str() {
            Some("HEAD" | "commondir" | "gitdir" | "graft-owner" | "index" | "locked") => {
                entry.file_type()?.is_file()
            }
            Some("refs") => {
                entry.file_type()?.is_dir() && fs::read_dir(entry.path())?.next().is_none()
            }
            Some("logs") => {
                entry.file_type()?.is_dir()
                    && fs::read_dir(entry.path())?.all(|log| {
                        log.is_ok_and(|log| {
                            log.file_name() == "HEAD"
                                && log.file_type().is_ok_and(|kind| kind.is_file())
                        })
                    })
            }
            Some("COMMIT_EDITMSG")
                if entry.file_type()?.is_file() && entry.metadata()?.len() <= 64 * 1024 =>
            {
                let mut committed = git::bytes(
                    path,
                    &["--no-replace-objects", "log", "-1", "--format=%B", tip],
                )?;
                committed.pop();
                fs::read(entry.path())? == committed
            }
            _ => false,
        };
        if !generated {
            return Ok(false);
        }
    }
    let mut files = BTreeMap::new();
    let mut directories = BTreeSet::new();
    let entries = git::bytes(path, &["--no-replace-objects", "ls-tree", "-rz", tip])?;
    for entry in entries
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let Ok(entry) = std::str::from_utf8(entry) else {
            return Ok(false);
        };
        let (object, name) = entry.split_once('\t').context("invalid Git tree entry")?;
        let (mode, oid) = object.split_once(" blob ").unwrap_or(("", ""));
        if !matches!(mode, "100644" | "100755")
            || !valid_oid(oid)
            || name.starts_with('"')
            || name.contains(['\r', '\n'])
        {
            return Ok(false);
        }
        let relative = Path::new(name);
        directories.extend(relative.ancestors().skip(1));
        files.insert(relative, (name, mode == "100755", oid));
    }
    directories.insert(Path::new(""));
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let parent = fs::symlink_metadata(parent)?;
    let mut pending = vec![path.to_path_buf()];
    let mut input = tempfile::tempfile()?;
    let mut expected = Vec::new();
    while let Some(directory) = pending.pop() {
        if !ordinary_metadata(
            &directory,
            &fs::symlink_metadata(&directory)?,
            &parent,
            None,
        ) {
            return Ok(false);
        }
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let item = entry.path();
            let relative = item.strip_prefix(path)?;
            let metadata = fs::symlink_metadata(&item)?;
            if relative == Path::new(".git") {
                if !ordinary_metadata(&item, &metadata, &parent, Some(false)) {
                    return Ok(false);
                }
                continue;
            }
            if metadata.is_dir() && directories.remove(relative) {
                pending.push(item);
            } else if let Some((name, executable, oid)) = files.remove(relative) {
                if !ordinary_metadata(&item, &metadata, &parent, Some(executable)) {
                    return Ok(false);
                }
                writeln!(input, "{name}")?;
                expected.push(oid);
            } else {
                return Ok(false);
            }
        }
    }
    directories.remove(Path::new(""));
    if !files.is_empty() || !directories.is_empty() {
        return Ok(false);
    }
    input.rewind()?;
    let hashes = git::command(path)
        .args(["hash-object", "--no-filters", "--stdin-paths"])
        .stdin(input)
        .output()?;
    Ok(hashes.status.success() && String::from_utf8(hashes.stdout)?.lines().eq(expected))
}

#[cfg(unix)]
fn unadorned_git_metadata(path: &Path) -> bool {
    no_extended_metadata(path)
}

#[cfg(not(unix))]
fn unadorned_git_metadata(_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn ordinary_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    parent: &fs::Metadata,
    executable: Option<bool>,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mode = match executable {
        Some(true) => 0o755,
        Some(false) => 0o644,
        None => 0o755,
    };
    metadata.uid() == parent.uid()
        && metadata.gid() == parent.gid()
        && metadata.mode() & 0o7777 == mode
        && match executable {
            Some(_) => metadata.is_file() && metadata.nlink() == 1,
            None => metadata.is_dir(),
        }
        && no_extended_metadata(path)
}

#[cfg(target_os = "linux")]
pub(crate) fn no_extended_metadata(path: &Path) -> bool {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // A zero-sized, null buffer queries only the length of the xattr list.
    unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) == 0 }
}

#[cfg(target_os = "macos")]
pub(crate) fn no_extended_metadata(path: &Path) -> bool {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    unsafe extern "C" {
        fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_uint) -> *mut libc::c_void;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            id: libc::c_int,
            entry: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_free(data: *mut libc::c_void) -> libc::c_int;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // lstat and the metadata calls operate on the entry, without following it.
    unsafe {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if libc::lstat(path.as_ptr(), stat.as_mut_ptr()) != 0 || stat.assume_init().st_flags != 0 {
            return false;
        }
        // macOS stamps files written by some apps with system provenance,
        // which it regenerates on creation. All user metadata keeps the tree.
        let provenance = b"com.apple.provenance\0";
        let mut names = [0; b"com.apple.provenance\0".len()];
        let count = libc::listxattr(
            path.as_ptr(),
            names.as_mut_ptr().cast(),
            names.len(),
            libc::XATTR_NOFOLLOW,
        );
        if count != 0 && (count != names.len() as isize || &names != provenance) {
            return false;
        }
        let acl = acl_get_link_np(path.as_ptr(), 0x100); // ACL_TYPE_EXTENDED
        if acl.is_null() {
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT);
        }
        let result = acl_get_entry(acl, 0, &mut std::ptr::null_mut()); // ACL_FIRST_ENTRY
        let error = std::io::Error::last_os_error().raw_os_error();
        acl_free(acl);
        result == -1 && error == Some(libc::EINVAL)
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn no_extended_metadata(_path: &Path) -> bool {
    false
}

#[cfg(not(unix))]
fn ordinary_metadata(
    _path: &Path,
    _metadata: &fs::Metadata,
    _parent: &fs::Metadata,
    _executable: Option<bool>,
) -> bool {
    false
}

pub(crate) fn retire(p: &Project, r: &Record, tip: &str, discarded: bool) -> Result<()> {
    p.root(false)?;
    let base = p.workspace(&r.name);
    let archive = archived_path(p, &r.lease);
    let (runtime, saved_runtime) = runtime_paths(p, r);
    let mut saved = match load(p, Uuid::parse_str(&r.lease)?)? {
        Some(saved) => saved,
        None => {
            ensure!(
                !exists(&archive)? && !exists(&saved_runtime)?,
                "recovery destination is occupied"
            );
            let checkout = if exists(&base)? {
                p.verify(r)?;
                Checkout::Original
            } else {
                ensure!(
                    !discarded,
                    "an earlier destructive operation removed the checkout before recovery could save its files"
                );
                ensure!(
                    r.admin.as_ref().map(|path| exists(path)).transpose()? == Some(false),
                    "workspace is missing or moved; inspect its Git registration"
                );
                ensure!(
                    !git::worktrees(&p.repo)?
                        .iter()
                        .any(|w| w.path == base || w.branch.as_deref() == Some(&r.branch_ref())),
                    "workspace is still registered or moved"
                );
                Checkout::Commit
            };
            let saved = Saved {
                record: serde_json::from_value(serde_json::to_value(r)?)?,
                tip: tip.to_owned(),
                checkout,
                checkout_identity: if exists(&base)? {
                    Some(identity(&base)?)
                } else {
                    None
                },
                rebuild_attempt: 0,
                runtime: if exists(&runtime)? {
                    Some(identity(&runtime)?)
                } else {
                    None
                },
                state: State::Saving,
            };
            save(p, &saved)?;
            saved
        }
    };
    ensure!(
        saved.record.name == r.name && saved.tip == tip,
        "recovery manifest does not match this retirement"
    );
    if matches!(saved.state, State::Retired) {
        return Ok(());
    }
    ensure!(
        matches!(saved.state, State::Saving),
        "retired workspace is already being restored"
    );
    if exists(&base)? {
        ensure!(
            saved.checkout_identity == Some(identity(&base)?),
            "workspace directory was replaced"
        );
        ensure!(
            direct_ref(&p.repo, &r.branch_ref())?.as_deref() == Some(tip),
            "workspace branch changed before retirement"
        );
    }
    pin(p, &saved, exists(&base)?.then_some(base.as_path()))?;
    if saved.checkout != Checkout::Commit {
        let admin = r
            .admin
            .as_ref()
            .context("missing original Git registration")?;
        if saved.checkout == Checkout::Reclaiming && !exists(&archive)? {
            if exists(admin)? {
                ensure!(
                    owner_is(admin, &r.lease)? && points_to(admin, &archive)?,
                    "Git administrative directory was replaced during clean reclamation"
                );
            }
            saved.checkout = Checkout::Commit;
            save(p, &saved)?;
        } else {
            let found = move_checkout(p, &saved, &base, &archive, None)?;
            ensure!(
                &found == admin,
                "worktree registration differs from the retirement record"
            );
            lock(admin, r)?;
            detach(p, &saved, &archive)?;
            release_branch(p, &saved)?;
            if reconstructible(&archive, admin, r, tip)? {
                saved.checkout = Checkout::Reclaiming;
                save(p, &saved)?;
                unlock(admin, r)?;
                git::bytes(
                    &p.repo,
                    &["worktree", "remove", "--", git::path_str(&archive)?],
                )?;
                sync_directory(&p.root)?;
                let registrations = p.common.join("worktrees");
                if exists(&registrations)? {
                    sync_directory(&registrations)?;
                }
                sync_directory(&p.common)?;
                saved.checkout = Checkout::Commit;
            } else {
                saved.checkout = Checkout::Original;
            }
        }
    }
    release_branch(p, &saved)?;
    move_runtime(&runtime, &saved_runtime, saved.runtime)?;
    saved.state = State::Retired;
    save(p, &saved)
}

fn rebuild_checkout(p: &Project, saved: &mut Saved) -> Result<PathBuf> {
    for _ in 0..2 {
        let archive = rebuild_path(p, saved);
        let r = &saved.record;
        let mut incomplete = false;
        if !exists(&archive)? {
            incomplete = git::worktrees(&p.repo)?.iter().any(|w| w.path == archive);
        }
        if !incomplete && !exists(&archive)? {
            git::bytes(
                &p.repo,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    "--lock",
                    "--reason",
                    &lock_reason(r),
                    "--",
                    git::path_str(&archive)?,
                    &saved.tip,
                ],
            )?;
        }
        if !incomplete {
            let admin = git_admin(p, &archive)?;
            ensure!(
                owns_lock(&admin, r)? && points_to(&admin, &archive)?,
                "reconstructed worktree is foreign"
            );
            ensure!(
                git::oid(&archive, "HEAD")? == saved.tip
                    && git::symbolic_ref(&archive, "HEAD")?.is_none(),
                "reconstructed worktree HEAD changed"
            );
            let mark = admin.join("graft-owner");
            ensure!(
                !exists(&mark)? || owner_is(&admin, &r.lease)?,
                "reconstructed worktree belongs to another workspace"
            );
            let changes = git::changes(&archive)?;
            if git::check_idle(&archive).is_ok()
                && changes
                    .iter()
                    .all(|change| matches!(change.status.as_str(), "??" | "!!"))
            {
                if !exists(&mark)? {
                    registry::atomic(&mark, r.lease.as_bytes())?;
                }
                saved.checkout_identity = Some(identity(&archive)?);
                save(p, saved)?;
                return Ok(archive);
            }
        }
        // A crash may leave a registered worktree with only part of its
        // checkout. Leave it as found and rebuild into a fresh staging slot.
        saved.rebuild_attempt = saved
            .rebuild_attempt
            .checked_add(1)
            .context("recovery attempts exhausted")?;
        save(p, saved)?;
    }
    bail!("Git left an incomplete recovery checkout; its files are retained")
}

fn attach(p: &Project, saved: &Saved, worktree: &Path) -> Result<()> {
    let branch = saved.record.branch_ref();
    let marker = marker_ref(&saved.record, "restoring");
    if let Some(oid) = direct_ref(&p.repo, &marker)? {
        ensure!(
            oid == saved.tip && direct_ref(&p.repo, &branch)?.as_deref() == Some(&saved.tip),
            "restore branch changed"
        );
    } else {
        let edits = format!(
            "create {branch} {}\ncreate {marker} {}\n",
            saved.tip, saved.tip
        );
        transaction(&p.repo, &edits, None, || {
            ensure_branch_unoccupied(p, &branch, worktree)
        })?;
    }
    ensure_branch_unoccupied(p, &branch, worktree)?;
    ensure!(
        git::oid(worktree, "HEAD")? == saved.tip,
        "recovery checkout HEAD changed"
    );
    match git::symbolic_ref(worktree, "HEAD")? {
        Some(target) => ensure!(target == branch, "recovery checkout branch changed"),
        None => {
            // Git locks HEAD and compares the detached OID before updating it.
            let edits = format!(
                "option no-deref\nsymref-update HEAD {branch} oid {}\n",
                saved.tip
            );
            transaction(worktree, &edits, None, || {
                ensure!(
                    git::symbolic_ref(worktree, "HEAD")?.is_none(),
                    "recovery checkout HEAD changed during restore"
                );
                ensure!(
                    direct_ref(&p.repo, &branch)?.as_deref() == Some(&saved.tip),
                    "restore branch changed"
                );
                Ok(())
            })?;
        }
    }
    Ok(())
}

fn ensure_branch_unoccupied(p: &Project, branch: &str, worktree: &Path) -> Result<()> {
    ensure!(
        !git::worktrees(&p.repo)?.iter().any(|w| {
            w.branch.as_deref() == Some(branch)
                && dunce::canonicalize(&w.path).ok().as_deref() != Some(worktree)
        }),
        "restore branch is checked out elsewhere"
    );
    Ok(())
}

pub fn restore(p: &Project, name: &str, lease: &str) -> Result<Value> {
    registry::validate_name(name)?;
    let mut saved = load(p, Uuid::parse_str(lease)?)?
        .filter(|saved| saved.record.name == name)
        .ok_or_else(|| {
            fault(
                "recovery_not_found",
                "No recovery matches this name and lease",
                json!({"name":name}),
            )
        })?;
    if let State::Restored { result } = &saved.state {
        return Ok(result.clone());
    }
    ensure_supported(p)?;
    let records = p.records()?;
    if let Some(active) = records.iter().find(|record| record.name == name) {
        if let State::Restoring { lease, .. } = &saved.state
            && active.lease == *lease
        {
            active.ready()?;
            p.verify(active)?;
            let mut result = p.summary(active);
            result["lease"] = json!(active.lease);
            saved.state = State::Restored {
                result: result.clone(),
            };
            save(p, &saved)?;
            return Ok(result);
        }
        return Err(fault(
            "already_exists",
            "Workspace name is occupied",
            json!({"name":name}),
        ));
    }
    p.root(false)?;
    let path = p.workspace(name);
    let (runtime, saved_runtime) = runtime_paths(p, &saved.record);
    if matches!(saved.state, State::Retired) {
        ensure!(
            !exists(&path)?
                && !exists(&runtime)?
                && direct_ref(&p.repo, &saved.record.branch_ref())?.is_none()
                && !git::worktrees(&p.repo)?
                    .iter()
                    .any(|w| w.path == path
                        || w.branch.as_deref() == Some(&saved.record.branch_ref())),
            "workspace destination path, runtime or branch is occupied"
        );
        saved.state = State::Restoring {
            lease: Uuid::new_v4().to_string(),
            port: registry::port_for(&records, name)?,
        };
        save(p, &saved)?;
    }
    ensure!(
        matches!(saved.state, State::Restoring { .. }),
        "workspace retirement is still pending; retry that operation first"
    );
    ensure!(
        direct_ref(&p.repo, &tip_ref(&saved.record))?.as_deref() == Some(&saved.tip),
        "recovery tip ref is missing or changed"
    );
    let archive = if saved.checkout != Checkout::Commit {
        archived_path(p, &saved.record.lease)
    } else if exists(&path)? {
        rebuild_path(p, &saved)
    } else {
        rebuild_checkout(p, &mut saved)?
    };
    let State::Restoring { lease, port } = &saved.state else {
        unreachable!()
    };
    let admin = move_checkout(p, &saved, &archive, &path, Some(lease))?;
    attach(p, &saved, &path)?;
    move_runtime(&saved_runtime, &runtime, saved.runtime)?;
    if !owner_is(&admin, lease)? {
        registry::atomic(&admin.join("graft-owner"), lease.as_bytes())?;
    }
    unlock(&admin, &saved.record)?;
    let mut record: Record = serde_json::from_value(serde_json::to_value(&saved.record)?)?;
    record.lease = lease.clone();
    record.port = *port;
    record.heartbeat = registry::now();
    record.admin = Some(admin);
    record.state = Phase::Ready;
    if saved.checkout == Checkout::Commit {
        record.storage = json!({"backend":"git","reconstructed":true});
    }
    p.verify(&record)?;
    p.save(&record)?;
    let mut result = p.summary(&record);
    result["lease"] = json!(record.lease);
    saved.state = State::Restored {
        result: result.clone(),
    };
    save(p, &saved)?;
    Ok(result)
}
