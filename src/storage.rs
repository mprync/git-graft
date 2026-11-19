//! Git-authoritative source materialization with optional native CoW reuse.
//! The caller holds the project lock shared by snapshot publication and GC.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(test)]
use std::path::PathBuf;
use std::{
    ffi::OsStr,
    fs,
    io::{self, BufWriter, Seek, Write},
    path::{Component, Path},
    process::{Command, Output, Stdio},
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod cache;
mod local;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod verify;
pub(crate) use local::materialize_local;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) use native::Root as NoFollowRoot;

fn git_command(worktree: &Path) -> Command {
    crate::git::command(worktree)
}

fn git(worktree: &Path, args: &[&str]) -> Result<Output> {
    let output = git_command(worktree)
        .args(args)
        .output()
        .context("run Git")?;
    ensure!(
        output.status.success(),
        "Git checkout: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

fn checkout_identity(worktree: &Path, oid: &str) -> Result<String> {
    crate::git::checked_oid(oid)?;
    let identity = crate::git::text(
        worktree,
        &[
            "rev-parse",
            "--verify",
            "--path-format=absolute",
            "--git-common-dir",
            "--absolute-git-dir",
            "--show-toplevel",
            "HEAD^{commit}",
        ],
    )?;
    // The last line is the OID. Fall back to separate Git queries for
    // newlines in directory names or differently spelled canonical paths.
    let (paths, head) = identity
        .rsplit_once('\n')
        .context("missing Git checkout root")?;
    let git_dir = worktree.join(".git");
    if !exact_git_paths(paths, &[&git_dir, &git_dir, worktree]) {
        let common = crate::git::text(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        validate_workspace(worktree, &common)?;
    }
    ensure!(
        head.eq_ignore_ascii_case(oid),
        "worktree HEAD does not match requested commit"
    );
    Ok(head.to_owned())
}

fn exact_git_paths(output: &str, paths: &[&Path]) -> bool {
    output
        .split('\n')
        .map(|line| {
            Some(if cfg!(windows) {
                line.strip_suffix('\r').unwrap_or(line)
            } else {
                line
            })
        })
        .eq(paths.iter().map(|path| path.to_str()))
}

fn ensure_empty(worktree: &Path) -> Result<()> {
    for entry in fs::read_dir(worktree)? {
        let entry = entry?;
        ensure!(
            entry.file_name() == ".git",
            "worktree is not empty: {}",
            entry.path().display()
        );
        crate::registry::owned_directory(&entry.path())?;
    }
    Ok(())
}

/// Confirm Git's live common directory, actual Git directory and checkout root
/// belong to the canonical workspace and its ordinary .git directory.
pub(crate) fn validate_workspace(workspace: &Path, common: &str) -> Result<()> {
    let git_dir = workspace.join(".git");
    crate::registry::owned_directory(&git_dir)?;
    let names = crate::git::text(
        workspace,
        &["rev-parse", "--absolute-git-dir", "--show-toplevel"],
    )?;
    let direct = exact_git_paths(&names, &[&git_dir, workspace]);
    ensure!(
        dunce::canonicalize(common)? == git_dir
            && (direct
                || (dunce::canonicalize(crate::git::text(
                    workspace,
                    &["rev-parse", "--absolute-git-dir"]
                )?)? == git_dir
                    && dunce::canonicalize(crate::git::text(
                        workspace,
                        &["rev-parse", "--show-toplevel"]
                    )?)? == workspace)),
        "expected an independent repository rooted at {}",
        workspace.display()
    );
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
        let kind = git(worktree, &["cat-file", "-t", &spec])
            .with_context(|| format!("invalid sparse cone directory: {path}"))?;
        if kind.stdout != b"tree\n" {
            bail!("sparse cone path is not a directory: {path}");
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    name: Vec<u8>,
    blob: String,
    mode: u32,
}

impl Entry {
    fn path(&self) -> Result<&Path> {
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStrExt;
            OsStr::from_bytes(&self.name)
        };
        #[cfg(not(unix))]
        let name = OsStr::new(std::str::from_utf8(&self.name)?);
        let path = Path::new(name);
        ensure!(
            path.components().all(|c| matches!(c, Component::Normal(_))),
            "unsafe Git tree path"
        );
        Ok(path)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn regular(&self) -> bool {
        matches!(self.mode, 0o100644 | 0o100755)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn creation_mode(&self, permissions: u32) -> u32 {
        match self.mode {
            0o100755 => permissions,
            _ => permissions & 0o666,
        }
    }

    fn attributes(&self) -> bool {
        self.name
            .split(|b| *b == b'/')
            .next_back()
            .is_some_and(|name| name.eq_ignore_ascii_case(b".gitattributes"))
    }
}

fn entries(worktree: &Path, oid: &str) -> Result<Vec<Entry>> {
    let tree = git_command(worktree)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args(["ls-tree", "-r", "-z", oid])
        .output()?;
    ensure!(
        tree.status.success(),
        "read Git checkout tree: {}",
        String::from_utf8_lossy(&tree.stderr).trim()
    );
    tree.stdout
        .split(|b| *b == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let tab = record
                .iter()
                .position(|b| *b == b'\t')
                .context("invalid Git tree entry")?;
            let name = &record[tab + 1..];
            ensure!(
                name.split(|b| *b == b'/').all(|p| !p.is_empty()
                    && p != b"."
                    && p != b".."
                    && !p.eq_ignore_ascii_case(b".git")),
                "unsafe path in Git tree"
            );
            let mut header = std::str::from_utf8(&record[..tab])?.split_whitespace();
            let mode = u32::from_str_radix(header.next().context("missing Git tree mode")?, 8)?;
            let kind = header.next().context("missing Git tree kind")?;
            let blob = crate::git::checked_oid(header.next().context("missing Git object ID")?)?
                .to_owned();
            ensure!(
                matches!(
                    (mode, kind),
                    (0o100644 | 0o100755 | 0o120000, "blob") | (0o160000, "commit")
                ),
                "unexpected Git checkout entry"
            );
            Ok(Entry {
                name: name.to_vec(),
                blob,
                mode,
            })
        })
        .collect()
}

#[derive(Default)]
struct Counts {
    files: u64,
    bytes: u64,
}

impl Counts {
    fn add(&mut self, size: u64) {
        self.files += 1;
        self.bytes += size;
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn merge(&mut self, other: Self) {
        self.files += other.files;
        self.bytes += other.bytes;
    }
}

#[derive(Default)]
struct Receipt {
    cloned: Counts,
    primary: Counts,
    parent: Counts,
    private: Counts,
    warm: bool,
}

impl Receipt {
    fn json(&self, backend: &str, reason: Option<&str>) -> Value {
        let mut result = json!({
            "backend":backend,
            "cloned_files":self.cloned.files, "cloned_bytes":self.cloned.bytes,
            "snapshot_reused":self.warm,
            "primary_reused_files":self.primary.files, "primary_reused_bytes":self.primary.bytes,
            "parent_reused_files":self.parent.files, "parent_reused_bytes":self.parent.bytes,
            "private_files":self.private.files, "private_bytes":self.private.bytes,
        });
        if let Some(reason) = reason {
            result["reason"] = reason.into();
        }
        result
    }
}

fn count_private(root: &Path, entry: &Entry, counts: &mut Counts) {
    let Ok(path) = entry.path() else { return };
    let mut path_at = root.to_path_buf();
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
        path_at.push(part);
        let Ok(metadata) = fs::symlink_metadata(&path_at) else {
            return;
        };
        if crate::registry::is_reparse_point(&metadata) {
            return;
        }
        if parts.peek().is_none() && metadata.is_file() {
            counts.add(metadata.len());
        }
        if !metadata.is_dir() {
            return;
        }
    }
}

fn native_checkout(
    worktree: &Path,
    oid: &str,
    sparse: &[String],
    entries: &[Entry],
    reason: &str,
) -> Result<Value> {
    if !sparse.is_empty() {
        let mut args = vec!["sparse-checkout", "set", "--cone", "--"];
        args.extend(sparse.iter().map(String::as_str));
        git(worktree, &args)?;
    }
    let output = parallel_checkout(git_command(worktree))
        .args(["read-tree", "-m", "-u", oid])
        .output()?;
    ensure!(
        output.status.success(),
        "Git checkout: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut result = Receipt::default();
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Ok(root) = native::Root::open(worktree) {
        for entry in entries {
            if let Ok(path) = entry.path()
                && let Ok(size) = root.regular_size(path)
            {
                result.private.add(size);
            }
        }
        return Ok(result.json("git", Some(reason)));
    }
    for entry in entries {
        count_private(worktree, entry, &mut result.private);
    }
    Ok(result.json("git", Some(reason)))
}

// Native Git parallelizes only entries whose conversions can run concurrently.
// Bound the worker pool so simultaneous spawns do not fan out to every CPU.
const PARALLEL_THRESHOLD: usize = 256;

fn parallel_checkout(mut command: Command) -> Command {
    let workers = std::thread::available_parallelism().map_or(1, |count| count.get().min(4));
    command
        .arg("-c")
        .arg(format!("checkout.workers={workers}"))
        .arg("-c")
        .arg(format!(
            "checkout.thresholdForParallelism={PARALLEL_THRESHOLD}"
        ));
    command
}

fn path_input<'a>(entries: impl Iterator<Item = &'a Entry>) -> Result<fs::File> {
    let mut input = BufWriter::new(tempfile::tempfile()?);
    for entry in entries {
        input.write_all(&entry.name)?;
        input.write_all(b"\0")?;
    }
    let mut input = input.into_inner()?;
    input.rewind()?;
    Ok(input)
}

struct Checkout<'a> {
    worktree: &'a Path,
    oid: &'a str,
    entries: &'a [Entry],
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    settings: String,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    frozen: FrozenGit,
    symlinks: bool,
    inspect_attributes: bool,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    may_convert_eol: bool,
}

impl<'a> Checkout<'a> {
    fn read(worktree: &'a Path, oid: &'a str, entries: &'a [Entry]) -> Result<Self> {
        let out = git_command(worktree).args([
            "config", "--null", "--get-regexp",
            r"^core\.(autocrlf|eol|safecrlf|symlinks|filemode|ignorecase|precomposeunicode|protecthfs|protectntfs|bigfilethreshold)$",
        ]).output()?;
        ensure!(
            matches!(out.status.code(), Some(0 | 1)),
            "cannot read Git checkout settings"
        );
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let settings = {
            let git_version = git(worktree, &["version"])?;
            let mut identity = blake3::Hasher::new();
            identity.update(b"git-graft-checkout-v6\0");
            identity.update(std::env::consts::OS.as_bytes());
            identity.update(&git_version.stdout);
            identity.update(&out.stdout);
            // A parent's files can only be reused under the same committed rules.
            for entry in entries.iter().filter(|entry| entry.attributes()) {
                identity.update(&entry.name);
                identity.update(b"\0");
                identity.update(entry.blob.as_bytes());
                identity.update(&entry.mode.to_le_bytes());
            }
            identity.finalize().to_hex().to_string()
        };
        let value = |key: &[u8]| {
            out.stdout
                .split(|b| *b == 0)
                .filter_map(|entry| {
                    let rest = entry.strip_prefix(key)?;
                    if rest.is_empty() {
                        Some(b"true".as_slice()) // a bare boolean key
                    } else {
                        rest.strip_prefix(b"\n")
                    }
                })
                .next_back()
        };
        let symlinks = value(b"core.symlinks").is_none_or(|value| {
            [b"true".as_slice(), b"yes", b"on", b"1"]
                .iter()
                .any(|valid| value.eq_ignore_ascii_case(valid))
        });
        // Non-ASCII names can have filesystem-specific aliases on checkout.
        let inspect_attributes = entries
            .iter()
            .any(|entry| entry.attributes() || !entry.name.is_ascii());
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let may_convert_eol = value(b"core.autocrlf").is_some_and(|value| {
            ![b"false".as_slice(), b"no", b"off", b"0", b"", b"input"]
                .iter()
                .any(|raw| value.eq_ignore_ascii_case(raw))
        });
        Ok(Self {
            worktree,
            oid,
            entries,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            settings,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            frozen: FrozenGit::new(worktree, oid, &out.stdout)?,
            symlinks,
            inspect_attributes,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            may_convert_eol,
        })
    }

    fn command(&self) -> Command {
        let command = git_command(self.worktree);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let command = self.frozen.configure(command, self.worktree, self.oid);
        command
    }
}

/// Cache commands see only committed attributes and the exact configuration
/// bytes used for the snapshot identity. The live repository remains untouched.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct FrozenGit {
    directory: tempfile::TempDir,
    settings: Vec<std::ffi::OsString>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl FrozenGit {
    fn new(worktree: &Path, oid: &str, config: &[u8]) -> Result<Self> {
        use std::os::unix::ffi::OsStringExt;

        crate::registry::owned_directory(&worktree.join(".git/objects"))?;
        let directory = tempfile::Builder::new()
            .prefix(".graft-checkout-")
            .tempdir_in(worktree.join(".git"))?;
        let admin = directory.path();
        fs::create_dir(admin.join("objects"))?;
        fs::create_dir(admin.join("refs"))?;
        fs::write(admin.join("HEAD"), format!("{oid}\n"))?;
        if oid.len() == 64 {
            fs::write(
                admin.join("config"),
                "[core]\n repositoryformatversion = 1\n[extensions]\n objectformat = sha256\n",
            )?;
        }
        let settings = config
            .split(|&byte| byte == 0)
            .filter(|item| !item.is_empty())
            .map(|item| {
                let mut item = item.to_vec();
                if let Some(separator) = item.iter().position(|&byte| byte == b'\n') {
                    item[separator] = b'=';
                } else {
                    item.extend_from_slice(b"=true");
                }
                std::ffi::OsString::from_vec(item)
            })
            .collect();
        Ok(Self {
            directory,
            settings,
        })
    }

    fn configure(&self, mut command: Command, worktree: &Path, oid: &str) -> Command {
        command
            .env("GIT_DIR", self.directory.path())
            .env("GIT_WORK_TREE", worktree)
            .env("GIT_OBJECT_DIRECTORY", worktree.join(".git/objects"))
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_GRAFT_FILE", "/dev/null")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_ATTR_SOURCE", oid)
            .args(["-c", "core.attributesFile=/dev/null"]);
        for setting in &self.settings {
            command.arg("-c").arg(setting);
        }
        command
    }
}

// External attributes, replacement blobs and arbitrary filters can change
// bytes without changing commit or blob IDs. They always use Git.
fn conversion_reason(checkout: &mut Checkout<'_>) -> Result<Option<&'static str>> {
    let (worktree, oid, entries) = (checkout.worktree, checkout.oid, checkout.entries);
    for variable in ["GIT_ATTR_SYSTEM", "GIT_ATTR_GLOBAL"] {
        if variable == "GIT_ATTR_SYSTEM"
            && std::env::var_os("GIT_ATTR_NOSYSTEM").as_deref() == Some(OsStr::new("1"))
        {
            continue;
        }
        let Ok(output) = git(worktree, &["var", variable]) else {
            return Ok(Some("cannot determine external attributes"));
        };
        if let Some(reason) = external_attributes(&output.stdout) {
            return Ok(Some(reason));
        }
    }
    // The common Git directory was verified to be this workspace's .git.
    if let Some(reason) = existing_attributes(&worktree.join(".git/info/attributes")) {
        return Ok(Some(reason));
    }
    if !checkout.symlinks {
        return Ok(Some("Git symlink emulation"));
    }
    if entries.iter().any(|entry| entry.mode == 0o160000) {
        return Ok(Some("submodules"));
    }
    if !git(
        worktree,
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "refs/replace",
        ],
    )?
    .stdout
    .is_empty()
    {
        return Ok(Some("Git replacement objects"));
    }
    if !checkout.inspect_attributes {
        return Ok(None);
    }
    let input = path_input(entries.iter())?;
    let source = format!("--source={oid}");
    let attrs = [
        "filter",
        "ident",
        "working-tree-encoding",
        "text",
        "eol",
        "crlf",
    ];
    let output = checkout
        .command()
        .args(["check-attr", &source, "-z", "--stdin"])
        .args(attrs)
        .stdin(Stdio::from(input))
        .output()?;
    if !output.status.success() {
        return Ok(Some("cannot inspect committed attributes"));
    }
    let mut fields = output.stdout.split(|b| *b == 0);
    for entry in entries {
        for attr in attrs {
            if fields.next() != Some(entry.name.as_slice())
                || fields.next() != Some(attr.as_bytes())
            {
                return Ok(Some("cannot inspect committed attributes"));
            }
            match fields.next() {
                None => return Ok(Some("cannot inspect committed attributes")),
                Some(b"unspecified" | b"unset") => (),
                Some(_) if matches!(attr, "text" | "eol" | "crlf") => {
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    {
                        checkout.may_convert_eol = true;
                    }
                }
                Some(_) => return Ok(Some("path-dependent checkout conversion")),
            }
        }
    }
    if fields.next() != Some(b"") || fields.next().is_some() {
        return Ok(Some("cannot inspect committed attributes"));
    }
    Ok(None)
}

fn external_attributes(output: &[u8]) -> Option<&'static str> {
    let Ok(path) = std::str::from_utf8(output) else {
        return Some("cannot determine external attributes");
    };
    #[cfg(windows)]
    let path = path
        .strip_suffix("\r\n")
        .or_else(|| path.strip_suffix('\n'));
    #[cfg(not(windows))]
    let path = path.strip_suffix('\n');
    let Some(path) = path else {
        return Some("cannot determine external attributes");
    };
    let path = Path::new(path);
    if !path.is_absolute() {
        return Some("cannot determine external attributes");
    }
    existing_attributes(path)
}

fn existing_attributes(path: &Path) -> Option<&'static str> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        _ => Some("external checkout attributes"),
    }
}

/// Populate a registered standalone checkout from its own Git object store.
/// The caller has authenticated the workspace and created its private HEAD.
pub(crate) fn materialize(
    workspace: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: &Path,
) -> Result<Value> {
    let workspace = dunce::canonicalize(workspace)?;
    ensure_empty(&workspace)?;
    let oid = checkout_identity(&workspace, oid)?;
    // A bare primary is the common directory and has no checkout files.
    let donor = (state.parent() != Some(primary)).then_some(primary);
    materialize_checkout(&workspace, state, &oid, sparse, donor)
}

fn materialize_checkout(
    worktree: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: Option<&Path>,
) -> Result<Value> {
    validate_sparse(worktree, oid, sparse)?;
    let entries = entries(worktree, oid)?;
    if !sparse.is_empty() {
        return native_checkout(worktree, oid, sparse, &entries, "sparse checkout");
    }
    if entries.is_empty() {
        return native_checkout(worktree, oid, sparse, &entries, "empty tree");
    }
    let mut checkout = Checkout::read(worktree, oid, &entries)?;
    if let Some(reason) = conversion_reason(&mut checkout)? {
        return native_checkout(worktree, oid, sparse, &entries, reason);
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        fs::create_dir_all(state)?;
        let state = dunce::canonicalize(state)?;
        ensure!(
            !state.starts_with(worktree),
            "storage state must be outside the worktree"
        );
        if probe(&state, worktree)? {
            if let Ok(prepared) = prepare(&checkout, &state, primary) {
                return prepared.install(&checkout);
            }
            return native_checkout(worktree, oid, sparse, &entries, "snapshot unavailable");
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = (state, primary);
    native_checkout(worktree, oid, sparse, &entries, "native clone unavailable")
}

/// Authenticate an unchanged commit snapshot, including after quarantine.
pub(crate) fn verify_base(path: &Path, state: &Path, oid: &str) -> Result<Vec<[u8; 32]>> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        cache::verify_base(path, state, oid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (path, state, oid);
        bail!("source snapshot ownership cannot be verified on this platform")
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct Prepared {
    staging: tempfile::TempDir,
    receipt: Receipt,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Prepared {
    fn install(self, checkout: &Checkout<'_>) -> Result<Value> {
        let worktree = checkout.worktree;
        let git_dir = worktree.join(".git");
        crate::registry::owned_directory(&git_dir)?;
        let destination = git_dir.join("index");
        let lock = git_dir.join("index.lock");
        // Use Git's lock protocol while publishing into a newly initialized
        // repository. The index must never overwrite one created by Git.
        let _lock = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .context("reserve workspace Git index")?;
        let lock = tempfile::TempPath::try_from_path(lock)?;
        ensure!(
            matches!(fs::symlink_metadata(&destination), Err(error) if error.kind() == io::ErrorKind::NotFound),
            "workspace Git index already exists"
        );
        for entry in fs::read_dir(self.staging.path().join("tree"))? {
            let entry = entry?;
            native::rename_new(&entry.path(), &worktree.join(entry.file_name()))?;
        }
        let index = self.staging.path().join("index");
        // Git verifies the final inodes and records their stats with its own
        // racy-index handling. APFS may change a top-level file's ctime on rename.
        StagedCheckout {
            checkout,
            index: &index,
            tree: worktree,
        }
        .run(&["update-index", "--refresh"], None)?;
        native::rename_new(&index, &destination)?;
        lock.close()?;
        let backend = if cfg!(target_os = "macos") {
            "clonefile"
        } else {
            "reflink"
        };
        Ok(self.receipt.json(backend, None))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn prepare(checkout: &Checkout<'_>, state: &Path, primary: Option<&Path>) -> Result<Prepared> {
    let (worktree, oid, entries) = (checkout.worktree, checkout.oid, checkout.entries);
    let bases = state.join("bases");
    crate::registry::dir(&bases)?;
    let commit = bases.join(oid);
    crate::registry::dir(&commit)?;
    let settings = checkout.settings.as_str();
    let base = commit.join(settings);
    let secret = cache::key(state, true)?;
    let mut receipt = Receipt::default();
    let mode = native::creation_mode(worktree)?;
    let staging = tempfile::Builder::new()
        .prefix(".graft-source-")
        .tempdir_in(worktree)?;
    let tree = staging.path().join("tree");
    fs::create_dir(&tree)?;
    let index = staging.path().join("index");
    let staged = StagedCheckout {
        checkout,
        index: &index,
        tree: &tree,
    };
    staged.run(&["read-tree", oid], None)?;
    let snapshot = match fs::symlink_metadata(&base) {
        Ok(_) => {
            receipt.warm = true;
            cache::Snapshot::load(&base, &secret, oid, settings)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = parent_candidate(checkout, &bases, settings, &secret);
            build_snapshot(
                &staged,
                &secret,
                &base,
                parent.as_ref(),
                primary,
                mode,
                &mut receipt,
            )?
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        snapshot
            .records()
            .iter()
            .map(|record| &record.entry)
            .eq(entries.iter()),
        "source snapshot differs from the Git tree"
    );
    let destination = native::Root::open(&tree)?;
    stage_parents(&tree, entries)?;
    // Each worker pins the source and destination roots independently. Files
    // still pass the same stamp checks before and after every clone.
    let records = snapshot.records();
    let counts = std::thread::scope(|scope| -> Result<Vec<Counts>> {
        let mut tasks = Vec::new();
        for chunk in source_chunks(records) {
            let source = snapshot.fork_root()?;
            let destination = destination.fork()?;
            let tree = &tree;
            tasks.push(scope.spawn(move || -> Result<Counts> {
                let mut counts = Counts::default();
                for record in chunk {
                    let target = tree.join(record.entry.path()?);
                    if let Some(size) = record.clone_into(&source, &destination, &target, mode)? {
                        counts.add(size);
                    }
                }
                Ok(counts)
            }));
        }
        tasks
            .into_iter()
            .map(|task| {
                task.join()
                    .map_err(|_| anyhow::anyhow!("source clone worker failed"))?
            })
            .collect()
    })?;
    for count in counts {
        receipt.cloned.merge(count);
    }
    Ok(Prepared { staging, receipt })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn source_chunks<T>(entries: &[T]) -> std::slice::Chunks<'_, T> {
    let workers = if entries.len() >= PARALLEL_THRESHOLD {
        std::thread::available_parallelism().map_or(1, |count| count.get().min(4))
    } else {
        1
    };
    entries.chunks(entries.len().div_ceil(workers).max(1))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stage_parents(tree: &Path, entries: &[Entry]) -> Result<()> {
    let mut previous = Path::new("");
    for entry in entries {
        let parent = entry.path()?.parent().context("missing source directory")?;
        if previous != parent {
            fs::create_dir_all(tree.join(parent))?;
            previous = parent;
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parent_candidate(
    checkout: &Checkout<'_>,
    bases: &Path,
    settings: &str,
    secret: &cache::Key,
) -> Option<cache::Snapshot> {
    // An empty cache is the common first spawn and has no parent to discover.
    fs::read_dir(bases).ok()?.filter_map(Result::ok).nth(1)?;
    let parents = checkout
        .command()
        .args(["show", "-s", "--format=%P", checkout.oid])
        .output()
        .ok()?;
    if !parents.status.success() {
        return None;
    }
    let parent = std::str::from_utf8(&parents.stdout)
        .ok()?
        .split_whitespace()
        .next()?;
    crate::git::checked_oid(parent).ok()?;
    let snapshot =
        cache::Snapshot::load(&bases.join(parent).join(settings), secret, parent, settings).ok()?;
    snapshot.verify().ok()?;
    Some(snapshot)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn build_snapshot(
    staged: &StagedCheckout<'_>,
    secret: &cache::Key,
    base: &Path,
    parent: Option<&cache::Snapshot>,
    primary: Option<&Path>,
    mode: u32,
    receipt: &mut Receipt,
) -> Result<cache::Snapshot> {
    let checkout = staged.checkout;
    let entries = checkout.entries;
    let base_parent = base.parent().context("missing snapshot directory")?;
    let staging = tempfile::Builder::new()
        .prefix(".building-")
        .tempdir_in(base_parent)?;
    let generation = staging.path().join("generation");
    let tree = cache::tree(&generation);
    fs::create_dir(&generation)?;
    fs::create_dir(&tree)?;
    stage_parents(&tree, entries)?;

    let donor = primary.and_then(|primary| native::Root::open(primary).ok());
    let private = native::Root::open(&tree)?;
    let previous: BTreeMap<_, _> = parent
        .into_iter()
        .flat_map(cache::Snapshot::records)
        .map(|record| (record.entry.name.as_slice(), record))
        .collect();
    let batches = std::thread::scope(|scope| -> Result<Vec<Reused<'_>>> {
        let mut tasks = Vec::new();
        for chunk in source_chunks(entries) {
            let donor = donor.as_ref().map(native::Root::fork).transpose()?;
            let parent = parent.map(cache::Snapshot::fork_root).transpose()?;
            let private = private.fork()?;
            let tree = &tree;
            let previous = &previous;
            tasks.push(scope.spawn(move || {
                reuse_snapshot_files(
                    checkout,
                    chunk,
                    &private,
                    tree,
                    parent.as_ref().map(|root| (root, previous)),
                    donor.as_ref(),
                    mode,
                )
            }));
        }
        tasks
            .into_iter()
            .map(|task| {
                task.join()
                    .map_err(|_| anyhow::anyhow!("source verification worker failed"))?
            })
            .collect()
    })?;
    let mut reused = BTreeMap::new();
    for batch in batches {
        receipt.parent.merge(batch.parent);
        receipt.primary.merge(batch.primary);
        reused.extend(batch.files);
    }

    if reused.len() < entries.len() {
        let missing: Vec<_> = entries
            .iter()
            .filter(|entry| !reused.contains_key(entry.name.as_slice()))
            .collect();
        StagedCheckout {
            checkout,
            index: staged.index,
            tree: &tree,
        }
        .run(
            &["checkout-index", "--stdin", "-z"],
            Some(path_input(missing.iter().copied())?),
        )?;
        for entry in missing.into_iter().filter(|entry| entry.regular()) {
            receipt.private.add(private.regular_size(entry.path()?)?);
        }
    }
    cache::Snapshot::publish(
        &generation,
        base,
        entries,
        reused,
        secret,
        checkout.oid,
        &checkout.settings,
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[derive(Default)]
struct Reused<'a> {
    parent: Counts,
    primary: Counts,
    files: Vec<(&'a [u8], Option<cache::Stamp>)>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn reuse_snapshot_files<'a>(
    checkout: &Checkout<'_>,
    entries: &'a [Entry],
    private: &native::Root,
    tree: &Path,
    parent: Option<(&native::Root, &BTreeMap<&[u8], &cache::Record>)>,
    donor: Option<&native::Root>,
    mode: u32,
) -> Result<Reused<'a>> {
    let mut reused = Reused::default();
    let mut primary_clones = Vec::new();
    for entry in entries.iter().filter(|entry| entry.regular()) {
        let path = entry.path()?;
        if let Some((source, previous)) = parent
            && let Some(&record) = previous.get(entry.name.as_slice())
            && record.entry == *entry
            && let Ok(Some(size)) = record.clone_into(source, private, &tree.join(path), mode)
        {
            reused.parent.add(size);
            reused.files.push((entry.name.as_slice(), None));
            continue;
        }
        // Git's filtered batch format separates the object and path by
        // whitespace. Let checkout-index handle names it cannot encode.
        if checkout.may_convert_eol && entry.name[0].is_ascii_whitespace() {
            continue;
        }
        if let Some(donor) = donor
            && let Ok((source, metadata)) = donor.regular_metadata(path)
            && private
                .clone_regular(path, &source, &metadata, entry.creation_mode(mode), false)
                .is_ok()
        {
            primary_clones.push(entry);
        }
    }
    let digests = verify::checkout_digests(checkout, &primary_clones)?;
    for (entry, expected) in primary_clones.into_iter().zip(digests) {
        let path = entry.path()?;
        let Ok((file, metadata)) = private.open_primary_clone(path, entry.creation_mode(mode))
        else {
            private.discard(path)?;
            continue;
        };
        if verify::digest(file)? == expected {
            reused.primary.add(metadata.len());
            reused
                .files
                .push((entry.name.as_slice(), Some(cache::Stamp::from(&metadata))));
        } else {
            private.discard(path)?;
        }
    }
    Ok(reused)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct StagedCheckout<'a> {
    checkout: &'a Checkout<'a>,
    index: &'a Path,
    tree: &'a Path,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl StagedCheckout<'_> {
    fn run(&self, args: &[&str], input: Option<fs::File>) -> Result<()> {
        let mut command = parallel_checkout(self.checkout.command());
        command
            .env("GIT_INDEX_FILE", self.index)
            .env("GIT_WORK_TREE", self.tree)
            .args([
                "-c",
                "core.sparseCheckout=false",
                "-c",
                "core.protectHFS=true",
                "-c",
                "core.protectNTFS=true",
                "-c",
                "core.splitIndex=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
                "-c",
                "core.trustctime=true",
                "-c",
                "core.checkStat=default",
                "-c",
                "core.ignorestat=false",
            ])
            .args(args);
        if let Some(input) = input {
            command.stdin(Stdio::from(input));
        }
        let output = command.output()?;
        ensure!(
            output.status.success(),
            "Git staged checkout: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
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
    // libc supplies the ioctl encoding and request type for this Linux target.
    if unsafe { libc::ioctl(dest.as_raw_fd(), libc::FICLONE, source.as_raw_fd()) } == 0 {
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
        fixture_with_format("sha1")
    }

    fn fixture_with_format(object_format: &str) -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let repo = dunce::canonicalize(repo).unwrap();
        command(
            &repo,
            &["init", "-q", &format!("--object-format={object_format}")],
        );
        command(&repo, &["config", "user.email", "test@example.invalid"]);
        command(&repo, &["config", "user.name", "Test"]);
        command(&repo, &["config", "core.autocrlf", "false"]);
        command(&repo, &["config", "core.symlinks", "true"]);
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
    fn independent(repo: &Path, workspace: &Path) {
        command(
            repo,
            &[
                "clone",
                "-q",
                "--no-local",
                "--no-checkout",
                ".",
                workspace.to_str().unwrap(),
            ],
        );
        command(workspace, &["config", "core.autocrlf", "false"]);
        command(workspace, &["config", "core.symlinks", "true"]);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn local_uses_only_clean_tracked_donor_files() {
        use std::os::unix::fs::MetadataExt;

        for format in ["sha1", "sha256"] {
            let (temp, repo, _) = fixture_with_format(format);
            for name in ["staged", "missing", "untracked", "ignored"] {
                fs::write(repo.join(name), b"base\n").unwrap();
            }
            fs::write(repo.join(".gitignore"), b"ignored\n").unwrap();
            command(&repo, &["add", "."]);
            command(&repo, &["add", "-f", "ignored"]);
            command(&repo, &["commit", "-qm", "more files"]);
            let oid = command(&repo, &["rev-parse", "HEAD"]);
            let workspace = temp.path().join("local");
            independent(&repo, &workspace);

            // The size and mtime are unchanged, and Git is told to skip the
            // stat check. The bytes must still be authenticated independently.
            let donor = repo.join("sub/file");
            let mtime = fs::metadata(&donor).unwrap().modified().unwrap();
            command(&repo, &["update-index", "--assume-unchanged", "sub/file"]);
            fs::write(&donor, b"unexpected").unwrap();
            fs::File::open(&donor)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(mtime))
                .unwrap();
            fs::write(repo.join("staged"), b"edit\n").unwrap();
            command(&repo, &["add", "staged"]);
            fs::write(repo.join("staged"), b"base\n").unwrap();
            command(&repo, &["rm", "--cached", "untracked", "ignored"]);
            fs::remove_file(repo.join("missing")).unwrap();
            fs::write(repo.join("ignored-extra"), b"private\n").unwrap();
            fs::write(repo.join(".git/info/exclude"), b"ignored-extra\n").unwrap();
            let donor_index = fs::read(repo.join(".git/index")).unwrap();

            let state = temp.path().join("state");
            let result = materialize_local(&workspace, &state, &oid, &[], &repo).unwrap();
            assert_eq!(result["snapshot_reused"], false);
            assert!(!state.exists());
            assert_eq!(
                command(&workspace, &["write-tree"]),
                command(&repo, &["rev-parse", &format!("{oid}^{{tree}}")]),
            );
            assert_eq!(command(&workspace, &["status", "--porcelain"]), "");
            assert_eq!(
                fs::read(workspace.join("sub/file")).unwrap(),
                b"committed\n"
            );
            for name in ["staged", "missing", "untracked", "ignored"] {
                assert_eq!(fs::read(workspace.join(name)).unwrap(), b"base\n");
            }
            assert!(!workspace.join("ignored-extra").exists());
            assert_eq!(
                fs::read_link(workspace.join("link")).unwrap(),
                Path::new("sub/file")
            );
            assert_ne!(
                fs::metadata(workspace.join("script")).unwrap().mode() & 0o100,
                0
            );
            assert_ne!(
                fs::metadata(workspace.join("script")).unwrap().ino(),
                fs::metadata(repo.join("script")).unwrap().ino(),
            );
            if probe(&workspace, &workspace).unwrap() {
                assert_eq!(result["primary_reused_files"], 2, "{result}");
                assert_eq!(result["private_files"], 5, "{result}");
                assert_eq!(result["cloned_files"], result["primary_reused_files"]);
            }
            fs::write(workspace.join("script"), b"workspace edit").unwrap();
            assert_eq!(fs::read(repo.join("script")).unwrap(), b"#!/bin/sh\n");
            assert_eq!(command(&workspace, &["status", "--porcelain"]), "M script");
            assert_eq!(fs::read(donor).unwrap(), b"unexpected");
            assert_eq!(fs::read(repo.join(".git/index")).unwrap(), donor_index);
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn local_defers_auto_crlf_to_native_git() {
        let (temp, repo, _) = fixture();
        fs::write(repo.join(".gitattributes"), b"explicit eol=lf\n").unwrap();
        fs::write(repo.join("explicit"), b"explicit\n").unwrap();
        fs::write(repo.join("binary"), b"\0binary\n").unwrap();
        fs::write(repo.join("mixed"), b"mixed\r\nend\n").unwrap();
        fs::write(
            repo.join("long"),
            [vec![b'a'; 65535], b"\nend\n".to_vec()].concat(),
        )
        .unwrap();
        command(&repo, &["add", "."]);
        command(&repo, &["commit", "-qm", "checkout conversions"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        let donor = temp.path().join("donor");
        let workspace = temp.path().join("local");
        for path in [&donor, &workspace] {
            independent(&repo, path);
            command(path, &["config", "core.autocrlf", "true"]);
        }
        command(&donor, &["read-tree", "-u", "-m", &oid]);
        let result =
            materialize_local(&workspace, &temp.path().join("state"), &oid, &[], &donor).unwrap();
        for name in [
            ".gitattributes",
            "explicit",
            "binary",
            "mixed",
            "long",
            "sub/file",
            "script",
        ] {
            assert_eq!(
                fs::read(workspace.join(name)).unwrap(),
                fs::read(donor.join(name)).unwrap(),
                "{name}"
            );
        }
        assert_eq!(
            fs::read(workspace.join("sub/file")).unwrap(),
            b"committed\r\n"
        );
        assert_eq!(fs::read(workspace.join("explicit")).unwrap(), b"explicit\n");
        assert_eq!(command(&workspace, &["status", "--porcelain"]), "");
        assert_eq!(result["backend"], "git");
        assert_eq!(result["primary_reused_files"], 0, "{result}");
        assert_eq!(result["private_files"], 7, "{result}");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn local_keeps_an_edit_made_before_index_publication() {
        let (temp, repo, _) = fixture();
        fs::write(repo.join(".gitattributes"), b"filtered filter=edit\n").unwrap();
        fs::write(repo.join("filtered"), b"filtered\n").unwrap();
        command(&repo, &["add", "."]);
        command(&repo, &["commit", "-qm", "checkout filter"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        let workspace = temp.path().join("local");
        independent(&repo, &workspace);
        if !probe(&workspace, &workspace).unwrap() {
            return;
        }
        command(
            &workspace,
            &[
                "config",
                "filter.edit.smudge",
                "printf modified!\\\\n > sub/file && cat",
            ],
        );
        let receipt =
            materialize_local(&workspace, &temp.path().join("state"), &oid, &[], &repo).unwrap();
        assert_eq!(receipt["primary_reused_files"], 3, "{receipt}");
        assert_eq!(
            fs::read(workspace.join("sub/file")).unwrap(),
            b"modified!\n"
        );
        assert_eq!(
            command(&workspace, &["status", "--porcelain"]),
            "M sub/file"
        );
        assert_eq!(command(&workspace, &["show", ":sub/file"]), "committed");
    }

    #[cfg(unix)]
    #[test]
    fn local_preserves_a_file_created_during_git_fallback() {
        let (temp, repo, _) = fixture();
        fs::write(repo.join(".gitattributes"), b"first filter=race\n").unwrap();
        fs::write(repo.join("first"), b"first\n").unwrap();
        fs::write(repo.join("z-new"), b"base\n").unwrap();
        command(&repo, &["add", "."]);
        command(&repo, &["commit", "-qm", "checkout race"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        fs::remove_file(repo.join("z-new")).unwrap();
        let state = temp.path().join("state");
        for autocrlf in ["false", "true"] {
            let workspace = temp.path().join(autocrlf);
            independent(&repo, &workspace);
            command(&workspace, &["config", "core.autocrlf", autocrlf]);
            let contested = workspace.join("z-new");
            command(
                &workspace,
                &[
                    "config",
                    "filter.race.smudge",
                    &format!("printf 'new work' > '{}' && cat", contested.display()),
                ],
            );
            let error = materialize_local(&workspace, &state, &oid, &[], &repo).unwrap_err();
            assert!(format!("{error:#}").contains("z-new"), "{error:#}");
            assert_eq!(fs::read(contested).unwrap(), b"new work");
            assert!(!workspace.join(".git/index").exists());
        }
    }

    #[test]
    fn local_respects_live_index_and_native_sparse_checkout() {
        let (temp, repo, oid) = fixture();
        let state = temp.path().join("state");
        let workspace = temp.path().join("local");
        independent(&repo, &workspace);
        let index_lock = workspace.join(".git/index.lock");
        fs::write(&index_lock, b"other writer").unwrap();
        assert!(materialize_local(&workspace, &state, &oid, &[], &repo).is_err());
        assert_eq!(fs::read(&index_lock).unwrap(), b"other writer");
        assert!(!workspace.join("script").exists());
        fs::remove_file(index_lock).unwrap();
        command(&workspace, &["read-tree", &oid]);
        let index = fs::read(workspace.join(".git/index")).unwrap();
        assert!(materialize_local(&workspace, &state, &oid, &[], &repo).is_err());
        assert_eq!(fs::read(workspace.join(".git/index")).unwrap(), index);

        let sparse = temp.path().join("sparse");
        independent(&repo, &sparse);
        let receipt = materialize_local(&sparse, &state, &oid, &["sub".into()], &repo).unwrap();
        assert_eq!(receipt["backend"], "git");
        assert_eq!(receipt["reason"], "sparse checkout");
        assert_eq!(command(&sparse, &["sparse-checkout", "list"]), "sub");
        assert_eq!(command(&sparse, &["status", "--porcelain"]), "");
    }

    #[test]
    fn independent_checkout_reuses_verified_snapshots_and_falls_back_to_git() {
        let (temp, repo, oid) = fixture();
        let repo = dunce::canonicalize(&repo).unwrap();
        let state = temp.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::write(repo.join("sub/file"), b"dirty primary").unwrap();
        let first = temp.path().join("first");
        independent(&repo, &first);
        let cold = materialize(&first, &state, &oid, &[], &repo).unwrap();
        assert_eq!(cold["snapshot_reused"], false);
        assert_eq!(fs::read(first.join("sub/file")).unwrap(), b"committed\n");
        assert_eq!(command(&first, &["status", "--porcelain"]), "");
        fs::write(first.join("sub/file"), b"edited workspace").unwrap();

        let second = temp.path().join("second");
        independent(&repo, &second);
        let warm = materialize(&second, &state, &oid, &[], &repo).unwrap();
        assert_eq!(fs::read(second.join("sub/file")).unwrap(), b"committed\n");
        assert_eq!(fs::read(repo.join("sub/file")).unwrap(), b"dirty primary");
        assert_eq!(command(&second, &["status", "--porcelain"]), "");
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let clone_supported = probe(&state, &second).unwrap();
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let clone_supported = false;
        if clone_supported {
            assert_ne!(cold["backend"], "git", "{cold}");
            assert!(cold["primary_reused_files"].as_u64().unwrap() > 0, "{cold}");
            assert_eq!(warm["snapshot_reused"], true, "{warm}");
            assert!(warm["cloned_files"].as_u64().unwrap() > 0, "{warm}");
        } else {
            for receipt in [&cold, &warm] {
                assert_eq!(receipt["backend"], "git", "{receipt}");
                assert_eq!(receipt["snapshot_reused"], false, "{receipt}");
                assert_eq!(receipt["cloned_files"], 0, "{receipt}");
            }
        }

        let fallback = temp.path().join("fallback");
        independent(&repo, &fallback);
        let info = fallback.join(".git/info");
        fs::create_dir_all(&info).unwrap();
        #[cfg(unix)]
        let (attributes, expected): (&str, &[u8]) = {
            command(
                &fallback,
                &["config", "filter.fixture.smudge", "tr a-z A-Z"],
            );
            command(&fallback, &["config", "filter.fixture.clean", "tr A-Z a-z"]);
            ("sub/file text eol=crlf filter=fixture\n", b"COMMITTED\r\n")
        };
        #[cfg(not(unix))]
        let (attributes, expected) = ("sub/file text eol=crlf\n", b"committed\r\n");
        fs::write(info.join("attributes"), attributes).unwrap();
        let receipt = materialize(&fallback, &state, &oid, &[], &repo).unwrap();
        assert_eq!(receipt["backend"], "git", "{receipt}");
        assert_eq!(receipt["reason"], "external checkout attributes");
        assert_eq!(fs::read(fallback.join("sub/file")).unwrap(), expected);
        assert_eq!(command(&fallback, &["status", "--porcelain"]), "");

        let sparse = temp.path().join("sparse");
        independent(&repo, &sparse);
        let receipt = materialize(&sparse, &state, &oid, &["sub".into()], &repo).unwrap();
        assert_eq!(receipt["backend"], "git", "{receipt}");
        assert_eq!(receipt["reason"], "sparse checkout");
        assert_eq!(fs::read(sparse.join("sub/file")).unwrap(), b"committed\n");
        assert_eq!(command(&sparse, &["status", "--porcelain"]), "");
    }

    #[test]
    fn independent_checkout_refuses_redirected_git_metadata() {
        let (temp, repo, oid) = fixture();
        let state = temp.path().join("state");
        let linked = temp.path().join("linked");
        command(
            &repo,
            &[
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                linked.to_str().unwrap(),
                &oid,
            ],
        );
        assert!(materialize(&linked, &state, &oid, &[], &repo).is_err());
        assert!(!linked.join("sub").exists());

        let redirected = temp.path().join("redirected");
        independent(&repo, &redirected);
        fs::write(
            redirected.join(".git/commondir"),
            format!("{}\n", repo.join(".git").display()),
        )
        .unwrap();
        assert!(materialize(&redirected, &state, &oid, &[], &repo).is_err());
        assert!(!redirected.join(".git/index").exists());
        assert!(!redirected.join("sub").exists());
    }

    #[cfg(unix)]
    #[test]
    fn checkout_identity_preserves_newlines_and_rejects_a_distinct_root() {
        let (temp, repo, oid) = fixture();
        let state = temp.path().join("state");
        let workspace = temp.path().join("workspace\nwith newline");
        independent(&repo, &workspace);
        materialize(&workspace, &state, &oid, &[], &repo).unwrap();
        assert_eq!(
            fs::read(workspace.join("sub/file")).unwrap(),
            b"committed\n"
        );

        let workspace = temp.path().join("redirected-root");
        independent(&repo, &workspace);
        let target = temp.path().join("redirected-root\r");
        fs::create_dir(&target).unwrap();
        command(
            &workspace,
            &["config", "core.worktree", target.to_str().unwrap()],
        );
        assert!(materialize(&workspace, &state, &oid, &[], &repo).is_err());
        assert!(!target.join("sub").exists());
        assert!(!workspace.join(".git/index").exists());
    }

    #[test]
    fn large_tree_clones_verified_files_and_preserves_conversions() {
        let (temp, repo, _) = fixture();
        let state = temp.path().join("state");
        fs::create_dir(&state).unwrap();
        for i in 0..PARALLEL_THRESHOLD {
            fs::write(repo.join(format!("sub/small-{i}")), b"small\n").unwrap();
        }
        fs::write(repo.join(".gitattributes"), "sub/small-* text eol=crlf\n").unwrap();
        let large = vec![0xa5; 2 * 1024 * 1024];
        fs::write(repo.join("sub/large"), &large).unwrap();
        fs::write(repo.join("top-level-large"), &large).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                repo.join("top-level-large"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        command(&repo, &["add", "."]);
        command(&repo, &["commit", "-qm", "mixed checkout"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        fs::write(repo.join("sub/small-0"), b"private\n").unwrap();
        fs::write(repo.join("sub/ignored"), b"private\n").unwrap();
        fs::write(repo.join(".git/info/exclude"), b"sub/ignored\n").unwrap();

        for (name, warm) in [("mixed-cold", false), ("mixed-warm", true)] {
            let workspace = temp.path().join(name);
            independent(&repo, &workspace);
            let receipt = materialize(&workspace, &state, &oid, &[], &repo).unwrap();
            assert_eq!(command(&workspace, &["diff-files", "--quiet"]), "");
            assert_eq!(command(&workspace, &["status", "--porcelain"]), "");
            assert_eq!(
                fs::read(workspace.join("sub/small-0")).unwrap(),
                b"small\r\n"
            );
            assert_eq!(fs::read(workspace.join("sub/large")).unwrap(), large);
            assert_eq!(fs::read(workspace.join("top-level-large")).unwrap(), large);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_ne!(
                    fs::metadata(workspace.join("top-level-large"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o111,
                    0
                );
            }
            assert!(!workspace.join("sub/ignored").exists());
            if probe(&state, &workspace).unwrap() {
                assert!(
                    receipt["cloned_files"].as_u64().unwrap() >= PARALLEL_THRESHOLD as u64 + 2,
                    "{receipt}"
                );
                assert_eq!(receipt["snapshot_reused"], warm, "{receipt}");
                if warm {
                    assert_eq!(receipt["private_files"], 0, "{receipt}");
                } else {
                    assert!(
                        receipt["primary_reused_files"].as_u64().unwrap() >= 2,
                        "{receipt}"
                    );
                    assert!(
                        receipt["private_files"].as_u64().unwrap() >= PARALLEL_THRESHOLD as u64,
                        "{receipt}"
                    );
                }
            }
            fs::write(workspace.join("sub/large"), b"workspace edit").unwrap();
        }
        assert_eq!(fs::read(repo.join("sub/large")).unwrap(), large);
        assert_eq!(fs::read(repo.join("sub/small-0")).unwrap(), b"private\n");
        assert_eq!(fs::read(repo.join("sub/ignored")).unwrap(), b"private\n");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn publication_preserves_an_existing_index_lock_index_or_checkout_file() -> Result<()> {
        let (temp, repo, oid) = fixture();
        let state = dunce::canonicalize(temp.path())?.join("state");
        fs::create_dir(&state)?;
        if !probe(&state, &state)? {
            return Ok(());
        }
        for conflict in [".git/index.lock", ".git/index", "SCRIPT"] {
            let workspace = state.join(conflict.replace('/', "-"));
            independent(&repo, &workspace);
            let entries = entries(&workspace, &oid)?;
            let checkout = Checkout::read(&workspace, &oid, &entries)?;
            let prepared = prepare(&checkout, &state, Some(&repo))?;
            let mut conflict = workspace.join(conflict);
            fs::write(&conflict, b"preserve this")?;
            if conflict.ends_with("SCRIPT") && !workspace.join("script").exists() {
                fs::rename(&conflict, workspace.join("script"))?;
                conflict = workspace.join("script");
            }
            assert!(prepared.install(&checkout).is_err());
            assert_eq!(fs::read(&conflict)?, b"preserve this");
            if !conflict.ends_with("index") {
                assert!(!workspace.join(".git/index").exists());
            }
            if !conflict.ends_with("index.lock") {
                assert!(!workspace.join(".git/index.lock").exists());
            }
        }
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn publication_does_not_record_stats_for_a_same_size_edit() -> Result<()> {
        for format in ["sha1", "sha256"] {
            let (temp, repo, oid) = fixture_with_format(format);
            let root = dunce::canonicalize(temp.path())?;
            let state = root.join("state");
            fs::create_dir(&state)?;
            if !probe(&state, &state)? {
                return Ok(());
            }
            let workspace = root.join("workspace");
            independent(&repo, &workspace);
            let entries = entries(&workspace, &oid)?;
            let checkout = Checkout::read(&workspace, &oid, &entries)?;
            let prepared = prepare(&checkout, &state, Some(&repo))?;
            let file = prepared.staging.path().join("tree/script");
            let modified = fs::metadata(&file)?.modified()?;
            let replacement = file.with_extension("replacement");
            fs::write(&replacement, b"#!/bin/xx\n")?;
            fs::set_permissions(&replacement, fs::metadata(&file)?.permissions())?;
            fs::File::open(&replacement)?.set_modified(modified)?;
            fs::rename(&replacement, file)?;
            assert!(prepared.install(&checkout).is_err());
            assert!(!workspace.join(".git/index").exists());
        }
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_checkout_config_and_external_attributes_cannot_poison_a_snapshot() -> Result<()> {
        let (temp, repo, _) = fixture();
        fs::write(repo.join(".gitattributes"), "sub/file text eol=lf\n")?;
        command(&repo, &["add", ".gitattributes"]);
        command(&repo, &["commit", "-qm", "committed checkout attributes"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        let root = dunce::canonicalize(temp.path())?;
        let state = root.join("state");
        fs::create_dir(&state)?;
        if !probe(&state, &state)? {
            return Ok(());
        }

        let raced = root.join("raced");
        independent(&repo, &raced);
        let entries = entries(&raced, &oid)?;
        let mut checkout = Checkout::read(&raced, &oid, &entries)?;
        assert_eq!(conversion_reason(&mut checkout)?, None);
        assert!(checkout.may_convert_eol);

        // Mutate input after eligibility was checked, but before building and
        // signing the generation. No dependency on process scheduling.
        command(&raced, &["config", "core.autocrlf", "true"]);
        let attrs = raced.join(".git/info/attributes");
        fs::write(&attrs, "sub/file text eol=crlf\n")?;
        let native = root.join("live-reference");
        independent(&repo, &native);
        command(&native, &["config", "core.autocrlf", "true"]);
        fs::copy(&attrs, native.join(".git/info/attributes"))?;
        command(&native, &["read-tree", "-m", "-u", &oid]);
        assert_eq!(fs::read(native.join("sub/file"))?, b"committed\r\n");
        assert_eq!(fs::read(native.join("script"))?, b"#!/bin/sh\r\n");

        // With no donor, Git produces the bytes that will be signed.
        prepare(&checkout, &state, None)?.install(&checkout)?;
        assert_eq!(fs::read(raced.join("sub/file"))?, b"committed\n");
        assert_eq!(fs::read(raced.join("script"))?, b"#!/bin/sh\n");

        let warm = root.join("warm");
        independent(&repo, &warm);
        let receipt = materialize(&warm, &state, &oid, &[], &repo)?;
        assert_eq!(receipt["snapshot_reused"], true, "{receipt}");
        assert_eq!(fs::read(warm.join("sub/file"))?, b"committed\n");
        assert_eq!(fs::read(warm.join("script"))?, b"#!/bin/sh\n");
        assert_eq!(command(&warm, &["diff-files", "--quiet"]), "");
        Ok(())
    }

    #[test]
    fn materializes_committed_tree_and_isolates_edits() {
        let (temp, repo, oid) = fixture();
        fs::write(repo.join("sub/file"), b"dirty main").unwrap();
        fs::write(repo.join("secret"), b"untracked").unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let state = temp.path().join("state");
        independent(&repo, &first);
        let result = materialize(&first, &state, &oid, &[], &repo).unwrap();
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
        independent(&repo, &second);
        let second_result = materialize(&second, &state, &oid, &[], &repo).unwrap();
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
        independent(&repo, &wt);
        for invalid in [
            "../escape",
            "/absolute",
            "sub/file",
            "missing",
            "-option",
            "sub//file",
        ] {
            assert!(
                materialize(
                    &wt,
                    &temp.path().join("state"),
                    &oid,
                    &[invalid.into()],
                    &repo
                )
                .is_err(),
                "{invalid}"
            );
        }
        let result = materialize(
            &wt,
            &temp.path().join("state"),
            &oid,
            &["sub".into()],
            &repo,
        )
        .unwrap();
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
        independent(&repo, &wt);
        fs::write(wt.join("occupied"), b"preserve").unwrap();
        assert!(materialize(&wt, &temp.path().join("state"), &oid, &[], &repo).is_err());
        assert_eq!(fs::read(wt.join("occupied")).unwrap(), b"preserve");
        fs::remove_file(wt.join("occupied")).unwrap();
        let result = materialize(&wt, &temp.path().join("state"), &oid, &[], &repo).unwrap();
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
        independent(&repo, &first);
        let first_result = materialize(&first, &state, &oid, &[], &repo).unwrap();
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
        let second = temp.path().join("second-attrs");
        independent(&repo, &second);
        command(&second, &["config", "core.autocrlf", "true"]);
        let result = materialize(&second, &state, &oid, &[], &repo).unwrap();
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
        command(&repo, &["add", ".gitattributes", "data.dat"]);
        command(&repo, &["commit", "-qm", "filter"]);
        let oid = command(&repo, &["rev-parse", "HEAD"]);
        let wt = temp.path().join("filtered");
        independent(&repo, &wt);
        command(&wt, &["config", "filter.demo.smudge", "cat"]);
        command(&wt, &["config", "filter.demo.clean", "cat"]);
        let result = materialize(&wt, &temp.path().join("state"), &oid, &[], &repo).unwrap();
        assert_eq!(result["backend"], "git");
        assert_eq!(result["reason"], "path-dependent checkout conversion");
        assert_eq!(command(&wt, &["status", "--porcelain"]), "");
        let external = temp.path().join("attributes");
        fs::write(&external, "*.dat text\n").unwrap();
        let wt = temp.path().join("external");
        independent(&repo, &wt);
        command(
            &wt,
            &["config", "core.attributesfile", external.to_str().unwrap()],
        );
        assert_eq!(
            materialize(&wt, &temp.path().join("state"), &oid, &[], &repo).unwrap()["reason"],
            "external checkout attributes"
        );

        let wt = temp.path().join("info-attributes");
        independent(&repo, &wt);
        let info = wt.join(".git/info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("attributes"), "*.dat text eol=crlf\n").unwrap();
        let result = materialize(&wt, &temp.path().join("state"), &oid, &[], &repo).unwrap();
        assert_eq!(result["backend"], "git");
        assert_eq!(result["reason"], "external checkout attributes");
        assert_eq!(fs::read(wt.join("data.dat")).unwrap(), b"content\r\n");
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
        independent(&repo, &wt);
        let result = materialize(&wt, &temp.path().join("state"), &oid, &[], &repo).unwrap();
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
        independent(&repo, &first);
        let initial = materialize(&first, &state, &parent, &[], &repo).unwrap();
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
        independent(&repo, &second);
        let result = materialize(&second, &state, &child, &[], &repo).unwrap();
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
        independent(&repo, &third);
        let again = materialize(&third, &state, &child, &[], &repo).unwrap();
        assert_eq!(fs::read(third.join("large.bin")).unwrap(), large);
        if initial["backend"] != "git" {
            assert_eq!(again["snapshot_reused"], true);
        }

        fs::write(repo.join(".gitattributes"), "changed text eol=crlf\n").unwrap();
        command(&repo, &["add", ".gitattributes"]);
        command(&repo, &["commit", "-qm", "attribute change"]);
        let attr_oid = command(&repo, &["rev-parse", "HEAD"]);
        let fourth = temp.path().join("attrs-tree");
        independent(&repo, &fourth);
        let attrs = materialize(&fourth, &state, &attr_oid, &[], &repo).unwrap();
        if initial["backend"] != "git" {
            assert_eq!(attrs["parent_reused_files"], 0);
        }
        assert_eq!(fs::read(fourth.join("changed")).unwrap(), b"new\r\n");
        assert_eq!(command(&fourth, &["status", "--porcelain"]), "");
    }
}
