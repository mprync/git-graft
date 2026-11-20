//! Record installed files' stat data in a private Git index.
//!
//! `read-tree` leaves every stat field empty, so the first `git status` in a new
//! workspace would rehash every file. A native Git checkout avoids that by
//! recording `lstat` data right after it writes each file. Graft does the same
//! for files it cloned from verified donor bytes: the stat is captured from the
//! installed inode immediately after installation, and an index entry is only
//! updated when its stage, mode and object ID match what was verified.
//!
//! Cloned files keep the donor's modification time, so Git never treats them as
//! racily clean. Any later write changes the nanosecond ctime Git compares, so a
//! same-size edit that restores the old mtime is still detected.
use anyhow::{Context, Result, ensure};
use sha2::Digest;
use std::{collections::HashMap, fs, path::Path};

/// The index fields Git compares against `lstat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Stat {
    ctime: (u32, u32),
    mtime: (u32, u32),
    dev: u32,
    ino: u32,
    uid: u32,
    gid: u32,
    size: u32,
}

impl Stat {
    /// Git stores each field truncated to 32 bits, exactly as `fill_stat_data` does.
    #[cfg(unix)]
    pub(super) fn from_libc(info: &libc::stat) -> Self {
        Self {
            ctime: (info.st_ctime as u32, info.st_ctime_nsec as u32),
            mtime: (info.st_mtime as u32, info.st_mtime_nsec as u32),
            dev: info.st_dev as u32,
            ino: info.st_ino as u32,
            uid: info.st_uid,
            gid: info.st_gid,
            size: info.st_size as u32,
        }
    }

    /// Write every 32-bit stat field except the mode at offset 24, which the
    /// tree already determined.
    /// Git's stat fields for an open file.
    #[cfg(unix)]
    pub(super) fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            ctime: (metadata.ctime() as u32, metadata.ctime_nsec() as u32),
            mtime: (metadata.mtime() as u32, metadata.mtime_nsec() as u32),
            dev: metadata.dev() as u32,
            ino: metadata.ino() as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            size: metadata.len() as u32,
        }
    }

    fn write(&self, field: &mut [u8]) {
        let values = [
            (0, self.ctime.0),
            (4, self.ctime.1),
            (8, self.mtime.0),
            (12, self.mtime.1),
            (16, self.dev),
            (20, self.ino),
            (28, self.uid),
            (32, self.gid),
            (36, self.size),
        ];
        for (offset, value) in values {
            field[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
        }
    }
}

/// A verified installation: the object ID and Git mode the bytes were checked against.
pub(super) struct Installed {
    pub(super) blob: String,
    pub(super) mode: u32,
    pub(super) stat: Stat,
}

const HEADER: usize = 12;
const STAT: usize = 40;
const EXTENDED: u16 = 0x4000;
const STAGE: u16 = 0x3000;
const SKIP_WORKTREE: u16 = 0x4000;

/// Fill stat data for `installed` paths and rewrite the index checksum.
/// Returns how many entries were updated.
pub(super) fn record(index: &Path, installed: &HashMap<Vec<u8>, Installed>) -> Result<usize> {
    if installed.is_empty() {
        return Ok(0);
    }
    let mut bytes = fs::read(index).context("read private Git index")?;
    let updated = patch(&mut bytes, installed)?;
    if updated > 0 {
        let parent = index.parent().context("private Git index has no parent")?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut staged, &bytes)?;
        staged
            .persist(index)
            .map_err(|error| error.error)
            .context("replace private Git index")?;
    }
    Ok(updated)
}

fn be32(bytes: &[u8], at: usize) -> Result<u32> {
    let field = bytes.get(at..at + 4).context("truncated Git index")?;
    Ok(u32::from_be_bytes(field.try_into()?))
}

fn be16(bytes: &[u8], at: usize) -> Result<u16> {
    let field = bytes.get(at..at + 2).context("truncated Git index")?;
    Ok(u16::from_be_bytes(field.try_into()?))
}

/// Git's offset varint (`decode_varint` in varint.c).
fn varint(bytes: &[u8], at: &mut usize) -> Result<usize> {
    let mut byte = *bytes.get(*at).context("truncated Git index")?;
    *at += 1;
    let mut value = usize::from(byte & 0x7f);
    while byte & 0x80 != 0 {
        byte = *bytes.get(*at).context("truncated Git index")?;
        *at += 1;
        value = value
            .checked_add(1)
            .and_then(|v| v.checked_mul(128))
            .and_then(|v| v.checked_add(usize::from(byte & 0x7f)))
            .context("invalid Git index path prefix")?;
    }
    Ok(value)
}

/// The location and flags of one index entry.
struct Slot {
    start: usize,
    flags: u16,
    extended: u16,
    path: Vec<u8>,
}

/// Parse a version 2, 3 or 4 index. Returns its entries and where the entries
/// end; the checksum covers everything before the last `width` bytes.
fn walk(bytes: &[u8], width: usize) -> Result<(Vec<Slot>, usize)> {
    ensure!(
        bytes.len() >= HEADER + width && &bytes[..4] == b"DIRC",
        "unsupported Git index"
    );
    let version = be32(bytes, 4)?;
    ensure!((2..=4).contains(&version), "unsupported Git index version");
    let count = be32(bytes, 8)? as usize;
    let body = bytes.len() - width;
    let mut slots = Vec::with_capacity(count.min(body / (STAT + width)));
    let mut at = HEADER;
    let mut path = Vec::new();
    for _ in 0..count {
        let start = at;
        let flags_at = start + STAT + width;
        let flags = be16(bytes, flags_at)?;
        let mut name_at = flags_at + 2;
        let extended = if version >= 3 && flags & EXTENDED != 0 {
            name_at += 2;
            be16(bytes, flags_at + 2)?
        } else {
            0
        };
        ensure!(name_at <= body, "truncated Git index");
        if version == 4 {
            let mut cursor = name_at;
            let strip = varint(bytes, &mut cursor)?;
            ensure!(strip <= path.len(), "invalid Git index path prefix");
            path.truncate(path.len() - strip);
            let end = bytes[cursor..body]
                .iter()
                .position(|&b| b == 0)
                .context("unterminated Git index path")?;
            path.extend_from_slice(&bytes[cursor..cursor + end]);
            at = cursor + end + 1;
        } else {
            let end = bytes[name_at..body]
                .iter()
                .position(|&b| b == 0)
                .context("unterminated Git index path")?;
            path.clear();
            path.extend_from_slice(&bytes[name_at..name_at + end]);
            // Entries are NUL-padded to a multiple of eight bytes.
            let length = name_at - start + end;
            at = start + (length + 8) / 8 * 8;
            ensure!(at <= body, "truncated Git index");
        }
        slots.push(Slot {
            start,
            flags,
            extended,
            path: path.clone(),
        });
    }
    Ok((slots, at))
}

fn digest(bytes: &[u8], width: usize) -> Vec<u8> {
    match width {
        20 => sha1::Sha1::digest(bytes).to_vec(),
        _ => sha2::Sha256::digest(bytes).to_vec(),
    }
}

fn patch(bytes: &mut [u8], installed: &HashMap<Vec<u8>, Installed>) -> Result<usize> {
    // Every installed entry names the same object format.
    let width = installed
        .values()
        .next()
        .map(|entry| entry.blob.len() / 2)
        .context("no installed entries")?;
    ensure!(matches!(width, 20 | 32), "unsupported object ID width");
    let (slots, _) = walk(bytes, width)?;
    let body = bytes.len() - width;
    let trailer_was_zero = bytes[body..].iter().all(|&b| b == 0);
    let mut updated = 0;
    for slot in slots {
        let Some(entry) = installed.get(&slot.path) else {
            continue;
        };
        let start = slot.start;
        let mode = be32(bytes, start + 24)?;
        let oid = &bytes[start + STAT..start + STAT + width];
        if slot.flags & STAGE != 0
            || slot.extended & SKIP_WORKTREE != 0
            || mode != entry.mode
            || hex(oid) != entry.blob
        {
            continue;
        }
        entry.stat.write(&mut bytes[start..start + STAT]);
        updated += 1;
    }
    if updated > 0 && !trailer_was_zero {
        let sum = digest(&bytes[..body], width);
        bytes[body..].copy_from_slice(&sum);
    }
    Ok(updated)
}

/// Stat data from the primary's own index, trusted exactly as `git status`
/// trusts it: an unchanged, non-racy stat means the file still holds the
/// staged blob. Anything unusual leaves a file to be hashed instead.
pub(super) struct Trusted {
    entries: HashMap<Vec<u8>, ([u8; STAT], String)>,
    /// Blob sizes. A checkout conversion (CRLF, `ident`, LFS and similar)
    /// changes the size, so a file whose size differs from its blob's was not
    /// written as raw bytes, even when Git's stat test calls it clean.
    sizes: HashMap<String, u64>,
    /// The index file's modification time; entries at or after it are racy.
    written: (i64, i64),
}

const ASSUME_VALID: u16 = 0x8000;
const INTENT_TO_ADD: u16 = 0x2000;

impl Trusted {
    /// Read `index`, or `None` when it is absent, split, unverifiable or
    /// changed while being read.
    #[cfg(unix)]
    pub(super) fn read(index: &Path, width: usize, sizes: HashMap<String, u64>) -> Option<Self> {
        use std::{io::Read, os::unix::fs::MetadataExt};
        let mut file = fs::File::open(index).ok()?;
        let before = file.metadata().ok()?;
        if !before.is_file() || before.len() > 256 << 20 {
            return None;
        }
        let mut bytes = Vec::with_capacity(before.len() as usize);
        file.read_to_end(&mut bytes).ok()?;
        let after = file.metadata().ok()?;
        if (
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime_nsec(),
        ) != (
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime_nsec(),
        ) {
            return None;
        }
        let (slots, end) = walk(&bytes, width).ok()?;
        let body = bytes.len() - width;
        let trailer = &bytes[body..];
        if trailer.iter().any(|&b| b != 0) && digest(&bytes[..body], width) != trailer {
            return None;
        }
        // A split index keeps most entries in a shared file; trust none of them.
        let mut at = end;
        while at + 8 <= body {
            if &bytes[at..at + 4] == b"link" {
                return None;
            }
            at += 8 + be32(&bytes, at + 4).ok()? as usize;
        }
        let mut entries = HashMap::with_capacity(slots.len());
        for slot in slots {
            if slot.flags & (STAGE | ASSUME_VALID) != 0
                || slot.extended & (SKIP_WORKTREE | INTENT_TO_ADD) != 0
            {
                continue;
            }
            let stat: [u8; STAT] = bytes[slot.start..slot.start + STAT].try_into().ok()?;
            let oid = hex(&bytes[slot.start + STAT..slot.start + STAT + width]);
            entries.insert(slot.path, (stat, oid));
        }
        Some(Self {
            entries,
            sizes,
            written: (after.mtime(), after.mtime_nsec()),
        })
    }

    /// Whether the donor file at `name`, with stat data `stat` of a file
    /// `size` bytes long, still holds the staged `blob` in Git mode `mode` by
    /// Git's own stat test.
    pub(super) fn holds(&self, name: &[u8], blob: &str, mode: u32, stat: &Stat, size: u64) -> bool {
        let Some((recorded, oid)) = self.entries.get(name) else {
            return false;
        };
        let field =
            |at: usize| u32::from_be_bytes(recorded[at..at + 4].try_into().unwrap_or([0; 4]));
        let mut current = [0; STAT];
        stat.write(&mut current);
        let mtime = (i64::from(field(8)), i64::from(field(12)));
        oid == blob
            && field(24) == mode
            && self.sizes.get(blob) == Some(&size)
            && size <= u64::from(u32::MAX)
            // Every stat field except the mode, which Git owns, must match.
            && recorded[..24] == current[..24]
            && recorded[28..] == current[28..]
            && mtime < self.written
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{os::unix::ffi::OsStrExt, process::Command};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn stat(path: &Path) -> Stat {
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
        assert_eq!(unsafe { libc::lstat(name.as_ptr(), info.as_mut_ptr()) }, 0);
        Stat::from_libc(&unsafe { info.assume_init() })
    }

    /// Mimic a Graft checkout: read-tree an index, copy files in, record stats.
    fn checkout(format: &str, version: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        git(
            root.path(),
            &["init", "-q", &format!("--object-format={format}"), "source"],
        );
        fs::create_dir_all(source.join("deep/nested/dir")).unwrap();
        fs::write(source.join("a.txt"), "alpha\n").unwrap();
        fs::write(source.join("deep/nested/dir/b.txt"), "bravo\n").unwrap();
        fs::write(source.join("deep/c.txt"), "charlie\n").unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "files"]);
        let tree = git(&source, &["rev-parse", "HEAD^{tree}"]);
        let target = root.path().join("target");
        git(
            root.path(),
            &["init", "-q", &format!("--object-format={format}"), "target"],
        );
        git(
            &target,
            &[
                "fetch",
                "-q",
                source.to_str().unwrap(),
                "HEAD:refs/heads/main",
            ],
        );
        git(&target, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(
            &target,
            &[
                "-c",
                &format!("index.version={version}"),
                "read-tree",
                tree.trim(),
            ],
        );
        let listing = git(&target, &["ls-files", "--stage", "-z"]);
        let mut installed = HashMap::new();
        for row in listing.split('\0').filter(|row| !row.is_empty()) {
            let (meta, name) = row.split_once('\t').unwrap();
            let mut fields = meta.split(' ');
            let mode = u32::from_str_radix(fields.next().unwrap(), 8).unwrap();
            let blob = fields.next().unwrap().to_owned();
            let path = target.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::copy(source.join(name), &path).unwrap();
            installed.insert(
                name.as_bytes().to_vec(),
                Installed {
                    blob,
                    mode,
                    stat: stat(&path),
                },
            );
        }
        let updated = record(&target.join(".git/index"), &installed).unwrap();
        assert_eq!(updated, 3);
        (root, target)
    }

    #[test]
    fn recorded_stats_make_status_clean_without_rehashing() {
        for format in ["sha1", "sha256"] {
            for version in ["2", "3", "4"] {
                let (_root, target) = checkout(format, version);
                // diff-files only compares stat data; it reports every entry
                // whose stat is unknown without reading file contents.
                assert_eq!(git(&target, &["diff-files", "--name-only"]), "");
                assert_eq!(git(&target, &["status", "--porcelain"]), "");
                let debug = git(&target, &["ls-files", "--debug", "a.txt"]);
                assert!(!debug.contains("ctime: 0:0"), "{debug}");
            }
        }
    }

    #[test]
    fn same_size_edit_with_restored_mtime_is_still_detected() {
        let (_root, target) = checkout("sha1", "4");
        let file = target.join("deep/nested/dir/b.txt");
        let before = fs::metadata(&file).unwrap().modified().unwrap();
        fs::write(&file, "BRAVO\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(before)
            .unwrap();
        assert_eq!(
            git(&target, &["status", "--porcelain"]),
            " M deep/nested/dir/b.txt\n"
        );
    }

    /// The primary's index vouches for a file only as `git status` would, and
    /// only when the file is as large as its blob.
    #[test]
    fn trusted_index_stats_follow_git_and_reject_edits_and_conversions() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        git(root.path(), &["init", "-q", "repo"]);
        fs::write(repo.join("a.txt"), "alpha\n").unwrap();
        fs::write(repo.join("b.txt"), "bravo\n").unwrap();
        git(&repo, &["add", "."]);
        let blob = |name: &str| {
            git(&repo, &["ls-files", "--stage", name])
                .split(' ')
                .nth(1)
                .unwrap()
                .to_owned()
        };
        let (a, b) = (blob("a.txt"), blob("b.txt"));
        let sizes: HashMap<_, _> = [(a.clone(), 6), (b.clone(), 6)].into();
        let index = repo.join(".git/index");
        let trusted = Trusted::read(&index, 20, sizes.clone()).unwrap();
        let check = |trusted: &Trusted, name: &str, oid: &str| {
            let path = repo.join(name);
            let metadata = fs::symlink_metadata(&path).unwrap();
            trusted.holds(
                name.as_bytes(),
                oid,
                0o100644,
                &Stat::from_metadata(&metadata),
                metadata.len(),
            )
        };
        assert!(check(&trusted, "a.txt", &a));
        assert!(!check(&trusted, "a.txt", &b), "a different blob");
        // A same-size edit that restores the old modification time.
        let before = fs::metadata(repo.join("b.txt"))
            .unwrap()
            .modified()
            .unwrap();
        fs::write(repo.join("b.txt"), "BRAVO\n").unwrap();
        fs::File::options()
            .write(true)
            .open(repo.join("b.txt"))
            .unwrap()
            .set_modified(before)
            .unwrap();
        assert!(!check(&trusted, "b.txt", &b));
        // A stat-clean file whose size differs from its blob was converted.
        let converted = Trusted::read(&index, 20, [(a.clone(), 7)].into()).unwrap();
        assert!(!check(&converted, "a.txt", &a));
        // Assume-unchanged entries are never trusted.
        git(&repo, &["update-index", "--assume-unchanged", "a.txt"]);
        let assumed = Trusted::read(&index, 20, sizes).unwrap();
        assert!(!check(&assumed, "a.txt", &a));
    }

    #[test]
    fn mismatched_object_ids_and_modes_are_left_unknown() {
        let (_root, target) = checkout("sha1", "4");
        // Start again from an index without stat data.
        git(&target, &["read-tree", "HEAD"]);
        let row = git(&target, &["ls-files", "--stage", "deep/c.txt"]);
        let blob = row.split(' ').nth(1).unwrap().to_owned();
        let mut installed = HashMap::new();
        installed.insert(
            b"a.txt".to_vec(),
            Installed {
                blob: "0".repeat(40),
                mode: 0o100644,
                stat: stat(&target.join("a.txt")),
            },
        );
        installed.insert(
            b"deep/c.txt".to_vec(),
            Installed {
                blob,
                mode: 0o100755,
                stat: stat(&target.join("deep/c.txt")),
            },
        );
        assert_eq!(record(&target.join(".git/index"), &installed).unwrap(), 0);
    }
}
