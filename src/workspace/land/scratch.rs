//! Reclaim only a private, identity-bound unfinished landing store.
use super::{STORE_OWNER, objects};
use crate::{
    cache::{self, DirectoryIdentity, PurgeFailure},
    recovery,
    registry::{self, Project},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

const OWNER: &str = ".graft-land-pending";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    project: String,
    directory: DirectoryIdentity,
    marker: FileKey,
    snapshot: Option<Snapshot>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct FileKey {
    volume: u64,
    file: u64,
}

#[cfg(unix)]
fn file_key(file: &File) -> Result<FileKey> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.nlink() == 1,
        "landing scratch file is shared"
    );
    Ok(FileKey {
        volume: m.dev(),
        file: m.ino(),
    })
}

#[cfg(windows)]
fn file_key(file: &File) -> Result<FileKey> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut i = BY_HANDLE_FILE_INFORMATION::default();
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut i) } != 0
            && i.nNumberOfLinks == 1,
        "landing scratch file is shared"
    );
    Ok(FileKey {
        volume: u64::from(i.dwVolumeSerialNumber),
        file: (u64::from(i.nFileIndexHigh) << 32) | u64::from(i.nFileIndexLow),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_key(_: &File) -> Result<FileKey> {
    bail!("landing scratch file identity is unsupported")
}

pub(super) fn mark(p: &Project, dir: &Path) -> Result<()> {
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(OWNER))?;
    let owner = Owner {
        project: p.id().into(),
        directory: cache::directory_identity(dir)?,
        marker: file_key(&marker)?,
        snapshot: None,
    };
    marker.write_all(&serde_json::to_vec(&owner)?)?;
    marker.sync_all()?;
    super::super::registry::sync_directory(dir)
}

fn owned(p: &Project, dir: &Path) -> Result<Owner> {
    let marker = objects::open_regular(&dir.join(OWNER))?;
    let owner: Owner = serde_json::from_slice(&registry::regular(&dir.join(OWNER))?)?;
    ensure!(
        owner.project == p.id()
            && cache::directory_identity(dir)? == owner.directory
            && recovery::no_extended_metadata(&dir.join(OWNER))
            && file_key(&marker)? == owner.marker
            && file_key(&objects::open_regular(&dir.join(OWNER))?)? == owner.marker,
        "landing scratch ownership changed"
    );
    Ok(owner)
}

fn hex(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn allowed(path: &Path, directory: bool) -> bool {
    let parts: Vec<_> = path.iter().filter_map(|p| p.to_str()).collect();
    match parts.as_slice() {
        ["HEAD" | "config" | "description" | "shallow" | STORE_OWNER | OWNER] => !directory,
        ["branches" | "hooks" | "info" | "objects" | "refs"] => directory,
        ["objects", "pack" | "info"] => directory,
        ["objects", dir] if dir.len() == 2 && hex(dir) => directory,
        ["objects", dir, file] if dir.len() == 2 && hex(dir) => {
            !directory && hex(file) && (file.len() == 38 || file.len() == 62)
        }
        ["objects", "pack", name] if name.starts_with(".tmp") => directory,
        ["objects", "pack", name] | ["objects", "pack", _, name] => {
            !directory
                && name
                    .strip_prefix("pack-")
                    .and_then(|n| n.rsplit_once('.'))
                    .is_some_and(|(id, ext)| {
                        matches!(id.len(), 40 | 64)
                            && hex(id)
                            && matches!(ext, "pack" | "idx" | "keep")
                    })
        }
        ["refs", "heads" | "tags" | "graft"] => directory,
        ["refs", "graft", "import" | "land"] => directory,
        ["refs", "graft", "import" | "land", _] => directory,
        [
            "refs",
            "graft",
            "import" | "land",
            _,
            "base" | "old" | "new" | "tip",
        ] => !directory,
        _ => false,
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Snapshot {
    dirs: Vec<(PathBuf, DirectoryIdentity)>,
    files: Vec<(PathBuf, FileKey, String)>,
}

fn scan(root: &Path) -> Result<Snapshot> {
    fn walk(root: &Path, rel: &Path, snapshot: &mut Snapshot) -> Result<()> {
        for entry in fs::read_dir(root.join(rel))? {
            let entry = entry?;
            let name = entry.file_name();
            ensure!(name.to_str().is_some(), "non-UTF-8 landing scratch entry");
            let next = rel.join(name);
            if next == Path::new(OWNER) {
                continue;
            }
            let path = root.join(&next);
            let kind = entry.file_type()?;
            ensure!(
                allowed(&next, kind.is_dir())
                    && !kind.is_symlink()
                    && recovery::no_extended_metadata(&path),
                "unrecognized landing scratch entry {}",
                path.display()
            );
            if kind.is_dir() {
                snapshot
                    .dirs
                    .push((next.clone(), cache::directory_identity(&path)?));
                walk(root, &next, snapshot)?;
            } else {
                let file = objects::open_regular(&path)?;
                let key = file_key(&file)?;
                let digest = blake3::Hasher::new()
                    .update_reader(&file)?
                    .finalize()
                    .to_hex()
                    .to_string();
                ensure!(
                    file_key(&file)? == key,
                    "landing scratch file changed while reading"
                );
                snapshot.files.push((next, key, digest));
            }
        }
        Ok(())
    }
    let mut snapshot = Snapshot {
        dirs: Vec::new(),
        files: Vec::new(),
    };
    walk(root, Path::new(""), &mut snapshot)?;
    snapshot.dirs.sort_by(|a, b| a.0.cmp(&b.0));
    snapshot.files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(snapshot)
}

fn unchanged(root: &Path, snapshot: &Snapshot) -> Result<()> {
    ensure!(scan(root)? == *snapshot, "landing scratch entries changed");
    Ok(())
}

pub(super) fn checkpoint(p: &Project, dir: &Path) -> Result<()> {
    let mut owner = owned(p, dir)?;
    owner.snapshot = Some(scan(dir)?);
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut marker = options.open(dir.join(OWNER))?;
    ensure!(
        file_key(&marker)? == owner.marker,
        "landing scratch marker changed"
    );
    marker.set_len(0)?;
    marker.write_all(&serde_json::to_vec(&owner)?)?;
    marker.sync_all()?;
    Ok(())
}

pub(super) fn purge(p: &Project, root: &Path, dir: &Path) -> Result<()> {
    let owner = owned(p, dir)?;
    let snapshot = owner
        .snapshot
        .as_ref()
        .context("landing scratch has no verified checkpoint")?;
    unchanged(dir, snapshot)?;
    match cache::purge_directory(
        dir,
        &owner.directory,
        || super::super::registry::marked_dir(root, p.id(), false),
        |moved| {
            owned(p, moved)?;
            unchanged(moved, snapshot)
        },
    ) {
        Ok(()) => Ok(()),
        Err(PurgeFailure::Unmoved(error)) => Err(error),
        Err(PurgeFailure::Retained { path, error }) => {
            bail!("landing scratch retained at {}: {error:#}", path.display())
        }
    }
}

pub(super) fn recover(p: &Project, root: &Path) -> Result<()> {
    let mut retained = 0usize;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".planning-"))
        {
            retained += 1;
            if retained <= 3 {
                eprintln!(
                    "git-graft: interrupted landing planning scratch retained at {}",
                    entry.path().display()
                );
            }
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".pending-"))
            && let Err(error) = purge(p, root, &entry.path())
        {
            retained += 1;
            if retained <= 3 {
                eprintln!(
                    "git-graft: landing scratch retained at {}: {error:#}",
                    entry.path().display()
                );
            }
        }
    }
    if retained > 3 {
        eprintln!(
            "git-graft: {} additional landing scratch directories retained",
            retained - 3
        );
    }
    Ok(())
}
