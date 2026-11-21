//! Opt-in snapshots of closed, relocatable `node_modules` trees.
//!
//! The caller attests that installation output is relocatable and that its key
//! describes the exact toolchain, install flags and relevant environment. Only
//! clean, single-package committed source with one root lockfile is eligible.
//! Transfers require native CoW unless the caller explicitly requests copies.
//! Layers live until explicitly forgotten.
//! Windows copies only the unnamed data stream and read-only attribute; other
//! source metadata is not represented. Unverifiable Windows layers are retained.

use crate::registry::exists as entry_exists;
use crate::{git, registry};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path};
use uuid::Uuid;

mod registered;
pub(crate) use registered::{
    REGISTERED_INSTALL_DIR, attach_registered, registered_install_root, seed_registered,
    verify_registered_install_generation,
};
mod shared;
pub use shared::{check_attached_for_add, inherit_shared, seed_shared};

/// Root lockfiles recognised for Node package managers.
const LOCKS: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];

/// Git status arguments whose output is empty only for clean tracked source and index.
const TRACKED_STATUS: &[&str] = &[
    "--no-optional-locks",
    "-c",
    "core.fsmonitor=false",
    "status",
    "--porcelain=v1",
    "--untracked-files=no",
    "--ignore-submodules=none",
];

fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Fingerprint the permission bits a layer can represent on this platform.
fn permission_field(hash: &mut blake3::Hasher, metadata: &fs::Metadata) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        field(hash, &(metadata.permissions().mode() & 0o777).to_le_bytes());
    }
    #[cfg(windows)]
    field(hash, &[u8::from(metadata.permissions().readonly())]);
    #[cfg(not(any(unix, windows)))]
    let _ = (hash, metadata);
}

fn compatibility(checkout: &Path, key: &str) -> Result<String> {
    if key.trim().is_empty() {
        bail!("a nonempty toolchain and environment key is required");
    }
    let root = git::text(checkout, &["rev-parse", "--show-toplevel"])?;
    let root = dunce::canonicalize(root)?;
    if root != checkout {
        bail!("layer actions require the checkout root");
    }
    if !git::bytes(checkout, TRACKED_STATUS)?.is_empty() {
        bail!("tracked source and index must be clean");
    }
    // Git's status deliberately hides changes to assume-unchanged and sparse
    // entries, so those indexes cannot establish a clean source tree.
    let entries = git::bytes(checkout, &["ls-files", "-v", "-z"])?;
    for entry in entries
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let path = entry
            .strip_prefix(b"H ")
            .context("layer source cannot contain hidden or sparse index entries")?;
        if path
            .split(|byte| *byte == b'/')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case(b"node_modules"))
        {
            bail!("tracked node_modules is ineligible for layers");
        }
    }
    if entry_exists(&checkout.join("pnpm-workspace.yaml"))? {
        bail!("workspace projects are not eligible for layers");
    }
    let manifest_path = checkout.join("package.json");
    let manifest_metadata =
        fs::symlink_metadata(&manifest_path).context("root package.json is required")?;
    if !manifest_metadata.is_file() || registry::is_reparse_point(&manifest_metadata) {
        bail!("root package.json must be a regular file");
    }
    let manifest = fs::read(&manifest_path).context("root package.json is required")?;
    let parsed: Value = serde_json::from_slice(&manifest).context("parse package.json")?;
    if parsed.get("workspaces").is_some() {
        bail!("workspace projects are not eligible for layers");
    }
    for section in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        if let Some(deps) = parsed.get(section).and_then(Value::as_object) {
            for value in deps.values().filter_map(Value::as_str) {
                if ["file:", "link:", "workspace:"]
                    .iter()
                    .any(|prefix| value.starts_with(prefix))
                {
                    bail!("local and workspace dependencies are not eligible for layers");
                }
            }
        }
    }
    let mut locks = Vec::new();
    for &name in LOCKS {
        if entry_exists(&checkout.join(name))? {
            locks.push(name);
        }
    }
    if locks.len() != 1 {
        bail!("exactly one root lockfile is required");
    }
    let lock_metadata = fs::symlink_metadata(checkout.join(locks[0]))?;
    if !lock_metadata.is_file() || registry::is_reparse_point(&lock_metadata) {
        bail!("lockfile must be a regular file");
    }
    git::bytes(
        checkout,
        &[
            "ls-files",
            "--error-unmatch",
            "--",
            "package.json",
            locks[0],
        ],
    )
    .context("manifest and lockfile must be tracked")?;
    let lock = fs::read(checkout.join(locks[0]))?;
    if git::bytes(checkout, &["show", "HEAD:package.json"])? != manifest
        || git::bytes(checkout, &["show", &format!("HEAD:{}", locks[0])])? != lock
    {
        bail!("manifest and lockfile must match committed source");
    }
    Ok(exact_digest(
        key,
        &git::bytes(checkout, &["rev-parse", "HEAD^{tree}"])?,
        &manifest,
        locks[0],
        &lock,
    ))
}

fn exact_digest(key: &str, tree: &[u8], manifest: &[u8], lock_name: &str, lock: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [
        b"git-graft-node-layer-v1".as_slice(),
        std::env::consts::OS.as_bytes(),
        std::env::consts::ARCH.as_bytes(),
        key.as_bytes(),
        tree,
        b"package.json",
        manifest,
        lock_name.as_bytes(),
        lock,
    ] {
        field(&mut hasher, value);
    }
    hasher.finalize().to_hex().to_string()
}

fn safe_link(root: &Path, link: &Path) -> Result<std::path::PathBuf> {
    let target = fs::read_link(link)?;
    if target.is_absolute() {
        bail!("absolute symlink in node_modules: {}", link.display());
    }
    let parent = link
        .parent()
        .context("symlink parent")?
        .strip_prefix(root)?;
    let mut depth = parent.components().count();
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => (),
            Component::ParentDir if depth > 0 => depth -= 1,
            _ => bail!("escaping symlink in node_modules: {}", link.display()),
        }
    }
    // Compare like with like: `root` is also free of Windows' `\\?\` prefix.
    let resolved = dunce::canonicalize(link).context("broken or cyclic package symlink")?;
    if !resolved.starts_with(root) {
        bail!("symlink leaves node_modules: {}", link.display());
    }
    Ok(target)
}

fn check_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !metadata.file_type().is_symlink() && metadata.permissions().mode() & 0o7000 != 0 {
            bail!("special permission bits in layer: {}", path.display());
        }
        if !crate::recovery::no_extended_metadata(path) {
            bail!(
                "unrepresentable extended metadata in layer: {}",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (path, metadata);
    Ok(())
}

fn check_cache_metadata(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    check_metadata(path, metadata)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || (!metadata.is_dir() && metadata.nlink() != 1)
        {
            bail!("layer cache entry is not private: {}", path.display());
        }
    }
    Ok(())
}

type CheckMetadata = fn(&Path, &fs::Metadata) -> Result<()>;

// Inspect and fingerprint every entry before publication and again before use.
// No symlink is followed while walking or copying the installed tree.
fn inspect(root: &Path, forbidden: Option<&[u8]>, check: CheckMetadata) -> Result<String> {
    let root = dunce::canonicalize(root)?;
    check(&root, &fs::symlink_metadata(&root)?)?;
    let mut hash = blake3::Hasher::new();
    permission_field(&mut hash, &fs::metadata(&root)?);
    inspect_dir(&root, &root, forbidden, check, &mut hash)?;
    Ok(hash.finalize().to_hex().to_string())
}

fn inspect_dir(
    root: &Path,
    dir: &Path,
    forbidden: Option<&[u8]>,
    check: CheckMetadata,
    hash: &mut blake3::Hasher,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        check(&path, &meta)?;
        let relative = path.strip_prefix(root)?;
        field(
            hash,
            relative
                .to_str()
                .context("non-UTF8 package path is unsupported")?
                .as_bytes(),
        );
        if meta.is_dir() {
            field(hash, b"dir");
            permission_field(hash, &meta);
            inspect_dir(root, &path, forbidden, check, hash)?;
        } else if meta.file_type().is_symlink() {
            field(hash, b"symlink");
            let target = safe_link(root, &path)?;
            field(
                hash,
                target
                    .to_str()
                    .context("non-UTF8 symlink target")?
                    .as_bytes(),
            );
        } else if meta.is_file() {
            field(hash, b"file");
            permission_field(hash, &meta);
            field(hash, &meta.len().to_le_bytes());
            let mut file = fs::File::open(&path)?;
            let mut buffer = [0u8; 65536];
            let mut tail = Vec::new();
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
                if let Some(needle) = forbidden.filter(|needle| !needle.is_empty()) {
                    tail.extend_from_slice(&buffer[..n]);
                    if tail.windows(needle.len()).any(|window| window == needle) {
                        bail!(
                            "installed file contains an absolute checkout path: {}",
                            relative.display()
                        );
                    }
                    let keep = needle.len().saturating_sub(1).min(tail.len());
                    tail.drain(..tail.len() - keep);
                }
            }
        } else {
            bail!("special file in node_modules: {}", path.display());
        }
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Read a regular file of at most 1 KiB that passes `check`, without following links.
fn small_file(path: &Path, invalid: &'static str, check: CheckMetadata) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || registry::is_reparse_point(&metadata) || metadata.len() > 1024 {
        bail!(invalid);
    }
    check(path, &metadata)?;
    let mut bytes = Vec::new();
    File::open(path)?.take(1025).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 {
        bail!(invalid);
    }
    Ok(bytes)
}

// A layer is deletable only when its complete visible inventory and bytes match
// our narrowly defined provenance record. Unknown files must be retained.
fn layer_provenance(layer: &Path, digest: &str) -> Result<Value> {
    layer_provenance_for(layer, digest, "node_modules")
}

fn layer_provenance_for(layer: &Path, digest: &str, tree_name: &str) -> Result<Value> {
    let metadata = registry::owned_directory(layer).context("invalid layer directory")?;
    check_cache_metadata(layer, &metadata)?;
    let mut names = fs::read_dir(layer)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    let mut expected = [
        std::ffi::OsString::from(tree_name),
        std::ffi::OsString::from("provenance.json"),
    ];
    expected.sort();
    if names != expected {
        bail!("unexpected files in layer directory");
    }
    let bytes = small_file(
        &layer.join("provenance.json"),
        "invalid layer provenance file",
        check_cache_metadata,
    )?;
    let value: Value = serde_json::from_slice(&bytes)?;
    let object = value.as_object().context("invalid layer provenance")?;
    let contents = value["contents"]
        .as_str()
        .context("invalid layer contents digest")?;
    let valid_version = match value["version"].as_u64() {
        Some(1) => object.len() == 4,
        Some(2) => {
            object.len() == 5
                && value["backend"] == "copy-on-write"
                && value["generation"]
                    .as_str()
                    .and_then(|id| Uuid::parse_str(id).ok().map(|uuid| uuid.to_string() == id))
                    == Some(true)
        }
        _ => false,
    };
    if !valid_version
        || value["compatibility"] != digest
        || !valid_digest(digest)
        || !valid_digest(contents)
        || !matches!(
            value["backend"].as_str(),
            Some("copy" | "copy-on-write" | "mixed")
        )
        || serde_json::to_vec(&value)? != bytes
    {
        bail!("invalid layer provenance");
    }
    Ok(value)
}

fn verify_layer_contents(layer: &Path, digest: &str) -> Result<Value> {
    registry::check_owned_tree(layer).context("unsafe layer directory")?;
    let value = layer_provenance(layer, digest)?;
    let contents = value["contents"]
        .as_str()
        .context("invalid layer contents digest")?;
    let tree = layer.join("node_modules");
    registry::owned_directory(&tree).context("invalid layer tree")?;
    if inspect(&tree, None, check_cache_metadata)? != contents {
        bail!("layer contents failed integrity verification");
    }
    Ok(value)
}

/// Name the reason a cache purge must keep this layer, if it must.
///
/// Layers saved with `deps layer save` or `deps share --snapshot` (provenance
/// version 1), and the generation that `deps share` currently registers for new
/// grafts, exist because the user asked for them. Only `deps layer remove`
/// retires them. A shared generation that a later `deps share` superseded is a
/// cache. An unreadable layer or registration is an error, so the purge keeps
/// the layer and reports it as blocked.
pub(crate) fn purge_keeps_layer(
    state: &Path,
    layer: &Path,
    digest: &str,
) -> Result<Option<&'static str>> {
    if layer_provenance(layer, digest)?["version"] != 2 {
        return Ok(Some("saved_layer"));
    }
    if shared::registered_digest(state)?.as_deref() == Some(digest) {
        return Ok(Some("registered_shared_layer"));
    }
    Ok(None)
}

/// Verify the generation against its original digest, including after quarantine.
pub(crate) fn verify_layer(layer: &Path, digest: &str) -> Result<Value> {
    if cfg!(windows) {
        bail!("Windows layer metadata cannot be verified for deletion");
    }
    verify_layer_contents(layer, digest)
}

#[derive(Default)]
struct Transfer {
    cloned_bytes: u64,
    copied_bytes: u64,
    cloned_files: u64,
}

impl Transfer {
    fn backend(force_copy: bool) -> &'static str {
        if force_copy { "copy" } else { "copy-on-write" }
    }

    fn receipt(&self, action: &str, digest: &str, backend: &str) -> Value {
        json!({"action":action,"digest":digest,"backend":backend,"cloned_bytes":self.cloned_bytes,
            "copied_bytes":self.copied_bytes,"logical_bytes":self.cloned_bytes + self.copied_bytes})
    }
}

// Distinguish unavailable cloning from permission, disk-full and other I/O failures.
fn clone_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        return [
            libc::EXDEV,
            libc::ENOTTY,
            libc::EOPNOTSUPP,
            libc::ENOTSUP,
            libc::ENOSYS,
            libc::EINVAL,
        ]
        .contains(&code);
    }
    #[cfg(windows)]
    if let Some(code) = error.raw_os_error() {
        return [1, 17, 50].contains(&code);
    }
    false
}

fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    io::copy(&mut input, &mut output)?;
    output.set_permissions(input.metadata()?.permissions())?;
    Ok(())
}

fn transfer_file(
    source: &Path,
    destination: &Path,
    size: u64,
    force_copy: bool,
    transfer: &mut Transfer,
    clone: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> Result<()> {
    if force_copy {
        copy_file(source, destination).with_context(|| format!("copy {}", source.display()))?;
        transfer.copied_bytes += size;
    } else {
        match clone(source, destination) {
            Ok(()) => {
                transfer.cloned_bytes += size;
                transfer.cloned_files += 1;
            }
            Err(error) if clone_unsupported(&error) => {
                return Err(crate::api::fault(
                    "cow_unavailable",
                    "native copy-on-write is unavailable between these locations",
                    Value::Null,
                ));
            }
            Err(error) => return Err(error).with_context(|| format!("clone {}", source.display())),
        }
    }
    Ok(())
}

fn clone_tree(
    source: &Path,
    destination: &Path,
    force_copy: bool,
    check: CheckMetadata,
    transfer: &mut Transfer,
) -> Result<()> {
    check(source, &fs::symlink_metadata(source)?)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let meta = fs::symlink_metadata(&source)?;
        check(&source, &meta)?;
        if meta.is_dir() {
            fs::create_dir(&destination)?;
            clone_tree(&source, &destination, force_copy, check, transfer)?;
        } else if meta.file_type().is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(&source)?, &destination)?;
            #[cfg(windows)]
            {
                let target = fs::read_link(&source)?;
                let resolved = fs::metadata(&source).context("resolve relative package symlink")?;
                if resolved.is_dir() {
                    std::os::windows::fs::symlink_dir(&target, &destination)
                } else if resolved.is_file() {
                    std::os::windows::fs::symlink_file(&target, &destination)
                } else { bail!("unrepresentable package symlink: {}", source.display()); }
                .context("cannot create Windows package symlink; enable Developer Mode or grant symlink privileges")?;
            }
            #[cfg(not(any(unix, windows)))]
            bail!("symlink layers are unsupported on this platform");
        } else if meta.is_file() {
            transfer_file(
                &source,
                &destination,
                meta.len(),
                force_copy,
                transfer,
                crate::storage::clone_file,
            )?;
        } else {
            bail!("special file in package layer");
        }
    }
    fs::set_permissions(destination, fs::metadata(source)?.permissions())?;
    Ok(())
}

// Publication must not replace an entry created after the collision check.
// `std::fs::rename` would replace an existing file on Windows.
fn publish(source: &Path, destination: &Path) -> Result<()> {
    crate::recovery::rename_exclusive(source, destination).context("publish without replacement")
}

fn source_contents(checkout: &Path) -> Result<String> {
    let modules = checkout.join("node_modules");
    registry::check_owned_tree(&modules).context("node_modules must be a real directory")?;
    inspect(
        &modules,
        Some(git::path_str(checkout)?.as_bytes()),
        check_metadata,
    )
}

fn stage_tree(
    source: &Path,
    staging: &Path,
    source_probe_dir: &Path,
    force_copy: bool,
    check: CheckMetadata,
) -> Result<Transfer> {
    let tree = staging.join("node_modules");
    fs::create_dir(&tree)?;
    let mut transfer = Transfer::default();
    clone_tree(source, &tree, force_copy, check, &mut transfer)?;
    if !force_copy {
        probe_if_empty(source_probe_dir, staging, &transfer)?;
    }
    Ok(transfer)
}

/// An empty install has no regular file to exercise the native backend, so
/// prove that a probe clones from `source_dir` into `stage_dir` instead.
fn probe_if_empty(source_dir: &Path, stage_dir: &Path, transfer: &Transfer) -> Result<()> {
    if transfer.cloned_files > 0 {
        return Ok(());
    }
    let mut source = tempfile::NamedTempFile::new_in(source_dir)?;
    source.write_all(b"git-graft layer probe")?;
    let probe = tempfile::tempdir_in(stage_dir)?;
    transfer_file(
        source.path(),
        &probe.path().join("clone"),
        0,
        false,
        &mut Transfer::default(),
        crate::storage::clone_file,
    )
}

fn save_layer(
    checkout: &Path,
    layer: &Path,
    digest: &str,
    generation: Option<Uuid>,
    force_copy: bool,
    check_source: impl FnOnce() -> Result<()>,
) -> Result<Value> {
    if entry_exists(layer)? {
        bail!("layer already exists; remove it explicitly before replacing it");
    }
    let source_digest = source_contents(checkout)?;
    let layers = layer.parent().context("missing layer directory")?;
    registry::dir(layers)?;
    let temp = tempfile::Builder::new()
        .prefix(".building-")
        .tempdir_in(layers)?;
    let transfer = stage_tree(
        &checkout.join("node_modules"),
        temp.path(),
        checkout,
        force_copy,
        check_metadata,
    )?;
    let backend = Transfer::backend(force_copy);
    let mut provenance = json!({"version":1,"compatibility":digest,"contents":source_digest,
        "backend":backend});
    if let Some(generation) = generation {
        provenance["version"] = json!(2);
        provenance["generation"] = json!(generation.to_string());
    }
    fs::write(
        temp.path().join("provenance.json"),
        serde_json::to_vec(&provenance)?,
    )?;
    verify_layer_contents(temp.path(), digest)
        .context("installed tree changed while capturing layer")?;
    check_source()?;
    publish(temp.path(), layer).context("publish layer")?;
    Ok(transfer.receipt("save", digest, backend))
}

fn restore_layer(
    checkout: &Path,
    layer: &Path,
    digest: &str,
    provenance: &Value,
    force_copy: bool,
    check_source: impl FnOnce() -> Result<()>,
) -> Result<Value> {
    let modules = checkout.join("node_modules");
    if entry_exists(&modules)? {
        bail!("node_modules already exists");
    }
    registry::check_owned_tree(layer).context("invalid layer tree")?;
    let tree = layer.join("node_modules");
    let expected = provenance["contents"]
        .as_str()
        .context("invalid layer contents digest")?;
    let temp = tempfile::Builder::new()
        .prefix(".graft-restore-")
        .tempdir_in(checkout)?;
    let probe_dir = layer.parent().context("missing layer directory")?;
    let transfer = stage_tree(
        &tree,
        temp.path(),
        probe_dir,
        force_copy,
        check_cache_metadata,
    )?;
    let copy = temp.path().join("node_modules");
    if inspect(&copy, None, check_cache_metadata)? != expected {
        bail!("layer contents failed integrity verification");
    }
    check_source()?;
    if layer_provenance(layer, digest)? != *provenance {
        bail!("layer provenance changed during restore");
    }
    publish(&copy, &modules)?;
    Ok(transfer.receipt("restore", digest, Transfer::backend(force_copy)))
}

/// Save, restore or forget a caller-attested layer under the project lock.
/// Managed checkouts also require their workspace lock.
pub fn run(
    checkout: &Path,
    state: &Path,
    action: &str,
    key: &str,
    force_copy: bool,
) -> Result<Value> {
    if !["save", "restore", "remove"].contains(&action) {
        bail!("unknown layer action");
    }
    let checkout = dunce::canonicalize(checkout)?;
    let digest = compatibility(&checkout, key)?;
    let layers = state.join("layers");
    if entry_exists(&layers)? {
        registry::owned_directory(&layers).context("layers must be a real directory")?;
    }
    let layer = layers.join(&digest);
    let check_source = || {
        if compatibility(&checkout, key)? != digest {
            bail!("source changed during layer transfer");
        }
        Ok(())
    };
    match action {
        "save" => save_layer(&checkout, &layer, &digest, None, force_copy, check_source),
        "restore" => restore_layer(
            &checkout,
            &layer,
            &digest,
            &layer_provenance(&layer, &digest)?,
            force_copy,
            check_source,
        ),
        "remove" => {
            let provenance = verify_layer_contents(&layer, &digest)?;
            let backend = provenance["backend"]
                .as_str()
                .context("invalid layer backend")?;
            let result = Transfer::default().receipt(action, &digest, backend);
            #[cfg(not(windows))]
            {
                fs::remove_dir_all(&layer)?;
                Ok(result)
            }
            #[cfg(windows)]
            {
                let retained = state.join("retained-layers");
                registry::dir(&retained)?;
                let destination = retained.join(format!("{}-{}", digest, Uuid::new_v4()));
                publish(&layer, &destination)?;
                let mut result = result;
                result["retained"] = json!(true);
                Ok(result)
            }
        }
        _ => unreachable!(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use tempfile::tempdir;

    fn command(dir: &Path, program: &str, args: &[&str]) -> Result<std::process::Output> {
        let out = Command::new(program).current_dir(dir).args(args).output()?;
        if !out.status.success() {
            bail!("{}: {}", program, String::from_utf8_lossy(&out.stderr));
        }
        Ok(out)
    }

    fn mkfifo(path: &Path) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn set_xattr(path: &std::ffi::CStr, name: &std::ffi::CStr, value: &[u8]) {
        let result = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert_eq!(result, 0);
    }

    #[cfg(target_os = "macos")]
    fn xattr_len(path: &std::ffi::CStr, name: &std::ffi::CStr) -> isize {
        unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0, 0, 0) }
    }

    #[cfg(target_os = "macos")]
    fn remove_xattr(path: &std::ffi::CStr, name: &std::ffi::CStr) {
        assert_eq!(
            unsafe { libc::removexattr(path.as_ptr(), name.as_ptr(), 0) },
            0
        );
    }

    #[test]
    fn installed_layer_reuses_cow_and_rejects_unsafe_inputs() -> Result<()> {
        if Command::new("npm").arg("--version").output().is_err() {
            eprintln!("npm unavailable; skipping");
            return Ok(());
        }
        let root = tempdir()?;
        let a = root.path().join("repo ");
        let b = root.path().join("second");
        let pkg = root.path().join("package");
        let state = root.path().join("state");
        fs::create_dir_all(&a)?;
        fs::create_dir_all(&pkg)?;
        fs::write(
            pkg.join("package.json"),
            r#"{"name":"graft-layer-fixture","version":"1.0.0"}"#,
        )?;
        fs::write(pkg.join("index.js"), "module.exports = 42;\n")?;
        let packed = Command::new("npm")
            .args(["pack", "--json", "--ignore-scripts"])
            .env("npm_config_cache", root.path().join("pack-cache"))
            .current_dir(&pkg)
            .output()?;
        assert!(
            packed.status.success(),
            "{}",
            String::from_utf8_lossy(&packed.stderr)
        );
        let meta: Value = serde_json::from_slice(&packed.stdout)?;
        let tarball = pkg.join(meta[0]["filename"].as_str().unwrap());
        let npm_cache = state.join("cache/npm");
        let cache_add = Command::new("npm")
            .args(["cache", "add"])
            .arg(tarball)
            .env("npm_config_cache", &npm_cache)
            .current_dir(&a)
            .output()?;
        assert!(
            cache_add.status.success(),
            "{}",
            String::from_utf8_lossy(&cache_add.stderr)
        );
        let manifest = json!({"name":"consumer","version":"1.0.0","dependencies":{"graft-layer-fixture":"1.0.0"}});
        let lock = json!({"name":"consumer","version":"1.0.0","lockfileVersion":3,"packages":{
            "":{"name":"consumer","version":"1.0.0","dependencies":{"graft-layer-fixture":"1.0.0"}},
            "node_modules/graft-layer-fixture":{"version":"1.0.0","resolved":"https://127.0.0.1:1/fixture.tgz","integrity":meta[0]["integrity"]}}});
        fs::write(a.join("package.json"), manifest.to_string())?;
        fs::write(a.join("package-lock.json"), lock.to_string())?;
        command(&a, "git", &["init", "-q"])?;
        command(&a, "git", &["config", "user.email", "fixture@example.test"])?;
        command(&a, "git", &["config", "user.name", "Fixture"])?;
        command(&a, "git", &["add", "package.json", "package-lock.json"])?;
        command(&a, "git", &["commit", "-qm", "fixture"])?;
        command(
            &a,
            "git",
            &["update-index", "--assume-unchanged", "package.json"],
        )?;
        assert!(compatibility(&a, "node-toolchain").is_err());
        command(
            &a,
            "git",
            &["update-index", "--no-assume-unchanged", "package.json"],
        )?;
        command(
            &a,
            "git",
            &["clone", "-q", "--no-local", ".", b.to_str().unwrap()],
        )?;
        let install = Command::new("npm")
            .args([
                "ci",
                "--offline",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
            ])
            .env("npm_config_cache", &npm_cache)
            .current_dir(&a)
            .output()?;
        assert!(
            install.status.success(),
            "{}",
            String::from_utf8_lossy(&install.stderr)
        );
        let source = a.join("node_modules/graft-layer-fixture/index.js");
        let unsafe_link = a.join("node_modules/external");
        symlink("../../package", &unsafe_link)?;
        assert!(run(&a, &state, "save", "node-toolchain", true).is_err());
        fs::remove_file(unsafe_link)?;
        let embedded = a.join("node_modules/absolute-path");
        fs::write(&embedded, a.canonicalize()?.to_string_lossy().as_bytes())?;
        assert!(run(&a, &state, "save", "node-toolchain", true).is_err());
        fs::remove_file(embedded)?;
        let fifo = a.join("node_modules/special");
        mkfifo(&fifo)?;
        assert!(run(&a, &state, "save", "node-toolchain", true).is_err());
        assert!(fifo.exists());
        fs::remove_file(fifo)?;
        symlink(
            "graft-layer-fixture/index.js",
            a.join("node_modules/relative-link"),
        )?;
        assert!(run(&a, &state, "save", "", true).is_err());
        let saved = run(&a, &state, "save", "node-toolchain", true)?;
        assert_eq!(saved["backend"], "copy");
        assert!(saved["copied_bytes"].as_u64().unwrap() > 0);
        assert_eq!(saved["cloned_bytes"], 0);
        let cached = state
            .join("layers")
            .join(saved["digest"].as_str().unwrap())
            .join("node_modules/graft-layer-fixture/index.js");
        let generation = cached.parent().unwrap().parent().unwrap().parent().unwrap();
        let digest = saved["digest"].as_str().unwrap();
        verify_layer(generation, digest)?;
        let private = cached.parent().unwrap().join("private-user-file");
        fs::write(&private, "do not delete")?;
        assert!(verify_layer(generation, digest).is_err());
        assert!(run(&a, &state, "remove", "node-toolchain", false).is_err());
        assert_eq!(fs::read_to_string(&private)?, "do not delete");
        fs::remove_file(private)?;
        let extra = generation.join("private-top-level");
        fs::write(&extra, "do not delete")?;
        assert!(verify_layer(generation, digest).is_err());
        assert!(run(&a, &state, "remove", "node-toolchain", false).is_err());
        assert_eq!(fs::read_to_string(&extra)?, "do not delete");
        fs::remove_file(extra)?;
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::ffi::OsStrExt;
            let path = std::ffi::CString::new(cached.as_os_str().as_bytes())?;
            let name = c"com.git-graft.private";
            set_xattr(&path, name, b"secret");
            assert!(verify_layer(generation, digest).is_err());
            assert!(run(&a, &state, "remove", "node-toolchain", false).is_err());
            assert_eq!(xattr_len(&path, name), 6);
            remove_xattr(&path, name);
        }
        fs::write(&cached, "tampered")?;
        assert!(run(&b, &state, "restore", "node-toolchain", true).is_err());
        fs::write(&cached, "module.exports = 42;\n")?;
        assert!(run(&b, &state, "restore", "another-toolchain", true).is_err());
        let restored = run(&b, &state, "restore", "node-toolchain", true)?;
        assert_eq!(restored["backend"], "copy");
        assert_eq!(restored["copied_bytes"], saved["copied_bytes"]);
        assert_eq!(saved["digest"], restored["digest"]);
        assert_eq!(
            fs::read_link(b.join("node_modules/relative-link"))?,
            Path::new("graft-layer-fixture/index.js")
        );
        let copy = b.join("node_modules/graft-layer-fixture/index.js");
        assert_eq!(fs::read_to_string(&copy)?, "module.exports = 42;\n");
        fs::write(&copy, "changed")?;
        assert_eq!(fs::read_to_string(&source)?, "module.exports = 42;\n");
        assert!(run(&b, &state, "restore", "node-toolchain", true).is_err());
        fs::write(b.join("package.json"), "{}")?;
        assert!(run(&b, &state, "remove", "node-toolchain", false).is_err());
        let cached_special = cached.parent().unwrap().join("special");
        mkfifo(&cached_special)?;
        assert!(run(&a, &state, "remove", "node-toolchain", false).is_err());
        assert!(cached_special.exists());
        assert!(cached.exists());
        fs::remove_file(cached_special)?;
        let forgotten = run(&a, &state, "remove", "node-toolchain", false)?;
        assert_eq!(forgotten["backend"], "copy");
        assert!(run(&a, &state, "remove", "node-toolchain", false).is_err());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_layer_rejects_metadata_on_source_and_clone() -> Result<()> {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source)?;
        fs::create_dir(&destination)?;
        let file = source.join("file");
        fs::write(&file, "data")?;
        let path = CString::new(file.as_os_str().as_bytes())?;
        let clone = || {
            clone_tree(
                &source,
                &destination,
                false,
                check_metadata,
                &mut Transfer::default(),
            )
        };
        let assert_rejected = || {
            assert!(inspect(&source, None, check_metadata).is_err());
            assert!(clone().is_err());
            assert!(!destination.join("file").exists());
        };
        let name = c"com.git-graft.test";
        set_xattr(&path, name, b"private");
        assert_rejected();
        remove_xattr(&path, name);
        let fork = c"com.apple.ResourceFork";
        set_xattr(&path, fork, b"private fork");
        assert_rejected();
        remove_xattr(&path, fork);
        let user = command(root.path(), "id", &["-un"])?;
        let user = std::str::from_utf8(&user.stdout)?.trim();
        let acl = format!("{user} allow read");
        command(root.path(), "chmod", &["+a", &acl, file.to_str().unwrap()])?;
        assert_rejected();
        command(root.path(), "chmod", &["-N", file.to_str().unwrap()])?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o1755))?;
        if fs::metadata(&file)?.permissions().mode() & 0o7000 != 0 {
            assert!(inspect(&source, None, check_metadata).is_err());
        }
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755))?;
        clone()?;
        assert_eq!(
            inspect(&source, None, check_metadata)?,
            inspect(&destination, None, check_metadata)?
        );
        let cloned = CString::new(destination.join("file").as_os_str().as_bytes())?;
        set_xattr(&cloned, name, b"private");
        assert!(inspect(&destination, None, check_metadata).is_err());
        assert_eq!(xattr_len(&path, name), -1);
        Ok(())
    }

    #[test]
    fn publication_never_replaces_existing_directory_or_symlink() -> Result<()> {
        let root = tempdir()?;
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        fs::create_dir(&source)?;
        fs::write(source.join("source"), "source")?;
        fs::create_dir(&destination)?;
        assert!(publish(&source, &destination).is_err());
        assert!(source.join("source").exists());
        fs::remove_dir(&destination)?;
        symlink(&source, &destination)?;
        assert!(publish(&source, &destination).is_err());
        assert!(source.join("source").exists());
        assert!(fs::symlink_metadata(destination)?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    fn identifies_unsupported_clone_errors() {
        assert!(clone_unsupported(&io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported"
        )));
        for code in [libc::EXDEV, libc::EOPNOTSUPP, libc::ENOTTY] {
            assert!(clone_unsupported(&io::Error::from_raw_os_error(code)));
        }
        for code in [libc::EACCES, libc::ENOSPC] {
            assert!(!clone_unsupported(&io::Error::from_raw_os_error(code)));
        }
    }

    #[test]
    fn only_explicit_copy_copies_without_native_clone() -> Result<()> {
        let dir = tempdir()?;
        let source = dir.path().join("source");
        let copied = dir.path().join("copied");
        let denied = dir.path().join("denied");
        fs::write(&source, "original")?;
        let mut transfer = Transfer::default();
        let unsupported = transfer_file(&source, &copied, 8, false, &mut transfer, |_, _| {
            Err(io::Error::from_raw_os_error(libc::EXDEV))
        })
        .unwrap_err();
        assert_eq!(
            unsupported
                .downcast_ref::<crate::api::Fault>()
                .unwrap()
                .code,
            "cow_unavailable"
        );
        assert!(!copied.exists());
        assert_eq!(transfer.copied_bytes, 0);
        transfer_file(&source, &copied, 8, true, &mut transfer, |_, _| {
            panic!("explicit copy must not attempt native cloning");
        })?;
        assert_eq!(Transfer::backend(true), "copy");
        assert_eq!(transfer.copied_bytes, 8);
        fs::write(&copied, "modified")?;
        assert_eq!(fs::read_to_string(&source)?, "original");
        assert!(
            transfer_file(&source, &denied, 8, false, &mut transfer, |_, _| Err(
                io::Error::from_raw_os_error(libc::EACCES)
            ))
            .is_err()
        );
        assert!(!denied.exists());
        Ok(())
    }
}
