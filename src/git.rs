//! Git porcelain and plumbing used by the workspace lifecycle. Paths in
//! porcelain records use NUL delimiters so whitespace never becomes syntax.
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::json;
use std::{
    fs, io,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use crate::api::fault;

pub(crate) const MINIMUM_VERSION: &str = "2.46.0";

pub(crate) fn supported_version(version: &str) -> bool {
    let Some(token) = version
        .strip_prefix("git version ")
        .and_then(|v| v.split_whitespace().next())
    else {
        return false;
    };
    let mut fields = token.split('.');
    let Some(major) = fields.next().and_then(|n| n.parse::<u32>().ok()) else {
        return false;
    };
    let Some(minor) = fields.next().and_then(|n| n.parse::<u32>().ok()) else {
        return false;
    };
    (major, minor) > (2, 46)
        || ((major, minor) == (2, 46) && !(token.starts_with("2.46.0") && token.contains("rc")))
}

#[test]
fn minimum_git_includes_vendor_builds() {
    for supported in [
        "2.46.0",
        "2.46.0.windows.1",
        "2.46.1.rc0",
        "2.47.0 (Apple Git-154)",
        "3.0.0",
    ] {
        assert!(supported_version(&format!("git version {supported}")));
    }
    for unsupported in [
        "git version 2.45.4",
        "git version 2.46.0-rc1",
        "unknown version",
    ] {
        assert!(!supported_version(unsupported));
    }
}

/// Set repository selection explicitly, even if called from inside a Git hook.
pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    clear_location(&mut cmd);
    cmd.arg("-C")
        .arg(dir)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    cmd
}

/// Read optional Git metadata within a fixed memory budget. Closing the pipe
/// before waiting also releases a writer whose output exceeded the limit.
pub(crate) fn bounded_stdout(command: &mut Command, limit: u64) -> Result<Vec<u8>> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .context("missing Git metadata output")?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes);
    let status = child.wait()?;
    ensure!(
        read? as u64 <= limit && status.success(),
        "cannot read bounded Git metadata"
    );
    Ok(bytes)
}

/// Read and write the owned object database of a private bare repository while
/// retaining the caller's repository configuration and checkout context.
pub(crate) fn use_private_objects(command: &mut Command, bare: &Path) {
    command
        .env("GIT_OBJECT_DIRECTORY", bare.join("objects"))
        .env("GIT_SHALLOW_FILE", bare.join("shallow"))
        .args([
            "-c",
            "core.fsync=all",
            "-c",
            "core.fsyncMethod=fsync",
            "-c",
            "core.commitGraph=false",
        ]);
}

/// Create a recovery pin inside an owned repository without running user hooks.
pub(crate) fn create_private_ref(repo: &Path, reference: &str, oid: &str) -> Result<()> {
    let hooks = if cfg!(windows) {
        "core.hooksPath=NUL"
    } else {
        "core.hooksPath=/dev/null"
    };
    bytes(
        repo,
        &[
            "-c",
            hooks,
            "-c",
            "core.fsync=all",
            "-c",
            "core.fsyncMethod=fsync",
            "update-ref",
            "--no-deref",
            reference,
            oid,
            &"0".repeat(oid.len()),
        ],
    )?;
    Ok(())
}

/// Forget the calling checkout before starting Git or a workspace subprocess.
pub fn clear_location(cmd: &mut Command) {
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_QUARANTINE_PATH",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_SHALLOW_FILE",
        "GIT_CONFIG",
        // Git exports command-line configuration to hooks and external commands.
        // Use the selected repository's normal configuration.
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_GRAFT_FILE",
        "GIT_REPLACE_REF_BASE",
        "GIT_ATTR_SOURCE",
        "GIT_GRAFT_LEASE",
    ] {
        cmd.env_remove(key);
    }
}
pub fn raw(dir: &Path, args: &[&str]) -> Result<Output> {
    command(dir).args(args).output().context("start Git")
}
pub fn bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = raw(dir, args)?;
    ensure!(
        out.status.success(),
        "git {} in {}: {}",
        args[0],
        dir.display(),
        String::from_utf8_lossy(&out.stderr).trim_end()
    );
    Ok(out.stdout)
}
pub fn text(dir: &Path, args: &[&str]) -> Result<String> {
    let mut out = String::from_utf8(bytes(dir, args)?)
        .context("Git returned a non-UTF-8 path or revision")?;
    if out.ends_with('\n') {
        out.pop();
    }
    if cfg!(windows) && out.ends_with('\r') {
        out.pop();
    }
    Ok(out)
}
pub fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .context("repository and workspace roots must be UTF-8")
}
pub fn checked_oid(oid: &str) -> Result<&str> {
    ensure!(
        matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected a full Git object ID"
    );
    Ok(oid)
}

pub fn symbolic_ref(dir: &Path, reference: &str) -> Result<Option<String>> {
    let out = raw(dir, &["symbolic-ref", "--quiet", "--no-recurse", reference])?;
    match out.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8(out.stdout)?
                .trim_end_matches(['\n', '\r'])
                .into(),
        )),
        Some(1) => Ok(None),
        _ => bail!(
            "read symbolic ref {reference}: {}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        ),
    }
}

pub fn oid(dir: &Path, revision: &str) -> Result<String> {
    text(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ],
    )
}

/// Resolve Git's primary administrative directory, even from a linked checkout.
pub(crate) fn common_dir(dir: &Path) -> Result<PathBuf> {
    Ok(dunce::canonicalize(text(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?)?)
}

/// Resolve the administrative directory selected by Git for this checkout.
pub(crate) fn absolute_git_dir(dir: &Path) -> Result<PathBuf> {
    Ok(dunce::canonicalize(text(
        dir,
        &["rev-parse", "--absolute-git-dir"],
    )?)?)
}

#[derive(Serialize)]
pub struct Change {
    pub status: String,
    pub path: String,
}
pub fn changes(dir: &Path) -> Result<Vec<Change>> {
    let raw = bytes(
        dir,
        &[
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=normal",
            "--ignored=matching",
            "--no-renames",
            "--ignore-submodules=none",
        ],
    )?;
    raw.split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| {
            ensure!(r.len() > 3 && r[2] == b' ', "malformed Git status record");
            Ok(Change {
                status: std::str::from_utf8(&r[..2])?.into(),
                path: String::from_utf8_lossy(&r[3..]).into_owned(),
            })
        })
        .collect()
}

/// A clean porcelain status can hide user work behind index flags or a paused
/// operation. Fail closed before committing, integrating or retiring a tree.
pub fn check_idle(dir: &Path) -> Result<()> {
    check_idle_with_locks(dir, false, false)
}

pub(crate) fn check_idle_with_locks(dir: &Path, index: bool, head: bool) -> Result<()> {
    let admin = absolute_git_dir(dir)?;
    for mark in [
        "MERGE_HEAD",
        "MERGE_AUTOSTASH",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "BISECT_START",
        "index.lock",
        "HEAD.lock",
    ] {
        if (index && mark == "index.lock") || (head && mark == "HEAD.lock") {
            continue;
        }
        match fs::symlink_metadata(admin.join(mark)) {
            Ok(_) => {
                return Err(fault(
                    "git_busy",
                    format!("Finish the Git operation in {} first", dir.display()),
                    json!({"marker":mark}),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    // Read flags and conflict stages from a single view of the index. Paths
    // may contain tabs or newlines, so only the first tab delimits the header.
    let raw = bytes(dir, &["ls-files", "-v", "--stage", "-z"])?;
    for row in raw.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let tab = row
            .iter()
            .position(|b| *b == b'\t')
            .context("invalid Git index entry")?;
        let (header, path) = (&row[..tab], &row[tab + 1..]);
        ensure!(
            header.ends_with(b" 0"),
            "Resolve the workspace index conflicts first"
        );
        let skips_present_file = if header[0] == b'S' {
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStrExt;
                dir.join(std::ffi::OsStr::from_bytes(path))
            };
            #[cfg(not(unix))]
            let path = dir.join(std::str::from_utf8(path)?);
            match fs::symlink_metadata(path) {
                Ok(_) => true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.into()),
            }
        } else {
            false
        };
        if header[0].is_ascii_lowercase() || skips_present_file {
            return Err(fault(
                "hidden_changes",
                "Clear assume-unchanged or skip-worktree flags on present files first",
                json!({"path":String::from_utf8_lossy(path)}),
            ));
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn internal_git_does_not_fetch_missing_promisor_objects() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        let partial = temp.path().join("partial");
        fs::create_dir(&source)?;
        bytes(&source, &["init", "-q", "--template="])?;
        for (key, value) in [
            ("user.name", "Offline Fixture"),
            ("user.email", "offline@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "disabled-hooks"),
            ("uploadpack.allowFilter", "true"),
            ("uploadpack.allowAnySHA1InWant", "true"),
        ] {
            bytes(&source, &["config", key, value])?;
        }
        fs::create_dir(source.join("folder"))?;
        fs::write(source.join("folder/file"), b"local source\n")?;
        bytes(&source, &["add", "folder"])?;
        bytes(&source, &["commit", "-qm", "base"])?;
        bytes(
            temp.path(),
            &[
                "clone",
                "-q",
                "--no-checkout",
                "--filter=tree:0",
                &format!("file://{}", path_str(&dunce::canonicalize(&source)?)?),
                path_str(&partial)?,
            ],
        )?;
        let packs = || -> Result<usize> {
            Ok(fs::read_dir(partial.join(".git/objects/pack"))?
                .collect::<io::Result<Vec<_>>>()?
                .len())
        };

        let before = packs()?;
        assert!(
            !raw(&partial, &["cat-file", "-t", "HEAD:folder"])?
                .status
                .success()
        );
        assert_eq!(packs()?, before);
        let control = command(&partial)
            .env_remove("GIT_NO_LAZY_FETCH")
            .args(["cat-file", "-t", "HEAD:folder"])
            .output()?;
        assert!(control.status.success());
        assert!(packs()? > before);
        Ok(())
    }
}
