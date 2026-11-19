//! Independent snapshots of trusted local Git object databases.
//!
//! Git publishes hash-named objects as immutable files. Compare inventories
//! around the clone and check each open file's generation before and after
//! cloning. This checks stability and pack/index pairing. Object contents
//! and graph completeness rely on the donor's Git database being consistent.

use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, OsStr, OsString},
    fs::{self, File, Metadata},
    io,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::{FileExt, MetadataExt},
        },
    },
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::{checked, clone_file, open_regular, optional_config, output_text, run, transfer_git};

const PARTIAL_CONFIG: &str =
    r"^(extensions\.(partialclone|compatobjectformat)|remote\..*\.(promisor|partialclonefilter))$";

pub(super) fn bootstrap(
    primary: &Path,
    root: &Path,
    format: &str,
    base: &str,
) -> Result<Option<Value>> {
    // An unstable inventory gets one fresh attempt. All input stays in the
    // disposable directory until a complete attempt passes validation.
    for _ in 0..2 {
        match prepare(primary, root, format, base) {
            Ok(Some(snapshot)) => return snapshot.publish(root),
            Ok(None) => {}
            Err(_) => break,
        }
    }
    Ok(None)
}

fn prepare(primary: &Path, root: &Path, format: &str, base: &str) -> Result<Option<Snapshot>> {
    let width = if format == "sha1" { 20 } else { 32 };
    let source = Source::open(primary)?;
    let before = source.inventory(width)?;
    source.verify_layout()?;

    let admin_path = root.canonicalize()?.join(".git");
    let admin = Directory::absolute(&admin_path)?;
    let staging = tempfile::Builder::new()
        .prefix(".graft-local-objects-")
        .tempdir_in(&admin_path)?;
    let stage_dir = admin.directory(staging.path().file_name().context("missing stage name")?)?;
    let objects = staging.path().join("objects");
    let receipt = source.clone_objects(&before, &objects, format, width)?;

    let mut cat = transfer_git(root);
    cat.env("GIT_OBJECT_DIRECTORY", &objects)
        .args(["cat-file", "-t", base]);
    ensure!(
        output_text(&checked(cat)?)? == "commit",
        "the selected commit is missing from the independent object snapshot"
    );

    // Reopen Git's path from the filesystem root. A stable detached directory
    // inode is insufficient if the donor has replaced it at its original name.
    let current = Source::open(primary)?;
    current.verify_layout()?;
    if before != current.inventory(width)? {
        return Ok(None);
    }
    Ok(Some(Snapshot {
        _staging: staging,
        stage_dir,
        admin,
        receipt,
    }))
}

struct Snapshot {
    _staging: tempfile::TempDir,
    stage_dir: Directory,
    admin: Directory,
    receipt: Value,
}

impl Snapshot {
    fn publish(self, root: &Path) -> Result<Option<Value>> {
        let current = Directory::absolute(&root.canonicalize()?.join(".git"))?;
        ensure!(
            self.admin.stamp()? == current.stamp()?,
            "workspace Git directory was replaced"
        );
        self.admin.remove_init_objects()?;
        match self.stage_dir.install_objects(&self.admin) {
            Ok(()) => {}
            Err(error)
                if error.kind() == io::ErrorKind::Unsupported
                    || matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) =>
            {
                self.admin.restore_init_objects()?;
                return Ok(None);
            }
            Err(error) => {
                return Err(error).context(
                    "publish independent Git objects without replacing an existing directory",
                );
            }
        }
        self.admin
            .0
            .sync_all()
            .context("sync independent Git object snapshot")?;
        Ok(Some(self.receipt))
    }
}

impl Directory {
    fn remove_init_objects(&self) -> Result<()> {
        let objects_name = OsStr::new("objects");
        let objects = self.directory(objects_name)?;
        let mut entries = objects.names()?;
        entries.sort_unstable();
        ensure!(
            entries == ["info", "pack"],
            "workspace Git object directory is occupied or has been replaced"
        );
        // unlinkat with AT_REMOVEDIR can remove only empty directories. The
        // subsequent rename must also refuse a new entrant at the same name.
        objects.remove_directory(c"pack")?;
        objects.remove_directory(c"info")?;
        ensure!(
            Some(objects.stamp()?) == self.entry(objects_name)?,
            "workspace Git object directory was replaced"
        );
        self.remove_directory(c"objects")?;
        Ok(())
    }

    fn restore_init_objects(&self) -> io::Result<()> {
        self.create_directory(c"objects")?;
        let objects = self.directory(OsStr::new("objects"))?;
        objects.create_directory(c"pack")?;
        objects.create_directory(c"info")
    }

    fn install_objects(&self, admin: &Self) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::renameatx_np(
                self.0.as_raw_fd(),
                c"objects".as_ptr(),
                admin.0.as_raw_fd(),
                c"objects".as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::renameat2(
                self.0.as_raw_fd(),
                c"objects".as_ptr(),
                admin.0.as_raw_fd(),
                c"objects".as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn remove_directory(&self, name: &CStr) -> io::Result<()> {
        if unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn create_directory(&self, name: &CStr) -> io::Result<()> {
        if unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), 0o777) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

struct Source {
    path: PathBuf,
    root: Directory,
    admin: Directory,
    objects: Directory,
}

impl Source {
    fn open(primary: &Path) -> Result<Self> {
        let path = primary.canonicalize()?;
        let root = Directory::absolute(&path)?;
        let admin = root.directory(OsStr::new(".git"))?;
        let objects = admin.directory(OsStr::new("objects"))?;
        let device = root.stamp()?.device;
        ensure!(
            device == admin.stamp()?.device && device == objects.stamp()?.device,
            "mounted Git object directory"
        );
        Ok(Self {
            path,
            root,
            admin,
            objects,
        })
    }

    fn verify_layout(&self) -> Result<()> {
        let admin = self.path.join(".git");
        let objects = admin.join("objects");
        let paths = run(
            &self.path,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--absolute-git-dir",
                "--git-common-dir",
                "--git-path",
                "objects",
                "--is-shallow-repository",
            ],
        )?;
        ensure!(
            output_text(&paths)?.split('\n').map(Some).eq([
                self.path.to_str(),
                admin.to_str(),
                admin.to_str(),
                objects.to_str(),
                Some("false"),
            ]),
            "snapshot requires an ordinary full Git repository"
        );
        ensure!(
            optional_config(crate::git::raw(
                &self.path,
                &["config", "--includes", "--get-regexp", PARTIAL_CONFIG],
            )?)?
            .is_empty(),
            "snapshot requires a complete native Git object database"
        );
        Ok(())
    }

    fn inventory(&self, width: usize) -> Result<Inventory> {
        let root = self.root.stamp()?;
        let admin = self.admin.stamp()?;
        let objects = self.objects.stamp()?;
        let config = self.admin.required_regular(OsStr::new("config"))?;
        let worktree_config = self.admin.entry(OsStr::new("config.worktree"))?;
        ensure!(
            worktree_config.as_ref().is_none_or(Stamp::regular),
            "unsupported Git worktree config"
        );
        for name in ["shallow", "shallow.lock", "commondir"] {
            ensure!(
                self.admin.entry(OsStr::new(name))?.is_none(),
                "unsupported Git repository state ({name})"
            );
        }
        let mut directories = BTreeMap::new();
        for name in self.objects.names()? {
            let kind = match name.as_bytes() {
                b"info" => Kind::Info,
                b"pack" => Kind::Pack,
                bytes if lower_hex(bytes, 2) => Kind::Loose,
                _ => bail!("unsupported entry in Git object directory"),
            };
            let child = self.objects.directory(&name)?;
            let listing = child.inventory(kind, objects.device, width)?;
            directories.insert(name, listing);
        }
        ensure!(
            directories.contains_key(OsStr::new("pack"))
                && directories.contains_key(OsStr::new("info")),
            "incomplete Git object directory layout"
        );
        ensure!(
            admin == self.admin.stamp()?
                && objects == self.objects.stamp()?
                && Some(config) == self.admin.entry(OsStr::new("config"))?
                && worktree_config == self.admin.entry(OsStr::new("config.worktree"))?,
            "Git object directory changed during enumeration"
        );
        Ok(Inventory {
            root: (root.device, root.inode),
            admin,
            objects,
            config,
            worktree_config,
            directories,
        })
    }

    fn clone_objects(
        &self,
        inventory: &Inventory,
        destination: &Path,
        format: &str,
        width: usize,
    ) -> Result<Value> {
        fs::create_dir(destination)?;
        let (mut pack_bytes, mut index_bytes, mut loose_bytes) = (0, 0, 0);
        let (mut pack_count, mut loose_count) = (0, 0);
        for (name, listing) in &inventory.directories {
            let target = destination.join(name);
            fs::create_dir(&target)?;
            let directory = self.objects.directory(name)?;
            ensure!(
                directory.stamp()? == listing.stamp,
                "Git object directory changed before cloning"
            );
            for (filename, entry) in &listing.entries {
                let copy = match listing.kind {
                    Kind::Info => continue,
                    Kind::Pack if !is_pack_or_index(filename) => continue,
                    _ => target.join(filename),
                };
                let file = directory.open(filename, libc::O_RDONLY | libc::O_NONBLOCK)?;
                ensure!(
                    *entry == Stamp::from(&file.metadata()?),
                    "Git object changed before cloning"
                );
                clone_file(&file, &copy)?;
                let cloned = open_regular(&copy)?;
                let cloned_meta = cloned.metadata()?;
                ensure!(
                    cloned_meta.len() == entry.size
                        && (cloned_meta.dev(), cloned_meta.ino()) != (entry.device, entry.inode)
                        && cloned_meta.nlink() == 1
                        && *entry == Stamp::from(&file.metadata()?),
                    "Git object changed during cloning"
                );
                sync_snapshot_entry(&cloned)?;
                match listing.kind {
                    Kind::Pack if filename.as_bytes().ends_with(b".pack") => {
                        pack_bytes += entry.size;
                        pack_count += 1;
                    }
                    Kind::Pack => index_bytes += entry.size,
                    Kind::Loose => {
                        loose_bytes += entry.size;
                        loose_count += 1;
                    }
                    Kind::Info => unreachable!(),
                }
            }
            if listing.kind == Kind::Pack {
                for name in listing
                    .entries
                    .keys()
                    .filter(|name| name.as_bytes().ends_with(b".pack"))
                {
                    check_pack_pair(&target.join(name), width)?;
                }
            }
            sync_snapshot_entry(&File::open(&target)?)?;
        }
        finish_snapshot_writeback(destination)?;
        let bytes = pack_bytes + index_bytes + loose_bytes;
        Ok(json!({
            "method": if cfg!(target_os = "macos") { "clonefile" } else { "reflink" },
            "source": "local-snapshot",
            "bytes": bytes,
            "pack_bytes": pack_bytes,
            "index_bytes": index_bytes,
            "loose_bytes": loose_bytes,
            "shared_bytes": bytes,
            "cache_bytes": 0,
            "pack_count": pack_count,
            "loose_count": loose_count,
            "object_format": format,
        }))
    }
}

// All staged files and directories are on the same device. On macOS fsync
// waits for writeback, and F_FULLFSYNC then drains the device cache once for
// the batch. A failed final flush disqualifies the unpublished snapshot.
fn sync_snapshot_entry(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        loop {
            if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    #[cfg(target_os = "linux")]
    file.sync_all()
}

fn finish_snapshot_writeback(directory: &Path) -> io::Result<()> {
    let root = File::open(directory)?;
    #[cfg(target_os = "macos")]
    {
        if unsafe { libc::fcntl(root.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "linux")]
    root.sync_all()
}

#[derive(Debug, PartialEq, Eq)]
struct Inventory {
    root: (u64, u64),
    admin: Stamp,
    objects: Stamp,
    config: Stamp,
    worktree_config: Option<Stamp>,
    directories: BTreeMap<OsString, Listing>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Info,
    Pack,
    Loose,
}

#[derive(Debug, PartialEq, Eq)]
struct Listing {
    kind: Kind,
    stamp: Stamp,
    entries: BTreeMap<OsString, Stamp>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<&Metadata> for Stamp {
    fn from(meta: &Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            mode: meta.mode(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

impl Stamp {
    // libc stat field widths differ between these targets.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(info: libc::stat) -> Self {
        Self {
            device: info.st_dev as u64,
            inode: info.st_ino as u64,
            size: info.st_size as u64,
            mode: info.st_mode as u32,
            modified: (info.st_mtime as i64, info.st_mtime_nsec as i64),
            changed: (info.st_ctime as i64, info.st_ctime_nsec as i64),
        }
    }

    fn regular(&self) -> bool {
        self.has_type(libc::S_IFREG)
    }

    fn directory(&self) -> bool {
        self.has_type(libc::S_IFDIR)
    }

    #[allow(clippy::unnecessary_cast)]
    fn has_type(&self, kind: libc::mode_t) -> bool {
        (self.mode as libc::mode_t) & libc::S_IFMT == kind
    }
}

struct Directory(File);

impl Directory {
    fn absolute(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "expected an absolute directory");
        let mut root = Self(File::open("/")?);
        for part in path.components() {
            match part {
                Component::RootDir => {}
                Component::Normal(name) => root = root.directory(name)?,
                _ => bail!("invalid absolute repository path"),
            }
        }
        Ok(root)
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

    fn directory(&self, name: &OsStr) -> io::Result<Self> {
        Ok(Self(self.open(name, libc::O_RDONLY | libc::O_DIRECTORY)?))
    }

    fn stamp(&self) -> io::Result<Stamp> {
        Ok(Stamp::from(&self.0.metadata()?))
    }

    fn entry(&self, name: &OsStr) -> io::Result<Option<Stamp>> {
        let name = CString::new(name.as_bytes())?;
        let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                info.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Ok(Some(Stamp::from_stat(unsafe { info.assume_init() })));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }

    fn required_regular(&self, name: &OsStr) -> Result<Stamp> {
        let stamp = self.entry(name)?.context("missing Git database file")?;
        ensure!(stamp.regular(), "Git database entry is not a regular file");
        Ok(stamp)
    }

    fn names(&self) -> io::Result<Vec<OsString>> {
        // Reopen instead of duplicating the descriptor. Each listing must
        // start at the beginning independently of prior directory iteration.
        let fd = self.directory(OsStr::new("."))?.0.into_raw_fd();
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

    fn inventory(&self, kind: Kind, device: u64, width: usize) -> Result<Listing> {
        let stamp = self.stamp()?;
        ensure!(stamp.device == device, "mounted Git object directory");
        let mut entries = BTreeMap::new();
        for name in self.names()? {
            let entry = self.entry(&name)?.context("Git object entry disappeared")?;
            let directory = expected_directory(kind, name.as_bytes(), width)?;
            ensure!(
                entry.device == device
                    && if directory {
                        entry.directory()
                    } else {
                        entry.regular()
                    },
                "unsupported Git object entry type"
            );
            entries.insert(name, entry);
        }
        if kind == Kind::Pack {
            for name in entries.keys().filter(|name| is_pack_or_index(name)) {
                let pair_ext = if name.as_bytes().ends_with(b".pack") {
                    "idx"
                } else {
                    "pack"
                };
                let pair = Path::new(name).with_extension(pair_ext);
                ensure!(
                    entries.contains_key(pair.as_os_str()),
                    "unpaired Git pack or index"
                );
            }
        }
        ensure!(
            stamp == self.stamp()?,
            "Git object directory changed during enumeration"
        );
        Ok(Listing {
            kind,
            stamp,
            entries,
        })
    }
}

fn lower_hex(name: &[u8], width: usize) -> bool {
    name.len() == width
        && name
            .iter()
            .all(|ch| matches!(ch, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_pack_or_index(name: &OsStr) -> bool {
    name.as_bytes().ends_with(b".pack") || name.as_bytes().ends_with(b".idx")
}

// Accept only native object names and known Git-derived metadata. Alternates,
// promisor markers and in-flight temporary files make a snapshot ineligible.
fn expected_directory(kind: Kind, name: &[u8], width: usize) -> Result<bool> {
    match kind {
        Kind::Info => match name {
            b"commit-graphs" => return Ok(true),
            b"commit-graph" | b"packs" => return Ok(false),
            _ => {}
        },
        Kind::Pack => match name {
            b"multi-pack-index.d" => return Ok(true),
            b"multi-pack-index" => return Ok(false),
            _ => {
                if let Some(dot) = name.iter().rposition(|ch| *ch == b'.') {
                    let (prefix, extension) = (&name[..dot], &name[dot + 1..]);
                    if prefix
                        .strip_prefix(b"pack-")
                        .is_some_and(|oid| lower_hex(oid, 2 * width))
                        && matches!(
                            extension,
                            b"pack" | b"idx" | b"bitmap" | b"rev" | b"mtimes" | b"keep"
                        )
                        || prefix
                            .strip_prefix(b"multi-pack-index-")
                            .is_some_and(|oid| lower_hex(oid, 2 * width))
                            && matches!(extension, b"bitmap" | b"rev")
                    {
                        return Ok(false);
                    }
                }
            }
        },
        Kind::Loose if lower_hex(name, 2 * width - 2) => return Ok(false),
        Kind::Loose => {}
    }
    bail!("unsupported Git object database entry")
}

fn check_pack_pair(path: &Path, width: usize) -> Result<()> {
    let pack = open_regular(path)?;
    let index = open_regular(&path.with_extension("idx"))?;
    let pack_size = pack.metadata()?.len();
    let index_size = index.metadata()?.len();
    ensure!(
        pack_size >= (12 + width) as u64 && index_size >= (1032 + 2 * width) as u64,
        "incomplete Git pack or index"
    );
    let mut pack_header = [0; 12];
    pack.read_exact_at(&mut pack_header, 0)?;
    ensure!(
        &pack_header[..4] == b"PACK" && matches!(pack_header[4..8], [0, 0, 0, 2 | 3]),
        "unsupported Git pack version"
    );
    let mut index_header = [0; 8];
    index.read_exact_at(&mut index_header, 0)?;
    let mut count = [0; 4];
    index.read_exact_at(&mut count, 1028)?;
    ensure!(
        index_header == *b"\xfftOc\0\0\0\x02" && count == pack_header[8..12],
        "unsupported or mismatched Git pack index"
    );
    let entries = u64::from(u32::from_be_bytes(count));
    let minimum = (1032 + 2 * width) as u64 + entries * (width as u64 + 8);
    ensure!(
        index_size >= minimum
            && index_size <= minimum + entries * 8
            && (index_size - minimum).is_multiple_of(8),
        "unexpected Git pack index size"
    );
    let mut pack_hash = [0; 32];
    let mut index_pack_hash = [0; 32];
    pack.read_exact_at(&mut pack_hash[..width], pack_size - width as u64)?;
    index.read_exact_at(
        &mut index_pack_hash[..width],
        index_size - (2 * width) as u64,
    )?;
    let stem = path
        .file_stem()
        .context("Git pack name missing")?
        .as_bytes();
    let hex = b"0123456789abcdef";
    ensure!(
        pack_hash == index_pack_hash
            && pack_hash[..width]
                .iter()
                .flat_map(|byte| { [hex[(byte >> 4) as usize], hex[(byte & 15) as usize]] })
                .eq(stem[5..].iter().copied()),
        "Git pack name and index do not match the pack checksum"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::History,
        git,
        workspace::objects::{initialize, preflight},
    };

    fn make_repo(repo: &Path, format: &str) -> Result<String> {
        fs::create_dir(repo)?;
        run(
            repo,
            &[
                "init",
                "--quiet",
                "--template=",
                &format!("--object-format={format}"),
            ],
        )?;
        run(repo, &["config", "user.name", "Snapshot Test"])?;
        run(
            repo,
            &["config", "user.email", "snapshot-test@example.test"],
        )?;
        run(repo, &["config", "gc.auto", "0"])?;
        commit(repo, "first")
    }

    fn commit(repo: &Path, text: &str) -> Result<String> {
        fs::write(repo.join("content"), text)?;
        run(repo, &["add", "content"])?;
        run(
            repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                text,
            ],
        )?;
        git::oid(repo, "HEAD")
    }

    fn staged_blob(repo: &Path) -> Result<String> {
        fs::write(repo.join("staged"), "exists only in primary index")?;
        run(repo, &["add", "staged"])?;
        git::text(repo, &["rev-parse", ":staged"])
    }

    #[test]
    fn snapshot_keeps_all_objects_and_survives_source_gc_with_private_git_state() -> Result<()> {
        for format in ["sha1", "sha256"] {
            let fixture = tempfile::tempdir()?;
            let primary = fixture.path().join("primary");
            let workspace = fixture.path().join("workspace");
            let state = fixture.path().join("state");
            let first = make_repo(&primary, format)?;
            fs::create_dir(&workspace)?;
            fs::create_dir(&state)?;
            let tree = git::text(&primary, &["rev-parse", "HEAD^{tree}"])?;
            let other = git::text(&primary, &["commit-tree", &tree, "-m", "unrelated"])?;
            run(&primary, &["update-ref", "refs/heads/other", &other])?;
            run(
                &primary,
                &["update-ref", "refs/remotes/origin/archive", &other],
            )?;
            run(&primary, &["tag", "other-tag", &other])?;
            run(
                &primary,
                &["remote", "add", "origin", "https://example.invalid/project"],
            )?;
            run(&primary, &["repack", "-adq"])?;
            let base = commit(&primary, "second")?;
            let staged = staged_blob(&primary)?;
            fs::write(primary.join("discardable"), "untagged, unstaged object")?;
            let dangling = git::text(&primary, &["hash-object", "-w", "discardable"])?;
            fs::write(primary.join("unstaged"), "primary worktree file")?;
            let source_objects = primary.join(".git/objects");
            let loose = Path::new(&staged[..2]).join(&staged[2..]);
            let donor = fs::read_dir(source_objects.join("pack"))?
                .collect::<io::Result<Vec<_>>>()?
                .into_iter()
                .map(|entry| entry.path())
                .find(|path| path.extension().is_some_and(|ext| ext == "pack"))
                .context("missing donor pack")?;
            let probe = fixture.path().join("probe");
            let can_clone = clone_file(&open_regular(&donor)?, &probe).is_ok();
            let receipt = initialize(
                &preflight(&primary)?,
                &workspace,
                &state,
                Some(&base),
                Some("topic"),
                History::Local,
                false,
            )?
            .0;
            assert_eq!(git::oid(&workspace, "HEAD")?, base);
            assert!(run(&workspace, &["remote"])?.is_empty());
            assert_eq!(
                run(&workspace, &["for-each-ref", "--format=%(refname)"])?,
                b"refs/heads/topic\n"
            );
            assert!(!workspace.join(".git/index").exists());
            assert!(!workspace.join("staged").exists());
            assert!(!workspace.join("unstaged").exists());

            if !can_clone {
                assert_ne!(receipt["source"], "local-snapshot");
                assert!(run(&workspace, &["cat-file", "-e", &other]).is_err());
                continue;
            }

            assert_eq!(receipt["source"], "local-snapshot", "{receipt}");
            assert_eq!(receipt["pack_count"], 1);
            assert_eq!(receipt["shared_bytes"], receipt["bytes"]);
            let local_objects = workspace.join(".git/objects");
            for (source, target) in [
                (
                    donor.clone(),
                    local_objects.join("pack").join(donor.file_name().unwrap()),
                ),
                (source_objects.join(&loose), local_objects.join(&loose)),
            ] {
                let (source, target) = (fs::metadata(source)?, fs::metadata(target)?);
                assert_ne!((source.dev(), source.ino()), (target.dev(), target.ino()));
                assert_eq!(target.nlink(), 1);
                assert_eq!(source.len(), target.len());
            }
            assert!(!local_objects.join("info/alternates").exists());
            assert!(fs::read_dir(&state)?.next().is_none());

            run(
                &primary,
                &["-c", "gc.cruftPacks=false", "gc", "--prune=now"],
            )?;
            assert!(run(&primary, &["cat-file", "-e", &dangling]).is_err());
            assert!(!donor.exists(), "the donor was not repacked");
            let moved = fixture.path().join("moved-primary");
            fs::rename(&primary, &moved)?;
            assert_eq!(
                git::text(&workspace, &["log", "--format=%H"])?,
                format!("{base}\n{first}")
            );
            for oid in [&staged, &dangling] {
                run(&workspace, &["cat-file", "-e", oid])?;
            }
            run(&workspace, &["switch", "-c", "recovered", &other])?;
            assert_eq!(git::oid(&workspace, "HEAD")?, other);
            let private = commit(&workspace, "workspace only")?;
            assert!(run(&moved, &["cat-file", "-e", &private]).is_err());
            assert_eq!(git::oid(&moved, "HEAD")?, base);
            assert_eq!(git::text(&moved, &["rev-parse", ":staged"])?, staged);
            run(&workspace, &["fsck", "--full"])?;
        }
        Ok(())
    }

    #[test]
    fn unsupported_object_sources_fall_back_to_exact_export() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let primary = fixture.path().join("primary");
        let state = fixture.path().join("state");
        let first = make_repo(&primary, "sha1")?;
        let base = commit(&primary, "second")?;
        let staged = staged_blob(&primary)?;
        fs::create_dir(&state)?;
        run(&primary, &["repack", "-adq"])?;
        let admin = primary.join(".git");
        let alternates = admin.join("objects/info/alternates");
        let shallow = admin.join("shallow");
        let marker = fs::read_dir(admin.join("objects/pack"))?
            .collect::<io::Result<Vec<_>>>()?
            .into_iter()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "pack"))
            .context("missing donor pack")?
            .with_extension("promisor");

        for name in ["alternate", "shallow", "partial-config", "promisor"] {
            let workspace = fixture.path().join(name);
            fs::create_dir(&workspace)?;
            match name {
                "alternate" => {
                    let other = fixture.path().join("alternate-repository");
                    make_repo(&other, "sha1")?;
                    fs::write(
                        &alternates,
                        format!("{}\n", other.join(".git/objects").display()),
                    )?;
                }
                "shallow" => fs::write(&shallow, format!("{base}\n"))?,
                "partial-config" => {
                    run(&primary, &["config", "remote.origin.promisor", "true"]).map(drop)?
                }
                "promisor" => fs::write(&marker, b"")?,
                _ => unreachable!(),
            }

            let receipt = initialize(
                &preflight(&primary)?,
                &workspace,
                &state,
                Some(&base),
                None,
                History::Local,
                false,
            )?
            .0;
            assert_ne!(receipt["source"], "local-snapshot", "{name}: {receipt}");
            assert!(receipt["source"].is_string(), "{receipt}");
            assert!(run(&workspace, &["cat-file", "-e", &staged]).is_err());
            assert!(!workspace.join(".git/objects/info/alternates").exists());
            let expected = if name == "shallow" {
                base.clone()
            } else {
                format!("{base}\n{first}")
            };
            assert_eq!(git::text(&workspace, &["log", "--format=%H"])?, expected);
            assert!(
                fs::read_dir(workspace.join(".git"))?
                    .collect::<io::Result<Vec<_>>>()?
                    .iter()
                    .all(|entry| !entry
                        .file_name()
                        .as_bytes()
                        .starts_with(b".graft-local-objects-"))
            );

            match name {
                "alternate" => fs::remove_file(&alternates)?,
                "shallow" => fs::remove_file(&shallow)?,
                "partial-config" => {
                    run(&primary, &["config", "--unset", "remote.origin.promisor"]).map(drop)?
                }
                "promisor" => fs::remove_file(&marker)?,
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    #[test]
    fn relist_checks_loose_generations_and_path_identity() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let primary = fixture.path().join("primary");
        let first = make_repo(&primary, "sha1")?;
        let source = Source::open(&primary)?;
        let before = source.inventory(20)?;
        staged_blob(&primary)?;
        let after = Source::open(&primary)?.inventory(20)?;
        assert_ne!(before, after);
        let loose = primary
            .join(".git/objects")
            .join(&first[..2])
            .join(&first[2..]);
        let contents = fs::read(&loose)?;
        let moved_file = fixture.path().join("old-object");
        fs::rename(&loose, &moved_file)?;
        fs::write(&loose, contents)?;
        let replaced = Source::open(&primary)?.inventory(20)?;
        assert_ne!(after, replaced);

        let objects = primary.join(".git/objects");
        let moved = fixture.path().join("moved-objects");
        fs::rename(&objects, &moved)?;
        fs::create_dir(&objects)?;
        fs::create_dir(objects.join("info"))?;
        fs::create_dir(objects.join("pack"))?;
        assert_ne!(
            replaced.objects,
            Source::open(&primary)?.inventory(20)?.objects
        );
        fs::rename(&objects, fixture.path().join("unused-objects"))?;
        std::os::unix::fs::symlink(&moved, &objects)?;
        assert!(Source::open(&primary).is_err());
        Ok(())
    }

    #[test]
    fn publishing_never_replaces_a_destination_or_deletes_its_contents() -> Result<()> {
        let fixture = tempfile::tempdir()?;
        let admin_path = fixture.path().join("admin");
        fs::create_dir_all(admin_path.join("objects/pack"))?;
        fs::create_dir(admin_path.join("objects/info"))?;
        let scratch = tempfile::tempdir_in(&admin_path)?;
        fs::create_dir(scratch.path().join("objects"))?;
        fs::write(scratch.path().join("objects/snapshot"), b"staged bytes")?;
        let admin = Directory::absolute(&admin_path.canonicalize()?)?;
        let stage = admin.directory(scratch.path().file_name().unwrap())?;
        let existing = admin_path.join("objects/pack/owner-data");
        fs::write(&existing, b"existing bytes")?;
        assert!(admin.remove_init_objects().is_err());
        assert_eq!(fs::read(&existing)?, b"existing bytes");
        fs::remove_file(existing)?;

        admin.remove_init_objects()?;
        let replacement = admin_path.join("objects");
        fs::create_dir(&replacement)?;
        let replacement_before = fs::metadata(&replacement)?.ino();
        assert!(stage.install_objects(&admin).is_err());
        assert_eq!(fs::metadata(&replacement)?.ino(), replacement_before);
        assert!(fs::read_dir(&replacement)?.next().is_none());
        assert_eq!(
            fs::read(scratch.path().join("objects/snapshot"))?,
            b"staged bytes"
        );
        Ok(())
    }
}
