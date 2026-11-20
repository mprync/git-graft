//! Filesystem and ref operations used for durable workspace transitions.
use crate::git;
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    process::{Command, Stdio},
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn rename_exclusive(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    rename_exclusive_at(libc::AT_FDCWD, &source, libc::AT_FDCWD, &destination)
}

/// Rename `source` under `source_dir` to `destination` under
/// `destination_dir`. Exclusive rename cannot replace anything, not even an
/// empty directory created after a preflight check.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn rename_exclusive_at(
    source_dir: std::os::fd::RawFd,
    source: &std::ffi::CStr,
    destination_dir: std::os::fd::RawFd,
    destination: &std::ffi::CStr,
) -> std::io::Result<()> {
    // Both terminated paths live through the call.
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            source_dir,
            source.as_ptr(),
            destination_dir,
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    // The raw syscall: musl, used for the static release builds, has no
    // renameat2 wrapper.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            source_dir,
            source.as_ptr(),
            destination_dir,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
pub(crate) fn rename_exclusive(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<_> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // No REPLACE_EXISTING or COPY_ALLOWED: move the directory on its volume.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn rename_exclusive(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

pub(crate) fn direct_ref(dir: &Path, reference: &str) -> Result<Option<String>> {
    let output = git::raw(dir, &["show-ref", "--verify", "--hash", reference])?;
    if !output.status.success() {
        // The exact lookup distinguishes absence from dangling symbolic
        // refs and malformed refs. Retain the older checks for diagnostics
        // and for callers using Git without show-ref --exists.
        let exists = git::raw(dir, &["show-ref", "--exists", reference])?;
        if exists.status.code() == Some(2) {
            return Ok(None);
        }
        let verify = git::raw(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                reference,
            ],
        )?;
        ensure!(
            verify.status.code() == Some(1) && verify.stderr.is_empty(),
            "cannot resolve {reference}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            git::symbolic_ref(dir, reference)?.is_none(),
            "{reference} is symbolic"
        );
        return Ok(None);
    }
    let oid = String::from_utf8(output.stdout)?.trim_end().to_owned();
    git::checked_oid(&oid).with_context(|| format!("invalid object ID for {reference}"))?;
    let (listed_oid, symbolic) = listed_ref(dir, reference)?
        .with_context(|| format!("Git did not list the exact ref {reference}"))?;
    ensure!(symbolic.is_empty(), "{reference} is symbolic");
    ensure!(listed_oid == oid, "{reference} changed during lookup");
    Ok(Some(oid))
}

/// The object ID and symbolic target that Git lists for exactly `reference`.
/// Matching the exact name avoids a differently cased ref on case-insensitive
/// filesystems standing in for the requested one.
pub(crate) fn listed_ref(dir: &Path, reference: &str) -> Result<Option<(String, String)>> {
    let refs = git::text(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
            reference,
        ],
    )?;
    for row in refs.lines() {
        if let Some((name, fields)) = row.split_once('\0')
            && name == reference
        {
            let (oid, symbolic) = fields.split_once('\0').context("missing ref fields")?;
            return Ok(Some((oid.to_owned(), symbolic.to_owned())));
        }
    }
    Ok(None)
}

pub(crate) fn alternate_objects(command: &mut Command, objects: &Path) -> Result<()> {
    // Git accepts a C-quoted entry so a colon or semicolon inside this single
    // absolute path cannot introduce another alternate.
    let path = git::path_str(objects)?;
    let quoted = format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""));
    command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", quoted);
    Ok(())
}

pub(crate) fn transaction_with_objects(
    dir: &Path,
    edits: &str,
    message: Option<&str>,
    objects: Option<&Path>,
    check: impl FnOnce() -> Result<()>,
) -> Result<()> {
    // Reference-transaction hooks may produce more than a pipe can hold
    // while we wait for Git's prepare acknowledgement on stdout.
    let mut diagnostics = tempfile::tempfile()?;
    let mut command = git::command(dir);
    if let Some(objects) = objects {
        alternate_objects(&mut command, objects)?;
    }
    command.args([
        "-c",
        "core.fsync=reference",
        "-c",
        "core.fsyncMethod=fsync",
        "update-ref",
        "--stdin",
    ]);
    if let Some(message) = message {
        command.args(["-m", message]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(diagnostics.try_clone()?)
        .spawn()?;
    let mut rejected = false;
    let attempt = (|| -> Result<()> {
        let mut input = child
            .stdin
            .take()
            .context("missing ref transaction input")?;
        let mut reply = BufReader::new(
            child
                .stdout
                .take()
                .context("missing ref transaction output")?,
        );
        write!(input, "start\n{edits}prepare\n")?;
        input.flush()?;
        let mut line = String::new();
        reply.read_line(&mut line)?;
        ensure!(
            line.trim_end() == "start: ok",
            "Git could not start a ref transaction"
        );
        line.clear();
        reply.read_line(&mut line)?;
        ensure!(
            line.trim_end() == "prepare: ok",
            "Git could not lock the refs"
        );
        // no-deref + an old OID alone also permits a symbolic ref resolving to
        // that OID. Check the type while Git holds the prepared ref lock.
        if let Err(error) = check() {
            rejected = true;
            return Err(error);
        }
        input.write_all(b"commit\n")?;
        input.flush()?;
        line.clear();
        reply.read_line(&mut line)?;
        ensure!(
            line.trim_end() == "commit: ok",
            "Git could not commit the refs"
        );
        Ok(())
    })();
    let status = child.wait()?;
    if !status.success() && !rejected {
        let length = diagnostics.metadata()?.len();
        if length > 0 {
            diagnostics.seek(SeekFrom::Start(length.saturating_sub(16 * 1024)))?;
            let mut tail = Vec::new();
            diagnostics.take(16 * 1024).read_to_end(&mut tail)?;
            bail!("{}", String::from_utf8_lossy(&tail).trim_end());
        }
        ensure!(attempt.is_err(), "Git ref transaction failed: {status}");
    }
    attempt
}

#[cfg(target_os = "linux")]
pub(crate) fn no_extended_metadata(path: &Path) -> bool {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // A zero-sized, null buffer queries only the length of the xattr list.
    unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) == 0 }
}

#[cfg(all(test, windows))]
mod windows_metadata_tests {
    use super::no_extended_metadata;
    use std::{fs, io::Write};

    #[test]
    fn rejects_named_streams_on_files_and_directories() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let file = temp.path().join("file");
        let directory = temp.path().join("directory");
        fs::write(&file, b"data")?;
        fs::create_dir(&directory)?;
        assert!(no_extended_metadata(&file));
        assert!(no_extended_metadata(&directory));
        for path in [&file, &directory] {
            let stream = format!("{}:private", path.display());
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(stream)?
                .write_all(b"preserve")?;
            assert!(!no_extended_metadata(path));
        }
        Ok(())
    }

    #[test]
    fn rejects_directory_reparse_points_when_available() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        fs::create_dir(&target)?;
        if std::os::windows::fs::symlink_dir(&target, &link).is_ok() {
            assert!(!no_extended_metadata(&link));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn no_extended_metadata(path: &Path) -> bool {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    unsafe extern "C" {
        fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_uint) -> *mut libc::c_void;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            id: libc::c_int,
            entry: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_free(data: *mut libc::c_void) -> libc::c_int;
    }
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // lstat and the metadata calls operate on the entry, without following it.
    unsafe {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if libc::lstat(path.as_ptr(), stat.as_mut_ptr()) != 0 || stat.assume_init().st_flags != 0 {
            return false;
        }
        // macOS stamps files written by some apps with system provenance,
        // which it regenerates on creation. All user metadata keeps the tree.
        let provenance = b"com.apple.provenance\0";
        let mut names = [0; b"com.apple.provenance\0".len()];
        let count = libc::listxattr(
            path.as_ptr(),
            names.as_mut_ptr().cast(),
            names.len(),
            libc::XATTR_NOFOLLOW,
        );
        if count != 0 && (count != names.len() as isize || &names != provenance) {
            return false;
        }
        let acl = acl_get_link_np(path.as_ptr(), 0x100); // ACL_TYPE_EXTENDED
        if acl.is_null() {
            return std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT);
        }
        let result = acl_get_entry(acl, 0, &mut std::ptr::null_mut()); // ACL_FIRST_ENTRY
        let error = std::io::Error::last_os_error().raw_os_error();
        acl_free(acl);
        result == -1 && error == Some(libc::EINVAL)
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
pub(crate) fn no_extended_metadata(_path: &Path) -> bool {
    false
}

#[cfg(windows)]
pub(crate) fn no_extended_metadata(path: &Path) -> bool {
    use std::{
        fs::OpenOptions,
        os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_HANDLE_EOF, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FindClose, FindFirstStreamW,
            FindNextStreamW, FindStreamInfoStandard, GetFileInformationByHandle,
            WIN32_FIND_STREAM_DATA,
        },
    };

    // Open the entry itself, not a junction or mounted volume. Compare its
    // identity again after the path-based stream enumeration.
    let inspect = || -> Option<BY_HANDLE_FILE_INFORMATION> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .ok()?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0
            || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return None;
        }
        Some(info)
    };
    let Some(before) = inspect() else {
        return false;
    };
    let directory = before.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut data = WIN32_FIND_STREAM_DATA::default();
    let handle = unsafe {
        FindFirstStreamW(
            wide.as_ptr(),
            FindStreamInfoStandard,
            (&mut data as *mut WIN32_FIND_STREAM_DATA).cast(),
            0,
        )
    };
    let clean = if handle == INVALID_HANDLE_VALUE {
        // Directories normally have no $DATA streams. Any other failure is
        // inconclusive (including filesystems without stream enumeration).
        directory && std::io::Error::last_os_error().raw_os_error() == Some(ERROR_HANDLE_EOF as i32)
    } else {
        let end = data
            .cStreamName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.cStreamName.len());
        let unnamed: Vec<u16> = "::$DATA".encode_utf16().collect();
        let only_default = !directory && data.cStreamName[..end] == unnamed;
        let no_next =
            unsafe { FindNextStreamW(handle, (&mut data as *mut WIN32_FIND_STREAM_DATA).cast()) }
                == 0
                && std::io::Error::last_os_error().raw_os_error() == Some(ERROR_HANDLE_EOF as i32);
        let closed = unsafe { FindClose(handle) } != 0;
        only_default && no_next && closed
    };
    let Some(after) = inspect() else { return false };
    clean
        && before.dwVolumeSerialNumber == after.dwVolumeSerialNumber
        && before.nFileIndexHigh == after.nFileIndexHigh
        && before.nFileIndexLow == after.nFileIndexLow
        && before.dwFileAttributes == after.dwFileAttributes
}
