//! Detect checkout paths that the filesystem resolves to an ignored entry.
//!
//! Comparing path bytes cannot follow every filesystem's folding rules: APFS
//! and NTFS fold case beyond ASCII, and APFS also treats NFC and NFD spellings
//! as the same name. Instead of reimplementing those rules, ask the filesystem
//! which directory entry each incoming path resolves to and compare file
//! identities with the ignored entries Git reported.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;

/// A file's device and file number, read without following a final symlink.
type Identity = (u64, u64);

/// Return the first changed path that the filesystem resolves onto an ignored
/// entry, onto a path inside one, or onto a directory holding one.
///
/// `ignored` and `changed` are slash-separated paths relative to `root`, as
/// Git prints them with `-z`.
pub(crate) fn ignored_collision<'a>(
    root: &Path,
    ignored: &[&[u8]],
    changed: &[&'a [u8]],
) -> Result<Option<&'a [u8]>> {
    let mut cache = HashMap::new();
    let mut entries = HashSet::new();
    let mut holders = HashSet::new();
    for path in ignored {
        let path = path.strip_suffix(b"/").unwrap_or(path);
        if let Some(identity) = identity(root, path, &mut cache)? {
            entries.insert(identity);
        }
        for prefix in prefixes(path) {
            if let Some(identity) = identity(root, prefix, &mut cache)? {
                holders.insert(identity);
            }
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    for path in changed {
        let exact = identity(root, path, &mut cache)?;
        if exact.is_some_and(|id| entries.contains(&id) || holders.contains(&id)) {
            return Ok(Some(path));
        }
        for prefix in prefixes(path) {
            if identity(root, prefix, &mut cache)?.is_some_and(|id| entries.contains(&id)) {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

/// The proper ancestors of a relative path, shortest first.
fn prefixes(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(move |(index, _)| &path[..index])
        .filter(|prefix| !prefix.is_empty())
}

fn identity(
    root: &Path,
    path: &[u8],
    cache: &mut HashMap<Vec<u8>, Option<Identity>>,
) -> Result<Option<Identity>> {
    if let Some(known) = cache.get(path) {
        return Ok(*known);
    }
    let found = match read_identity(&join(root, path)?) {
        Ok(identity) => Some(identity),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    cache.insert(path.to_vec(), found);
    Ok(found)
}

fn join(root: &Path, path: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(root.join(std::ffi::OsStr::from_bytes(path)))
    }
    #[cfg(windows)]
    {
        Ok(root.join(std::str::from_utf8(path)?))
    }
}

#[cfg(unix)]
fn read_identity(path: &Path) -> io::Result<Identity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn read_identity(path: &Path) -> io::Result<Identity> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // The handle and output storage are live through this call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Whether `root` folds non-ASCII case, as APFS and NTFS do by default.
    fn folds_unicode_case(root: &Path) -> Result<bool> {
        fs::write(root.join("probe-\u{c9}"), "")?;
        let folded = root.join("probe-\u{e9}").exists();
        fs::remove_file(root.join("probe-\u{c9}"))?;
        Ok(folded)
    }

    #[test]
    fn exact_ancestor_and_descendant_collisions_are_found() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        fs::create_dir_all(root.join("build/out"))?;
        fs::write(root.join("build/out/app"), "ignored build output\n")?;
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/.env"), "SECRET=1\n")?;
        fs::write(root.join("src/main.rs"), "tracked\n")?;
        let ignored: &[&[u8]] = &[b"build/", b"src/.env"];
        let find = |changed: &'static [u8]| ignored_collision(root, ignored, &[changed]);
        assert_eq!(find(b"build/out/app")?, Some(&b"build/out/app"[..]));
        assert_eq!(find(b"build")?, Some(&b"build"[..]));
        assert_eq!(find(b"src/.env")?, Some(&b"src/.env"[..]));
        assert_eq!(find(b"src")?, Some(&b"src"[..]));
        assert_eq!(find(b"src/main.rs")?, None);
        assert_eq!(find(b"new/file")?, None);
        Ok(())
    }

    #[test]
    fn folded_unicode_spellings_match_where_the_filesystem_folds_them() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        // An uppercase E with acute accent, written decomposed (NFD).
        fs::create_dir(root.join("CAFE\u{301}"))?;
        fs::write(root.join("CAFE\u{301}/secret"), "keep\n")?;
        let ignored: &[&[u8]] = &["CAFE\u{301}/".as_bytes()];
        // Lowercase and precomposed (NFC).
        let incoming = "caf\u{e9}/secret".as_bytes();
        let found = ignored_collision(root, ignored, &[incoming])?;
        let same = root.join("caf\u{e9}").exists();
        assert_eq!(found.is_some(), same);
        if folds_unicode_case(root)? && cfg!(target_os = "macos") {
            assert_eq!(found, Some(incoming));
        }
        Ok(())
    }
}
