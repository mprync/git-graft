//! Git-authoritative source materialization with optional native CoW reuse.
//! The caller holds the project lock shared by snapshot publication and GC.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(test)]
use std::path::PathBuf;
use std::{
    ffi::OsStr,
    fs,
    io::{self, Seek, Write},
    path::{Component, Path},
    process::{Command, Output, Stdio},
};

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod cache;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod verify;

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

fn checked_oid(worktree: &Path, oid: &str) -> Result<String> {
    crate::git::checked_oid(oid)?;
    let head = git(worktree, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let head = String::from_utf8(head.stdout)?.trim().to_owned();
    ensure!(
        head.eq_ignore_ascii_case(oid),
        "worktree HEAD does not match requested commit"
    );
    Ok(head)
}

fn ensure_empty(worktree: &Path) -> Result<()> {
    for entry in fs::read_dir(worktree)? {
        let entry = entry?;
        ensure!(
            entry.file_name() == ".git",
            "worktree is not empty: {}",
            entry.path().display()
        );
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_file() && !crate::registry::is_reparse_point(&metadata),
            "expected a registered worktree .git file"
        );
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
    let tree = git(worktree, &["ls-tree", "-r", "-z", oid])?;
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
    git(worktree, &["read-tree", "-m", "-u", oid])?;
    let mut result = Receipt::default();
    for entry in entries {
        count_private(worktree, entry, &mut result.private);
    }
    Ok(result.json("git", Some(reason)))
}

struct Checkout<'a> {
    worktree: &'a Path,
    oid: &'a str,
    entries: &'a [Entry],
    settings: String,
    symlinks: bool,
    may_convert_eol: bool,
}

impl<'a> Checkout<'a> {
    fn read(worktree: &'a Path, oid: &'a str, entries: &'a [Entry]) -> Result<Self> {
        let out = git_command(worktree).args([
            "config", "--null", "--get-regexp",
            r"^core\.(autocrlf|eol|safecrlf|symlinks|ignorecase|precomposeunicode|protecthfs|protectntfs|bigfilethreshold)$",
        ]).output()?;
        ensure!(
            matches!(out.status.code(), Some(0 | 1)),
            "cannot read Git checkout settings"
        );
        let git_version = git(worktree, &["version"])?;
        let mut identity = blake3::Hasher::new();
        identity.update(b"git-graft-checkout-v3\0");
        identity.update(std::env::consts::OS.as_bytes());
        identity.update(&git_version.stdout);
        identity.update(&out.stdout);
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
        let may_convert_eol = value(b"core.autocrlf").is_some_and(|value| {
            ![b"false".as_slice(), b"no", b"off", b"0", b"", b"input"]
                .iter()
                .any(|raw| value.eq_ignore_ascii_case(raw))
        });
        Ok(Self {
            worktree,
            oid,
            entries,
            settings: identity.finalize().to_hex().to_string(),
            symlinks,
            may_convert_eol,
        })
    }
}

// External attributes, replacement blobs and arbitrary filters can change
// bytes without changing commit or blob IDs. They always use Git.
fn conversion_reason(
    checkout: &Checkout<'_>,
    inspect_attributes: bool,
) -> Result<Option<&'static str>> {
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
    let Ok(attr) = git(
        worktree,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/attributes",
        ],
    ) else {
        return Ok(Some("cannot determine external attributes"));
    };
    if let Some(reason) = external_attributes(&attr.stdout) {
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
    if !inspect_attributes {
        return Ok(None);
    }
    let mut input = tempfile::tempfile()?;
    for entry in entries {
        input.write_all(&entry.name)?;
        input.write_all(b"\0")?;
    }
    input.rewind()?;
    let source = format!("--source={oid}");
    let attrs = ["filter", "ident", "working-tree-encoding"];
    let output = git_command(worktree)
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
            if !matches!(fields.next(), Some(b"unspecified" | b"unset")) {
                return Ok(Some("path-dependent checkout conversion"));
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
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        _ => Some("external checkout attributes"),
    }
}

/// Populate a registered worktree, using the existing primary checkout as a
/// donor when a private CoW copy can be verified against committed Git bytes.
pub fn materialize_from(
    worktree: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: &Path,
) -> Result<Value> {
    let worktree = dunce::canonicalize(worktree)?;
    ensure_empty(&worktree)?;
    let oid = checked_oid(&worktree, oid)?;
    validate_sparse(&worktree, &oid, sparse)?;
    let entries = entries(&worktree, &oid)?;
    if !sparse.is_empty() {
        return native_checkout(&worktree, &oid, sparse, &entries, "sparse checkout");
    }
    let mut checkout = Checkout::read(&worktree, &oid, &entries)?;
    // Non-ASCII names can have filesystem-specific aliases on checkout.
    let inspect_attributes = entries
        .iter()
        .any(|entry| entry.attributes() || !entry.name.is_ascii());
    if let Some(reason) = conversion_reason(&checkout, inspect_attributes)? {
        return native_checkout(&worktree, &oid, sparse, &entries, reason);
    }
    checkout.may_convert_eol |= inspect_attributes;
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        fs::create_dir_all(state)?;
        let state = dunce::canonicalize(state)?;
        ensure!(
            !state.starts_with(&worktree),
            "storage state must be outside the worktree"
        );
        if probe(&state, &worktree)? {
            if let Ok(prepared) = prepare(&checkout, &state, primary) {
                return prepared.install(&worktree, &oid);
            }
            return native_checkout(&worktree, &oid, sparse, &entries, "snapshot unavailable");
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let _ = (state, primary, checkout);
    native_checkout(
        &worktree,
        &oid,
        sparse,
        &entries,
        "native clone unavailable",
    )
}

/// Report ownership only for a complete, authenticated commit snapshot directory.
pub fn owned_base(path: &Path) -> Result<bool> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        Ok(cache::owned_base(path).is_ok())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(test)]
fn materialize(worktree: &Path, state: &Path, oid: &str, sparse: &[String]) -> Result<Value> {
    let primary = crate::git::worktrees(worktree)?.remove(0).path;
    materialize_from(worktree, state, oid, sparse, &primary)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
struct Prepared {
    staging: tempfile::TempDir,
    receipt: Receipt,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl Prepared {
    fn install(self, worktree: &Path, oid: &str) -> Result<Value> {
        git(
            worktree,
            &[
                "-c",
                "core.protectHFS=true",
                "-c",
                "core.protectNTFS=true",
                "read-tree",
                oid,
            ],
        )?;
        for entry in fs::read_dir(self.staging.path())? {
            let entry = entry?;
            native::rename_new(&entry.path(), &worktree.join(entry.file_name()))?;
        }
        git(worktree, &["update-index", "--refresh"])?;
        let backend = if cfg!(target_os = "macos") {
            "clonefile"
        } else {
            "reflink"
        };
        Ok(self.receipt.json(backend, None))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn prepare(checkout: &Checkout<'_>, state: &Path, primary: &Path) -> Result<Prepared> {
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
    let snapshot = match fs::symlink_metadata(&base) {
        Ok(_) => {
            receipt.warm = true;
            cache::Snapshot::load(&base, &secret, oid, settings)?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = parent_candidate(worktree, &bases, oid, entries, settings, &secret);
            build_snapshot(
                checkout,
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
            .eq(entries),
        "source snapshot differs from the Git tree"
    );
    let staging = tempfile::Builder::new()
        .prefix(".graft-source-")
        .tempdir_in(worktree)?;
    let destination = native::Root::open(staging.path())?;
    let mut directory = std::path::PathBuf::new();
    for record in snapshot.records() {
        let target = staging.path().join(record.entry.path()?);
        stage_parent(&target, &mut directory)?;
        if let Some(size) = snapshot.clone_record(record, &destination, &target, mode)? {
            receipt.cloned.add(size);
        }
    }
    Ok(Prepared { staging, receipt })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stage_parent(target: &Path, previous: &mut std::path::PathBuf) -> Result<()> {
    let parent = target.parent().context("missing source directory")?;
    if *previous != parent {
        fs::create_dir_all(parent)?;
        parent.clone_into(previous);
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn parent_candidate(
    worktree: &Path,
    bases: &Path,
    oid: &str,
    entries: &[Entry],
    settings: &str,
    secret: &cache::Key,
) -> Option<cache::Snapshot> {
    // An empty cache is the common first spawn and has no parent to discover.
    if fs::read_dir(bases).ok()?.filter_map(Result::ok).count() <= 1 {
        return None;
    }
    let parents = git(worktree, &["show", "-s", "--format=%P", oid]).ok()?;
    let parent = std::str::from_utf8(&parents.stdout)
        .ok()?
        .split_whitespace()
        .next()?;
    crate::git::checked_oid(parent).ok()?;
    let snapshot =
        cache::Snapshot::load(&bases.join(parent).join(settings), secret, parent, settings).ok()?;
    let old_attrs = snapshot
        .records()
        .iter()
        .map(|record| &record.entry)
        .filter(|entry| entry.attributes());
    if !old_attrs.eq(entries.iter().filter(|entry| entry.attributes())) {
        return None;
    }
    snapshot.verify().ok()?;
    Some(snapshot)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn build_snapshot(
    checkout_settings: &Checkout<'_>,
    secret: &cache::Key,
    base: &Path,
    parent: Option<&cache::Snapshot>,
    primary: &Path,
    mode: u32,
    receipt: &mut Receipt,
) -> Result<cache::Snapshot> {
    use std::collections::{BTreeMap, BTreeSet};
    let (worktree, oid, entries) = (
        checkout_settings.worktree,
        checkout_settings.oid,
        checkout_settings.entries,
    );
    let base_parent = base.parent().context("missing snapshot directory")?;
    let staging = tempfile::Builder::new()
        .prefix(".building-")
        .tempdir_in(base_parent)?;
    let generation = staging.path().join("generation");
    let tree = cache::tree(&generation);
    fs::create_dir(&generation)?;
    fs::create_dir(&tree)?;
    let index = staging.path().join("index");
    let checkout = |args: &[&str], input: Option<fs::File>| -> Result<()> {
        let mut command = git_command(worktree);
        command
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_WORK_TREE", &tree)
            .env("GIT_ATTR_SOURCE", oid)
            .args([
                "-c",
                "core.sparseCheckout=false",
                "-c",
                "core.protectHFS=true",
                "-c",
                "core.protectNTFS=true",
            ])
            .args(args);
        if let Some(input) = input {
            command.stdin(Stdio::from(input));
        }
        let output = command.output()?;
        ensure!(
            output.status.success(),
            "Git snapshot checkout: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    checkout(&["read-tree", oid], None)?;

    let previous: BTreeMap<_, _> = parent
        .into_iter()
        .flat_map(|p| p.records())
        .map(|record| (record.entry.name.as_slice(), record))
        .collect();
    let donor = native::Root::open(primary).ok();
    let private = native::Root::open(&tree)?;
    let mut reused = BTreeSet::new();
    let mut primary_clones = Vec::new();
    let mut directory = std::path::PathBuf::new();
    for entry in entries.iter().filter(|entry| entry.regular()) {
        let path = entry.path()?;
        let target = tree.join(path);
        if let Some(snapshot) = parent
            && let Some(record) = previous.get(entry.name.as_slice())
            && record.entry == *entry
        {
            stage_parent(&target, &mut directory)?;
            if let Ok(Some(size)) = snapshot.clone_record(record, &private, &target, mode) {
                receipt.parent.add(size);
                reused.insert(entry.name.as_slice());
                continue;
            }
        }
        // Git's filtered batch format separates the object and path by
        // whitespace. Let checkout-index handle names it cannot encode.
        if checkout_settings.may_convert_eol && entry.name[0].is_ascii_whitespace() {
            continue;
        }
        if let Some(donor) = &donor
            && let Ok((source, metadata)) = donor.regular_metadata(path)
        {
            stage_parent(&target, &mut directory)?;
            if private
                .clone_regular(path, &source, &metadata, entry.creation_mode(mode), false)
                .is_ok()
            {
                primary_clones.push(entry);
            }
        }
    }
    let digests = verify::checkout_digests(
        worktree,
        oid,
        &primary_clones,
        checkout_settings.may_convert_eol,
    )?;
    for (entry, expected) in primary_clones.into_iter().zip(digests) {
        let path = entry.path()?;
        let (file, metadata) = private.regular_metadata(path)?;
        if verify::digest(file)? == expected {
            receipt.primary.add(metadata.len());
            reused.insert(entry.name.as_slice());
        } else {
            private.discard(path)?;
        }
    }

    if reused.len() < entries.len() {
        let mut missing = tempfile::tempfile()?;
        for entry in entries
            .iter()
            .filter(|entry| !reused.contains(entry.name.as_slice()))
        {
            missing.write_all(&entry.name)?;
            missing.write_all(b"\0")?;
        }
        missing.rewind()?;
        checkout(&["checkout-index", "--stdin", "-z"], Some(missing))?;
        for entry in entries
            .iter()
            .filter(|entry| entry.regular() && !reused.contains(entry.name.as_slice()))
        {
            receipt
                .private
                .add(private.regular_metadata(entry.path()?)?.1.len());
        }
    }
    cache::Snapshot::publish(
        &generation,
        base,
        entries,
        secret,
        oid,
        &checkout_settings.settings,
    )
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
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        command(&repo, &["init", "-q"]);
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
