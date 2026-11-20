//! Project discovery and filesystem ownership for the workspace registry.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

use crate::{api::fault, git, recovery::rename_exclusive};

pub const LEASE_SECS: u64 = 24 * 60 * 60;
pub const CACHE_OWNER: &str = ".git-graft-id";
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn validate_name(name: &str) -> Result<()> {
    let reserved = [
        "aux", "con", "nul", "prn", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if name.len() > 48
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || reserved.contains(&name)
    {
        return Err(fault(
            "invalid_name",
            "Use a portable graft name of 1–48 lowercase letters, numbers or hyphens, starting with a letter",
            Value::Null,
        ));
    }
    Ok(())
}
pub fn dir(path: &Path) -> Result<()> {
    match create_private_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => owned_directory(path).map(|_| ()),
        Err(e) => Err(e).with_context(|| format!("create directory {}", path.display())),
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    fs::create_dir(path)
}

#[cfg(unix)]
fn require_private_mode(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "directory is accessible to other users: {}. Restrict this Graft-owned directory to mode 0700 (chmod 700) before making changes",
        path.display()
    );
    #[cfg(target_os = "macos")]
    macos_acl::verify_private_directory(path)
        .with_context(|| format!("verify macOS ACL for {}", path.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
mod macos_acl {
    use anyhow::{Result, bail};
    use std::{
        ffi::{c_int, c_uint, c_void},
        fs::OpenOptions,
        io,
        os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
        path::Path,
        ptr::{self, NonNull},
    };

    // Darwin's sys/acl.h uses opaque handles and an extended allow/deny ACL.
    const ACL_TYPE_EXTENDED: c_uint = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: c_uint = 1;
    const ACL_EXTENDED_DENY: c_uint = 2;

    unsafe extern "C" {
        fn acl_get_fd_np(fd: c_int, acl_type: c_uint) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_uint) -> c_int;
        fn acl_free(acl: *mut c_void) -> c_int;
    }

    struct Acl(NonNull<c_void>);
    impl Drop for Acl {
        fn drop(&mut self) {
            // acl_get_fd_np allocates this handle and acl_free releases it.
            unsafe { acl_free(self.0.as_ptr()) };
        }
    }

    pub(super) fn verify_private_directory(path: &Path) -> Result<()> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        // Read from the pinned directory so ENOENT means the opened object
        // has no ACL. With a path lookup it could mean the directory vanished.
        let acl = unsafe { acl_get_fd_np(directory.as_raw_fd(), ACL_TYPE_EXTENDED) };
        let Some(acl) = NonNull::new(acl).map(Acl) else {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ENOENT) {
                Ok(())
            } else {
                Err(error.into())
            };
        };
        let mut id = ACL_FIRST_ENTRY;
        loop {
            let mut entry = ptr::null_mut();
            // Entry and tag pointers are written into valid stack slots. The
            // selected entry is borrowed from acl, which outlives this loop.
            if unsafe { acl_get_entry(acl.0.as_ptr(), id, &mut entry) } != 0 {
                let error = io::Error::last_os_error();
                // Darwin uses EINVAL at the end of a valid ACL.
                return if error.raw_os_error() == Some(libc::EINVAL) {
                    Ok(())
                } else {
                    Err(error.into())
                };
            }
            let mut tag = 0;
            if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
            match tag {
                ACL_EXTENDED_DENY => {}
                ACL_EXTENDED_ALLOW => bail!(
                    "extended allow entry compromises directory privacy, including inherited access"
                ),
                _ => bail!("unrecognized ACL entry"),
            }
            id = ACL_NEXT_ENTRY;
        }
    }
}

pub fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Inspect the endpoint without following symlinks or Windows junctions.
pub fn owned_directory(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !is_reparse_point(&metadata),
        "expected an ordinary directory at {}",
        path.display()
    );
    Ok(metadata)
}

pub(crate) fn check_unmounted_directory(path: &Path, recursive: bool) -> Result<fs::Metadata> {
    let root = owned_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = path.parent().context("tree has no parent directory")?;
        ensure!(
            root.dev() == fs::metadata(parent)?.dev(),
            "mounted tree: {}",
            path.display()
        );
    }
    #[cfg(target_os = "linux")]
    check_mounts(path, recursive)?;
    #[cfg(not(target_os = "linux"))]
    let _ = recursive;
    Ok(root)
}

/// Check the contents of a tree before recursive removal. Ordinary symlinks
/// are leaves. Nested repositories and mounted filesystems are retained.
pub fn check_owned_tree(path: &Path) -> Result<()> {
    let root = check_unmounted_directory(path, true)?;
    #[cfg(unix)]
    let device = {
        use std::os::unix::fs::MetadataExt;
        root.dev()
    };
    #[cfg(not(unix))]
    let _ = root;

    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut head = false;
        let mut objects = false;
        let mut refs = false;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            head |= name.eq_ignore_ascii_case("HEAD");
            objects |= name.eq_ignore_ascii_case("objects");
            refs |= name.eq_ignore_ascii_case("refs") || name.eq_ignore_ascii_case("reftable");
            let child = entry.path();
            let metadata = fs::symlink_metadata(&child)?;
            ensure!(
                !name.eq_ignore_ascii_case(".git"),
                "nested Git metadata in {}",
                directory.display()
            );
            if metadata.file_type().is_symlink() {
                continue;
            }
            ensure!(
                !is_reparse_point(&metadata) && (metadata.is_file() || metadata.is_dir()),
                "unexpected filesystem entry at {}",
                child.display()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.dev() == device,
                    "mounted entry: {}",
                    child.display()
                );
            }
            if metadata.is_dir() {
                pending.push(child);
            }
        }
        ensure!(
            !(head && objects && refs),
            "nested bare Git repository in {}",
            directory.display()
        );
    }
    Ok(())
}

/// A relocation must leave every nested checkout and its Git administration
/// connected. Reject nested metadata instead of opening or rewriting it.
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
pub(crate) fn check_relocation_tree(root: &Path) -> Result<()> {
    verify_relocation(root, scan_relocation_tree)
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn verify_relocation(root: &Path, mut scan: impl FnMut(&Path) -> Result<()>) -> Result<()> {
    let identity = crate::cache::directory_identity(root)?;
    let mut attempts = 0;
    loop {
        let result = scan(root);
        ensure!(
            crate::cache::directory_identity(root)? == identity,
            "graft directory changed during relocation checks"
        );
        attempts += 1;
        // Native Git maintenance can rename a pack index between readdir and
        // lstat. A fresh scan must validate every current entry before moving.
        if attempts < 3
            && result.as_ref().is_err_and(|error| {
                error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
            })
        {
            continue;
        }
        return result;
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", windows))]
fn scan_relocation_tree(root: &Path) -> Result<()> {
    check_unmounted_directory(root, true)?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let tree = crate::storage::NoFollowRoot::open(root)?;
    #[cfg(windows)]
    let _root_anchors = {
        let ancestors: Vec<_> = root.ancestors().collect();
        ancestors
            .into_iter()
            .rev()
            .map(pin_windows_directory)
            .collect::<Result<Vec<_>>>()?
    };

    let admin = Path::new(".git");
    let mut pending = vec![PathBuf::new()];
    while let Some(directory) = pending.pop() {
        let absolute = root.join(&directory);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let names = tree.names(&directory)?;
        #[cfg(windows)]
        let (_pins, names) = {
            let mut path = root.to_path_buf();
            let mut pins = Vec::new();
            for part in directory.components() {
                path.push(part);
                pins.push(pin_windows_directory(&path)?);
            }
            let names = fs::read_dir(&absolute)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<io::Result<Vec<_>>>()?;
            (pins, names)
        };
        let mut head = false;
        let mut objects = false;
        let mut refs = false;
        for name in names {
            let child = directory.join(&name);
            if name.eq_ignore_ascii_case(".git") && child != admin {
                return Err(nested_git_metadata(&root.join(&child)));
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            let is_dir = tree
                .entry_is_directory(&child)
                .with_context(|| format!("inspect {}", root.join(&child).display()))?;
            #[cfg(windows)]
            let is_dir = {
                let metadata = fs::symlink_metadata(root.join(&child))?;
                if metadata.file_type().is_symlink() {
                    false
                } else {
                    ensure!(
                        !is_reparse_point(&metadata) && (metadata.is_file() || metadata.is_dir()),
                        "unexpected filesystem entry at {}",
                        root.join(&child).display()
                    );
                    metadata.is_dir()
                }
            };
            if child == admin {
                ensure!(
                    is_dir,
                    "graft Git administration is not an ordinary directory"
                );
            }
            head |= !is_dir && name.eq_ignore_ascii_case("HEAD");
            objects |= is_dir && name.eq_ignore_ascii_case("objects");
            refs |= is_dir
                && (name.eq_ignore_ascii_case("refs") || name.eq_ignore_ascii_case("reftable"));
            if is_dir {
                pending.push(child);
            }
        }
        if head && objects && refs && !directory.as_os_str().is_empty() && directory != admin {
            return Err(nested_git_metadata(&absolute));
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn check_relocation_tree(_root: &Path) -> Result<()> {
    anyhow::bail!("graft relocation is unsupported on this platform");
}

fn nested_git_metadata(path: &Path) -> anyhow::Error {
    fault(
        "nested_git_metadata",
        format!(
            "nested Git metadata at {} prevents relocation",
            path.display()
        ),
        serde_json::json!({"path":path}),
    )
}

#[cfg(windows)]
fn pin_windows_directory(path: &Path) -> Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    // Deny delete sharing while enumerating descendants by path so Windows
    // cannot replace a checked directory with a reparse point mid-walk.
    let handle = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = handle.metadata()?;
    ensure!(
        metadata.is_dir() && !is_reparse_point(&metadata),
        "expected an ordinary directory at {}",
        path.display()
    );
    Ok(handle)
}

#[cfg(target_os = "linux")]
fn check_mounts(path: &Path, recursive: bool) -> Result<()> {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let root = dunce::canonicalize(path)?;
    let mountinfo = fs::read("/proc/self/mountinfo").context("read Linux mount table")?;
    for line in mountinfo
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let encoded = line
            .split(|&byte| byte == b' ')
            .nth(4)
            .context("invalid mount table")?;
        // The kernel escapes whitespace and literal backslashes with three octal digits.
        let mut decoded = Vec::with_capacity(encoded.len());
        let mut remaining = encoded;
        while let [first, rest @ ..] = remaining {
            if let [
                b'\\',
                a @ b'0'..=b'3',
                b @ b'0'..=b'7',
                c @ b'0'..=b'7',
                tail @ ..,
            ] = remaining
            {
                decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
                remaining = tail;
            } else {
                decoded.push(*first);
                remaining = rest;
            }
        }
        let mount = Path::new(OsStr::from_bytes(&decoded));
        ensure!(
            mount != root && (!recursive || !mount.starts_with(&root)),
            "mounted entry: {}",
            mount.display()
        );
    }
    Ok(())
}

pub fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub(crate) fn open_lock(path: &Path) -> Result<File> {
    if exists(path)? {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && !is_reparse_point(&metadata),
            "lock is not a regular file"
        );
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}
pub(crate) fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    write_atomic(path, bytes, false)
}

pub(crate) fn publish_marked_directory(
    stage: tempfile::TempDir,
    path: &Path,
    ownership: &[u8],
) -> Result<()> {
    let parent = path.parent().context("missing publication directory")?;
    ensure!(
        stage.path().parent() == Some(parent),
        "directory publication requires a sibling stage"
    );
    write_atomic(&stage.path().join(CACHE_OWNER), ownership, true)?;
    // After rename the staging name can belong to someone else.
    let staged = stage.keep();
    rename_exclusive(&staged, path)
        .with_context(|| format!("publish {} from {}", path.display(), staged.display()))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8], _flush_at_directory_publication: bool) -> Result<()> {
    let parent = path.parent().context("missing state directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    #[cfg(target_os = "macos")]
    if apple_sync(tmp.as_file(), libc::F_BARRIERFSYNC)? {
        let file = tmp.persist(path)?;
        let directory = File::open(parent)?;
        // Order the marker's directory entry before publishing the stage.
        // Its sibling rename and parent flush complete the same-device write.
        if _flush_at_directory_publication && apple_sync(&directory, libc::F_BARRIERFSYNC)? {
            return Ok(());
        }
        if !apple_sync(&directory, libc::F_FULLFSYNC)? {
            directory.sync_all()?;
            file.sync_all()?;
        }
        return Ok(());
    }
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// A filesystem without the Apple barrier/flush commands uses ordinary sync.
#[cfg(target_os = "macos")]
fn apple_sync(file: &File, command: libc::c_int) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    loop {
        if unsafe { libc::fcntl(file.as_raw_fd(), command) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EINVAL | libc::ENOTSUP | libc::EOPNOTSUPP | libc::ENOTTY) => {
                return Ok(false);
            }
            _ => return Err(error),
        }
    }
}
pub(crate) fn regular(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !is_reparse_point(&metadata),
        "expected regular file at {}",
        path.display()
    );
    Ok(fs::read(path)?)
}

fn discover_primary(invocation: &Path) -> Result<(PathBuf, PathBuf)> {
    let discovery = git::raw(
        invocation,
        &[
            "rev-parse",
            "--is-bare-repository",
            "--path-format=absolute",
            "--git-common-dir",
            "--absolute-git-dir",
            "--show-toplevel",
        ],
    )?;
    let text = if discovery.status.success() {
        std::str::from_utf8(&discovery.stdout)
            .context("Git returned a non-UTF-8 repository path")?
    } else {
        ""
    };
    let fields: Vec<_> = text
        .split_terminator('\n')
        .map(|field| {
            if cfg!(windows) {
                field.strip_suffix('\r').unwrap_or(field)
            } else {
                field
            }
        })
        .collect();
    let (common, admin, toplevel) = match fields.as_slice() {
        ["false", common, admin, toplevel] => (
            dunce::canonicalize(common)?,
            dunce::canonicalize(admin)?,
            Some(*toplevel),
        ),
        // Bare repositories have no top level. Paths containing LF make
        // the combined output ambiguous, so query each field separately.
        _ => (
            git::common_dir(invocation)?,
            git::absolute_git_dir(invocation)?,
            None,
        ),
    };
    ensure!(
        admin == common,
        "select the primary repository with --repo when invoking from a native linked checkout"
    );
    let repo = match toplevel {
        Some(root) => dunce::canonicalize(root)?,
        None => match git::text(invocation, &["rev-parse", "--is-bare-repository"])?.as_str() {
            "true" => common.clone(),
            "false" => {
                dunce::canonicalize(git::text(invocation, &["rev-parse", "--show-toplevel"])?)?
            }
            _ => {
                anyhow::bail!("cannot determine whether the primary Git repository is bare")
            }
        },
    };
    Ok((repo, common))
}

/// The primary checkout, its Git common directory, the Graft state directory
/// and the workspace root, as resolved by [`Project::paths`].
pub(crate) type Paths = (PathBuf, PathBuf, PathBuf, PathBuf);

pub struct Project {
    pub repo: PathBuf,
    pub common: PathBuf,
    pub state: PathBuf,
    pub root: PathBuf,
    id: String,
}

impl Project {
    /// Resolve the primary checkout, its Git common directory, the Graft
    /// state directory and the workspace root. Runs one read-only Git query.
    pub(crate) fn paths(repo: &Path) -> Result<Paths> {
        let invocation = dunce::canonicalize(repo)?;
        let (repo, common) = discover_primary(&invocation)?;
        let name = repo
            .file_name()
            .context("cannot place grafts adjacent to filesystem root")?;
        let mut sibling = name.to_os_string();
        sibling.push(".graft");
        let root = repo.with_file_name(sibling);
        git::path_str(&root)?;
        let state = common.join("graft");
        Ok((repo, common, state, root))
    }

    #[cfg(test)]
    pub(crate) fn open_existing(repo: &Path) -> Result<Option<(Self, File)>> {
        Self::open_existing_at(Self::paths(repo)?)
    }

    /// Open an existing owned project at paths resolved by [`Self::paths`],
    /// without creating state, locks or caches. A present but incomplete
    /// state directory is ambiguous and is left alone.
    pub(crate) fn open_existing_at(paths: Paths) -> Result<Option<(Self, File)>> {
        let (repo, common, state, root) = paths;
        if !exists(&state)? {
            return Ok(None);
        }
        let metadata = owned_directory(&state)?;
        #[cfg(unix)]
        require_private_mode(&state, &metadata)?;
        #[cfg(not(unix))]
        let _ = metadata;
        let lock_path = state.join("project.lock");
        let lock_metadata = fs::symlink_metadata(&lock_path)
            .context("existing project lock is missing or inaccessible")?;
        ensure!(
            lock_metadata.is_file() && !is_reparse_point(&lock_metadata),
            "lock is not a regular file"
        );
        let lock = OpenOptions::new().read(true).write(true).open(&lock_path)?;
        lock.lock().context("lock graft registry")?;
        let id = String::from_utf8(regular(&state.join("project-id"))?)?;
        ensure!(
            Uuid::parse_str(&id)?.to_string() == id,
            "invalid project ownership ID"
        );
        Ok(Some((
            Self {
                repo,
                common,
                state,
                root,
                id,
            },
            lock,
        )))
    }

    /// The returned file holds the project lock. Long-running child commands
    /// release it after taking their workspace lock.
    #[cfg(test)]
    pub fn open(repo: &Path) -> Result<(Self, File)> {
        Self::open_at(Self::paths(repo)?, true)
    }

    /// Open the registry at resolved `paths`. Spawning passes `false`: it can
    /// attach an existing dependency installation but never seeds one.
    pub(crate) fn open_at(paths: Paths, create_dependency_caches: bool) -> Result<(Self, File)> {
        let (repo, common, state, root) = paths;
        dir(&state)?;
        #[cfg(unix)]
        require_private_mode(&state, &owned_directory(&state)?)?;
        let lock_path = state.join("project.lock");
        let id_file = state.join("project-id");
        if !exists(&id_file)? && !exists(&lock_path)? {
            // Leave a preexisting foreign registry untouched, even before locking.
            for entry in fs::read_dir(&state)? {
                let name = entry?.file_name();
                ensure!(
                    name == "project.lock" || exists(&lock_path)?,
                    "unrecognized graft data in {}",
                    state.display()
                );
            }
        }
        let lock = open_lock(&lock_path)?;
        lock.lock().context("lock graft registry")?;
        let id = if exists(&id_file)? {
            let id = String::from_utf8(regular(&id_file)?)?;
            ensure!(
                Uuid::parse_str(&id)?.to_string() == id,
                "invalid project ownership ID"
            );
            id
        } else {
            // A nonempty foreign or future-version store must never be adopted.
            for entry in fs::read_dir(&state)? {
                ensure!(
                    entry?.file_name() == "project.lock",
                    "unrecognized graft data in {}",
                    state.display()
                );
            }
            let id = Uuid::new_v4().to_string();
            atomic(&id_file, id.as_bytes())?;
            id
        };
        let project = Self {
            repo,
            common,
            state,
            root,
            id,
        };
        for name in ["cache", "layers"] {
            let path = project.state.join(name);
            if create_dependency_caches {
                project.init_cache_directory(&path)?;
            } else if exists(&path)? {
                owned_directory(&path)?;
            }
        }
        Ok((project, lock))
    }
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// The old registry can still contain references to workspace data and caches.
    pub(crate) fn has_legacy_workspace_records(&self) -> Result<bool> {
        if exists(&self.state.join("independent"))? {
            return Ok(true);
        }
        let records = self.state.join("records");
        if !exists(&records)? {
            return Ok(false);
        }
        Ok(owned_directory(&records).is_err()
            || fs::read_dir(&records)?.next().transpose()?.is_some())
    }

    /// Mark only directories Graft creates. Legacy unmarked caches remain usable.
    fn init_cache_directory(&self, path: &Path) -> Result<()> {
        match create_private_dir(path) {
            Ok(()) => atomic(&path.join(CACHE_OWNER), self.id().as_bytes()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                owned_directory(path).map(|_| ())
            }
            Err(error) => Err(error).with_context(|| format!("create cache {}", path.display())),
        }
    }
    pub fn check_cache_owner(&self, path: &Path) -> Result<()> {
        check_unmounted_directory(path, false)?;
        let marker = path.join(CACHE_OWNER);
        ensure!(
            exists(&marker)? && regular(&marker)? == self.id().as_bytes(),
            "cache ownership is unverified at {}",
            path.display()
        );
        Ok(())
    }
    pub fn require_private_state(&self) -> Result<()> {
        #[cfg(unix)]
        require_private_mode(&self.state, &owned_directory(&self.state)?)?;
        Ok(())
    }
    pub fn require_private_workspaces(&self) -> Result<()> {
        if exists(&self.root)? {
            self.root(false)?;
        }
        Ok(())
    }
    /// Only a directory created for this repository may contain managed trees.
    pub fn root(&self, create: bool) -> Result<()> {
        if create && !exists(&self.root)? {
            let parent = self.root.parent().context("graft root has no parent")?;
            let mut builder = tempfile::Builder::new();
            builder.prefix(".git-graft-pending-");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(fs::Permissions::from_mode(0o700));
            }
            let staged = builder.tempdir_in(parent)?;
            #[cfg(unix)]
            if let Err(error) = owned_directory(staged.path())
                .and_then(|metadata| require_private_mode(staged.path(), &metadata))
            {
                // The stage may be writable by another user. Remove it only
                // if it is still empty, without traversing its contents.
                let _ = fs::remove_dir(staged.keep());
                return Err(error).context("cannot create a private graft root");
            }
            publish_marked_directory(staged, &self.root, self.id().as_bytes())?;
        }
        let metadata =
            owned_directory(&self.root).context("graft root is not an ordinary directory")?;
        #[cfg(unix)]
        require_private_mode(&self.root, &metadata)?;
        #[cfg(not(unix))]
        let _ = metadata;
        ensure!(
            regular(&self.root.join(CACHE_OWNER))? == self.id().as_bytes(),
            "graft root belongs to another repository: {}",
            self.root.display()
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    fn relocation_rescans_every_entry_after_a_git_temporary_file_disappears() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = dunce::canonicalize(temporary.path())?;
        let pack = root.join(".git").join("objects").join("pack");
        fs::create_dir_all(&pack)?;
        for add_nested in [false, true] {
            let transient = pack.join("temporary.rev");
            fs::write(&transient, b"index")?;
            let mut attempts = 0;
            let result = verify_relocation(&root, |root| {
                attempts += 1;
                if attempts == 1 {
                    let path = fs::read_dir(&pack)?
                        .next()
                        .expect("temporary index")?
                        .path();
                    fs::remove_file(&path)?;
                    if add_nested {
                        fs::create_dir_all(root.join("child").join(".git"))?;
                    }
                    fs::symlink_metadata(path).context("inspect listed Git index")?;
                }
                scan_relocation_tree(root)
            });
            assert_eq!(attempts, 2);
            if add_nested {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("nested Git metadata")
                );
            } else {
                result?;
            }
        }
        Ok(())
    }

    #[test]
    fn existing_project_discovery_does_not_initialize_or_repair_state() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("primary");
        fs::create_dir(&repo)?;
        git::bytes(&repo, &["init", "-q", "--template="])?;
        let state = repo.join(".git/graft");
        assert!(Project::open_existing(&repo)?.is_none());
        assert!(!state.exists());

        let (project, lock) = Project::open(&repo)?;
        drop(lock);
        fs::rename(
            project.state.join("cache"),
            project.state.join("cache-saved"),
        )?;
        let (existing, lock) = Project::open_existing(&repo)?.expect("existing project");
        assert_eq!(existing.id(), project.id());
        assert!(!state.join("cache").exists());
        drop(lock);

        fs::rename(state.join("project.lock"), state.join("lock-saved"))?;
        assert!(Project::open_existing(&repo).is_err());
        assert!(!state.join("project.lock").exists());
        assert!(state.join("lock-saved").exists());
        Ok(())
    }

    #[test]
    fn existing_project_discovery_refuses_unowned_state() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("primary");
        fs::create_dir(&repo)?;
        git::bytes(&repo, &["init", "-q", "--template="])?;
        let state = repo.join(".git/graft");
        create_private_dir(&state)?;
        fs::write(state.join("foreign"), b"retain")?;
        assert!(Project::open_existing(&repo).is_err());
        assert_eq!(fs::read(state.join("foreign"))?, b"retain");
        assert!(!state.join("project.lock").exists());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn macos_fixture_repo(path: &Path) -> Result<()> {
        fs::create_dir(path)?;
        git::bytes(path, &["init", "-q", "--template="])?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn add_inheritable_allow(path: &Path) -> Result<()> {
        let output = std::process::Command::new("/bin/chmod")
            .args([
                "+a",
                "everyone allow list,search,file_inherit,directory_inherit",
            ])
            .arg(path)
            .output()?;
        ensure!(
            output.status.success(),
            "add fixture ACL: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn fixture_acl(path: &Path) -> Result<String> {
        let output = std::process::Command::new("/bin/ls")
            .arg("-led")
            .arg(path)
            .output()?;
        ensure!(output.status.success(), "read fixture ACL");
        Ok(String::from_utf8(output.stdout)?
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_an_inherited_state_acl_before_creating_the_lock_or_project_id() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("primary");
        macos_fixture_repo(&repo)?;
        let common = repo.join(".git");
        add_inheritable_allow(&common)?;
        let parent_acl = fixture_acl(&common)?;

        let Err(error) = Project::open(&repo) else {
            panic!("opened a registry with an inherited allow ACL");
        };
        assert!(format!("{error:#}").contains("extended allow entry"));
        let state = common.join("graft");
        let state_acl = fixture_acl(&state)?;
        assert!(state_acl.contains("inherited allow"));
        assert_eq!(fs::metadata(&state)?.permissions().mode() & 0o777, 0o700);
        assert!(fs::read_dir(&state)?.next().is_none());

        fs::write(state.join("private-note"), "existing data")?;
        assert!(Project::open(&repo).is_err());
        assert_eq!(
            fs::read_to_string(state.join("private-note"))?,
            "existing data"
        );
        assert_eq!(fixture_acl(&state)?, state_acl);
        assert_eq!(fixture_acl(&common)?, parent_acl);
        assert!(!state.join("project.lock").exists());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refuses_an_inherited_workspace_acl_without_publishing_or_changing_it() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("primary");
        macos_fixture_repo(&repo)?;
        let (project, _lock) = Project::open(&repo)?;

        // ACLs on ordinary source directories do not participate in this check.
        let source = repo.join("ordinary source");
        fs::create_dir(&source)?;
        add_inheritable_allow(&source)?;
        let source_acl = fixture_acl(&source)?;
        project.require_private_state()?;

        add_inheritable_allow(temp.path())?;
        let parent_acl = fixture_acl(temp.path())?;
        let error = project.root(true).unwrap_err();
        assert!(format!("{error:#}").contains("extended allow entry"));
        assert!(!project.root.exists());
        assert_eq!(fs::read_dir(temp.path())?.count(), 1);

        create_private_dir(&project.root)?;
        fs::write(project.root.join(CACHE_OWNER), project.id())?;
        fs::write(project.root.join("private-note"), "existing graft data")?;
        let root_acl = fixture_acl(&project.root)?;
        assert!(root_acl.contains("inherited allow"));
        assert_eq!(
            fs::metadata(&project.root)?.permissions().mode() & 0o777,
            0o700
        );
        assert!(project.require_private_workspaces().is_err());
        assert_eq!(
            fs::read_to_string(project.root.join("private-note"))?,
            "existing graft data"
        );
        assert_eq!(fixture_acl(&project.root)?, root_acl);
        assert_eq!(fixture_acl(temp.path())?, parent_acl);
        assert_eq!(fixture_acl(&source)?, source_acl);
        Ok(())
    }

    #[test]
    fn discovers_a_checkout_with_a_separate_git_directory() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("working repo");
        let admin = temp.path().join("Git admin");
        let nested = repo.join("nested");
        fs::create_dir_all(&nested)?;
        git::bytes(
            &repo,
            &[
                "init",
                "-q",
                "--template=",
                "--separate-git-dir",
                git::path_str(&admin)?,
            ],
        )?;

        let (project, _lock) = Project::open(&nested)?;
        assert_eq!(project.repo, dunce::canonicalize(repo)?);
        assert_eq!(project.common, dunce::canonicalize(admin)?);
        assert_eq!(project.state, project.common.join("graft"));
        Ok(())
    }

    #[test]
    fn refuses_native_linked_checkout_before_writing_project_state() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("primary");
        let linked = temp.path().join("linked");
        fs::create_dir(&repo)?;
        git::bytes(&repo, &["init", "-q", "--template="])?;
        git::bytes(
            &repo,
            &[
                "-c",
                "user.name=Repository Fixture",
                "-c",
                "user.email=repository@example.invalid",
                "-c",
                "core.hooksPath=disabled-hooks",
                "commit",
                "--allow-empty",
                "--no-gpg-sign",
                "-m",
                "base",
            ],
        )?;
        git::bytes(
            &repo,
            &[
                "-c",
                "core.hooksPath=disabled-hooks",
                "worktree",
                "add",
                "--detach",
                git::path_str(&linked)?,
            ],
        )?;

        let Err(error) = Project::open(&linked) else {
            panic!("opened a linked checkout as the primary");
        };
        assert!(error.to_string().contains("native linked checkout"));
        assert!(!exists(&repo.join(".git/graft"))?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn preserves_newlines_in_working_and_bare_repository_paths() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let working = temp.path().join("working\nrepo\n");
        let bare = temp.path().join("bare\nrepo\n");
        for (path, args) in [
            (&working, &["init", "-q", "--template="][..]),
            (&bare, &["init", "-q", "--template=", "--bare"][..]),
        ] {
            fs::create_dir(path)?;
            git::bytes(path, args)?;
            let (project, _lock) = Project::open(path)?;
            assert_eq!(project.repo, dunce::canonicalize(path)?);
            let common = if path == &working {
                path.join(".git")
            } else {
                path.clone()
            };
            assert_eq!(project.common, dunce::canonicalize(common)?);
        }
        Ok(())
    }
}
