//! Manage a private APFS tree without cleaning up by its original pathname.

use anyhow::{Context, Result, ensure};
use std::{
    ffi::CStr,
    fs::{self, DirBuilder, File, Metadata, Permissions},
    io::Write,
    mem::MaybeUninit,
    os::{
        fd::AsRawFd,
        macos::fs::MetadataExt as _,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    },
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

pub(super) struct Scratch {
    entries: Vec<Entry>,
    sealed: bool,
}

enum Kind {
    Directory,
    File(Vec<u8>),
    Link(PathBuf),
}

struct Entry {
    relative: PathBuf,
    parent: usize,
    id: PathBuf,
    pinned: File,
    kind: Kind,
    stamp: Stamp,
}

#[derive(PartialEq, Eq)]
struct Stamp {
    identity: (u64, u64),
    owner: (u32, u32),
    mode: u32,
    links: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    flags: u32,
}

impl Stamp {
    fn read(metadata: &Metadata) -> Self {
        Self {
            identity: (metadata.dev(), metadata.ino()),
            owner: (metadata.uid(), metadata.gid()),
            mode: metadata.mode(),
            links: metadata.nlink(),
            size: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            flags: metadata.st_flags(),
        }
    }
}

impl Scratch {
    pub(super) fn new(parent: &Path) -> Result<Self> {
        let parent = open_directory(parent)?;
        let metadata = parent.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "preview scratch parent is not privately writable"
        );
        ensure_apfs(&parent)?;
        let parent_id = id_path(&metadata);
        ensure!(
            Stamp::read(&fs::symlink_metadata(&parent_id)?).identity
                == Stamp::read(&metadata).identity,
            "cannot resolve preview scratch parent by directory ID"
        );
        let start = SystemTime::now();
        // Disable pathname cleanup as soon as the exclusive mkdir succeeds.
        let name = tempfile::Builder::new()
            .prefix(".tmp-graft-preview-")
            .rand_bytes(24)
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(parent_id)?
            .keep();
        let entry = Entry::capture(
            PathBuf::new(),
            0,
            &name,
            open_directory(&name)?,
            Kind::Directory,
            start,
            metadata.dev(),
        )?;
        ensure!(
            fs::read_dir(&entry.id)?.next().is_none(),
            "preview scratch is occupied"
        );
        Ok(Self {
            entries: vec![entry],
            sealed: false,
        })
    }

    pub(super) fn path(&self) -> &Path {
        &self.entries[0].id
    }

    pub(super) fn directory(&mut self, relative: &Path) -> Result<()> {
        self.create(relative, Kind::Directory, |name| {
            DirBuilder::new().mode(0o700).create(name)?;
            open_directory(name)
        })
    }

    pub(super) fn file(&mut self, relative: &Path, data: &[u8]) -> Result<()> {
        self.create(relative, Kind::File(data.to_vec()), |name| {
            let mut file = File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o400)
                .open(name)?;
            file.write_all(data)?;
            file.set_permissions(Permissions::from_mode(0o400))?;
            Ok(file)
        })
    }

    pub(super) fn link(&mut self, relative: &Path, target: &Path) -> Result<()> {
        self.create(relative, Kind::Link(target.to_owned()), |name| {
            symlink(target, name)?;
            ensure!(
                fs::symlink_metadata(name)?.is_symlink(),
                "preview link was replaced"
            );
            // O_SYMLINK opens the link itself on macOS. O_NOFOLLOW would
            // reject it instead of returning a descriptor for the link.
            Ok(File::options()
                .read(true)
                .custom_flags(libc::O_SYMLINK | libc::O_NONBLOCK)
                .open(name)?)
        })
    }

    fn create(
        &mut self,
        relative: &Path,
        kind: Kind,
        create: impl FnOnce(&Path) -> Result<File>,
    ) -> Result<()> {
        ensure!(!self.sealed, "preview scratch is sealed");
        ensure!(
            relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
            "invalid preview scratch path"
        );
        let parent = relative
            .parent()
            .context("missing preview scratch parent")?;
        let leaf = relative
            .file_name()
            .context("missing preview scratch filename")?;
        let parent = self
            .entries
            .iter()
            .position(|entry| entry.relative == parent && matches!(entry.kind, Kind::Directory))
            .context("preview scratch parent is not managed")?;
        let relative = self.entries[parent].relative.join(leaf);
        ensure!(
            !self.entries.iter().any(|entry| entry.relative == relative),
            "preview scratch entry already exists"
        );
        let directory = &self.entries[parent];
        directory.check(&directory.id)?;
        let name = directory.id.join(leaf);
        let start = SystemTime::now();
        let pinned = create(&name)?;
        let entry = Entry::capture(
            relative,
            parent,
            &name,
            pinned,
            kind,
            start,
            directory.stamp.identity.0,
        )?;
        self.entries.push(entry);
        self.entries[parent].stamp = self.entries[parent].observe()?;
        Ok(())
    }

    pub(super) fn seal(&mut self) -> Result<()> {
        ensure!(!self.sealed, "preview scratch is already sealed");
        // Validate the tree against the stamps recorded after our own changes.
        // In particular, a substitution before seal cannot become the baseline.
        self.validate()?;
        self.sealed = true;
        Ok(())
    }

    pub(super) fn check(&self) -> Result<()> {
        ensure!(self.sealed, "preview scratch has not been sealed");
        self.validate()
    }

    fn validate(&self) -> Result<()> {
        for (index, entry) in self.entries.iter().enumerate() {
            if matches!(entry.kind, Kind::Directory) {
                let expected = self.entries[1..]
                    .iter()
                    .filter(|child| child.parent == index)
                    .count();
                let mut count = 0;
                for item in fs::read_dir(&entry.id)? {
                    item?;
                    count += 1;
                    ensure!(count <= expected, "unexpected preview scratch entries");
                }
                ensure!(count == expected, "missing preview scratch entries");
            }
        }
        for entry in &self.entries {
            entry.check(&self.name(entry))?;
        }
        Ok(())
    }

    fn name(&self, entry: &Entry) -> PathBuf {
        match entry.relative.file_name() {
            Some(leaf) => self.entries[entry.parent].id.join(leaf),
            None => entry.id.clone(),
        }
    }
}

impl Entry {
    fn capture(
        relative: PathBuf,
        parent: usize,
        name: &Path,
        pinned: File,
        kind: Kind,
        start: SystemTime,
        device: u64,
    ) -> Result<Self> {
        let metadata = pinned.metadata()?;
        ensure!(
            metadata.dev() == device && metadata.created()? >= start,
            "preview scratch entry does not match the new creation"
        );
        let entry = Self {
            relative,
            parent,
            id: id_path(&metadata),
            pinned,
            kind,
            stamp: Stamp::read(&metadata),
        };
        entry.check(name)?;
        Ok(entry)
    }

    fn observe(&self) -> Result<Stamp> {
        let metadata = self.pinned.metadata()?;
        let stamp = Stamp::read(&metadata);
        let mode = stamp.mode & 0o7777;
        let clean = match self.kind {
            Kind::Directory => metadata.is_dir() && mode == 0o700 && stamp.links >= 2,
            Kind::File(_) => metadata.is_file() && mode == 0o400 && stamp.links == 1,
            Kind::Link(_) => metadata.is_symlink() && mode & 0o7000 == 0 && stamp.links == 1,
        };
        ensure!(
            clean
                && stamp.identity == self.stamp.identity
                && stamp.owner == self.stamp.owner
                && stamp.owner.0 == unsafe { libc::geteuid() }
                && stamp.flags == 0
                && stamp == Stamp::read(&fs::symlink_metadata(&self.id)?),
            "invalid preview scratch entry"
        );
        Ok(stamp)
    }

    fn check(&self, name: &Path) -> Result<()> {
        if let Kind::File(expected) = &self.kind {
            use std::os::unix::fs::FileExt;
            let mut contents = vec![0; expected.len()];
            self.pinned.read_exact_at(&mut contents, 0)?;
            ensure!(
                contents == *expected && self.pinned.metadata()?.len() == expected.len() as u64,
                "unexpected private Git configuration"
            );
        }
        ensure!(
            self.observe()? == self.stamp
                && Stamp::read(&fs::symlink_metadata(name)?) == self.stamp,
            "preview scratch entry changed at {}",
            name.display()
        );
        if let Kind::Link(target) = &self.kind {
            ensure!(
                fs::read_link(&self.id)? == *target,
                "preview scratch link changed"
            );
        }
        Ok(())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for entry in self.entries.iter().rev() {
            let Ok(stamp) = entry.observe() else { continue };
            // Children must still have their names under their managed parent.
            // The root may have moved. Nothing is ever removed by name.
            if !fs::symlink_metadata(self.name(entry))
                .is_ok_and(|metadata| Stamp::read(&metadata) == stamp)
            {
                continue;
            }
            if matches!(entry.kind, Kind::Directory) {
                // rmdir fails if unknown entries have appeared.
                let _ = fs::remove_dir(&entry.id);
            } else if stamp == entry.stamp {
                let _ = fs::remove_file(&entry.id);
            }
        }
    }
}

fn id_path(metadata: &Metadata) -> PathBuf {
    format!("/.vol/{}/{}", metadata.dev(), metadata.ino()).into()
}

fn open_directory(path: &Path) -> Result<File> {
    Ok(File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?)
}

fn ensure_apfs(directory: &File) -> Result<()> {
    let mut info = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(directory.as_raw_fd(), info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let info = unsafe { info.assume_init() };
    let kind = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) };
    ensure!(kind.to_bytes() == b"apfs", "preview scratch requires APFS");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_removes_moved_scratch_and_preserves_replacement() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let mut scratch = Scratch::new(parent.path())?;
        scratch.directory(Path::new("pack"))?;
        scratch.file(Path::new("pack/shallow"), b"isolated")?;
        scratch.link(Path::new("pack/pack.idx"), Path::new("/dev/fd/0"))?;
        scratch.seal()?;
        scratch.check()?;

        let original = fs::read_dir(parent.path())?
            .next()
            .context("missing scratch")??
            .path();
        let moved = parent.path().join("moved");
        fs::rename(&original, &moved)?;
        fs::create_dir(&original)?;
        fs::write(original.join("keep"), b"user data")?;
        assert_eq!(fs::read(scratch.path().join("pack/shallow"))?, b"isolated");
        drop(scratch);
        assert!(!moved.exists());
        assert_eq!(fs::read(original.join("keep"))?, b"user data");
        Ok(())
    }

    #[test]
    fn check_and_cleanup_preserve_unmanaged_entries() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let mut scratch = Scratch::new(parent.path())?;
        scratch.directory(Path::new("pack"))?;
        assert!(scratch.file(Path::new("../foreign"), b"escape").is_err());
        assert!(scratch.file(Path::new("missing/file"), b"escape").is_err());
        let relative = Path::new("pack/shallow");
        scratch.file(relative, b"owned")?;
        assert!(scratch.file(relative, b"overwrite").is_err());
        scratch.seal()?;
        let unknown = scratch.path().join("pack/new");
        fs::write(&unknown, b"user data")?;
        assert!(scratch.check().is_err());
        drop(scratch);
        assert_eq!(fs::read(unknown)?, b"user data");
        Ok(())
    }

    #[test]
    fn check_and_cleanup_preserve_changed_or_replaced_contents() -> Result<()> {
        let parent = tempfile::tempdir()?;
        let mut scratch = Scratch::new(parent.path())?;
        scratch.file(Path::new("changed"), b"owned")?;
        scratch.link(Path::new("replaced"), Path::new("/dev/fd/0"))?;
        scratch.seal()?;
        assert!(scratch.directory(Path::new("too-late")).is_err());

        let changed = scratch.path().join("changed");
        fs::set_permissions(&changed, Permissions::from_mode(0o600))?;
        fs::write(&changed, b"other")?;
        fs::set_permissions(&changed, Permissions::from_mode(0o400))?;
        assert!(scratch.check().is_err());

        let replaced = scratch.path().join("replaced");
        fs::remove_file(&replaced)?;
        fs::write(&replaced, b"user data")?;
        drop(scratch);
        assert_eq!(fs::read(changed)?, b"other");
        assert_eq!(fs::read(replaced)?, b"user data");
        Ok(())
    }
}
