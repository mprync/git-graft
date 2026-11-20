//! File-descriptor based cloning. A changing checkout can replace any donor
//! path component while a spawn is in progress.
use std::{
    cell::RefCell,
    ffi::{CStr, CString, OsStr, OsString},
    fs::{File, Metadata, OpenOptions},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

#[cfg(target_os = "macos")]
use std::{fs, os::unix::fs::PermissionsExt};

#[cfg(target_os = "macos")]
pub(super) mod parents;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn open_at(parent: &File, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // The descriptor belongs to this call and is closed with the File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) struct Root {
    directory: File,
    device: u64,
    parent: RefCell<Option<(PathBuf, File)>>,
    #[cfg(target_os = "macos")]
    ancestors: Option<RefCell<parents::Ancestors>>,
}

impl Root {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid("expected an absolute directory"));
        }
        let mut directory = File::open("/")?;
        for part in path.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => {
                    directory = open_at(&directory, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
                }
                _ => return Err(invalid("unexpected directory path")),
            }
        }
        let device = directory.metadata()?.dev();
        Ok(Self {
            directory,
            device,
            parent: RefCell::new(None),
            #[cfg(target_os = "macos")]
            ancestors: None,
        })
    }

    // Each worker keeps its own directory cache while pinning the same root.
    pub(super) fn fork(&self) -> io::Result<Self> {
        Ok(Self {
            directory: self.directory.try_clone()?,
            device: self.device,
            parent: RefCell::new(None),
            #[cfg(target_os = "macos")]
            ancestors: None,
        })
    }

    pub(super) fn checkout_worker(&self, reuse_ancestors: bool) -> io::Result<Self> {
        let worker = self.fork()?;
        #[cfg(target_os = "macos")]
        let worker = Self {
            ancestors: reuse_ancestors.then(RefCell::default),
            ..worker
        };
        #[cfg(not(target_os = "macos"))]
        let _ = reuse_ancestors;
        Ok(worker)
    }

    pub(super) fn same_device(&self, other: &Self) -> bool {
        self.device == other.device
    }

    pub(super) fn create_parent(&self, path: &Path) -> io::Result<()> {
        let directory = path
            .parent()
            .ok_or_else(|| invalid("missing file parent"))?;
        if directory.as_os_str().is_empty() {
            return Ok(());
        }
        let mut parent = self.parent.borrow_mut();
        if parent
            .as_ref()
            .is_some_and(|(cached, _)| cached == directory)
        {
            return Ok(());
        }
        let current = self.walk(directory, true)?;
        *parent = Some((directory.to_path_buf(), current));
        Ok(())
    }

    pub(super) fn create_empty_directory(&self, path: &Path) -> io::Result<()> {
        self.create_parent(path)?;
        self.with_parent(path, |parent, name| {
            let name = CString::new(name.as_bytes())?;
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn install_local_clone(&self, path: &Path, scratch: &Self) -> io::Result<()> {
        self.with_parent(path, |parent, name| {
            let name = CString::new(name.as_bytes())?;
            if unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    scratch.directory.as_raw_fd(),
                    c"clone".as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            } == 0
            {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        })
    }

    fn with_parent<T>(
        &self,
        path: &Path,
        use_parent: impl FnOnce(&File, &OsStr) -> io::Result<T>,
    ) -> io::Result<T> {
        let name = path
            .file_name()
            .ok_or_else(|| invalid("missing file name"))?;
        let directory = path
            .parent()
            .ok_or_else(|| invalid("missing file parent"))?;
        if directory.as_os_str().is_empty() {
            return use_parent(&self.directory, name);
        }
        let mut parent = self.parent.borrow_mut();
        if parent
            .as_ref()
            .is_none_or(|(cached, _)| cached != directory)
        {
            *parent = Some((directory.to_path_buf(), self.subdirectory(directory)?));
        }
        use_parent(&parent.as_ref().expect("pinned parent").1, name)
    }

    pub(super) fn nested(&self, path: &Path) -> io::Result<Self> {
        Ok(Self {
            directory: self.subdirectory(path)?,
            device: self.device,
            parent: RefCell::new(None),
            #[cfg(target_os = "macos")]
            ancestors: None,
        })
    }

    fn subdirectory(&self, path: &Path) -> io::Result<File> {
        self.walk(path, false)
    }

    fn walk(&self, path: &Path, create: bool) -> io::Result<File> {
        #[cfg(target_os = "macos")]
        let prefix = self
            .ancestors
            .as_ref()
            .map(|cache| {
                cache
                    .borrow_mut()
                    .prefix(&self.directory, self.device, path)
            })
            .transpose()?
            .flatten();
        #[cfg(not(target_os = "macos"))]
        let prefix: Option<(File, &Path)> = None;
        let (start, path) = prefix
            .as_ref()
            .map_or((&self.directory, path), |(fd, rest)| (fd, *rest));
        let mut directory = None;
        for part in path.components() {
            let Component::Normal(component) = part else {
                return Err(invalid("unexpected relative path"));
            };
            let parent = directory.as_ref().unwrap_or(start);
            let flags = libc::O_RDONLY | libc::O_DIRECTORY;
            let child = match open_at(parent, component, flags) {
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    let name = CString::new(component.as_bytes())?;
                    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o777) } != 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(error);
                        }
                    }
                    open_at(parent, component, flags)?
                }
                result => result?,
            };
            if child.metadata()?.dev() != self.device {
                return Err(invalid(if create {
                    "mounted destination directory"
                } else {
                    "mounted donor directory"
                }));
            }
            directory = Some(child);
        }
        match directory {
            Some(directory) => Ok(directory),
            None => start.try_clone(),
        }
    }

    pub(crate) fn names(&self, path: &Path) -> io::Result<Vec<OsString>> {
        // Use a new open description: duplicating the root descriptor would
        // share its directory iteration offset between validations.
        let fresh = if path.as_os_str().is_empty() {
            open_at(
                &self.directory,
                OsStr::new("."),
                libc::O_RDONLY | libc::O_DIRECTORY,
            )?
        } else {
            self.subdirectory(path)?
        };
        list(fresh)
    }

    /// Clone the donor directory at `path` to the same relative path under
    /// this root with one APFS call. The destination must not exist; symlinks
    /// are cloned as links and the clone is owned by the caller.
    #[cfg(target_os = "macos")]
    pub(super) fn clone_directory(&self, path: &Path, donor: &Self) -> io::Result<()> {
        const CLONE_NOFOLLOW: u32 = 0x0001;
        const CLONE_NOOWNERCOPY: u32 = 0x0002;
        donor.with_parent(path, |source, name| {
            let name = CString::new(name.as_bytes())?;
            let info = stat_at(source, &name)?;
            if info.st_mode & libc::S_IFMT != libc::S_IFDIR || info.st_dev as u64 != donor.device {
                return Err(invalid("expected a donor directory"));
            }
            self.with_parent(path, |parent, _| {
                if unsafe {
                    libc::clonefileat(
                        source.as_raw_fd(),
                        name.as_ptr(),
                        parent.as_raw_fd(),
                        name.as_ptr(),
                        CLONE_NOFOLLOW | CLONE_NOOWNERCOPY,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })
        })
    }

    /// `lstat` a symbolic link on this root's device.
    #[cfg(target_os = "macos")]
    pub(super) fn link_stat(&self, path: &Path) -> io::Result<libc::stat> {
        self.with_parent(path, |parent, name| {
            let info = stat_at(parent, &CString::new(name.as_bytes())?)?;
            if info.st_mode & libc::S_IFMT != libc::S_IFLNK || info.st_dev as u64 != self.device {
                return Err(invalid("expected a symbolic link"));
            }
            Ok(info)
        })
    }

    /// Remove a subtree this process created, through pinned descriptors
    /// that never follow symlinks.
    #[cfg(target_os = "macos")]
    pub(super) fn discard_tree(&self, path: &Path) -> io::Result<()> {
        self.with_parent(path, |parent, name| {
            remove_tree_at(parent, &CString::new(name.as_bytes())?)
        })
    }
}

#[cfg(target_os = "macos")]
fn remove_tree_at(parent: &File, name: &CStr) -> io::Result<()> {
    let info = stat_at(parent, name)?;
    if info.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return unlink_at(parent, name);
    }
    let name_os = OsStr::from_bytes(name.to_bytes());
    let directory = open_at(parent, name_os, libc::O_RDONLY | libc::O_DIRECTORY)?;
    for child in list(open_at(
        parent,
        name_os,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?)? {
        remove_tree_at(&directory, &CString::new(child.into_vec())?)?;
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// List a directory's entries, excluding `.` and `..`. Consumes the descriptor.
fn list(fresh: File) -> io::Result<Vec<OsString>> {
    let fd = fresh.into_raw_fd();
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let mut names = Vec::new();
    let result = loop {
        #[cfg(target_os = "macos")]
        let errno = unsafe { libc::__error() };
        #[cfg(target_os = "linux")]
        let errno = unsafe { libc::__errno_location() };
        unsafe { *errno = 0 };
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break if unsafe { *errno } == 0 {
                Ok(names)
            } else {
                Err(io::Error::last_os_error())
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(OsString::from_vec(name.to_vec()));
        }
    };
    unsafe { libc::closedir(stream) };
    result
}

impl Root {
    /// Classify a tree entry relative to a pinned directory without opening
    /// special files or following symlinks.
    pub(crate) fn entry_is_directory(&self, path: &Path) -> io::Result<bool> {
        self.with_parent(path, |parent, name| {
            let info = stat_at(parent, &CString::new(name.as_bytes())?)?;
            let kind = info.st_mode & libc::S_IFMT;
            if kind == libc::S_IFLNK {
                return Ok(false);
            }
            #[cfg(target_os = "macos")]
            let device = info.st_dev as u64;
            #[cfg(target_os = "linux")]
            let device = info.st_dev;
            if device != self.device {
                return Err(invalid("mounted entry"));
            }
            match kind {
                libc::S_IFDIR => Ok(true),
                libc::S_IFREG => Ok(false),
                _ => Err(invalid("unexpected filesystem entry")),
            }
        })
    }

    pub(super) fn regular(&self, path: &Path) -> io::Result<File> {
        self.regular_metadata(path).map(|(file, _)| file)
    }

    pub(super) fn regular_metadata(&self, path: &Path) -> io::Result<(File, Metadata)> {
        self.with_parent(path, |parent, name| {
            regular_at(parent, &CString::new(name.as_bytes())?, self.device)
        })
    }

    pub(super) fn regular_size(&self, path: &Path) -> io::Result<u64> {
        u64::try_from(self.regular_stat(path)?.st_size)
            .map_err(|_| invalid("invalid regular file size"))
    }

    pub(super) fn regular_stat(&self, path: &Path) -> io::Result<libc::stat> {
        self.with_parent(path, |parent, name| {
            let info = stat_regular_at(parent, &CString::new(name.as_bytes())?)?;
            #[cfg(target_os = "macos")]
            let device = info.st_dev as u64;
            #[cfg(target_os = "linux")]
            let device = info.st_dev;
            if info.st_nlink != 1 || device != self.device {
                return Err(invalid("expected an independent regular file"));
            }
            Ok(info)
        })
    }

    pub(super) fn open_primary_clone(
        &self,
        path: &Path,
        mode: u32,
    ) -> io::Result<(File, Metadata)> {
        // The pinned file is used for both metadata and content validation.
        let (file, metadata) = self.regular_metadata(path)?;
        if !plain(&file, &metadata)? {
            return Err(invalid("cloned file has extended attributes or flags"));
        }
        #[cfg(target_os = "macos")]
        if metadata.mode() & 0o7777 != mode {
            file.set_permissions(fs::Permissions::from_mode(mode))?;
            let permissions_applied = file.metadata()?;
            return Ok((file, permissions_applied));
        }
        #[cfg(target_os = "linux")]
        let _ = mode; // Linux applies the Git mode when creating the clone.
        Ok((file, metadata))
    }

    pub(super) fn link(&self, path: &Path) -> io::Result<OsString> {
        self.with_parent(path, |parent, name| {
            let name = CString::new(name.as_bytes())?;
            let mut bytes = vec![0; 4096];
            let size = unsafe {
                libc::readlinkat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            let size = usize::try_from(size).map_err(|_| io::Error::last_os_error())?;
            if size == bytes.len() {
                return Err(invalid("symbolic link target is too long"));
            }
            bytes.truncate(size);
            Ok(OsString::from_vec(bytes))
        })
    }

    pub(super) fn clone_regular(
        &self,
        path: &Path,
        source: &File,
        metadata: &Metadata,
        mode: u32,
        authenticated: bool,
    ) -> io::Result<()> {
        self.with_parent(path, |parent, name| {
            clone_at(
                parent,
                &CString::new(name.as_bytes())?,
                source,
                metadata,
                mode,
                authenticated,
            )
        })
    }

    pub(super) fn discard(&self, path: &Path) -> io::Result<()> {
        self.with_parent(path, |parent, name| {
            unlink_at(parent, &CString::new(name.as_bytes())?)
        })
    }
}

fn stat_at(parent: &File, name: &CStr) -> io::Result<libc::stat> {
    let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.assume_init() })
}

fn stat_regular_at(parent: &File, name: &CStr) -> io::Result<libc::stat> {
    let info = stat_at(parent, name)?;
    if info.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(invalid("expected a regular file"));
    }
    Ok(info)
}

fn regular_at(parent: &File, name: &CStr, device: u64) -> io::Result<(File, Metadata)> {
    // O_NONBLOCK makes opening a FIFO harmless even if it replaces a file.
    // fstatat avoids opening special files in the normal (non-racing) case.
    stat_regular_at(parent, name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.dev() != device {
        return Err(invalid("expected an independent regular file"));
    }
    Ok((file, metadata))
}

pub(super) fn plain(file: &File, metadata: &Metadata) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt as _;
        if metadata.st_flags() != 0 {
            return Ok(false);
        }
        // APFS copies xattrs and forks. macOS assigns the protected
        // provenance attribute itself. Reject any user-controlled metadata.
        let mut names = [0; 128];
        let length = unsafe {
            libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len(), 0)
        };
        if length < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ERANGE) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        Ok(length == 0 || names.get(..length as usize) == Some(b"com.apple.provenance\0"))
    }
    #[cfg(target_os = "linux")]
    {
        // FICLONE does not transfer xattrs, flags or ownership.
        let _ = (file, metadata);
        Ok(true)
    }
}

fn unlink_at(parent: &File, name: &CStr) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd >= 0 {
            let cloned = unsafe { File::from_raw_fd(fd) };
            unsafe { libc::fchflags(cloned.as_raw_fd(), 0) };
        }
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn clone_at(
    parent: &File,
    name: &CStr,
    source: &File,
    metadata: &Metadata,
    mode: u32,
    authenticated: bool,
) -> io::Result<()> {
    if !authenticated && !plain(source, metadata)? {
        return Err(invalid("donor has extended attributes or flags"));
    }
    const NO_OWNER_COPY: u32 = 0x0002;
    if unsafe {
        libc::fclonefileat(
            source.as_raw_fd(),
            parent.as_raw_fd(),
            name.as_ptr(),
            NO_OWNER_COPY,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // Primary clones are validated before hashing their bytes. The caller
    // checks a signed inode's stamp again after cloning it.
    if !authenticated || metadata.mode() & 0o7777 == mode {
        return Ok(());
    }
    let result = (|| {
        let (cloned, copied) = regular_at(parent, name, metadata.dev())?;
        if copied.mode() & 0o7777 != mode {
            cloned.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    })();
    if result.is_err() {
        unlink_at(parent, name)?;
    }
    result
}

#[cfg(target_os = "linux")]
fn clone_at(
    parent: &File,
    name: &CStr,
    source: &File,
    _metadata: &Metadata,
    mode: u32,
    _authenticated: bool,
) -> io::Result<()> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let dest = unsafe { File::from_raw_fd(fd) };
    // FICLONE clones the data of the pinned inode into a fresh file.
    if unsafe { libc::ioctl(dest.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    unlink_at(parent, name)?;
    Err(error)
}

pub(super) fn creation_mode(directory: &Path) -> io::Result<u32> {
    let temp = tempfile::tempdir_in(directory)?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o777)
        .open(temp.path().join("mode"))?;
    Ok(file.metadata()?.mode() & 0o777)
}

pub(super) fn rename_new(source: &Path, destination: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
