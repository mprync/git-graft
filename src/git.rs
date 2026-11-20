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

#[cfg(target_os = "macos")]
pub(crate) mod optional;

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

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn require_blobs<'a>(
    mut command: Command,
    blobs: impl Iterator<Item = &'a str>,
) -> Result<()> {
    use io::{BufWriter, Seek, Write};
    let mut names = BufWriter::new(tempfile::tempfile()?);
    let mut count = 0;
    for blob in blobs {
        writeln!(names, "{blob}")?;
        count += 1;
    }
    let mut names = names.into_inner()?;
    names.rewind()?;
    let kinds = bounded_stdout(
        command
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .args(["cat-file", "--batch-check=%(objecttype)", "--buffer"])
            .stdin(names),
        count * 5,
    )
    .context("verify installed Git checkout blobs")?;
    ensure!(
        kinds.len() as u64 == count * 5
            && kinds
                .as_chunks::<5>()
                .0
                .iter()
                .all(|kind| kind == b"blob\n"),
        "installed Git checkout blob missing or invalid"
    );
    Ok(())
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
/// `-c` overrides that disable every filter driver defined by the
/// repository's own configuration, including files it includes.
///
/// `git status` runs a clean filter on any file whose stat changed. A
/// workspace's `.git/config` is writable by whoever works in it, so without
/// this an MCP client denied `--allow-exec` could still run a command through
/// Graft. Drivers from global and system configuration are the user's own and
/// stay active. An empty command makes Git skip a driver.
fn repository_filter_overrides(dir: &Path) -> Result<Vec<String>> {
    let listing = raw(
        dir,
        &[
            "config",
            "--show-scope",
            "--name-only",
            "-z",
            "--get-regexp",
            r"^filter\.",
        ],
    )?;
    ensure!(
        matches!(listing.status.code(), Some(0 | 1)),
        "read Git filter configuration: {}",
        String::from_utf8_lossy(&listing.stderr).trim()
    );
    let mut drivers = std::collections::BTreeSet::new();
    let mut fields = listing.stdout.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let (Some(scope), Some(key)) = (fields.next(), fields.next()) {
        if matches!(scope, b"global" | b"system") {
            continue;
        }
        let key = std::str::from_utf8(key).context("Git filter names must be UTF-8")?;
        let driver = key
            .strip_prefix("filter.")
            .and_then(|rest| rest.rsplit_once('.'))
            .map(|(driver, _)| driver)
            .context("malformed Git filter key")?;
        drivers.insert(driver.to_owned());
    }
    Ok(drivers
        .iter()
        .flat_map(|driver| {
            ["clean=", "smudge=", "process=", "required=false"]
                .map(|setting| format!("filter.{driver}.{setting}"))
        })
        .flat_map(|setting| ["-c".to_owned(), setting])
        .collect())
}

pub fn changes(dir: &Path) -> Result<Vec<Change>> {
    // Settings shared by the status and any follow-up hashing.
    let mut overrides = vec![
        "-c".to_owned(),
        "core.fsmonitor=false".to_owned(),
        "-c".to_owned(),
        "core.untrackedCache=false".to_owned(),
    ];
    overrides.extend(repository_filter_overrides(dir)?);
    let mut status: Vec<&str> = vec!["--no-optional-locks"];
    status.extend(overrides.iter().map(String::as_str));
    status.extend([
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=normal",
        "--ignored=matching",
        "--no-renames",
        "--ignore-submodules=none",
    ]);
    let raw = bytes(dir, &status)?;
    let mut changes = raw
        .split(|&b| b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| {
            ensure!(r.len() > 3 && r[2] == b' ', "malformed Git status record");
            Ok(Change {
                status: std::str::from_utf8(&r[..2])?.into(),
                path: String::from_utf8_lossy(&r[3..]).into_owned(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    changes.extend(subsecond_edits(dir, &overrides, &changes)?);
    Ok(changes)
}

/// Edits Git reports clean because it compares timestamps in whole seconds,
/// confirmed by hashing the files as `git add` would. `overrides` are the
/// `-c` settings used for the status, so no repository filter runs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn subsecond_edits(dir: &Path, overrides: &[String], reported: &[Change]) -> Result<Vec<Change>> {
    let git_dir = dir.join(".git");
    if !fs::symlink_metadata(&git_dir).is_ok_and(|metadata| metadata.is_dir()) {
        return Ok(Vec::new());
    }
    let suspects: Vec<_> = crate::storage::subsecond_changes(&git_dir, dir)
        .into_iter()
        .filter(|(name, _)| {
            let name = String::from_utf8_lossy(name);
            !reported.iter().any(|change| change.path == name)
        })
        .collect();
    if suspects.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for (name, _) in &suspects {
        input.extend_from_slice(name);
        input.push(b'\n');
    }
    let mut stdin = tempfile::tempfile()?;
    std::io::Write::write_all(&mut stdin, &input)?;
    std::io::Seek::rewind(&mut stdin)?;
    let mut hash = command(dir);
    hash.arg("--no-optional-locks")
        .args(overrides)
        .args(["hash-object", "--stdin-paths"])
        .stdin(stdin);
    let output = hash.output()?;
    ensure!(
        output.status.success(),
        "hash changed files: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let hashes = String::from_utf8(output.stdout)?;
    let hashes: Vec<&str> = hashes.lines().collect();
    ensure!(
        hashes.len() == suspects.len(),
        "Git hashed an unexpected number of files"
    );
    Ok(suspects
        .into_iter()
        .zip(hashes)
        .filter(|((_, staged), now)| staged != now)
        .map(|((name, _), _)| Change {
            status: " M".into(),
            path: String::from_utf8_lossy(&name).into_owned(),
        })
        .collect())
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

    /// A filter defined in the repository's own config, directly or through an
    /// include, never runs when Graft reads the status.
    #[cfg(unix)]
    #[test]
    fn status_never_runs_repository_defined_filters() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path().join("repo");
        fs::create_dir(&repo)?;
        bytes(&repo, &["init", "-q", "--template="])?;
        fs::write(repo.join("f.txt"), "a\n")?;
        bytes(&repo, &["add", "f.txt"])?;
        bytes(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "x",
            ],
        )?;
        let marker = temp.path().join("ran");
        let command = format!("touch '{}'; cat", marker.display());
        bytes(&repo, &["config", "filter.direct.clean", &command])?;
        bytes(&repo, &["config", "filter.direct.required", "true"])?;
        let included = temp.path().join("included");
        fs::write(
            &included,
            format!("[filter \"a.b\"]\n\tprocess = {command}\n"),
        )?;
        bytes(
            &repo,
            &["config", "include.path", included.to_str().unwrap()],
        )?;
        fs::create_dir_all(repo.join(".git/info"))?;
        fs::write(
            repo.join(".git/info/attributes"),
            "f.txt filter=direct\n* filter=a.b\n",
        )?;
        // A changed stat makes Git consult the clean filter.
        fs::write(repo.join("f.txt"), "a\n")?;
        changes(&repo)?;
        assert!(!marker.exists(), "a repository-defined filter ran");
        Ok(())
    }

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
