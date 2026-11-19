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
    // Both terminated paths live through the syscall. Exclusive rename cannot
    // replace even an empty directory created after our preflight checks.
    #[cfg(target_os = "macos")]
    let result =
        unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL) };
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
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
            let (listed_oid, symbolic) = fields.split_once('\0').context("missing ref fields")?;
            ensure!(symbolic.is_empty(), "{reference} is symbolic");
            ensure!(listed_oid == oid, "{reference} changed during lookup");
            return Ok(Some(oid));
        }
    }
    bail!("Git did not list the exact ref {reference}")
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
