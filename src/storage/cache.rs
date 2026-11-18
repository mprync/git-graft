//! Published base snapshots are authenticated against a project-local key
//! stored outside the cache. Unknown versions and polluted bases stay in place.
use super::{
    Entry,
    native::{self, Root},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, File, Metadata},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, symlink},
    },
    path::{Path, PathBuf},
};
use uuid::Uuid;

const KEY: &str = "source-cache-key-v1";
const MANIFEST: &str = "manifest-v1";
const TREE: &str = "tree";

pub(super) type Key = [u8; 32];

pub(super) fn key(state: &Path, create: bool) -> Result<Key> {
    let root = Root::open(state)?;
    if create
        && matches!(fs::symlink_metadata(state.join(KEY)), Err(e) if e.kind() == io::ErrorKind::NotFound)
    {
        let mut secret = [0; 32];
        secret[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        secret[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        let mut temp = tempfile::NamedTempFile::new_in(state)?;
        temp.write_all(&secret)?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(state.join(KEY))?;
    }
    let mut file = root.regular(Path::new(KEY))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.len() == 32 && metadata.mode() & 0o077 == 0,
        "invalid source cache key"
    );
    let mut secret = [0; 32];
    file.read_exact(&mut secret)?;
    Ok(secret)
}

/// Git owns the tracked mode. A cached inode also has size and change-time
/// constraints, so an edit, rename, chmod, xattr or hardlink invalidates it.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    mode: u32,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<&Metadata> for Stamp {
    fn from(m: &Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            mode: m.mode(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        }
    }
}

#[derive(Serialize, Deserialize)]
enum Content {
    Regular(Stamp),
    Link(Vec<u8>),
}

#[derive(Serialize, Deserialize)]
pub(super) struct Record {
    pub(super) entry: Entry,
    data: Content,
}

impl Record {
    fn capture(root: &Root, entry: &Entry) -> Result<Self> {
        let path = entry.path()?;
        let data = if entry.regular() {
            let (file, metadata) = root.regular_metadata(path)?;
            ensure!(
                native::plain(&file, &metadata)?,
                "cached file has extended attributes or flags"
            );
            Content::Regular(Stamp::from(&metadata))
        } else {
            Content::Link(root.link(path)?.into_vec())
        };
        Ok(Self {
            entry: entry.clone(),
            data,
        })
    }

    fn matching_file(&self, root: &Root, stamp: &Stamp) -> Result<(File, Metadata)> {
        let (file, metadata) = root.regular_metadata(self.entry.path()?)?;
        ensure!(Stamp::from(&metadata) == *stamp, "cached file changed");
        Ok((file, metadata))
    }

    fn verify(&self, root: &Root) -> Result<()> {
        match &self.data {
            Content::Regular(stamp) => {
                self.matching_file(root, stamp)?;
            }
            Content::Link(target) => {
                ensure!(
                    root.link(self.entry.path()?)?.as_encoded_bytes() == target,
                    "cached link changed"
                );
            }
        }
        Ok(())
    }

    fn clone_into(
        &self,
        root: &Root,
        destination: &Root,
        output: &Path,
        mode: u32,
    ) -> Result<Option<u64>> {
        match &self.data {
            Content::Regular(stamp) => {
                let (source, metadata) = self.matching_file(root, stamp)?;
                let path = self.entry.path()?;
                destination.clone_regular(
                    path,
                    &source,
                    &metadata,
                    self.entry.creation_mode(mode),
                    true,
                )?;
                if Stamp::from(&source.metadata()?) != *stamp {
                    destination.discard(path)?;
                    anyhow::bail!("cached file changed while cloning");
                }
                Ok(Some(stamp.size))
            }
            Content::Link(target) => {
                self.verify(root)?;
                symlink(OsString::from_vec(target.clone()), output)?;
                Ok(None)
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    commit: String,
    settings: String,
    records: Vec<Record>,
}

pub(super) struct Snapshot {
    source: Root,
    manifest: Manifest,
}

impl Snapshot {
    pub(super) fn load(base: &Path, secret: &Key, oid: &str, settings: &str) -> Result<Self> {
        let root = Root::open(base)?;
        let names: BTreeSet<_> = root.names(Path::new(""))?.into_iter().collect();
        ensure!(
            names == [MANIFEST.into(), TREE.into()].into(),
            "unrecognized source snapshot entries"
        );
        let mut file = root.regular(Path::new(MANIFEST))?;
        let mut signature = [0; 32];
        file.read_exact(&mut signature)?;
        let mut payload = Vec::new();
        file.read_to_end(&mut payload)?;
        ensure!(
            blake3::keyed_hash(secret, &payload) == signature,
            "source snapshot provenance changed"
        );
        let manifest: Manifest = serde_json::from_slice(&payload)?;
        ensure!(
            manifest.commit == oid && manifest.settings == settings,
            "source snapshot belongs to another checkout"
        );
        let source = root.nested(Path::new(TREE))?;
        check_inventory(&source, manifest.records.iter().map(|record| &record.entry))?;
        Ok(Self { source, manifest })
    }

    pub(super) fn verify(&self) -> Result<()> {
        for record in self.records() {
            record.verify(&self.source)?;
        }
        Ok(())
    }

    pub(super) fn records(&self) -> &[Record] {
        &self.manifest.records
    }

    pub(super) fn clone_record(
        &self,
        record: &Record,
        destination: &Root,
        output: &Path,
        mode: u32,
    ) -> Result<Option<u64>> {
        record.clone_into(&self.source, destination, output, mode)
    }

    pub(super) fn publish(
        temporary: &Path,
        base: &Path,
        entries: &[Entry],
        secret: &Key,
        oid: &str,
        settings: &str,
    ) -> Result<Self> {
        let root = Root::open(&temporary.join(TREE))?;
        check_inventory(&root, entries.iter())?;
        let records = entries
            .iter()
            .map(|entry| Record::capture(&root, entry))
            .collect::<Result<Vec<_>>>()?;
        let manifest = Manifest {
            commit: oid.into(),
            settings: settings.into(),
            records,
        };
        let payload = serde_json::to_vec(&manifest)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary.join(MANIFEST))?;
        file.write_all(blake3::keyed_hash(secret, &payload).as_bytes())?;
        file.write_all(&payload)?;
        native::rename_new(temporary, base)?;
        Ok(Self {
            source: root,
            manifest,
        })
    }
}

fn check_inventory<'a>(root: &Root, entries: impl Iterator<Item = &'a Entry>) -> Result<()> {
    let mut expected: BTreeMap<&Path, BTreeSet<&OsStr>> = BTreeMap::new();
    expected.entry(Path::new("")).or_default();
    for entry in entries {
        let mut path = entry.path()?;
        while let Some(parent) = path.parent() {
            let name = path.file_name().context("expected a Git path name")?;
            if !expected.entry(parent).or_default().insert(name) {
                break;
            }
            path = parent;
        }
    }
    for (directory, expected_names) in expected {
        let names = root.names(directory)?;
        ensure!(
            names.len() == expected_names.len()
                && names
                    .iter()
                    .all(|name| expected_names.contains(name.as_os_str())),
            "unrecognized source snapshot payload"
        );
    }
    Ok(())
}

/// A whole commit directory can be retired only when every child is a trusted,
/// unchanged generation. An empty directory carries no ownership evidence.
pub(super) fn owned_base(path: &Path) -> Result<()> {
    let oid = path
        .file_name()
        .and_then(OsStr::to_str)
        .context("invalid source snapshot commit")?;
    crate::git::checked_oid(oid)?;
    let state = path
        .parent()
        .and_then(Path::parent)
        .context("missing source cache state")?;
    let secret = key(state, false)?;
    let root = Root::open(path)?;
    let names = root.names(Path::new(""))?;
    ensure!(!names.is_empty(), "empty source snapshot");
    for name in names {
        let settings = name.to_str().context("invalid checkout settings key")?;
        ensure!(
            settings.len() == 64 && settings.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid checkout settings key"
        );
        Snapshot::load(&path.join(&name), &secret, oid, settings)?.verify()?;
    }
    Ok(())
}

pub(super) fn tree(path: &Path) -> PathBuf {
    path.join(TREE)
}
