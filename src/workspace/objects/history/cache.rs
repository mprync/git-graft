//! Cache only pack sets whose object IDs have been independently verified.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    collections::HashSet,
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
};

use super::{ObjectId, Pack};
use crate::recovery::rename_exclusive;
use crate::workspace::objects::{clone_file, open_regular, sync_directory};

pub(crate) const DIR: &str = "independent-packs-v2";
pub(crate) const MARKER: &str = "graft-pack-cache";
const MAGIC: &[u8] = b"git-graft verified independent pack cache v2\n";
const RECEIPT: &str = "verified.json";
const RECEIPT_LIMIT: u64 = 512 * 1024;
const PARALLEL_RESTORE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RESTORE_WORKERS: usize = 4;

pub(super) struct Cache<'a> {
    root: PathBuf,
    key: PathBuf,
    format: &'a str,
    base: &'a str,
}

#[derive(Serialize, Deserialize)]
struct Verification {
    format: String,
    base: String,
    packs: Vec<VerifiedPack>,
}

#[derive(Serialize, Deserialize)]
struct VerifiedPack {
    hash: String,
    pack: Artifact,
    index: Artifact,
}

impl VerifiedPack {
    fn bytes(&self) -> u64 {
        self.pack.stamp.size.saturating_add(self.index.stamp.size)
    }
}

#[derive(Serialize, Deserialize)]
struct Artifact {
    stamp: Stamp,
    digest: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: [i64; 2],
    changed: [i64; 2],
}

impl Stamp {
    #[cfg(unix)]
    fn read(file: &File) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        ensure!(
            m.is_file() && m.nlink() == 1 && m.mode() & 0o222 == 0,
            "expected an immutable regular Git pack file"
        );
        Ok(Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            modified: [m.mtime(), m.mtime_nsec()],
            changed: [m.ctime(), m.ctime_nsec()],
        })
    }

    #[cfg(not(unix))]
    fn read(_file: &File) -> Result<Self> {
        anyhow::bail!("immutable Git pack cloning is unsupported on this platform")
    }
}

impl<'a> Cache<'a> {
    pub(super) fn new(state: &Path, format: &'a str, base: &'a str) -> Self {
        let root = state.join(DIR);
        let key = root.join(format!("{format}-{base}"));
        Self {
            root,
            key,
            format,
            base,
        }
    }

    pub(super) fn for_base<'b>(&'b self, base: &'b str) -> Cache<'b> {
        let root = self.root.clone();
        let key = root.join(format!("{}-{base}", self.format));
        Cache {
            root,
            key,
            format: self.format,
            base,
        }
    }

    pub(super) fn has_root(&self) -> bool {
        reflinks_available() && validate_root(&self.root).is_ok()
    }

    pub(super) fn has_entry(&self) -> bool {
        fs::symlink_metadata(&self.key).is_ok_and(|meta| meta.is_dir())
    }

    pub(super) fn restore(&self, stage: &Path, width: usize) -> Result<Option<Vec<Pack>>> {
        self.restore_bounded(stage, width, None)
    }

    pub(super) fn restore_bounded(
        &self,
        stage: &Path,
        width: usize,
        budget: Option<std::ops::RangeInclusive<u64>>,
    ) -> Result<Option<Vec<Pack>>> {
        if !reflinks_available() {
            return Ok(None);
        }
        if matches!(fs::symlink_metadata(&self.root), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(None);
        }
        validate_root(&self.root)?;
        if matches!(fs::symlink_metadata(&self.key), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(None);
        }
        let (receipt, _) = read_receipt(&self.key, self.format, self.base)?;
        let bytes = receipt
            .packs
            .iter()
            .map(VerifiedPack::bytes)
            .fold(0, u64::saturating_add);
        if budget.is_some_and(|range| !range.contains(&bytes) || receipt.packs.len() >= 16) {
            return Ok(None);
        }
        let restore = |entry: &VerifiedPack| -> Result<Pack> {
            let from = Pack::at(&self.key, &entry.hash, width)?;
            let to = Pack::at(stage, &entry.hash, width)?;
            restore_file(&entry.pack, &from.pack, &to.pack)?;
            restore_file(&entry.index, &from.index, &to.index)?;
            Ok(to)
        };
        let workers = thread::available_parallelism()
            .map_or(1, usize::from)
            .min(MAX_RESTORE_WORKERS)
            .min(receipt.packs.len());
        if workers < 2 || bytes < PARALLEL_RESTORE_BYTES {
            return receipt
                .packs
                .iter()
                .map(restore)
                .collect::<Result<_>>()
                .map(Some);
        }

        // Spread large pack pairs across workers while returning the original
        // receipt order. Both files of a pair use the same descriptor checks.
        let mut entries: Vec<_> = receipt.packs.iter().enumerate().collect();
        entries.sort_unstable_by_key(|(_, entry)| Reverse(entry.bytes()));
        let mut restored = thread::scope(|scope| {
            let entries = &entries;
            let restore = &restore;
            let jobs: Vec<_> = (0..workers)
                .map(|worker| {
                    thread::Builder::new().spawn_scoped(scope, move || {
                        entries
                            .iter()
                            .skip(worker)
                            .step_by(workers)
                            .map(|&(index, entry)| restore(entry).map(|pack| (index, pack)))
                            .collect::<Result<Vec<_>>>()
                    })
                })
                .collect();
            // Join every worker before propagating any error. Failed staging
            // must be quiescent before the caller resumes with another source.
            let batches: Vec<_> = jobs
                .into_iter()
                .map(|job| {
                    job.context("start Git pack cache verification worker")?
                        .join()
                        .map_err(|_| anyhow::anyhow!("Git pack cache worker panicked"))?
                })
                .collect();
            let mut restored = Vec::with_capacity(entries.len());
            for batch in batches {
                restored.extend(batch?);
            }
            Ok::<_, anyhow::Error>(restored)
        })?;
        restored.sort_unstable_by_key(|(index, _)| *index);
        Ok(Some(restored.into_iter().map(|(_, pack)| pack).collect()))
    }

    pub(super) fn publish(&self, packs: &[Pack]) -> Result<u64> {
        if !reflinks_available() {
            return Ok(0);
        }
        prepare_root(&self.root)?;
        match fs::symlink_metadata(&self.key) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => return Ok(0),
        }
        let stage = private_tempdir_in(&self.root)?;
        let mut receipt = Verification {
            format: self.format.into(),
            base: self.base.into(),
            packs: Vec::with_capacity(packs.len()),
        };
        let mut bytes = 0;
        for pack in packs {
            let hash = pack
                .pack
                .file_stem()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix("pack-"))
                .context("missing verified Git pack hash")?;
            let packed = publish_file(&pack.pack, stage.path())?;
            let index = publish_file(&pack.index, stage.path())?;
            bytes += packed.stamp.size + index.stamp.size;
            receipt.packs.push(VerifiedPack {
                hash: hash.into(),
                pack: packed,
                index,
            });
        }
        let data = serde_json::to_vec(&receipt)?;
        ensure!(
            data.len() as u64 <= RECEIPT_LIMIT,
            "pack cache receipt is too large"
        );
        write_synced(&stage.path().join(RECEIPT), &data)?;
        sync_directory(stage.path())?;
        // Publishing the directory atomically binds the receipt and both
        // verified files of each pair. An existing entry always wins.
        rename_exclusive(stage.path(), &self.key)?;
        let _ = stage.keep();
        sync_directory(&self.root)?;
        Ok(bytes)
    }
}

/// Locate only the cache namespace created by this object backend.
pub(crate) fn history_cache_root(state: &Path) -> Result<Option<PathBuf>> {
    let root = state.join(DIR);
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => {
            validate_root(&root)?;
            Ok(Some(root))
        }
    }
}

/// Verify a generation and return a stable proof for GC's quarantine check.
/// Ordinary workspaces hold separate filesystem copies of every pack and index.
pub(crate) fn verify_history_cache_generation(
    path: &Path,
    state: &Path,
    original_name: &OsStr,
) -> Result<[u8; 32]> {
    history_cache_root(state)?.context("Git pack cache root is missing")?;
    #[cfg(unix)]
    {
        use crate::recovery::no_extended_metadata;
        use std::os::unix::fs::MetadataExt;
        let receipt = path.join(RECEIPT);
        let metadata = open_regular(&receipt)?.metadata()?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.nlink() == 1
                && no_extended_metadata(path)
                && no_extended_metadata(&receipt),
            "Git pack cache generation has additional ownership or metadata"
        );
    }
    let (format, key) = original_name
        .to_str()
        .and_then(|name| name.split_once('-'))
        .context("invalid Git pack cache generation name")?;
    let width = match format {
        "sha1" => 20,
        "sha256" => 32,
        _ => anyhow::bail!("invalid Git pack cache object format"),
    };
    if let Some((base, boundary)) = key.split_once("-shallow-") {
        ObjectId::from_hex(base.as_bytes(), width)?;
        ObjectId::from_hex(boundary.as_bytes(), 32)?;
    } else {
        ObjectId::from_hex(key.as_bytes(), width)?;
    }
    let (receipt, proof) = read_receipt(path, format, key)?;
    for entry in receipt.packs {
        let pack = Pack::at(path, &entry.hash, width)?;
        for (path, artifact) in [(&pack.pack, &entry.pack), (&pack.index, &entry.index)] {
            #[cfg(unix)]
            ensure!(
                crate::recovery::no_extended_metadata(path),
                "Git pack cache file has additional metadata"
            );
            let file = open_regular(path)?;
            ensure!(
                Stamp::read(&file)? == artifact.stamp
                    && digest(&file)? == artifact.digest
                    && Stamp::read(&file)? == artifact.stamp,
                "Git pack cache artifact changed"
            );
        }
    }
    Ok(proof)
}

fn read_receipt(dir: &Path, format: &str, key: &str) -> Result<(Verification, [u8; 32])> {
    private_directory(dir)?;
    let bytes = read_bounded(&dir.join(RECEIPT), RECEIPT_LIMIT)?;
    let receipt: Verification = serde_json::from_slice(&bytes)?;
    ensure!(
        receipt.format == format
            && receipt.base == key
            && !receipt.packs.is_empty()
            && fs::read_dir(dir)?.count() == 2 * receipt.packs.len() + 1,
        "invalid pack cache receipt"
    );
    let mut hashes = HashSet::new();
    ensure!(
        receipt.packs.iter().all(|entry| hashes.insert(&entry.hash)),
        "duplicate cached Git pack"
    );
    Ok((receipt, *blake3::hash(&bytes).as_bytes()))
}

fn reflinks_available() -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
}

fn private_directory(path: &Path) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    ensure!(m.is_dir(), "invalid Git pack cache directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "Git pack cache directory must be private to its owner"
        );
    }
    Ok(())
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_regular(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "Git pack cache metadata is too large"
    );
    Ok(bytes)
}

fn validate_root(root: &Path) -> Result<()> {
    private_directory(root)?;
    ensure!(
        read_bounded(&root.join(MARKER), MAGIC.len() as u64)? == MAGIC,
        "unrecognized Git pack cache directory"
    );
    Ok(())
}

pub(in crate::workspace::objects) fn prepare_root(root: &Path) -> Result<()> {
    if matches!(fs::symlink_metadata(root), Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
        let parent = root.parent().context("missing Git pack cache parent")?;
        let stage = private_tempdir_in(parent)?;
        write_synced(&stage.path().join(MARKER), MAGIC)?;
        sync_directory(stage.path())?;
        match rename_exclusive(stage.path(), root) {
            Ok(()) => {
                let _ = stage.keep();
                sync_directory(parent)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    validate_root(root)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create_new(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub(super) fn private_tempdir_in(parent: &Path) -> Result<tempfile::TempDir> {
    private_tempdir(parent, ".tmp")
}

// Native Git prunes expired tmp_* directories under objects/pack, including
// children left behind when the process was interrupted.
pub(super) fn temporary_pack_dir(parent: &Path) -> Result<tempfile::TempDir> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let dir = fs::symlink_metadata(parent)?;
        ensure!(
            dir.is_dir() && dir.uid() == unsafe { libc::geteuid() } && dir.mode() & 0o022 == 0,
            "donor pack directory is not privately writable"
        );
    }
    private_tempdir(parent, "tmp_pack_graft_")
}

fn private_tempdir(parent: &Path, prefix: &str) -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    Ok(builder.tempdir_in(parent)?)
}

pub(super) fn clone_checked(source: &File, destination: &Path) -> Result<()> {
    let before = Stamp::read(source)?;
    clone_file(source, destination)?;
    ensure!(
        before == Stamp::read(source)?,
        "Git pack changed while cloning"
    );
    Ok(())
}

fn digest(file: &File) -> Result<String> {
    Ok(blake3::Hasher::new()
        .update_reader(file)?
        .finalize()
        .to_hex()
        .to_string())
}

fn publish_file(source: &Path, directory: &Path) -> Result<Artifact> {
    let name = source.file_name().context("missing Git pack filename")?;
    let cloned = directory.join(name);
    clone_checked(&open_regular(source)?, &cloned)?;
    let file = open_regular(&cloned)?;
    #[cfg(unix)]
    file.sync_all()
        .context("sync verified Git pack cache file")?;
    let stamp = Stamp::read(&file)?;
    let digest = digest(&file)?;
    Ok(Artifact { stamp, digest })
}

fn restore_file(expected: &Artifact, source: &Path, destination: &Path) -> Result<()> {
    let source = open_regular(source)?;
    ensure!(
        Stamp::read(&source)? == expected.stamp,
        "cached Git pack file has changed"
    );
    clone_checked(&source, destination)?;
    // Check the bytes that will actually be installed. This also detects a
    // damaged file on a filesystem with coarse change timestamps.
    let cloned = open_regular(destination)?;
    ensure!(
        digest(&cloned)? == expected.digest,
        "cached Git pack file failed verification"
    );
    Ok(())
}
