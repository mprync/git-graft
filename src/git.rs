//! Git porcelain and plumbing used by the workspace lifecycle. Paths in
//! porcelain records use NUL delimiters so whitespace never becomes syntax.
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use serde_json::json;
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use crate::api::fault;

/// Set repository selection explicitly, even if called from inside a Git hook.
pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    clear_location(&mut cmd);
    cmd.arg("-C")
        .arg(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    cmd
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

#[derive(Default, Debug, Serialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub locked: bool,
    pub bare: bool,
}
pub fn worktrees(dir: &Path) -> Result<Vec<Worktree>> {
    let raw = bytes(dir, &["worktree", "list", "--porcelain", "-z"])?;
    let mut result = Vec::new();
    for field in raw.split(|&b| b == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            result.push(Worktree {
                path: PathBuf::from(std::str::from_utf8(path)?),
                ..Default::default()
            });
        } else if let Some(wt) = result.last_mut() {
            if let Some(branch) = field.strip_prefix(b"branch ") {
                wt.branch = Some(std::str::from_utf8(branch)?.to_owned());
            }
            wt.bare |= field == b"bare";
            wt.locked |= field == b"locked" || field.starts_with(b"locked ");
        }
    }
    ensure!(!result.is_empty(), "Git reported no worktrees");
    Ok(result)
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
    let admin = PathBuf::from(text(dir, &["rev-parse", "--absolute-git-dir"])?);
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
    ] {
        if admin.join(mark).try_exists()? {
            return Err(fault(
                "git_busy",
                format!("Finish the Git operation in {} first", dir.display()),
                json!({"marker":mark}),
            ));
        }
    }
    let raw = bytes(dir, &["ls-files", "-v", "-z"])?;
    for r in raw.split(|&b| b == 0).filter(|r| r.len() > 2) {
        let skips_present_file = if r[0] == b'S' {
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStrExt;
                dir.join(std::ffi::OsStr::from_bytes(&r[2..]))
            };
            #[cfg(not(unix))]
            let path = dir.join(std::str::from_utf8(&r[2..])?);
            match fs::symlink_metadata(path) {
                Ok(_) => true,
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(e) => return Err(e.into()),
            }
        } else {
            false
        };
        if r[0].is_ascii_lowercase() || skips_present_file {
            return Err(fault(
                "hidden_changes",
                "Clear assume-unchanged or skip-worktree flags on present files first",
                json!({"path":String::from_utf8_lossy(&r[2..])}),
            ));
        }
    }
    if !bytes(dir, &["ls-files", "--unmerged", "-z"])?.is_empty() {
        bail!("Resolve the workspace index conflicts first");
    }
    Ok(())
}
