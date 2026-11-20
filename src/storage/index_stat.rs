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

fn patch(bytes: &mut [u8], installed: &HashMap<Vec<u8>, Installed>) -> Result<usize> {
    ensure!(
        bytes.len() >= HEADER && &bytes[..4] == b"DIRC",
        "unsupported Git index"
    );
    let version = be32(bytes, 4)?;
    ensure!((2..=4).contains(&version), "unsupported Git index version");
    let count = be32(bytes, 8)? as usize;
    // Every installed entry names the same object format.
    let width = installed
        .values()
        .next()
        .map(|entry| entry.blob.len() / 2)
        .context("no installed entries")?;
    ensure!(matches!(width, 20 | 32), "unsupported object ID width");
    ensure!(bytes.len() >= HEADER + width, "truncated Git index");
    let body = bytes.len() - width;
    let trailer_was_zero = bytes[body..].iter().all(|&b| b == 0);

    let mut at = HEADER;
    let mut path = Vec::new();
    let mut updated = 0;
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
        let Some(entry) = installed.get(&path) else {
            continue;
        };
        let mode = be32(bytes, start + 24)?;
        let oid = &bytes[start + STAT..start + STAT + width];
        if flags & STAGE != 0
            || extended & SKIP_WORKTREE != 0
            || mode != entry.mode
            || hex(oid) != entry.blob
        {
            continue;
        }
        entry.stat.write(&mut bytes[start..start + STAT]);
        updated += 1;
    }
    if updated > 0 && !trailer_was_zero {
        let digest = match width {
            20 => sha1::Sha1::digest(&bytes[..body]).to_vec(),
            _ => sha2::Sha256::digest(&bytes[..body]).to_vec(),
        };
        bytes[body..].copy_from_slice(&digest);
    }
    Ok(updated)
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
