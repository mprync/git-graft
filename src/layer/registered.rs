//! Private, immutable generations of caller-attested package installs.
//! The inputs and compatibility key describe the install. The caller
//! attests to reproducibility and any settings outside the tracked inputs.

use crate::{package_manager::InstallSpec, registry::Project};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(crate) const REGISTERED_INSTALL_DIR: &str = "registered-installs";

fn status(spec: &InstallSpec, status: &str, reason: &str) -> Value {
    json!({"manager": spec.manager, "status": status, "reason": reason})
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn seed_registered(_: &Project, checkout: &Path, spec: &InstallSpec) -> Result<Value> {
    crate::package_manager::validate_relative(&spec.directory)?;
    crate::registry::owned_directory(checkout)?;
    let mut directory = checkout.to_path_buf();
    for name in spec.directory.split('/') {
        directory.push(name);
        crate::registry::check_unmounted_directory(&directory, false)?;
    }
    Ok(status(spec, "unavailable", "cow_unavailable"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn attach_registered(_: &Project, _: &Path, spec: &InstallSpec) -> Result<Value> {
    Ok(status(spec, "unavailable", "cow_unavailable"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn registered_install_root(p: &Project) -> Result<Option<PathBuf>> {
    anyhow::ensure!(
        !crate::registry::exists(&p.state.join(REGISTERED_INSTALL_DIR))?,
        "registered install store cannot be verified on this platform"
    );
    Ok(None)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn verify_registered_install_generation(
    _: &Project,
    _: &Path,
    _: &str,
) -> Result<Value> {
    anyhow::bail!("registered install generations cannot be verified on this platform")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use native::{
    attach_registered, registered_install_root, seed_registered,
    verify_registered_install_generation,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod native {
    use super::*;
    use crate::{api::Fault, cache, git, package_manager::validate_relative, registry};
    use anyhow::{Context, bail, ensure};
    use serde::{Deserialize, Serialize};
    #[cfg(target_os = "linux")]
    use std::fs::OpenOptions;
    use std::{
        ffi::{CStr, CString, OsStr, OsString},
        fs::{self, File, Metadata},
        io::{self, BufRead, BufReader, Read, Seek, Write},
        os::{
            fd::{AsRawFd, FromRawFd, IntoRawFd},
            unix::{
                ffi::{OsStrExt, OsStringExt},
                fs::{MetadataExt, PermissionsExt, symlink},
            },
        },
        path::Component,
        process::Stdio,
    };

    use super::super::{
        Transfer, check_cache_metadata, clone_unsupported, entry_exists, field, inspect,
        layer_provenance_for, transfer_file, valid_digest,
    };

    const TREE: &str = "tree";

    /// A pinned directory on the checkout's filesystem. Open descendants one
    /// name at a time, so a replaced ancestor cannot redirect a transfer.
    struct Dir(File);

    impl Dir {
        fn absolute(path: &Path) -> Result<Self> {
            ensure!(path.is_absolute(), "expected an absolute directory");
            let mut dir = Self(File::open("/")?);
            for part in path.components() {
                match part {
                    Component::RootDir => (),
                    Component::Normal(name) => {
                        dir = Self(dir.open(name, libc::O_RDONLY | libc::O_DIRECTORY)?);
                    }
                    _ => bail!("invalid absolute directory"),
                }
            }
            Ok(dir)
        }

        fn open(&self, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
            let name = CString::new(name.as_bytes())?;
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { File::from_raw_fd(fd) })
        }

        fn child(&self, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
            let child = self.open(name, flags)?;
            if mount(&self.0)? != mount(&child)? {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "mounted entry"));
            }
            Ok(child)
        }

        fn path(&self, path: &Path) -> Result<Self> {
            let mut dir = Self(self.0.try_clone()?);
            for part in path.components() {
                let Component::Normal(name) = part else {
                    bail!("invalid relative directory");
                };
                dir = Self(dir.child(name, libc::O_RDONLY | libc::O_DIRECTORY)?);
            }
            Ok(dir)
        }

        fn is_current(&self, other: &Self) -> Result<bool> {
            let a = self.0.metadata()?;
            let b = other.0.metadata()?;
            Ok(a.dev() == b.dev() && a.ino() == b.ino() && mount(&self.0)? == mount(&other.0)?)
        }

        fn entry(&self, name: &OsStr) -> io::Result<libc::stat> {
            let name = CString::new(name.as_bytes())?;
            let mut output = std::mem::MaybeUninit::uninit();
            if unsafe {
                libc::fstatat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    output.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { output.assume_init() })
        }

        fn link(&self, name: &OsStr) -> io::Result<OsString> {
            let name = CString::new(name.as_bytes())?;
            let mut bytes = vec![0; 4096];
            let len = unsafe {
                libc::readlinkat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            let len = usize::try_from(len).map_err(|_| io::Error::last_os_error())?;
            if len == bytes.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "long symlink"));
            }
            bytes.truncate(len);
            Ok(OsString::from_vec(bytes))
        }

        fn publish(&self, source: &Path, name: &OsStr) -> io::Result<()> {
            let source = CString::new(source.as_os_str().as_bytes())?;
            let name = CString::new(name.as_bytes())?;
            #[cfg(target_os = "linux")]
            let result = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    source.as_ptr(),
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            #[cfg(target_os = "macos")]
            let result = unsafe {
                libc::renameatx_np(
                    libc::AT_FDCWD,
                    source.as_ptr(),
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    #[cfg(target_os = "linux")]
    fn mount(file: &File) -> io::Result<u64> {
        let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
        // The mount ID distinguishes bind mounts even when st_dev matches.
        if unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                libc::STATX_MNT_ID,
                stat.as_mut_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        if stat.stx_mask & libc::STATX_MNT_ID == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "mount ID unavailable",
            ));
        }
        Ok(stat.stx_mnt_id)
    }

    #[cfg(target_os = "macos")]
    fn mount(file: &File) -> io::Result<u64> {
        Ok(file.metadata()?.dev())
    }

    fn checkout_dir(checkout: &Path) -> Result<Dir> {
        let parent = Dir::absolute(checkout.parent().context("missing checkout parent")?)?;
        Ok(Dir(parent.child(
            checkout.file_name().context("missing checkout name")?,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )?))
    }

    pub(crate) fn registered_install_root(p: &Project) -> Result<Option<PathBuf>> {
        let root = p.state.join(REGISTERED_INSTALL_DIR);
        if !entry_exists(&root)? {
            return Ok(None);
        }
        p.require_private_state()?;
        let metadata = registry::check_unmounted_directory(&root, false)?;
        check_cache_metadata(&root, &metadata)?;
        ensure!(metadata.mode() & 0o077 == 0, "install store is not private");
        let owner = root.join(registry::CACHE_OWNER);
        let metadata = fs::symlink_metadata(&owner)?;
        ensure!(
            metadata.is_file() && metadata.len() == p.id().len() as u64,
            "invalid install store owner"
        );
        check_cache_metadata(&owner, &metadata)?;
        p.check_cache_owner(&root)?;
        Ok(Some(root))
    }

    fn install_root(p: &Project) -> Result<PathBuf> {
        if let Some(root) = registered_install_root(p)? {
            return Ok(root);
        }
        p.require_private_state()?;
        let stage = private_stage(&p.state, ".install-store-")?;
        registry::atomic(&stage.path().join(registry::CACHE_OWNER), p.id().as_bytes())?;
        let root = p.state.join(REGISTERED_INSTALL_DIR);
        match crate::recovery::rename_exclusive(stage.path(), &root) {
            Ok(()) => {
                let _ = stage.keep();
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e).context("publish private install store"),
        }
        registered_install_root(p)?.context("missing private install store")
    }

    fn private_stage(root: &Path, prefix: &str) -> Result<tempfile::TempDir> {
        Ok(tempfile::Builder::new()
            .prefix(prefix)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(root)?)
    }

    #[derive(PartialEq, Eq)]
    struct Inputs {
        head: String,
        digest: String,
    }

    impl Inputs {
        fn read(checkout: &Path, root: &Dir, spec: &InstallSpec) -> Result<Self> {
            validate_relative(&spec.directory)?;
            ensure!(
                !spec.key.trim().is_empty() && !spec.inputs.is_empty(),
                "a toolchain key and committed install inputs are required"
            );
            let mut paths = spec.inputs.iter().map(String::as_str).collect::<Vec<_>>();
            paths.sort_unstable();
            ensure!(
                !paths.windows(2).any(|pair| pair[0] == pair[1]),
                "duplicate install input"
            );
            for path in &paths {
                validate_relative(path)?;
            }
            let top = git::text(checkout, &["rev-parse", "--show-toplevel"])?;
            ensure!(
                dunce::canonicalize(top)? == checkout,
                "expected the checkout root"
            );
            let head = git::oid(checkout, "HEAD")?;
            let tree = git::bytes(
                checkout,
                &[
                    "--no-replace-objects",
                    "ls-tree",
                    "-r",
                    "-z",
                    "--full-tree",
                    &head,
                ],
            )?;
            let index = git::bytes(checkout, &["ls-files", "--stage", "-v", "-z"])?;
            let committed = input_entries(&tree, &paths, spec.directory.as_bytes(), false)?;
            let staged = input_entries(&index, &paths, spec.directory.as_bytes(), true)?;
            ensure!(
                committed == staged,
                "install inputs must match the committed index"
            );

            let mut digest = blake3::Hasher::new();
            for value in [
                b"git-graft-registered-install-v1".as_slice(),
                std::env::consts::OS.as_bytes(),
                std::env::consts::ARCH.as_bytes(),
                spec.manager.as_bytes(),
                spec.key.as_bytes(),
                spec.directory.as_bytes(),
            ] {
                field(&mut digest, value);
            }
            hash_committed_inputs(checkout, root, &paths, &committed, &mut digest)?;
            Ok(Self {
                head,
                digest: digest.finalize().to_hex().to_string(),
            })
        }
    }

    fn overlaps(path: &[u8], directory: &[u8]) -> bool {
        fn prefix(path: &[u8], candidate: &[u8]) -> bool {
            path.get(..candidate.len())
                .is_some_and(|name| name.eq_ignore_ascii_case(candidate))
                && (path.len() == candidate.len() || path.get(candidate.len()) == Some(&b'/'))
        }
        if prefix(path, directory) || prefix(directory, path) {
            return true;
        }
        if path.is_ascii() && directory.is_ascii() {
            return false;
        }
        let (Ok(path), Ok(directory)) = (std::str::from_utf8(path), std::str::from_utf8(directory))
        else {
            return false;
        };
        let path = path.to_lowercase();
        let directory = directory.to_lowercase();
        prefix(path.as_bytes(), directory.as_bytes())
            || prefix(directory.as_bytes(), path.as_bytes())
    }

    fn input_entries<'a>(
        listing: &'a [u8],
        inputs: &[&str],
        directory: &[u8],
        index: bool,
    ) -> Result<Vec<(&'a str, &'a str)>> {
        let mut result = vec![None; inputs.len()];
        for record in listing.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .context("invalid Git listing")?;
            let path = &record[tab + 1..];
            ensure!(
                !overlaps(path, directory),
                "install directory overlaps a tracked path"
            );
            let Ok(position) = inputs.binary_search_by(|value| value.as_bytes().cmp(path)) else {
                continue;
            };
            let mut fields = std::str::from_utf8(&record[..tab])?.split(' ');
            if index {
                ensure!(
                    fields.next() == Some("H"),
                    "install input is hidden or unmerged"
                );
            }
            let mode = fields.next().context("missing input mode")?;
            ensure!(
                matches!(mode, "100644" | "100755"),
                "install input must be a regular file"
            );
            if !index {
                ensure!(fields.next() == Some("blob"), "install input is not a blob");
            }
            let oid = git::checked_oid(fields.next().context("missing input object")?)?;
            if index {
                ensure!(fields.next() == Some("0"), "install input is unmerged");
            }
            ensure!(
                result[position].replace((mode, oid)).is_none(),
                "duplicate Git path"
            );
        }
        result
            .into_iter()
            .map(|entry| entry.context("install input must be committed and tracked"))
            .collect()
    }

    fn hash_committed_inputs(
        checkout: &Path,
        root: &Dir,
        paths: &[&str],
        committed: &[(&str, &str)],
        digest: &mut blake3::Hasher,
    ) -> Result<()> {
        let filemode = git::raw(checkout, &["config", "--bool", "--get", "core.filemode"])?;
        let trust_mode = filemode.stdout != b"false\n";
        let mut requests = tempfile::tempfile()?;
        for (_, oid) in committed {
            writeln!(requests, "{oid}")?;
        }
        requests.rewind()?;
        // Compare the exact blobs without checkout filters or Git status (which
        // can execute filters). Stream them to bound memory regardless of size.
        let mut child = git::command(checkout)
            .args(["--no-replace-objects", "cat-file", "--batch"])
            .stdin(Stdio::from(requests))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut blobs = BufReader::new(child.stdout.take().expect("piped Git output"));
        let result = (|| {
            let mut actual = [0; 65536];
            let mut expected = [0; 65536];
            let mut header = String::new();
            for (&path, &(mode, oid)) in paths.iter().zip(committed) {
                let path = Path::new(path);
                let parent = root.path(path.parent().context("missing input parent")?)?;
                let name = path.file_name().context("missing input filename")?;
                ensure!(
                    parent.entry(name)?.st_mode & libc::S_IFMT == libc::S_IFREG,
                    "install input must be a regular file"
                );
                let mut file = parent.child(name, libc::O_RDONLY | libc::O_NONBLOCK)?;
                let meta = file.metadata()?;
                ensure!(
                    meta.is_file() && meta.nlink() == 1,
                    "install input is not an independent regular file"
                );
                ensure!(
                    !trust_mode || (meta.mode() & 0o100 != 0) == (mode == "100755"),
                    "install input mode differs from the committed file"
                );
                header.clear();
                blobs.read_line(&mut header)?;
                let length: u64 = header
                    .strip_prefix(oid)
                    .and_then(|line| line.strip_prefix(" blob "))
                    .and_then(|line| line.strip_suffix('\n'))
                    .context("invalid committed Git blob")?
                    .parse()?;
                ensure!(
                    length == meta.len(),
                    "install input differs from its committed file"
                );
                field(digest, path.as_os_str().as_bytes());
                field(digest, mode.as_bytes());
                digest.update(&length.to_le_bytes());
                let mut remaining = length;
                while remaining > 0 {
                    let count = remaining.min(actual.len() as u64) as usize;
                    blobs.read_exact(&mut expected[..count])?;
                    file.read_exact(&mut actual[..count])?;
                    ensure!(
                        actual[..count] == expected[..count],
                        "install input differs from its committed file"
                    );
                    digest.update(&expected[..count]);
                    remaining -= count as u64;
                }
                ensure!(
                    file.read(&mut actual[..1])? == 0,
                    "install input changed during read"
                );
                blobs.read_exact(&mut expected[..1])?;
                ensure!(
                    expected[0] == b'\n',
                    "invalid committed Git blob terminator"
                );
            }
            ensure!(blobs.fill_buf()?.is_empty(), "unexpected Git blob output");
            Ok(())
        })();
        drop(blobs);
        let exit = child.wait()?;
        result?;
        ensure!(exit.success(), "could not read committed install inputs");
        Ok(())
    }

    fn receipt(spec: &InstallSpec, transfer: &Transfer, action: &str, digest: &str) -> Value {
        let mut receipt = transfer.receipt(action, digest, "copy-on-write");
        receipt["manager"] = json!(spec.manager);
        receipt["directory"] = json!(spec.directory);
        receipt["cloned_files"] = json!(transfer.cloned_files);
        receipt["status"] = json!(if action == "save" {
            "registered"
        } else {
            "attached"
        });
        receipt
    }

    fn entry_stamp(meta: &Metadata, hash: &mut blake3::Hasher) {
        for value in [meta.dev(), meta.ino(), meta.len(), u64::from(meta.mode())] {
            field(hash, &value.to_le_bytes());
        }
        for value in [
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        ] {
            field(hash, &value.to_le_bytes());
        }
    }

    fn names(dir: &Dir) -> io::Result<Vec<OsString>> {
        let fd = dir
            .open(OsStr::new("."), libc::O_RDONLY | libc::O_DIRECTORY)?
            .into_raw_fd();
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
                    Ok(())
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
        result?;
        names.sort_unstable();
        Ok(names)
    }

    #[cfg(target_os = "macos")]
    fn clone_open(source: &File, destination: &Path, _: &Metadata) -> io::Result<()> {
        let path = CString::new(destination.as_os_str().as_bytes())?;
        const CLONE_NO_OWNER_COPY: u32 = 0x0002;
        if unsafe {
            libc::fclonefileat(
                source.as_raw_fd(),
                libc::AT_FDCWD,
                path.as_ptr(),
                CLONE_NO_OWNER_COPY,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn clone_open(source: &File, destination: &Path, metadata: &Metadata) -> io::Result<()> {
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        if unsafe { libc::ioctl(output.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        output.set_permissions(metadata.permissions())
    }

    fn capture(
        source: &Dir,
        path: &Path,
        destination: Option<&Path>,
        transfer: &mut Transfer,
        hash: &mut blake3::Hasher,
        depth: usize,
    ) -> Result<()> {
        ensure!(depth < 128, "install tree is too deeply nested");
        let meta = source.0.metadata()?;
        check_cache_metadata(path, &meta)?;
        entry_stamp(&meta, hash);
        let entries = names(source)?;
        let has = |expected: &str| {
            entries
                .iter()
                .any(|name| name.eq_ignore_ascii_case(expected))
        };
        ensure!(
            !(has(".git") || has("HEAD") && has("objects") && (has("refs") || has("reftable"))),
            "nested Git metadata in installed tree"
        );
        for name in entries {
            field(hash, name.as_bytes());
            let entry = source.entry(&name)?;
            let child = path.join(&name);
            let target = destination.map(|parent| parent.join(&name));
            match entry.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {
                    field(hash, b"directory");
                    let dir = Dir(source.child(&name, libc::O_RDONLY | libc::O_DIRECTORY)?);
                    if let Some(target) = &target {
                        fs::create_dir(target)?;
                    }
                    capture(&dir, &child, target.as_deref(), transfer, hash, depth + 1)?;
                }
                libc::S_IFREG => {
                    field(hash, b"regular");
                    let file = source.child(&name, libc::O_RDONLY | libc::O_NONBLOCK)?;
                    let meta = file.metadata()?;
                    ensure!(
                        meta.is_file() && meta.ino() == entry.st_ino,
                        "install file changed during capture"
                    );
                    check_cache_metadata(&child, &meta)?;
                    entry_stamp(&meta, hash);
                    if let Some(target) = &target {
                        transfer_file(&child, target, meta.len(), false, transfer, |_, target| {
                            clone_open(&file, target, &meta)
                        })?;
                    }
                }
                libc::S_IFLNK => {
                    field(hash, b"symlink");
                    let meta = fs::symlink_metadata(&child)?;
                    ensure!(
                        meta.is_symlink() && meta.ino() == entry.st_ino,
                        "install symlink changed during capture"
                    );
                    check_cache_metadata(&child, &meta)?;
                    entry_stamp(&meta, hash);
                    let link = source.link(&name)?;
                    field(hash, link.as_bytes());
                    if let Some(target) = &target {
                        symlink(link, target)?;
                    }
                }
                _ => bail!("special file in installed tree"),
            }
        }
        if let Some(destination) = destination {
            fs::set_permissions(destination, meta.permissions())?;
        }
        Ok(())
    }

    fn snapshot(
        source: &Dir,
        path: &Path,
        dest: Option<&Path>,
        transfer: &mut Transfer,
    ) -> Result<String> {
        let mut hash = blake3::Hasher::new();
        capture(source, path, dest, transfer, &mut hash, 0)?;
        Ok(hash.finalize().to_hex().to_string())
    }

    fn verify_tree(path: &Path, forbidden: Option<&[u8]>) -> Result<String> {
        registry::check_owned_tree(path)?;
        inspect(path, forbidden, check_cache_metadata)
    }

    #[derive(PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct GenerationBinding {
        version: u8,
        project: String,
        digest: String,
        contents: String,
        stamp: String,
        identity: cache::DirectoryIdentity,
    }

    fn binding_path(p: &Project, digest: &str) -> PathBuf {
        p.state.join(format!("registered-install-{digest}-v1.json"))
    }

    fn read_binding(p: &Project, digest: &str) -> Result<GenerationBinding> {
        ensure!(valid_digest(digest), "invalid registered install digest");
        let bytes = cache::read_private_file(&binding_path(p, digest), 1024)
            .context("registered install ownership is unverified")?;
        let binding: GenerationBinding = serde_json::from_slice(&bytes)?;
        ensure!(
            binding.version == 1
                && binding.project == p.id()
                && binding.digest == digest
                && valid_digest(&binding.contents)
                && valid_digest(&binding.stamp)
                && serde_json::to_vec(&binding)? == bytes,
            "invalid registered install binding"
        );
        Ok(binding)
    }

    fn generation_record(
        p: &Project,
        generation: &Path,
        digest: &str,
    ) -> Result<GenerationBinding> {
        registry::check_unmounted_directory(generation, false)?;
        let value = layer_provenance_for(generation, digest, TREE)?;
        let binding = read_binding(p, digest)?;
        ensure!(
            value["version"] == 1
                && value["backend"] == "copy-on-write"
                && value["contents"] == binding.contents
                && cache::directory_identity(generation)? == binding.identity,
            "invalid registered install generation"
        );
        Ok(binding)
    }

    fn tree_stamp(generation: &Path) -> Result<String> {
        let tree = Dir::absolute(generation)?.path(Path::new(TREE))?;
        snapshot(
            &tree,
            &generation.join(TREE),
            None,
            &mut Transfer::default(),
        )
    }

    fn verified_generation(
        p: &Project,
        generation: &Path,
        digest: &str,
    ) -> Result<GenerationBinding> {
        let binding = generation_record(p, generation, digest)?;
        ensure!(
            verify_tree(&generation.join(TREE), None)? == binding.contents
                && tree_stamp(generation)? == binding.stamp,
            "registered install tree differs from its original generation"
        );
        Ok(binding)
    }

    /// Use the original binding even if GC has moved this directory into quarantine.
    pub(crate) fn verify_registered_install_generation(
        p: &Project,
        generation: &Path,
        digest: &str,
    ) -> Result<Value> {
        Ok(serde_json::to_value(verified_generation(
            p, generation, digest,
        )?)?)
    }

    fn probe_if_empty(root: &Path, transfer: &Transfer) -> Result<()> {
        if transfer.cloned_files == 0 {
            let source = tempfile::NamedTempFile::new_in(root)?;
            let stage = tempfile::tempdir_in(root)?;
            transfer_file(
                source.path(),
                &stage.path().join("probe"),
                0,
                false,
                &mut Transfer::default(),
                crate::storage::clone_file,
            )?;
        }
        Ok(())
    }

    /// Captures a generation under the caller's project and checkout locks.
    pub(crate) fn seed_registered(
        p: &Project,
        checkout: &Path,
        spec: &InstallSpec,
    ) -> Result<Value> {
        match seed(p, checkout, spec) {
            Err(error) if cow_unavailable(&error) => {
                Ok(status(spec, "unavailable", "cow_unavailable"))
            }
            result => result,
        }
    }

    fn seed(p: &Project, checkout: &Path, spec: &InstallSpec) -> Result<Value> {
        let checkout_root = checkout_dir(checkout)?;
        let before = Inputs::read(checkout, &checkout_root, spec)?;
        let source = checkout_root.path(Path::new(&spec.directory))?;
        let root = install_root(p)?;
        let root_identity = cache::directory_identity(&root)?;
        let src = checkout.join(&spec.directory);
        let stage = private_stage(&root, ".building-")?;
        let tree = stage.path().join(TREE);
        fs::create_dir(&tree)?;
        ensure!(
            mount(&source.0)? == mount(&File::open(&tree)?)?,
            crate::api::fault(
                "cow_unavailable",
                "native cloning across mounts is unavailable",
                Value::Null
            )
        );
        let mut transfer = Transfer::default();
        let installed = snapshot(&source, &src, Some(&tree), &mut transfer)?;
        probe_if_empty(&root, &transfer)?;
        let contents = verify_tree(&tree, Some(git::path_str(checkout)?.as_bytes()))?;
        if checkout != p.repo {
            verify_tree(&tree, Some(git::path_str(&p.repo)?.as_bytes()))?;
        }

        let generation = root.join(&before.digest);
        let value = json!({"version": 1, "compatibility": before.digest, "contents": contents,
            "backend": "copy-on-write"});
        let binding = GenerationBinding {
            version: 1,
            project: p.id().to_owned(),
            digest: before.digest.clone(),
            contents,
            stamp: tree_stamp(stage.path())?,
            identity: cache::directory_identity(stage.path())?,
        };
        registry::atomic(
            &stage.path().join("provenance.json"),
            &serde_json::to_vec(&value)?,
        )?;
        let current = checkout_dir(checkout)?;
        ensure!(
            current.is_current(&checkout_root)?
                && snapshot(
                    &current.path(Path::new(&spec.directory))?,
                    &src,
                    None,
                    &mut Transfer::default()
                )? == installed
                && Inputs::read(checkout, &current, spec)? == before,
            "checkout or installed files changed during capture"
        );
        ensure!(
            registered_install_root(p)?.as_deref() == Some(root.as_path())
                && cache::directory_identity(&root)? == root_identity,
            "registered install store changed during capture"
        );
        if entry_exists(&generation)? {
            ensure!(
                verified_generation(p, &generation, &before.digest)?.contents == binding.contents,
                "installed tree differs from the original registered generation"
            );
            return Ok(receipt(spec, &Transfer::default(), "save", &before.digest));
        }
        let record_path = binding_path(p, &before.digest);
        if entry_exists(&record_path)? {
            // A known record may outlive a completed purge or interrupted
            // publication. The vacant generation slot can be seeded again.
            read_binding(p, &before.digest)?;
        }
        registry::atomic(&record_path, &serde_json::to_vec(&binding)?)?;
        crate::recovery::rename_exclusive(stage.path(), &generation)
            .context("publish registered generation")?;
        let _ = stage.keep();
        File::open(&root)?.sync_all()?;
        Ok(receipt(spec, &transfer, "save", &before.digest))
    }

    fn cow_unavailable(error: &anyhow::Error) -> bool {
        error
            .downcast_ref::<Fault>()
            .is_some_and(|fault| fault.code == "cow_unavailable")
            || error
                .downcast_ref::<io::Error>()
                .is_some_and(clone_unsupported)
    }

    struct Destination<'a> {
        parent: Dir,
        parent_path: PathBuf,
        name: &'a OsStr,
        deeper: PathBuf,
    }

    /// Pin the nearest existing parent. A whole missing hierarchy is
    /// published as one directory, so no partial destination can escape.
    fn destination<'a>(root: &Dir, path: &'a Path) -> Result<Option<Destination<'a>>> {
        let mut parent = Dir(root.0.try_clone()?);
        let mut parent_path = PathBuf::new();
        let mut parts = path.components().peekable();
        while let Some(Component::Normal(name)) = parts.next() {
            match parent.entry(name) {
                Ok(_) if parts.peek().is_none() => return Ok(None),
                Ok(_) => {
                    parent = Dir(parent.child(name, libc::O_RDONLY | libc::O_DIRECTORY)?);
                    parent_path.push(name);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(Some(Destination {
                        parent,
                        parent_path,
                        name,
                        deeper: parts.collect(),
                    }));
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("invalid destination path")
    }

    /// Attach to a vacant path in an authenticated checkout under its lock.
    pub(crate) fn attach_registered(
        p: &Project,
        checkout: &Path,
        spec: &InstallSpec,
    ) -> Result<Value> {
        let root = match registered_install_root(p) {
            Ok(Some(root)) => root,
            Ok(None) => return Ok(status(spec, "unavailable", "generation_missing")),
            Err(_) => return Ok(status(spec, "unavailable", "invalid_install_store")),
        };
        let Ok(checkout_root) = checkout_dir(checkout) else {
            return Ok(status(spec, "unavailable", "unsafe_checkout"));
        };
        let Ok(before) = Inputs::read(checkout, &checkout_root, spec) else {
            return Ok(status(
                spec,
                "inputs_mismatch",
                "ineligible_checkout_inputs",
            ));
        };
        let destination = match destination(&checkout_root, Path::new(&spec.directory)) {
            Ok(Some(destination)) => destination,
            Ok(None) => return Ok(status(spec, "unavailable", "destination_occupied")),
            Err(_) => return Ok(status(spec, "unavailable", "unsafe_destination")),
        };
        let generation = root.join(&before.digest);
        match entry_exists(&generation) {
            Ok(false) => return Ok(status(spec, "inputs_mismatch", "generation_missing")),
            Ok(true) => (),
            Err(_) => return Ok(status(spec, "unavailable", "invalid_generation")),
        }
        let Ok(binding) = generation_record(p, &generation, &before.digest) else {
            return Ok(status(spec, "unavailable", "invalid_generation"));
        };
        let result = (|| {
            let stage = private_stage(&root, ".attaching-")?;
            if mount(&destination.parent.0)? != mount(&File::open(stage.path())?)? {
                return Ok(status(spec, "unavailable", "cow_unavailable"));
            }
            let first = stage.path().join("install");
            let tree = first.join(&destination.deeper);
            // Joining an empty path can add a trailing slash to the staged name.
            let tree = if destination.deeper.as_os_str().is_empty() {
                first.clone()
            } else {
                tree
            };
            fs::create_dir_all(&tree)?;
            let mut transfer = Transfer::default();
            let cached_path = generation.join(TREE);
            let source = Dir::absolute(&root)?.path(&Path::new(&before.digest).join(TREE))?;
            let stamp = snapshot(&source, &cached_path, Some(&tree), &mut transfer)?;
            probe_if_empty(&root, &transfer)?;
            ensure!(
                stamp == binding.stamp && verify_tree(&tree, None)? == binding.contents,
                "install copy failed integrity verification"
            );
            let current = checkout_dir(checkout)?;
            ensure!(
                current.is_current(&checkout_root)?
                    && Inputs::read(checkout, &current, spec)? == before
                    && generation_record(p, &generation, &before.digest)? == binding
                    && current
                        .path(&destination.parent_path)?
                        .is_current(&destination.parent)?,
                "checkout or destination changed during attachment"
            );
            match destination.parent.publish(&first, destination.name) {
                Ok(()) => Ok(receipt(spec, &transfer, "restore", &before.digest)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    Ok(status(spec, "unavailable", "destination_occupied"))
                }
                Err(error) => Err(error.into()),
            }
        })();
        Ok(match result {
            Ok(receipt) => receipt,
            Err(error) if cow_unavailable(&error) => status(spec, "unavailable", "cow_unavailable"),
            Err(_) => status(spec, "unavailable", "attachment_failed"),
        })
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::git;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, symlink},
        path::PathBuf,
    };

    struct Fixture {
        root: PathBuf,
        project: Project,
        _lock: fs::File,
        _temp: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let root = dunce::canonicalize(temp.path())?;
            let repo = root.join("primary");
            fs::create_dir(&repo)?;
            git::bytes(&repo, &["init", "-q", "--template="])?;
            fs::write(repo.join("fixture.lock"), "version one")?;
            fs::write(repo.join(".gitignore"), "vendor/\n")?;
            git::bytes(&repo, &["add", ".gitignore", "fixture.lock"])?;
            commit(&repo)?;
            let (project, lock) = Project::open(&repo)?;
            Ok(Self {
                root,
                project,
                _lock: lock,
                _temp: temp,
            })
        }

        fn sibling(&self, name: &str) -> Result<PathBuf> {
            let path = self.root.join(name);
            git::bytes(
                &self.root,
                &[
                    "-c",
                    "core.hooksPath=/dev/null",
                    "clone",
                    "-q",
                    "--template=",
                    "--no-hardlinks",
                    git::path_str(&self.project.repo)?,
                    git::path_str(&path)?,
                ],
            )?;
            Ok(path)
        }

        fn spec(&self) -> InstallSpec {
            InstallSpec {
                manager: "fixture".into(),
                key: "fixture ABI".into(),
                directory: "vendor/bundle".into(),
                inputs: vec!["fixture.lock".into()],
            }
        }

        fn write_tree(&self, checkout: &Path, value: &str) -> Result<PathBuf> {
            let tree = checkout.join("vendor/bundle");
            fs::create_dir_all(&tree)?;
            fs::write(tree.join("package"), value)?;
            Ok(tree)
        }
    }

    fn commit(checkout: &Path) -> Result<()> {
        git::bytes(
            checkout,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-q",
                "--no-gpg-sign",
                "-m",
                "fixture",
            ],
        )?;
        Ok(())
    }

    #[test]
    fn generations_are_idempotent_revision_specific_and_write_isolated() -> Result<()> {
        let f = Fixture::new()?;
        let project = &f.project;
        let repo = &project.repo;
        let spec = f.spec();
        let tree = f.write_tree(repo, "first version")?;
        symlink("package", tree.join("internal"))?;
        let a = f.sibling("first")?;
        let b = f.sibling("second")?;
        let first = seed_registered(project, repo, &spec)?;
        if first["reason"] == "cow_unavailable" {
            assert_ne!(attach_registered(project, &a, &spec)?["status"], "attached");
            assert!(!a.join("vendor").exists());
            return Ok(());
        }
        assert_eq!(first["status"], "registered", "{first}");
        assert_eq!(
            seed_registered(project, repo, &spec)?["digest"],
            first["digest"]
        );
        let attached = attach_registered(project, &a, &spec)?;
        assert_eq!(attached["status"], "attached", "{attached}");
        assert_eq!(attached["cloned_files"], 1);
        assert_eq!(attached["logical_bytes"], "first version".len());
        fs::create_dir(b.join("vendor"))?;
        assert_eq!(attach_registered(project, &b, &spec)?["status"], "attached");
        assert_eq!(
            fs::read_link(a.join("vendor/bundle/internal"))?,
            Path::new("package")
        );
        let pa = a.join("vendor/bundle/package");
        let pb = b.join("vendor/bundle/package");
        assert_ne!(fs::metadata(&pa)?.ino(), fs::metadata(&pb)?.ino());
        assert_ne!(
            fs::metadata(&pa)?.ino(),
            fs::metadata(tree.join("package"))?.ino()
        );
        assert_eq!(fs::metadata(&pa)?.nlink(), 1);
        fs::write(&pa, "private edit")?;
        assert_eq!(fs::read_to_string(&pb)?, "first version");
        assert_eq!(fs::read_to_string(tree.join("package"))?, "first version");

        fs::write(tree.join("package"), "unregistered mutation")?;
        assert!(seed_registered(project, repo, &spec).is_err());
        assert_eq!(
            attach_registered(project, &a, &spec)?["reason"],
            "destination_occupied"
        );
        assert_eq!(fs::read_to_string(&pa)?, "private edit");

        fs::write(repo.join("fixture.lock"), "version two")?;
        assert!(seed_registered(project, repo, &spec).is_err());
        git::bytes(repo, &["add", "fixture.lock"])?;
        assert!(seed_registered(project, repo, &spec).is_err());
        commit(repo)?;
        let c = f.sibling("changed-revision")?;
        assert_eq!(
            attach_registered(project, &c, &spec)?["reason"],
            "generation_missing"
        );
        assert!(!c.join("vendor").exists());
        let second = seed_registered(project, repo, &spec)?;
        assert_ne!(second["digest"], first["digest"]);
        assert_eq!(attach_registered(project, &c, &spec)?["status"], "attached");
        assert_eq!(
            fs::read_to_string(c.join("vendor/bundle/package"))?,
            "unregistered mutation"
        );

        fs::remove_dir_all(b.join("vendor/bundle"))?;
        assert_eq!(
            attach_registered(project, &b, &spec)?["digest"],
            first["digest"]
        );
        assert_eq!(fs::read_to_string(&pb)?, "first version");
        let mut changed = f.spec();
        changed.directory = "vendor/other".into();
        assert_eq!(
            attach_registered(project, &b, &changed)?["reason"],
            "generation_missing"
        );
        assert!(!b.join(&changed.directory).exists());
        Ok(())
    }

    #[test]
    fn rejects_redirects_and_paths_tracked_in_either_head_or_index() -> Result<()> {
        let f = Fixture::new()?;
        let repo = &f.project.repo;
        let spec = f.spec();
        let tree = f.write_tree(repo, "source")?;
        let outside = f.root.join("outside");
        fs::create_dir(&outside)?;
        fs::write(outside.join("secret"), "private")?;
        let linked = f.sibling("linked-destination")?;
        symlink(&outside, linked.join("vendor"))?;
        fs::hard_link(outside.join("secret"), tree.join("hardlink"))?;
        assert!(seed_registered(&f.project, repo, &spec).is_err());
        fs::remove_file(tree.join("hardlink"))?;
        symlink("../../outside/secret", tree.join("escape"))?;
        let escaped = seed_registered(&f.project, repo, &spec);
        if escaped
            .as_ref()
            .is_ok_and(|value| value["reason"] == "cow_unavailable")
        {
            return Ok(());
        }
        assert!(escaped.is_err());
        fs::remove_file(tree.join("escape"))?;
        assert_eq!(
            seed_registered(&f.project, repo, &spec)?["status"],
            "registered"
        );
        assert_eq!(
            attach_registered(&f.project, &linked, &spec)?["reason"],
            "unsafe_destination"
        );
        assert_eq!(fs::read_to_string(outside.join("secret"))?, "private");
        assert!(!outside.join("bundle").exists());

        git::bytes(repo, &["add", "-f", "vendor/bundle/package"])?;
        assert!(seed_registered(&f.project, repo, &spec).is_err());
        commit(repo)?;
        git::bytes(repo, &["rm", "--cached", "vendor/bundle/package"])?;
        assert!(seed_registered(&f.project, repo, &spec).is_err());
        assert_eq!(fs::read_to_string(tree.join("package"))?, "source");
        let tampered = f.sibling("tampered-input")?;
        fs::write(tampered.join("fixture.lock"), "modified")?;
        assert_eq!(
            attach_registered(&f.project, &tampered, &spec)?["status"],
            "inputs_mismatch"
        );
        Ok(())
    }
}
