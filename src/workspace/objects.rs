//! Object transfer between independent repositories. The default exports the
//! exact requested history. Trusted-local bootstrap can snapshot the complete
//! object store. Both keep refs private and use an owned object directory.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use super::registry::MARKER;
pub(super) use super::registry::sync_directory;
use crate::{git, recovery::rename_exclusive};

mod history;
pub(crate) use history::{
    HISTORY_CACHE_DIR, HISTORY_CACHE_MARKER, history_cache_root, verify_history_cache_generation,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod local;
mod plan_cache;
mod shallow;

const SETTINGS: &str = r"^(core\.(autocrlf|eol|safecrlf|symlinks|filemode|ignorecase|precomposeunicode|protecthfs|protectntfs|bigfilethreshold)|(user|author|committer)\.(name|email))$";
const FILTERS: &str = r"^filter\.";

#[cfg(windows)]
const NULL: &str = "NUL";
#[cfg(not(windows))]
const NULL: &str = "/dev/null";

fn transfer_git(path: &Path) -> Command {
    let mut cmd = git::command(path);
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            cmd.env_remove(key);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", NULL)
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-c")
        .arg(format!("core.hooksPath={NULL}"))
        .args([
            "-c",
            "core.commitGraph=false",
            "-c",
            "pack.writeReverseIndex=false",
            "-c",
            "core.fsync=all",
            "-c",
            "core.fsyncMethod=fsync",
        ]);
    cmd
}

fn checked(mut cmd: Command) -> Result<Vec<u8>> {
    let out = cmd.output().context("start Git object operation")?;
    ensure!(
        out.status.success(),
        "Git object operation failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

fn run(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = transfer_git(path);
    cmd.args(args);
    checked(cmd).with_context(|| format!("git {} in {}", args[0], path.display()))
}

fn output_text(bytes: &[u8]) -> Result<&str> {
    Ok(std::str::from_utf8(bytes)?.trim_end_matches(['\n', '\r']))
}

fn object_format(path: &Path) -> Result<&'static str> {
    match output_text(&run(path, &["rev-parse", "--show-object-format"])?)? {
        "sha1" => Ok("sha1"),
        "sha256" => Ok("sha256"),
        format => bail!("unsupported Git object format {format:?}"),
    }
}

fn exact_commit(path: &Path, oid: &str) -> Result<String> {
    let oid = git::checked_oid(oid)?.to_ascii_lowercase();
    let resolved = run(
        path,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{oid}^{{commit}}"),
        ],
    )?;
    ensure!(
        output_text(&resolved)? == oid,
        "expected the full commit ID {oid}"
    );
    Ok(oid)
}

fn graph_count(mut cmd: Command, oid: &str) -> Result<u64> {
    cmd.args([
        "rev-list",
        "--objects",
        "--missing=error",
        "--count",
        oid,
        "--",
    ]);
    let count = checked(cmd).context("verify complete Git object graph")?;
    Ok(output_text(&count)?.parse()?)
}

fn optional_config(out: Output) -> Result<Vec<u8>> {
    match out.status.code() {
        Some(0) => Ok(out.stdout),
        Some(1) => Ok(Vec::new()),
        _ => bail!(
            "read Git configuration: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

#[derive(Debug)]
pub(super) struct Primary<'a> {
    path: &'a Path,
    format: &'static str,
}

pub(super) fn preflight(primary: &Path) -> Result<Primary<'_>> {
    let format = object_format(primary)?;
    ensure!(
        run(primary, &["for-each-ref", "--count=1", "refs/replace/"])?.is_empty(),
        "independent workspaces do not support Git replacement refs"
    );
    let grafts = repository_path(primary, "info/grafts")?;
    match fs::metadata(grafts) {
        Ok(metadata) => ensure!(
            metadata.is_file() && metadata.len() == 0,
            "independent workspaces do not support legacy Git grafts"
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let scoped = optional_config(git::raw(
        primary,
        &["config", "--null", "--show-scope", "--get-regexp", FILTERS],
    )?)?;
    ensure!(
        !scoped
            .split(|b| *b == 0)
            .step_by(2)
            .any(|scope| matches!(scope, b"local" | b"worktree")),
        "independent workspaces do not support repository-local Git filters"
    );
    Ok(Primary {
        path: primary,
        format,
    })
}

fn config_values(path: &Path, pattern: &str) -> Result<BTreeMap<String, String>> {
    let raw = optional_config(git::raw(
        path,
        &["config", "--null", "--get-regexp", pattern],
    )?)?;
    let mut values = BTreeMap::new();
    for entry in raw.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
        let entry = std::str::from_utf8(entry).context("Git configuration must be UTF-8")?;
        let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
        values.insert(key.to_string(), value.to_string());
    }
    Ok(values)
}

fn copy_config(primary: &Path, root: &Path) -> Result<()> {
    ensure!(
        config_values(primary, FILTERS)? == config_values(root, FILTERS)?,
        "effective Git filter settings differ in the independent workspace"
    );
    for (key, value) in config_values(primary, SETTINGS)? {
        run(root, &["config", "--local", "--", &key, &value])?;
    }
    let raw = optional_config(git::raw(
        primary,
        &["config", "--path", "--null", "--get", "core.attributesFile"],
    )?)?;
    if let Some(raw) = raw.strip_suffix(b"\0") {
        let path = primary.join(std::str::from_utf8(raw)?);
        let target = root.join(".git/graft-global-attributes");
        copy_attributes(&path, &target)?;
        if !target.exists() {
            fs::write(&target, [])?;
        }
        // Relative to the checkout root, so restore and move preserve the setting.
        run(
            root,
            &[
                "config",
                "--local",
                "core.attributesFile",
                ".git/graft-global-attributes",
            ],
        )?;
    }
    let attrs = repository_path(primary, "info/attributes")?;
    copy_attributes(&attrs, &root.join(".git/info/attributes"))?;
    Ok(())
}

fn repository_path(repo: &Path, path: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(git::text(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-path", path],
    )?))
}

fn copy_attributes(source: &Path, target: &Path) -> Result<()> {
    let metadata = match fs::metadata(source) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    ensure!(metadata.is_file(), "Git attributes must be a regular file");
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(target, fs::read(source)?)?;
    Ok(())
}

fn require_pristine(root: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(root)?.is_dir(),
        "workspace root must be a directory"
    );
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        ensure!(
            entry.file_name() == ".git" && entry.file_type()?.is_dir(),
            "workspace is not empty"
        );
        for admin in fs::read_dir(entry.path())? {
            let admin = admin?;
            ensure!(
                admin.file_name() == MARKER && admin.file_type()?.is_file(),
                "workspace Git directory has already been initialized or contains foreign data"
            );
        }
    }
    Ok(())
}

pub(super) fn initialize(
    source: &Primary<'_>,
    root: &Path,
    state: &Path,
    base: Option<&str>,
    branch: Option<&str>,
) -> Result<Value> {
    require_pristine(root)?;
    let Primary {
        path: primary,
        format,
    } = *source;
    let base = base.map(|oid| exact_commit(primary, oid)).transpose()?;
    let branch_ref = branch.map(|branch| format!("refs/heads/{branch}"));
    if let Some(reference) = &branch_ref {
        run(primary, &["check-ref-format", reference])?;
        ensure!(
            branch.is_some_and(|name| !name.starts_with('-') && name != "HEAD"),
            "invalid Git branch name"
        );
    }
    ensure!(
        base.is_some() || branch.is_some(),
        "an unborn workspace requires a branch"
    );
    let initial = branch.unwrap_or("graft-unborn");
    run(
        root,
        &[
            "init",
            "--quiet",
            "--template=",
) -> Result<Value> {
    initialize_with(source, root, state, base, branch, |_, _, _, _| Ok(None))
}

/// Opt in to cloning all visible objects from a trusted, ordinary local Git
/// database. Unsupported sources or filesystems use the exact bootstrap.
pub(super) fn initialize_local(
    source: &Primary<'_>,
    root: &Path,
    state: &Path,
    base: Option<&str>,
    branch: Option<&str>,
) -> Result<Value> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return initialize_with(source, root, state, base, branch, local::bootstrap);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    initialize(source, root, state, base, branch)
}

fn initialize_with(
    source: &Primary<'_>,
    root: &Path,
    state: &Path,
    base: Option<&str>,
    branch: Option<&str>,
    snapshot: impl FnOnce(&Path, &Path, &str, &str) -> Result<Option<Value>>,
            &format!("--object-format={format}"),
            &format!("--initial-branch={initial}"),
        ],
    )?;
    let Some(base) = base else {
        copy_config(primary, root)?;
        return Ok(
            json!({"method": "none", "source": "none", "bytes": 0, "pack_bytes": 0, "index_bytes": 0, "shared_bytes": 0, "cache_bytes": 0, "object_format": format}),
        );
    };
    let receipt = match snapshot(primary, root, format, &base)? {
        Some(receipt) => receipt,
        None => {
            let boundary = shallow::initialize(primary, root, &base)?;
            history::bootstrap(
                primary,
                root,
                state,
                format,
                &base,
                boundary.as_ref().map(shallow::Boundary::history),
            )?
        }
    };
    let head = branch_ref.as_deref().unwrap_or("HEAD");
    run(root, &["update-ref", "--no-deref", head, &base])?;
    // includeIf.onbranch must observe the final branch or detached HEAD.
    copy_config(primary, root)?;
    // The checkout phase owns index and worktree population.
    Ok(receipt)
}

pub(super) fn open_regular(path: &Path) -> io::Result<File> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a regular file",
        ));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a regular file",
        ));
    }
    Ok(file)
}

fn sync_pack_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    open_regular(path)?
        .sync_all()
        .context("sync independent Git pack file")?;
    // Windows relies on native Git's fsync at creation and the write-through
    // rename used for exclusive publication.
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_file(source: &File, destination: &Path) -> io::Result<()> {
    use std::{
        ffi::CString,
        os::{fd::AsRawFd, macos::fs::MetadataExt, unix::ffi::OsStrExt},
    };
    let mut names = [0; 128];
    let length = unsafe {
        libc::flistxattr(
            source.as_raw_fd(),
            names.as_mut_ptr().cast(),
            names.len(),
            0,
        )
    };
    if source.metadata()?.st_flags() != 0
        || (length != 0 && names.get(..length as usize) != Some(b"com.apple.provenance\0"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Git pack has nonstandard file metadata",
        ));
    }
    let parent = File::open(destination.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    let name = CString::new(
        destination
            .file_name()
            .ok_or(io::ErrorKind::InvalidInput)?
            .as_bytes(),
    )?;
    const NO_OWNER_COPY: u32 = 0x0002;
    if unsafe {
        libc::fclonefileat(
            source.as_raw_fd(),
            parent.as_raw_fd(),
            name.as_ptr(),
            NO_OWNER_COPY,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn clone_file(source: &File, destination: &Path) -> io::Result<()> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let dest = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o444)
        .open(destination)?;
    if unsafe { libc::ioctl(dest.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn clone_file(_source: &File, _destination: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(test)]
fn pin_matches(primary: &Path, pin: &str, oid: &str) -> Result<bool> {
    let symbolic = transfer_git(primary)
        .args(["symbolic-ref", "--quiet", pin])
        .output()?;
    ensure!(
        symbolic.status.code() == Some(1),
        "private Git pin {pin} must be a direct ref"
    );
    let current = transfer_git(primary)
        .args(["show-ref", "--verify", "--hash", pin])
        .output()?;
    if current.status.success() {
        ensure!(
            output_text(&current.stdout)? == oid,
            "private Git pin {pin} already names another commit"
        );
        return Ok(true);
    }
    Ok(false)
}

pub(super) struct PackLock {
    path: PathBuf,
    file: File,
    marker: Vec<u8>,
}

impl PackLock {
    fn capture(path: PathBuf, pid: u32) -> Result<Self> {
        Self::capture_marker(path, &format!("fetch-pack {pid} on "))
    }

    fn capture_marker(path: PathBuf, prefix: &str) -> Result<Self> {
        let file = open_regular(&path)?;
        let mut marker = Vec::new();
        (&file).take(1025).read_to_end(&mut marker)?;
        ensure!(
            marker.len() <= 1024
                && marker.starts_with(prefix.as_bytes())
                && marker.ends_with(b"\n"),
            "Git pack lock is not owned by this fetch"
        );
        Ok(Self { path, file, marker })
    }

    fn matches(&self, path: &Path) -> Result<bool> {
        let file = open_regular(path)?;
        if !same_file(&self.file, &file)? {
            return Ok(false);
        }
        let mut marker = Vec::new();
        file.take((self.marker.len() + 1) as u64)
            .read_to_end(&mut marker)?;
        Ok(marker == self.marker)
    }
}

impl Drop for PackLock {
    fn drop(&mut self) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        if !self.matches(&self.path).unwrap_or(false) {
            return;
        }
        let Ok(temp) = tempfile::tempdir_in(parent) else {
            return;
        };
        let held = temp.path().join("fetch.keep");
        if rename_exclusive(&self.path, &held).is_err() {
            return;
        }
        // Recheck after acquiring the name. If another process raced the
        // rename, restore its entry or preserve it in the holding directory.
        if !self.matches(&held).unwrap_or(false) && rename_exclusive(&held, &self.path).is_err() {
            let _ = temp.keep();
        }
    }
}

#[cfg(unix)]
fn same_file(left: &File, right: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let (left, right) = (left.metadata()?, right.metadata()?);
    Ok(left.nlink() == 1
        && right.nlink() == 1
        && (left.dev(), left.ino()) == (right.dev(), right.ino()))
}

#[cfg(windows)]
fn same_file(left: &File, right: &File) -> io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let file_id = |file: &File| -> io::Result<_> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
            info.nNumberOfLinks,
        ))
    };
    let (left, right) = (file_id(left)?, file_id(right)?);
    Ok(left.3 == 1 && left == right)
}

#[cfg(not(any(unix, windows)))]
fn same_file(_left: &File, _right: &File) -> io::Result<bool> {
    Ok(false)
}

fn fetch_objects(
    primary: &Path,
    workspace: &Path,
    tip: &str,
    quarantine: Option<&shallow::Quarantine>,
) -> Result<Vec<PackLock>> {
    let workspace = dunce::canonicalize(workspace)?;
    let mut cmd = match quarantine {
        Some(stage) => stage.command(primary),
        None => transfer_git(primary),
    };
    // fetch-pack negotiates with the local upload-pack and writes only
    // objects. It avoids updating refs or FETCH_HEAD and running maintenance
    // or submodule fetches. The child inherits the protocol and config limits.
    cmd.env("GIT_ALLOW_PROTOCOL", "file")
        .args([
            "-c",
            "uploadpack.allowAnySHA1InWant=true",
            "fetch-pack",
            "--keep",
            "--lock-pack",
            "--thin",
            "--no-progress",
        ])
        .arg(&workspace)
        .arg(tip);
    let fetch = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start local Git fetch")?;
    let pid = fetch.id();
    let out = fetch.wait_with_output()?;
    let expected = format!("{tip} {tip}");
    let mut locks = Vec::new();
    let mut found = false;
    for line in output_text(&out.stdout)?.lines() {
        if let Some(path) = line.strip_prefix("lock ") {
            let name = Path::new(path).file_name().and_then(|name| name.to_str());
            let hash = name
                .and_then(|name| name.strip_prefix("pack-"))
                .and_then(|name| name.strip_suffix(".keep"))
                .context("invalid Git pack lock")?;
            git::checked_oid(hash)?;
            let name = format!("pack-{hash}.keep");
            let path = match quarantine {
                Some(stage) => stage.pack_dir().join(&name),
                None => repository_path(primary, &format!("objects/pack/{name}"))?,
            };
            // Git reports only newly created locks. Check its PID marker
            // and retain the file identity before accepting cleanup duty.
            if let Ok(lock) = PackLock::capture(path, pid) {
                locks.push(lock);
            }
        } else if line == expected {
            found = true;
        }
    }
    ensure!(
        out.status.success(),
        "fetch exact Git tip from local workspace: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    ensure!(
        found,
        "local Git fetch did not confirm the exact requested tip"
    );
    Ok(locks)
}

#[cfg(test)]
fn import(primary: &Path, workspace: &Path, tip: &str, pin: &str) -> Result<()> {
    ensure!(
        pin.starts_with("refs/graft/workspaces/"),
        "expected a private independent Git pin"
    );
    run(primary, &["check-ref-format", pin])?;
    let tip = git::checked_oid(tip)?.to_ascii_lowercase();
    if pin_matches(primary, pin, &tip)? {
        exact_commit(primary, &tip)?;
        return Ok(());
    }
    // Hold any newly fetched pack against GC until its exact tip is pinned.
    let _locks = transfer_result(primary, workspace, &tip)?;
    let null_oid = "0".repeat(tip.len());
    let pinned = run(primary, &["update-ref", "--no-deref", pin, &tip, &null_oid]);
    if let Err(error) = pinned
        && !pin_matches(primary, pin, &tip)?
    {
        return Err(error);
    }
    Ok(())
}

/// Keep newly installed packs safe from GC until the caller publishes a ref.
pub(super) fn transfer_result(
    destination: &Path,
    source: &Path,
    oid: &str,
) -> Result<Vec<PackLock>> {
    ensure!(
        object_format(destination)? == object_format(source)?,
        "Git object formats differ"
    );
    let oid = exact_commit(source, oid)?;
    let locks = if shallow::is_shallow(destination)? || shallow::is_shallow(source)? {
        shallow::fetch(destination, source, &oid)?
    } else {
        fetch_objects(destination, source, &oid, None)?
    };
    graph_count(transfer_git(destination), &oid)?;
    let objects = repository_path(destination, "objects")?;
    let packs = objects.join("pack");
    if packs.is_dir() || !locks.is_empty() {
        sync_directory(&packs)?;
    }
    sync_directory(&objects)?;
    Ok(locks)
}

pub(super) fn remove_import_pin(private: &Path, reference: &str, expected: &str) -> Result<()> {
    if let Some(oid) = crate::recovery::direct_ref(private, reference)? {
        ensure!(
            oid == expected,
            "temporary import pin was replaced: {reference}"
        );
        run(
            private,
            &["update-ref", "--no-deref", "-d", reference, &oid],
        )?;
    }
    Ok(())
}

/// Copy the old and workspace commits into a private landing repository.
pub(super) fn import_plan_inputs(
    private: &Path,
    primary: &Path,
    workspace: &Path,
    state: &Path,
    import_prefix: Option<&str>,
    old: &str,
    tip: &str,
) -> Result<Vec<PackLock>> {
    if shallow::is_shallow(private)?
        || shallow::is_shallow(primary)?
        || shallow::is_shallow(workspace)?
    {
        let format = object_format(primary)?;
        ensure!(
            object_format(private)? == format && object_format(workspace)? == format,
            "Git object formats differ"
        );
        let old = exact_commit(primary, old)?;
        let tip = exact_commit(workspace, tip)?;
        shallow::import_plan_inputs(private, primary, workspace, &old, &tip)?;
        Ok(Vec::new())
    } else {
        let old = exact_commit(primary, old)?;
        let format = object_format(primary)?;
        ensure!(
            object_format(private)? == format && object_format(workspace)? == format,
            "Git object formats differ"
        );
        // A ref makes the verified base visible to fetch-pack negotiation and
        // protects it until the durable plan pins have been written.
        let cached = if import_prefix.is_some() {
            plan_cache::restore(private, state, format, &old)?
        } else {
            None
        };
        let mut locks = if let Some(cached) = cached {
            cached
        } else {
            transfer_result(private, primary, &old)?
        };
        if let Some(import_prefix) = import_prefix {
            let pin = format!("{import_prefix}/base");
            run(
                private,
                &[
                    "update-ref",
                    "--no-deref",
                    &pin,
                    &old,
                    &"0".repeat(old.len()),
                ],
            )?;
        }
        locks.extend(transfer_result(private, workspace, tip)?);
        if let Some(import_prefix) = import_prefix {
            let tip = exact_commit(workspace, tip)?;
            let pin = format!("{import_prefix}/tip");
            run(
                private,
                &[
                    "update-ref",
                    "--no-deref",
                    &pin,
                    &tip,
                    &"0".repeat(tip.len()),
                ],
            )?;
        }
        Ok(locks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use history::cache;

    fn initialize_test(
        primary: &Path,
        root: &Path,
        state: &Path,
        base: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Value> {
        initialize(&preflight(primary)?, root, state, base, branch)
    }

    fn commit(repo: &Path, text: &str) -> Result<String> {
        fs::write(repo.join("file.txt"), text)?;
        git::bytes(repo, &["add", "file.txt"])?;
        git::bytes(repo, &["-c", "commit.gpgsign=false", "commit", "-qm", text])?;
        git::oid(repo, "HEAD")
    }

    #[test]
    fn independent_packs_and_private_pin_survive_move_and_gc() -> Result<()> {
        for format in ["sha1", "sha256"] {
            let temp = tempfile::tempdir()?;
            let primary = temp.path().join("original primary");
            let state = temp.path().join("state");
            let first = temp.path().join("first");
            let workspace = temp.path().join("workspace with ' and $ characters");
            for path in [&primary, &state, &first, &workspace] {
                fs::create_dir(path)?;
            }
            run(
                &primary,
                &[
                    "init",
                    "--quiet",
                    "--template=",
                    &format!("--object-format={format}"),
                ],
            )?;
            run(&primary, &["config", "user.name", "Workspace Test"])?;
            run(
                &primary,
                &["config", "user.email", "workspace-test@example.test"],
            )?;
            run(&primary, &["config", "core.autocrlf", "false"])?;
            let parent = commit(&primary, "parent")?;
            let base = commit(&primary, "base")?;
            fs::create_dir(workspace.join(".git"))?;
            fs::write(workspace.join(".git").join(MARKER), b"marker")?;
            let cold = initialize_test(&primary, &first, &state, Some(&base), None)?;
            let warm = initialize_test(&primary, &workspace, &state, Some(&base), Some("task"))?;
            assert_eq!(cold["method"], "local-pack");
            if cold["cache_bytes"].as_u64().unwrap() > 0 {
                assert!(matches!(
                    warm["method"].as_str(),
                    Some("clonefile" | "reflink")
                ));
                assert_eq!(warm["bytes"], warm["shared_bytes"]);
                assert_eq!(warm["source"], "cache");
                assert!(warm["timings"]["cache_seconds"].is_number());
                assert!(warm["timings"]["reachability_seconds"].is_null());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let pack = fs::read_dir(first.join(".git/objects/pack"))?
                        .find_map(|entry| {
                            let entry = entry.ok()?;
                            (entry.path().extension()?.to_str()? == "pack").then_some(entry)
                        })
                        .context("cold Git pack")?;
                    let left = pack.metadata()?;
                    let right =
                        fs::metadata(workspace.join(".git/objects/pack").join(pack.file_name()))?;
                    assert_ne!(left.ino(), right.ino());
                    assert_eq!((left.nlink(), right.nlink()), (1, 1));
                }
            }
            assert_eq!(git::oid(&first, "HEAD")?, base);
            assert_eq!(git::symbolic_ref(&first, "HEAD")?, None);
            assert_eq!(
                git::symbolic_ref(&workspace, "HEAD")?.as_deref(),
                Some("refs/heads/task")
            );
            assert_eq!(object_format(&workspace)?, format);
            assert!(!workspace.join(".git/index").exists());
            assert!(!workspace.join("file.txt").exists());
            assert!(!workspace.join(".git/objects/info/alternates").exists());
            assert!(initialize_test(&primary, &workspace, &state, Some(&base), None).is_err());

            let moved = temp.path().join("moved primary");
            fs::rename(&primary, &moved)?;
            git::bytes(&workspace, &["read-tree", "-m", "-u", "HEAD"])?;
            let tip = commit(&workspace, "workspace edit")?;
            run(&workspace, &["branch", "private", &parent])?;
            run(&workspace, &["tag", "private-tag", &tip])?;
            // upload-pack must ignore executable repository-local config.
            run(
                &workspace,
                &["config", "uploadpack.packObjectsHook", "exit 97"],
            )?;
            run(&workspace, &["gc", "--prune=now"])?;
            run(&workspace, &["fsck", "--full"])?;
            run(&moved, &["gc", "--prune=now"])?;
            assert!(exact_commit(&moved, &tip).is_err());
            let pin = "refs/graft/workspaces/test/land/tip";
            import(&moved, &workspace, &tip, pin)?;
            import(&moved, &workspace, &tip, pin)?;
            assert!(import(&moved, &workspace, &base, pin).is_err());
            assert!(!moved.join(".git/FETCH_HEAD").exists());
            for entry in fs::read_dir(moved.join(".git/objects/pack"))? {
                assert_ne!(
                    entry?.path().extension().and_then(|ext| ext.to_str()),
                    Some("keep")
                );
            }
            run(&moved, &["gc", "--prune=now"])?;
            run(&moved, &["fsck", "--full"])?;
            assert_eq!(git::oid(&moved, pin)?, tip);
            assert_eq!(git::oid(&moved, "HEAD")?, base);
            assert!(git::oid(&moved, "refs/heads/private").is_err());
            assert!(git::oid(&moved, "refs/tags/private-tag").is_err());
        }
        Ok(())
    }

    #[test]
    fn orphan_bootstraps_without_index_and_local_filters_fail_preflight() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("workspace");
        fs::create_dir(&root)?;
        run(temp.path(), &["init", "--quiet", "--template="])?;
        let receipt = initialize_test(temp.path(), &root, temp.path(), None, Some("orphan"))?;
        assert_eq!(receipt["method"], "none");
        assert_eq!(
            git::symbolic_ref(&root, "HEAD")?.as_deref(),
            Some("refs/heads/orphan")
        );
        assert!(git::oid(&root, "HEAD").is_err());
        assert!(!root.join(".git/index").exists());
        run(
            temp.path(),
            &["config", "filter.example.process", "unexpected command"],
        )?;
        assert!(
            preflight(temp.path())
                .unwrap_err()
                .to_string()
                .contains("local Git filters")
        );
        Ok(())
    }

    #[test]
    fn exact_primary_pack_excludes_private_objects_and_recovers_from_bad_cache() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let primary = temp.path().join("primary");
        let state = temp.path().join("state");
        let clean = temp.path().join("clean");
        let workspace = temp.path().join("workspace");
        for path in [&primary, &state, &clean, &workspace] {
            fs::create_dir(path)?;
        }
        run(
            &primary,
            &["init", "--quiet", "--object-format=sha1", "--template="],
        )?;
        run(&primary, &["config", "user.name", "Workspace Test"])?;
        run(
            &primary,
            &["config", "user.email", "workspace-test@example.test"],
        )?;
        let base = commit(&primary, "base")?;
        run(&primary, &["gc", "--prune=now"])?;
        fs::write(primary.join("staged-only.txt"), "private staged contents")?;
        run(&primary, &["add", "staged-only.txt"])?;
        let staged = run(&primary, &["rev-parse", ":staged-only.txt"])?;
        let staged = output_text(&staged)?;
        run(&primary, &["cat-file", "-e", staged])?;
        let first = initialize_test(&primary, &clean, &state, Some(&base), None)?;
        assert!(run(&clean, &["cat-file", "-e", staged]).is_err());

        let private = commit(&primary, "private")?;
        let newer = temp.path().join("newer");
        fs::create_dir(&newer)?;
        let missing_tip = initialize_test(&primary, &newer, &state, Some(&private), None)?;
        if first["cache_bytes"].as_u64().unwrap() > 0 {
            assert_eq!(missing_tip["source"], "ancestor-cache");
            assert_eq!(missing_tip["ancestor"], base);
        } else {
            assert_eq!(missing_tip["source"], "local-export");
        }
        run(&newer, &["fsck", "--full"])?;
        run(
            &primary,
            &["tag", "-am", "private", "private-tag", &private],
        )?;
        fs::write(primary.join("dangling.txt"), "dangling pack data")?;
        let dangling = run(&primary, &["hash-object", "-w", "dangling.txt"])?;
        let dangling = output_text(&dangling)?;
        run(&primary, &["repack", "--cruft", "-adq"])?;
        let private_pack = fs::read_dir(primary.join(".git/objects/pack"))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension().is_some_and(|ext| ext == "pack")
                    && !path.with_extension("mtimes").exists()
            })
            .context("primary history pack")?;
        assert!(
            !primary
                .join(".git/objects")
                .join(&dangling[..2])
                .join(&dangling[2..])
                .exists()
        );
        run(&primary, &["cat-file", "-e", dangling])?;

        let entry = state.join(cache::DIR).join(format!("sha1-{base}"));
        cache::prepare_root(&state.join(cache::DIR))?;
        if first["cache_bytes"].as_u64().unwrap() > 0 {
            let name = entry.file_name().unwrap();
            let proof = verify_history_cache_generation(&entry, &state, name)?;
            let quarantine = tempfile::tempdir_in(entry.parent().unwrap())?;
            let moved = quarantine.path().join(name);
            rename_exclusive(&entry, &moved)?;
            assert_eq!(
                verify_history_cache_generation(&moved, &state, name)?,
                proof
            );
            rename_exclusive(&moved, &entry)?;
            let original = fs::read_dir(&entry)?
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| path.extension().is_some_and(|ext| ext == "pack"))
                .context("cached Git pack")?;
            // Replace both complete files with a different valid pack and
            // keep the old receipt. Neither the old workspace nor the new
            // one may gain the replacement's private history.
            for ext in ["pack", "idx"] {
                let original = original.with_extension(ext);
                rename_exclusive(&original, &temp.path().join(format!("original.{ext}")))?;
                fs::copy(private_pack.with_extension(ext), &original)?;
            }
            assert!(verify_history_cache_generation(&entry, &state, name).is_err());
        } else {
            fs::create_dir_all(&entry)?;
        }
        let corrupt = entry.join("foreign-data");
        let isolated = initialize_test(&primary, &workspace, &state, Some(&base), None)?;
        assert_eq!(isolated["source"], "local-export");
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(isolated["cache_errors"]["restore"].is_string());
        }
        fs::write(&corrupt, b"not a Git pack")?;
        let later = temp.path().join("later");
        fs::create_dir(&later)?;
        let fallback = initialize_test(&primary, &later, &state, Some(&base), None)?;
        assert_eq!(fallback["source"], "local-export");
        assert_eq!(fs::read(&corrupt)?, b"not a Git pack");
        for root in [&workspace, &clean, &later] {
            assert!(exact_commit(root, &private).is_err());
            assert!(run(root, &["cat-file", "-e", dangling]).is_err());
            assert!(run(root, &["cat-file", "-e", staged]).is_err());
        }
        run(&clean, &["gc", "--prune=now"])?;
        run(&clean, &["fsck", "--full"])?;
        run(&workspace, &["fsck", "--full"])?;
        run(&primary, &["fsck", "--full"])?;
        Ok(())
    }

    #[test]
    fn exact_multi_pack_primary_is_reused_despite_dirty_loose_objects() -> Result<()> {
        use std::io::Write as _;
        let temp = tempfile::tempdir()?;
        let primary = temp.path().join("primary");
        let state = temp.path().join("state");
        let root = temp.path().join("workspace");
        for path in [&primary, &state, &root] {
            fs::create_dir(path)?;
        }
        run(
            &primary,
            &["init", "--quiet", "--template=", "--object-format=sha256"],
        )?;
        run(&primary, &["config", "user.name", "Workspace Test"])?;
        run(
            &primary,
            &["config", "user.email", "workspace-test@example.test"],
        )?;
        run(&primary, &["config", "maintenance.auto", "false"])?;
        run(&primary, &["config", "gc.auto", "0"])?;
        commit(&primary, "parent")?;
        run(&primary, &["repack", "-adbq"])?;
        let base = commit(&primary, "base")?;
        run(&primary, &["repack", "-dq"])?;
        let donors: Vec<_> = fs::read_dir(primary.join(".git/objects/pack"))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "pack"))
            .collect();
        assert_eq!(donors.len(), 2);
        let can_clone =
            clone_file(&open_regular(&donors[0])?, &temp.path().join("probe.pack")).is_ok();
        fs::write(primary.join("staged.txt"), "not part of the base")?;
        run(&primary, &["add", "staged.txt"])?;
        let staged = git::text(&primary, &["rev-parse", ":staged.txt"])?;

        let receipt = initialize_test(&primary, &root, &state, Some(&base), None)?;
        if can_clone {
            assert_eq!(receipt["source"], "primary-pack", "{receipt}");
            assert_eq!(receipt["pack_count"], 2);
            for donor in donors {
                let name = donor.file_name().unwrap();
                assert!(root.join(".git/objects/pack").join(name).is_file());
            }
        }
        assert!(run(&root, &["cat-file", "-e", &staged]).is_err());
        run(&root, &["fsck", "--full"])?;

        let next = commit(&primary, "new committed history")?;
        // The earlier staged blob became part of the preceding commit.
        // Put a fresh private blob in its own native pack, separate from the
        // exact donor packs and the not-yet-packed new base.
        fs::write(primary.join("staged.txt"), "fresh private contents")?;
        run(&primary, &["add", "staged.txt"])?;
        let secret = git::text(&primary, &["rev-parse", ":staged.txt"])?;
        let mut write_pack = transfer_git(&primary);
        let mut pack = write_pack
            .args(["pack-objects", "--quiet"])
            .arg(primary.join(".git/objects/pack/pack"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()?;
        writeln!(
            pack.stdin.take().context("Git pack input is missing")?,
            "{secret}"
        )?;
        ensure!(
            pack.wait()?.success(),
            "could not create private Git pack fixture"
        );

        let fresh_state = temp.path().join("fresh-state");
        let mixed = temp.path().join("mixed");
        fs::create_dir(&fresh_state)?;
        fs::create_dir(&mixed)?;
        let receipt = initialize_test(&primary, &mixed, &fresh_state, Some(&next), None)?;
        assert_eq!(receipt["source"], "local-export");
        if can_clone {
            assert_eq!(receipt["reused_primary_packs"], 2, "{receipt}");
            assert_eq!(receipt["pack_count"], 3);
        }
        assert!(run(&mixed, &["cat-file", "-e", &secret]).is_err());
        run(&mixed, &["fsck", "--full"])?;
        Ok(())
    }

    #[test]
    fn ancestor_generation_handles_a_resurrected_blob_and_rejects_divergence() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let primary = temp.path().join("primary");
        let state = temp.path().join("state");
        let old = temp.path().join("old");
        let descendant = temp.path().join("descendant");
        let diverged = temp.path().join("diverged");
        for path in [&primary, &state, &old, &descendant, &diverged] {
            fs::create_dir(path)?;
        }
        run(&primary, &["init", "--quiet", "--template="])?;
        run(&primary, &["config", "user.name", "Workspace Test"])?;
        run(
            &primary,
            &["config", "user.email", "workspace-test@example.test"],
        )?;
        let original = commit(&primary, "reappearing content")?;
        run(&primary, &["rm", "file.txt"])?;
        run(&primary, &["commit", "-qm", "delete file"])?;
        let cached = git::oid(&primary, "HEAD")?;
        let cold = initialize_test(&primary, &old, &state, Some(&cached), None)?;
        let next = commit(&primary, "reappearing content")?;
        let receipt = initialize_test(&primary, &descendant, &state, Some(&next), None)?;
        if cold["cache_bytes"].as_u64().unwrap() > 0 {
            assert_eq!(receipt["source"], "ancestor-cache", "{receipt}");
            assert_eq!(receipt["ancestor"], cached);
            assert_eq!(receipt["new_pack_count"], 1);
        }
        let inspect = |path: &Path, oid: &str| -> Result<()> {
            let raw = run(path, &["rev-list", "--objects", "--no-object-names", oid])?;
            let inventory = run(
                path,
                &[
                    "cat-file",
                    "--batch-all-objects",
                    "--batch-check=%(objectname)",
                ],
            )?;
            let sorted = |out| -> Result<Vec<String>> {
                let mut ids: Vec<_> = output_text(out)?.lines().map(str::to_owned).collect();
                ids.sort_unstable();
                Ok(ids)
            };
            assert_eq!(sorted(&inventory)?, sorted(&raw)?);
            run(path, &["fsck", "--full"])?;
            Ok(())
        };
        inspect(&descendant, &next)?;
        run(&primary, &["checkout", "--quiet", "--detach", &original])?;
        let other = commit(&primary, "divergent content")?;
        let receipt = initialize_test(&primary, &diverged, &state, Some(&other), None)?;
        assert_eq!(receipt["source"], "local-export");
        assert!(exact_commit(&diverged, &next).is_err());
        assert!(exact_commit(&diverged, &cached).is_err());
        inspect(&diverged, &other)?;
        Ok(())
    }

    #[test]
    fn bootstrap_leaves_foreign_cache_root_and_pack_files_alone() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let primary = temp.path().join("primary");
        let state = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        for path in [&primary, &state, &workspace] {
            fs::create_dir(path)?;
        }
        run(&primary, &["init", "--quiet", "--template="])?;
        run(&primary, &["config", "user.name", "Workspace Test"])?;
        run(
            &primary,
            &["config", "user.email", "workspace-test@example.test"],
        )?;
        let base = commit(&primary, "base")?;
        let foreign_cache = state.join(cache::DIR);
        fs::create_dir(&foreign_cache)?;
        fs::write(foreign_cache.join("foreign"), b"foreign")?;
        let receipt = initialize_test(&primary, &workspace, &state, Some(&base), None)?;
        assert_eq!(receipt["source"], "local-export");
        assert_eq!(receipt["cache_bytes"], 0);
        let mut entries = fs::read_dir(&foreign_cache)?;
        assert_eq!(entries.next().transpose()?.unwrap().file_name(), "foreign");
        assert!(entries.next().is_none());

        let pack_dir = workspace.join(".git/objects/pack");
        let format = object_format(&workspace)?;
        let foreign_pack = pack_dir.join("test.pack");
        fs::write(&foreign_pack, b"foreign")?;
        assert!(history::bootstrap(&primary, &workspace, &state, format, &base, None).is_err());
        assert_eq!(fs::read(&foreign_pack)?, b"foreign");
        Ok(())
    }

    #[test]
    fn pack_lock_cleanup_preserves_existing_or_replaced_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("test.keep");
        let old = temp.path().join("old.keep");
        let marker = b"fetch-pack 123 on localhost\n";
        fs::write(&path, b"outside process\n")?;
        assert!(PackLock::capture(path.clone(), 123).is_err());
        fs::write(&path, marker)?;
        let lock = PackLock::capture(path.clone(), 123)?;
        rename_exclusive(&path, &old)?;
        fs::write(&path, marker)?;
        drop(lock);
        assert_eq!(fs::read(&path)?, marker);
        let lock = PackLock::capture(path.clone(), 123)?;
        drop(lock);
        assert!(!path.exists());
        assert_eq!(fs::read(&old)?, marker);
        Ok(())
    }

    #[test]
    fn preflight_rejects_replacement_refs_and_legacy_grafts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let primary = temp.path();
        run(primary, &["init", "--quiet", "--template="])?;
        run(primary, &["config", "user.name", "Workspace Test"])?;
        run(
            primary,
            &["config", "user.email", "workspace-test@example.test"],
        )?;
        let base = commit(primary, "base")?;
        let replacement = commit(primary, "replacement")?;
        run(primary, &["replace", &base, &replacement])?;
        assert!(
            preflight(primary)
                .unwrap_err()
                .to_string()
                .contains("replacement refs")
        );
        run(primary, &["replace", "--delete", &base])?;
        let grafts = primary.join(".git/info/grafts");
        fs::create_dir_all(grafts.parent().unwrap())?;
        fs::write(&grafts, format!("{replacement}\n"))?;
        assert!(
            preflight(primary)
                .unwrap_err()
                .to_string()
                .contains("legacy Git grafts")
        );
        Ok(())
    }
}
