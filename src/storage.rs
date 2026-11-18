//! Safe worktree population from committed Git data.
//!
//! The caller must hold the project lock across this operation and snapshot GC,
//! and provide a newly registered, otherwise empty worktree.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs::{self};
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

// A hook or parent Git process may have selected a different repository or
// index through its environment. Only explicitly requested overrides survive.
fn git_command(worktree: &Path) -> Command {
    let mut command = crate::git::command(worktree);
    // Resolve snapshot attributes from its own index, not an inherited tree.
    command.env_remove("GIT_ATTR_SOURCE");
    command
}

fn git<I, S>(worktree: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = git_command(worktree)
        .args(args)
        .output()
        .context("run git")?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}
fn config(worktree: &Path, key: &str) -> Result<Option<String>> {
    let out = git_command(worktree)
        .args(["config", "--get", key])
        .output()?;
    match out.status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())),
        Some(1) => Ok(None),
        _ => bail!("git config {key}: {}", String::from_utf8_lossy(&out.stderr)),
    }
}
fn checked_oid(worktree: &Path, oid: &str) -> Result<String> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("expected full commit object ID");
    }
    let out = git(
        worktree,
        ["rev-parse", "--verify", &format!("{oid}^{{commit}}")],
    )?;
    let resolved = String::from_utf8(out.stdout)?.trim().to_owned();
    if !resolved.eq_ignore_ascii_case(oid) {
        bail!("object ID must name a commit");
    }
    let head = git(worktree, ["rev-parse", "--verify", "HEAD"])?;
    if String::from_utf8(head.stdout)?.trim() != resolved {
        bail!("worktree HEAD does not match requested commit");
    }
    Ok(resolved)
}
fn ensure_empty(worktree: &Path) -> Result<()> {
    for entry in fs::read_dir(worktree)? {
        let entry = entry?;
        if entry.file_name() != OsStr::new(".git") {
            bail!("worktree is not empty: {}", entry.path().display());
        }
        if !entry.file_type()?.is_file() {
            bail!("expected a registered worktree .git file");
        }
    }
    Ok(())
}
/// Validate cone paths against a commit before creating a branch or worktree.
pub(crate) fn validate_sparse(worktree: &Path, oid: &str, sparse: &[String]) -> Result<()> {
    if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|c| c.is_ascii_hexdigit()) {
        bail!("expected full commit object ID");
    }
    for path in sparse {
        if path.is_empty()
            || path.starts_with('-')
            || path.contains('\\')
            || path
                .split('/')
                .any(|x| x.is_empty() || x == "." || x == ".." || x.eq_ignore_ascii_case(".git"))
            || Path::new(path)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("invalid sparse cone directory: {path:?}");
        }
        let spec = format!("{oid}:{path}");
        let kind = git(worktree, ["cat-file", "-t", &spec])
            .with_context(|| format!("invalid sparse cone directory: {path}"))?;
        if kind.stdout != b"tree\n" {
            bail!("sparse cone path is not a directory: {path}");
        }
    }
    Ok(())
}
fn native_checkout(worktree: &Path, oid: &str, sparse: &[String], reason: &str) -> Result<Value> {
    if sparse.is_empty() {
        git(worktree, ["read-tree", "-m", "-u", oid])?;
    } else {
        let mut args = vec!["sparse-checkout", "set", "--cone", "--"];
        args.extend(sparse.iter().map(String::as_str));
        git(worktree, args)?;
        git(worktree, ["read-tree", "-m", "-u", oid])?;
    }
    Ok(
        json!({"backend":"git", "reason":reason, "cloned_files":0, "cloned_bytes":0, "snapshot_reused":false, "parent_reused_files":0, "parent_reused_bytes":0}),
    )
}

// Cache only when checkout semantics are determined by the commit and the
// fingerprinted settings. Unknown external attributes and filters use Git.
fn conversion_reason(worktree: &Path, oid: &str) -> Result<Option<&'static str>> {
    for variable in ["GIT_ATTR_SYSTEM", "GIT_ATTR_GLOBAL"] {
        let output = match git(worktree, ["var", variable]) {
            Ok(output) => output,
            Err(_) => return Ok(Some("cannot determine external attributes")),
        };
        let value = match std::str::from_utf8(&output.stdout) {
            Ok(value) => value.trim_end_matches(['\r', '\n']),
            Err(_) => return Ok(Some("cannot determine external attributes")),
        };
        if value.is_empty() || !Path::new(value).is_absolute() {
            return Ok(Some("cannot determine external attributes"));
        }
        match fs::symlink_metadata(value) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            _ => return Ok(Some("external checkout attributes")),
        }
    }
    let attr = match git(
        worktree,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/attributes",
        ],
    ) {
        Ok(output) => output,
        Err(_) => return Ok(Some("cannot determine external attributes")),
    };
    let attr = match std::str::from_utf8(&attr.stdout) {
        Ok(value) => value.trim_end_matches(['\r', '\n']),
        Err(_) => return Ok(Some("cannot determine external attributes")),
    };
    if !Path::new(attr).is_absolute() {
        return Ok(Some("cannot determine external attributes"));
    }
    match fs::symlink_metadata(attr) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        _ => return Ok(Some("external checkout attributes")),
    }
    if let Some(value) = config(worktree, "core.symlinks")?
        && !["true", "yes", "on", "1"]
            .iter()
            .any(|v| value.eq_ignore_ascii_case(v))
    {
        return Ok(Some("Git symlink emulation"));
    }
    // Gitlinks require Git's own empty-directory behavior and index handling.
    let tree = git(worktree, ["ls-tree", "-r", "-z", oid])?;
    if tree
        .stdout
        .split(|b| *b == 0)
        .any(|record| record.starts_with(b"160000 "))
    {
        return Ok(Some("submodules"));
    }
    let paths = git(worktree, ["ls-tree", "-r", "--name-only", "-z", oid])?;
    let mut input = tempfile::tempfile()?;
    input.write_all(&paths.stdout)?;
    use std::io::{Seek, SeekFrom};
    input.seek(SeekFrom::Start(0))?;
    // --source makes attribute lookup independent of both the dirty main tree
    // and the destination worktree. Old Git versions safely use native checkout.
    let source = format!("--source={oid}");
    let output = git_command(worktree)
        .args([
            "check-attr",
            &source,
            "-z",
            "--stdin",
            "filter",
            "ident",
            "working-tree-encoding",
        ])
        .stdin(Stdio::from(input))
        .output()?;
    if !output.status.success() {
        return Ok(Some("cannot inspect committed attributes"));
    }
    let fields: Vec<_> = output.stdout.split(|b| *b == 0).collect();
    if !fields.last().is_some_and(|field| field.is_empty()) || !(fields.len() - 1).is_multiple_of(3)
    {
        return Ok(Some("cannot inspect committed attributes"));
    }
    for fields in fields[..fields.len() - 1].as_chunks::<3>().0 {
        if fields[2] != b"unspecified" && fields[2] != b"unset" {
            return Ok(Some("path-dependent checkout conversion"));
        }
    }
    Ok(None)
}

// A commit can check out to different bytes under different effective Git
// settings. Keep separate snapshots for each set of conversion settings.
fn snapshot_key(worktree: &Path) -> Result<String> {
    let mut input = tempfile::tempfile()?;
    writeln!(input, "git-graft-checkout-v2:{}", std::env::consts::OS)?;
    for key in [
        "core.autocrlf",
        "core.eol",
        "core.safecrlf",
        "core.symlinks",
    ] {
        writeln!(input, "{key}={:?}", config(worktree, key)?)?;
    }
    use std::io::{Seek, SeekFrom};
    input.seek(SeekFrom::Start(0))?;
    let output = git_command(worktree)
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::from(input))
        .output()?;
    if !output.status.success() {
        bail!("git could not fingerprint checkout settings");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(target_os = "macos")]
pub(crate) fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if !fs::symlink_metadata(src)?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clone source must be a regular file",
        ));
    }
    let s = std::ffi::CString::new(src.as_os_str().as_bytes())?;
    let d = std::ffi::CString::new(dst.as_os_str().as_bytes())?;
    // clonefile creates a new inode and fails if the destination already exists.
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
#[cfg(target_os = "linux")]
pub(crate) fn clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    if !fs::symlink_metadata(src)?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clone source must be a regular file",
        ));
    }
    let source = File::open(src)?;
    let dest = OpenOptions::new().write(true).create_new(true).open(dst)?;
    const FICLONE: libc::c_ulong = 0x4004_9409;
    if unsafe { libc::ioctl(dest.as_raw_fd(), FICLONE, source.as_raw_fd()) } == 0 {
        let result = source
            .metadata()
            .and_then(|metadata| dest.set_permissions(metadata.permissions()));
        if let Err(error) = result {
            drop(dest);
            let _ = fs::remove_file(dst);
            return Err(error);
        }
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        drop(dest);
        let _ = fs::remove_file(dst);
        Err(error)
    }
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn clone_file(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no native CoW backend",
    ))
}

fn probe(src_dir: &Path, dst_dir: &Path) -> Result<bool> {
    let mut source = tempfile::NamedTempFile::new_in(src_dir)?;
    source.write_all(b"git-graft clone probe")?;
    let target = tempfile::Builder::new()
        .prefix(".graft-probe-")
        .tempdir_in(dst_dir)?;
    let cloned = target.path().join("clone");
    if clone_file(source.path(), &cloned).is_err() {
        return Ok(false);
    }
    fs::write(&cloned, b"changed")?;
    Ok(fs::read(source.path())? == b"git-graft clone probe")
}
fn create_cache_dir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(path)?.file_type().is_dir() {
                Ok(())
            } else {
                bail!("snapshot directory is not a directory: {}", path.display())
            }
        }
        Err(error) => Err(error.into()),
    }
}
// Only reuse an immediate parent with identical checkout settings and unchanged
// committed attribute files. Git itself applies the tree transition in private.
fn parent_candidate(
    worktree: &Path,
    state: &Path,
    oid: &str,
    key: &str,
) -> Result<Option<(String, PathBuf, Counts)>> {
    let parents = git(worktree, ["rev-list", "--parents", "-n", "1", oid])?;
    let parents = std::str::from_utf8(&parents.stdout)?;
    let Some(parent) = parents.split_whitespace().nth(1) else {
        return Ok(None);
    };
    if !matches!(parent.len(), 40 | 64) || !parent.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let parent_dir = state.join("bases").join(parent);
    if !fs::symlink_metadata(&parent_dir).is_ok_and(|m| m.file_type().is_dir()) {
        return Ok(None);
    }
    let parent_base = parent_dir.join(key);
    if !fs::symlink_metadata(&parent_base).is_ok_and(|m| m.file_type().is_dir()) {
        return Ok(None);
    }
    let changed = git(
        worktree,
        [
            "diff-tree",
            "--no-commit-id",
            "--no-renames",
            "--name-only",
            "-r",
            "-z",
            parent,
            oid,
        ],
    )?;
    if changed.stdout.split(|b| *b == 0).any(|path| {
        path.split(|b| *b == b'/')
            .any(|part| part.eq_ignore_ascii_case(b".gitattributes"))
    }) {
        return Ok(None);
    }
    let old = git(worktree, ["ls-tree", "-r", "-z", parent])?;
    let new = git(worktree, ["ls-tree", "-r", "-z", oid])?;
    let old: std::collections::HashSet<&[u8]> = old
        .stdout
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .collect();
    let mut counts = Counts::default();
    for record in new.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        // Count only identical regular-file mode, blob and path. Renames and
        // mode changes can still share blocks, but are deliberately excluded.
        if !(record.starts_with(b"100644 blob ") || record.starts_with(b"100755 blob "))
            || !old.contains(record)
        {
            continue;
        }
        let Some(tab) = record.iter().position(|b| *b == b'\t') else {
            return Ok(None);
        };
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStrExt;
            parent_base.join(OsStr::from_bytes(&record[tab + 1..]))
        };
        #[cfg(not(unix))]
        let path = match std::str::from_utf8(&record[tab + 1..]) {
            Ok(path) => parent_base.join(path),
            Err(_) => return Ok(None),
        };
        let metadata = match fs::symlink_metadata(path) {
            Ok(value) if value.file_type().is_file() => value,
            _ => return Ok(None),
        };
        counts.files += 1;
        counts.bytes += metadata.len();
    }
    Ok(Some((parent.to_owned(), parent_base, counts)))
}

fn snapshot(
    worktree: &Path,
    state: &Path,
    oid: &str,
    key: &str,
) -> Result<(PathBuf, bool, Counts)> {
    let bases = state.join("bases");
    create_cache_dir(&bases)?;
    let commit_bases = bases.join(oid);
    create_cache_dir(&commit_bases)?;
    let base = commit_bases.join(key);
    if let Ok(metadata) = fs::symlink_metadata(&base) {
        if metadata.file_type().is_dir() {
            return Ok((base, true, Counts::default()));
        }
        bail!("snapshot path is not a directory: {}", base.display());
    }
    let build = |parent: Option<(&str, &Path)>| -> Result<(tempfile::TempDir, PathBuf)> {
        let temp = tempfile::Builder::new()
            .prefix(".building-")
            .tempdir_in(&commit_bases)?;
        let tree = temp.path().join("tree");
        fs::create_dir(&tree)?;
        let index = temp.path().join("index");
        let run = |args: &[&str]| -> Result<()> {
            let output = git_command(worktree)
                .env("GIT_INDEX_FILE", &index)
                .env("GIT_WORK_TREE", &tree)
                .args(args)
                .output()?;
            if !output.status.success() {
                bail!("git snapshot: {}", String::from_utf8_lossy(&output.stderr));
            }
            Ok(())
        };
        if let Some((parent_oid, parent_tree)) = parent {
            run(&["read-tree", parent_oid])?;
            clone_tree(parent_tree, &tree, &mut Counts::default())?;
            run(&["update-index", "--refresh"])?;
            run(&["read-tree", "-m", "-u", parent_oid, oid])?;
        } else {
            run(&["read-tree", oid])?;
            run(&["checkout-index", "--all"])?;
        }
        Ok((temp, tree))
    };
    let parent = parent_candidate(worktree, state, oid, key).ok().flatten();
    let (temp, tree, counts) = if let Some((parent_oid, parent_tree, counts)) = parent {
        match build(Some((&parent_oid, &parent_tree))) {
            Ok((temp, tree)) => (temp, tree, counts),
            Err(_) => {
                let (temp, tree) = build(None)?;
                (temp, tree, Counts::default())
            }
        }
    } else {
        let (temp, tree) = build(None)?;
        (temp, tree, Counts::default())
    };
    fs::rename(&tree, &base).context("publish base snapshot")?;
    drop(temp);
    Ok((base, false, counts))
}

#[derive(Default)]
struct Counts {
    files: u64,
    bytes: u64,
}
fn clone_tree(src: &Path, dst: &Path, counts: &mut Counts) -> Result<()> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.file_type().is_dir() {
            fs::create_dir(&target)?;
            clone_tree(&entry.path(), &target, counts)?;
        } else if meta.file_type().is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(entry.path())?, &target)?;
            #[cfg(not(unix))]
            bail!("symlink cloning is unsupported");
        } else if meta.is_file() {
            clone_file(&entry.path(), &target)
                .with_context(|| format!("clone {}", entry.path().display()))?;
            counts.files += 1;
            counts.bytes += meta.len();
        } else {
            bail!("unsupported snapshot entry: {}", entry.path().display());
        }
    }
    Ok(())
}

/// Populate an empty registered worktree at its current commit. The project lock
/// must also guard garbage collection of `state/bases`.
pub fn materialize(worktree: &Path, state: &Path, oid: &str, sparse: &[String]) -> Result<Value> {
    let worktree = fs::canonicalize(worktree)?;
    ensure_empty(&worktree)?;
    let oid = checked_oid(&worktree, oid)?;
    validate_sparse(&worktree, &oid, sparse)?;
    if !sparse.is_empty() {
        return native_checkout(&worktree, &oid, sparse, "sparse checkout");
    }
    if let Some(reason) = conversion_reason(&worktree, &oid)? {
        return native_checkout(&worktree, &oid, sparse, reason);
    }
    fs::create_dir_all(state)?;
    let state = fs::canonicalize(state)?;
    if state.starts_with(&worktree) {
        bail!("storage state must be outside the worktree");
    }
    if !probe(&state, &worktree)? {
        return native_checkout(&worktree, &oid, sparse, "native clone unavailable");
    }
    let key = snapshot_key(&worktree)?;
    let (base, reused, parent_counts) = snapshot(&worktree, &state, &oid, &key)?;
    git(&worktree, ["read-tree", &oid])?;
    let mut counts = Counts::default();
    clone_tree(&base, &worktree, &mut counts)?;
    git(&worktree, ["update-index", "--refresh"])?;
    Ok(
        json!({"backend": if cfg!(target_os="macos") { "clonefile" } else { "reflink" }, "cloned_files":counts.files, "cloned_bytes":counts.bytes, "snapshot_reused":reused, "parent_reused_files":parent_counts.files, "parent_reused_bytes":parent_counts.bytes}),
    )
}

/// Probe native cloning on the state filesystem. A worktree on another volume
/// can still require the Git fallback; materialize checks the actual pair.
pub fn doctor(state: &Path) -> Result<Value> {
    fs::create_dir_all(state)?;
    let supported = probe(state, state)?;
    Ok(
        json!({"native_clone":supported, "backend": if supported { if cfg!(target_os="macos") {"clonefile"} else {"reflink"} } else {"git"}, "scope":"state filesystem only"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(dir: &Path, args: &[&str]) -> String {
        let output = git_command(dir).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
    fn fixture() -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        command(&repo, &["init", "-q"]);
        command(&repo, &["config", "user.email", "test@example.invalid"]);
        command(&repo, &["config", "user.name", "Test"]);
        command(&repo, &["config", "core.autocrlf", "false"]);
        fs::create_dir(repo.join("sub")).unwrap();
        fs::write(repo.join("sub/file"), b"committed\n").unwrap();
        fs::write(repo.join("script"), b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(repo.join("script"), fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("sub/file", repo.join("link")).unwrap();
        }
        command(&repo, &["add", "."]);
        command(&repo, &["commit", "-qm", "base"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        (temp, repo, oid)
    }
    fn worktree(repo: &Path, path: &Path, branch: &str, oid: &str) {
        command(
            repo,
            &[
                "worktree",
                "add",
                "--no-checkout",
                "-b",
                branch,
                path.to_str().unwrap(),
                oid,
            ],
        );
    }
    #[test]
    fn materializes_committed_tree_and_isolates_edits() {
        let (temp, repo, oid) = fixture();
        fs::write(repo.join("sub/file"), b"dirty main").unwrap();
        fs::write(repo.join("secret"), b"untracked").unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let state = temp.path().join("state");
        worktree(&repo, &first, "first", &oid);
        let result = materialize(&first, &state, &oid, &[]).unwrap();
        println!("first worktree backend: {result}");
        assert_eq!(fs::read(first.join("sub/file")).unwrap(), b"committed\n");
        assert!(!first.join("secret").exists());
        assert_eq!(command(&first, &["status", "--porcelain"]), "");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::read_link(first.join("link")).unwrap(),
                PathBuf::from("sub/file")
            );
            assert_ne!(
                fs::metadata(first.join("script"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
        }
        fs::write(first.join("sub/file"), b"changed in first").unwrap();
        worktree(&repo, &second, "second", &oid);
        let second_result = materialize(&second, &state, &oid, &[]).unwrap();
        println!("second worktree backend: {second_result}");
        assert_eq!(fs::read(second.join("sub/file")).unwrap(), b"committed\n");
        assert_eq!(fs::read(repo.join("sub/file")).unwrap(), b"dirty main");
        assert_eq!(command(&second, &["status", "--porcelain"]), "");
        if result["backend"] != "git" {
            assert_eq!(second_result["snapshot_reused"], true);
            assert!(result["cloned_files"].as_u64().unwrap() >= 2);
            #[cfg(target_os = "macos")]
            assert_eq!(result["backend"], "clonefile");
        }
    }
    #[test]
    fn sparse_validation_and_native_checkout() {
        let (temp, repo, oid) = fixture();
        let wt = temp.path().join("sparse");
        worktree(&repo, &wt, "sparse", &oid);
        for invalid in [
            "../escape",
            "/absolute",
            "sub/file",
            "missing",
            "-option",
            "sub//file",
        ] {
            assert!(
                materialize(&wt, &temp.path().join("state"), &oid, &[invalid.into()]).is_err(),
                "{invalid}"
            );
        }
        let result = materialize(&wt, &temp.path().join("state"), &oid, &["sub".into()]).unwrap();
        assert_eq!(result["backend"], "git");
        assert_eq!(fs::read(wt.join("sub/file")).unwrap(), b"committed\n");
        assert_eq!(command(&wt, &["status", "--porcelain"]), "");
    }
    #[test]
    fn refuses_nonempty_worktree_and_clones_committed_attributes() {
        let (temp, repo, _) = fixture();
        fs::write(repo.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
        command(&repo, &["add", ".gitattributes"]);
        command(&repo, &["commit", "-qm", "attributes"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        let wt = temp.path().join("attributes");
        worktree(&repo, &wt, "attrs", &oid);
        fs::write(wt.join("occupied"), b"preserve").unwrap();
        assert!(materialize(&wt, &temp.path().join("state"), &oid, &[]).is_err());
        assert_eq!(fs::read(wt.join("occupied")).unwrap(), b"preserve");
        fs::remove_file(wt.join("occupied")).unwrap();
        let result = materialize(&wt, &temp.path().join("state"), &oid, &[]).unwrap();
        if probe(temp.path(), &wt).unwrap() {
            assert_ne!(result["backend"], "git");
        }
        assert_eq!(command(&wt, &["status", "--porcelain"]), "");
    }

    #[test]
    fn checkout_settings_and_attributes_match_git_and_change_cache_key() {
        let (temp, repo, _) = fixture();
        fs::write(
            repo.join(".gitattributes"),
            "*.txt text eol=crlf\n*.bin binary\n*.rs linguist-language=Rust\n",
        )
        .unwrap();
        fs::write(repo.join("sample.txt"), b"one\ntwo\n").unwrap();
        fs::write(repo.join("unannotated.data"), b"one\ntwo\n").unwrap();
        fs::write(repo.join("opaque.bin"), b"binary\0\n").unwrap();
        command(
            &repo,
            &[
                "add",
                ".gitattributes",
                "sample.txt",
                "opaque.bin",
                "unannotated.data",
            ],
        );
        command(&repo, &["commit", "-qm", "attributes"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        let state = temp.path().join("state");
        fs::write(repo.join(".gitattributes"), "*.txt text eol=lf\n").unwrap();
        let first = temp.path().join("first-attrs");
        worktree(&repo, &first, "first-attrs", &oid);
        let first_result = materialize(&first, &state, &oid, &[]).unwrap();
        let normal = temp.path().join("normal");
        command(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "normal",
                normal.to_str().unwrap(),
                &oid,
            ],
        );
        assert_eq!(
            fs::read(first.join("sample.txt")).unwrap(),
            fs::read(normal.join("sample.txt")).unwrap()
        );
        assert_eq!(
            fs::read(first.join("sample.txt")).unwrap(),
            b"one\r\ntwo\r\n"
        );
        assert_eq!(
            fs::read(first.join("unannotated.data")).unwrap(),
            b"one\ntwo\n"
        );
        assert_eq!(
            fs::read(first.join("opaque.bin")).unwrap(),
            fs::read(normal.join("opaque.bin")).unwrap()
        );
        assert_eq!(command(&first, &["status", "--porcelain"]), "");
        command(&repo, &["config", "core.autocrlf", "true"]);
        let second = temp.path().join("second-attrs");
        worktree(&repo, &second, "second-attrs", &oid);
        let result = materialize(&second, &state, &oid, &[]).unwrap();
        assert_eq!(
            fs::read(second.join("sample.txt")).unwrap(),
            b"one\r\ntwo\r\n"
        );
        assert_eq!(
            fs::read(second.join("unannotated.data")).unwrap(),
            b"one\r\ntwo\r\n"
        );
        assert_eq!(command(&second, &["status", "--porcelain"]), "");
        if first_result["backend"] != "git" {
            assert_eq!(result["snapshot_reused"], false);
        }
    }

    #[test]
    fn filters_and_external_attributes_use_native_git() {
        let (temp, repo, _) = fixture();
        fs::write(repo.join(".gitattributes"), "*.dat filter=demo\n").unwrap();
        fs::write(repo.join("data.dat"), "content\n").unwrap();
        command(&repo, &["config", "filter.demo.smudge", "cat"]);
        command(&repo, &["config", "filter.demo.clean", "cat"]);
        command(&repo, &["add", ".gitattributes", "data.dat"]);
        command(&repo, &["commit", "-qm", "filter"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        let wt = temp.path().join("filtered");
        worktree(&repo, &wt, "filtered", &oid);
        let result = materialize(&wt, &temp.path().join("state"), &oid, &[]).unwrap();
        assert_eq!(result["backend"], "git");
        assert_eq!(result["reason"], "path-dependent checkout conversion");
        assert_eq!(command(&wt, &["status", "--porcelain"]), "");
        let external = temp.path().join("attributes");
        fs::write(&external, "*.dat text\n").unwrap();
        command(
            &repo,
            &["config", "core.attributesfile", external.to_str().unwrap()],
        );
        let wt = temp.path().join("external");
        worktree(&repo, &wt, "external", &oid);
        assert_eq!(
            materialize(&wt, &temp.path().join("state"), &oid, &[]).unwrap()["reason"],
            "external checkout attributes"
        );
    }

    #[test]
    fn submodule_gitlinks_use_native_git() {
        let (temp, repo, oid) = fixture();
        command(
            &repo,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{oid},module"),
            ],
        );
        command(&repo, &["commit", "-qm", "gitlink"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        let wt = temp.path().join("gitlink");
        worktree(&repo, &wt, "gitlink", &oid);
        let result = materialize(&wt, &temp.path().join("state"), &oid, &[]).unwrap();
        assert_eq!(result["reason"], "submodules");
        assert_eq!(command(&wt, &["status", "--porcelain"]), "");
        assert_eq!(fs::read_dir(wt.join("module")).unwrap().count(), 0);
    }

    #[test]
    fn advancing_commit_reuses_parent_and_preserves_git_tree() {
        let (temp, repo, _) = fixture();
        let large = vec![b'x'; 3 * 1024 * 1024];
        fs::write(repo.join("large.bin"), &large).unwrap();
        fs::write(repo.join("changed"), b"old\n").unwrap();
        fs::write(repo.join("deleted"), b"delete\n").unwrap();
        fs::write(repo.join("rename-old"), b"rename\n").unwrap();
        #[cfg(unix)]
        {
            fs::create_dir(temp.path().join("outside")).unwrap();
            fs::write(temp.path().join("outside/sentinel"), b"safe").unwrap();
            std::os::unix::fs::symlink(temp.path().join("outside"), repo.join("route")).unwrap();
        }
        command(&repo, &["add", "."]);
        command(&repo, &["commit", "-qm", "parent"]);
        let parent = command(&repo, &["rev-parse", "HEAD"]);
        let state = temp.path().join("state");
        let first = temp.path().join("parent-tree");
        worktree(&repo, &first, "parent-tree", &parent);
        let initial = materialize(&first, &state, &parent, &[]).unwrap();
        assert_eq!(initial["parent_reused_files"], 0);
        fs::write(repo.join("changed"), b"new\n").unwrap();
        fs::remove_file(repo.join("deleted")).unwrap();
        fs::rename(repo.join("rename-old"), repo.join("rename-new")).unwrap();
        #[cfg(unix)]
        {
            fs::remove_file(repo.join("route")).unwrap();
            fs::create_dir(repo.join("route")).unwrap();
            fs::write(repo.join("route/sentinel"), b"in tree").unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(repo.join("script"), fs::Permissions::from_mode(0o644)).unwrap();
        }
        command(&repo, &["add", "-A"]);
        command(&repo, &["commit", "-qm", "child"]);
        let child = command(&repo, &["rev-parse", "HEAD"]);
        let second = temp.path().join("child-tree");
        worktree(&repo, &second, "child-tree", &child);
        let result = materialize(&second, &state, &child, &[]).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(
                fs::read(temp.path().join("outside/sentinel")).unwrap(),
                b"safe"
            );
            assert_eq!(fs::read(second.join("route/sentinel")).unwrap(), b"in tree");
        }
        if initial["backend"] != "git" {
            assert_eq!(result["snapshot_reused"], false);
            assert!(
                result["parent_reused_files"].as_u64().unwrap() >= 1,
                "{result}"
            );
            assert!(
                result["parent_reused_bytes"].as_u64().unwrap() >= large.len() as u64,
                "{result}"
            );
        }
        assert_eq!(fs::read(second.join("large.bin")).unwrap(), large);
        assert_eq!(fs::read(second.join("changed")).unwrap(), b"new\n");
        assert!(!second.join("deleted").exists());
        assert!(!second.join("rename-old").exists());
        assert_eq!(fs::read(second.join("rename-new")).unwrap(), b"rename\n");
        assert_eq!(command(&second, &["status", "--porcelain"]), "");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(second.join("script"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
        }
        fs::write(second.join("large.bin"), b"changed locally").unwrap();
        assert_eq!(fs::read(first.join("large.bin")).unwrap(), large);
        let third = temp.path().join("child-again");
        worktree(&repo, &third, "child-again", &child);
        let again = materialize(&third, &state, &child, &[]).unwrap();
        assert_eq!(fs::read(third.join("large.bin")).unwrap(), large);
        if initial["backend"] != "git" {
            assert_eq!(again["snapshot_reused"], true);
        }

        fs::write(repo.join(".gitattributes"), "changed text eol=crlf\n").unwrap();
        command(&repo, &["add", ".gitattributes"]);
        command(&repo, &["commit", "-qm", "attribute change"]);
        let attr_oid = command(&repo, &["rev-parse", "HEAD"]);
        let fourth = temp.path().join("attrs-tree");
        worktree(&repo, &fourth, "attrs-tree", &attr_oid);
        let attrs = materialize(&fourth, &state, &attr_oid, &[]).unwrap();
        if initial["backend"] != "git" {
            assert_eq!(attrs["parent_reused_files"], 0);
        }
        assert_eq!(fs::read(fourth.join("changed")).unwrap(), b"new\r\n");
        assert_eq!(command(&fourth, &["status", "--porcelain"]), "");
    }
}
