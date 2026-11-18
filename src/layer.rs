//! Opt-in snapshots of closed, relocatable `node_modules` trees.
//!
//! The caller attests that installation output is relocatable and that its key
//! describes the exact toolchain, install flags and relevant environment. Only
//! clean, single-package committed source with one root lockfile is eligible.
//! Native CoW is preferred; unsupported filesystems use independent copies.
//! Layers live until explicitly forgotten.

use crate::git;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path};

fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn compatibility(worktree: &Path, key: &str) -> Result<String> {
    if key.trim().is_empty() {
        bail!("a nonempty toolchain and environment key is required");
    }
    let root = git::text(worktree, &["rev-parse", "--show-toplevel"])?;
    let root = Path::new(&root).canonicalize()?;
    if root != worktree {
        bail!("layer actions require the worktree root");
    }
    if !git::bytes(
        worktree,
        &["status", "--porcelain=v1", "--untracked-files=no"],
    )?
    .is_empty()
    {
        bail!("tracked source and index must be clean");
    }
    if worktree.join("pnpm-workspace.yaml").exists() {
        bail!("workspace projects are not eligible for layers");
    }
    let manifest_path = worktree.join("package.json");
    if !fs::symlink_metadata(&manifest_path)
        .context("root package.json is required")?
        .file_type()
        .is_file()
    {
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
    let locks: Vec<_> = [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lock",
        "bun.lockb",
    ]
    .into_iter()
    .filter(|name| worktree.join(name).exists())
    .collect();
    if locks.len() != 1 {
        bail!("exactly one root lockfile is required");
    }
    if !fs::symlink_metadata(worktree.join(locks[0]))?
        .file_type()
        .is_file()
    {
        bail!("lockfile must be a regular file");
    }
    git::bytes(
        worktree,
        &[
            "ls-files",
            "--error-unmatch",
            "--",
            "package.json",
            locks[0],
        ],
    )
    .context("manifest and lockfile must be tracked")?;
    let mut hasher = blake3::Hasher::new();
    for value in [
        b"git-graft-node-layer-v1".as_slice(),
        std::env::consts::OS.as_bytes(),
        std::env::consts::ARCH.as_bytes(),
        key.as_bytes(),
        git::bytes(worktree, &["rev-parse", "HEAD^{tree}"])?.as_slice(),
        b"package.json",
        manifest.as_slice(),
        locks[0].as_bytes(),
        fs::read(worktree.join(locks[0]))?.as_slice(),
    ] {
        field(&mut hasher, value);
    }
    Ok(hasher.finalize().to_hex().to_string())
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
    let resolved = link
        .canonicalize()
        .context("broken or cyclic package symlink")?;
    if !resolved.starts_with(root) {
        bail!("symlink leaves node_modules: {}", link.display());
    }
    Ok(target)
}

// Inspect and fingerprint every entry before publication and again before use.
// No symlink is followed while walking or copying the installed tree.
fn inspect(root: &Path, forbidden: Option<&[u8]>) -> Result<String> {
    let root = root.canonicalize()?;
    let mut hash = blake3::Hasher::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        field(
            &mut hash,
            &(fs::metadata(&root)?.permissions().mode() & 0o777).to_le_bytes(),
        );
    }
    #[cfg(windows)]
    field(
        &mut hash,
        &[u8::from(fs::metadata(&root)?.permissions().readonly())],
    );
    inspect_dir(&root, &root, forbidden, &mut hash)?;
    Ok(hash.finalize().to_hex().to_string())
}

fn inspect_dir(
    root: &Path,
    dir: &Path,
    forbidden: Option<&[u8]>,
    hash: &mut blake3::Hasher,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
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
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                field(hash, &(meta.permissions().mode() & 0o777).to_le_bytes());
            }
            #[cfg(windows)]
            field(hash, &[u8::from(meta.permissions().readonly())]);
            inspect_dir(root, &path, forbidden, hash)?;
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
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                field(hash, &(meta.permissions().mode() & 0o777).to_le_bytes());
            }
            #[cfg(windows)]
            field(hash, &[u8::from(meta.permissions().readonly())]);
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
                            "installed file contains an absolute worktree path: {}",
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

#[derive(Default)]
struct Transfer {
    cloned_bytes: u64,
    copied_bytes: u64,
    cloned_files: u64,
    copied_files: u64,
}

impl Transfer {
    fn backend(&self, force_copy: bool) -> &'static str {
        match (self.cloned_files > 0, self.copied_files > 0, force_copy) {
            (true, true, _) => "mixed",
            (true, false, _) => "copy-on-write",
            (false, true, _) | (false, false, true) => "copy",
            _ => "copy-on-write",
        }
    }
}

// Only errors meaning an unsupported clone operation permit ordinary copying.
// Permission, disk-full and other I/O failures must remain visible.
fn clone_unsupported(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        return code == libc::EXDEV
            || code == libc::ENOTTY
            || code == libc::EOPNOTSUPP
            || code == libc::ENOTSUP
            || code == libc::ENOSYS
            || code == libc::EINVAL;
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
    fs::set_permissions(destination, input.metadata()?.permissions())?;
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
    let cloned = if force_copy {
        false
    } else {
        match clone(source, destination) {
            Ok(()) => true,
            Err(error) if clone_unsupported(&error) => false,
            Err(error) => return Err(error).with_context(|| format!("clone {}", source.display())),
        }
    };
    if cloned {
        transfer.cloned_bytes += size;
        transfer.cloned_files += 1;
    } else {
        copy_file(source, destination).with_context(|| format!("copy {}", source.display()))?;
        transfer.copied_bytes += size;
        transfer.copied_files += 1;
    }
    Ok(())
}

fn clone_tree(
    source: &Path,
    destination: &Path,
    force_copy: bool,
    transfer: &mut Transfer,
) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        let meta = fs::symlink_metadata(&source)?;
        if meta.is_dir() {
            fs::create_dir(&destination)?;
            clone_tree(&source, &destination, force_copy, transfer)?;
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

/// Save, restore or forget a caller-attested layer. Caller holds both locks.
pub fn run(
    worktree: &Path,
    state: &Path,
    action: &str,
    key: &str,
    force_copy: bool,
) -> Result<Value> {
    if !["save", "restore", "forget"].contains(&action) {
        bail!("unknown layer action");
    }
    let worktree = worktree.canonicalize()?;
    let digest = compatibility(&worktree, key)?;
    let layers = state.join("layers");
    let layer = layers.join(&digest);
    let modules = worktree.join("node_modules");
    let mut transfer = Transfer::default();
    let mut saved_backend = None;
    match action {
        "save" => {
            if fs::symlink_metadata(&layer).is_ok() {
                bail!("layer already exists; forget it explicitly before replacing it");
            }
            let metadata = fs::symlink_metadata(&modules).context("node_modules is required")?;
            if !metadata.file_type().is_dir() {
                bail!("node_modules must be a real directory");
            }
            let source_digest = inspect(
                &modules,
                Some(
                    worktree
                        .to_str()
                        .context("non-UTF8 worktree path")?
                        .as_bytes(),
                ),
            )?;
            fs::create_dir_all(&layers)?;
            let temp = tempfile::Builder::new()
                .prefix(".building-")
                .tempdir_in(&layers)?;
            let tree = temp.path().join("node_modules");
            fs::create_dir(&tree)?;
            clone_tree(&modules, &tree, force_copy, &mut transfer)?;
            if inspect(&tree, None)? != source_digest {
                bail!("installed tree changed while capturing layer");
            }
            fs::write(
                temp.path().join("provenance.json"),
                serde_json::to_vec(
                    &json!({"version":1,"compatibility":digest,"contents":source_digest,
                        "backend":transfer.backend(force_copy)}),
                )?,
            )?;
            fs::rename(temp.path(), &layer).context("publish layer")?;
        }
        "restore" => {
            if fs::symlink_metadata(&modules).is_ok() {
                bail!("node_modules already exists");
            }
            if !fs::symlink_metadata(&layer)
                .context("layer not found")?
                .file_type()
                .is_dir()
            {
                bail!("invalid layer directory");
            }
            let provenance_path = layer.join("provenance.json");
            if !fs::symlink_metadata(&provenance_path)?
                .file_type()
                .is_file()
            {
                bail!("invalid layer provenance file");
            }
            let provenance: Value = serde_json::from_slice(&fs::read(provenance_path)?)?;
            if provenance["version"] != 1 || provenance["compatibility"] != digest {
                bail!("invalid layer provenance");
            }
            let tree = layer.join("node_modules");
            if !fs::symlink_metadata(&tree)?.file_type().is_dir() {
                bail!("invalid layer tree");
            }
            let actual = inspect(&tree, None)?;
            if provenance["contents"].as_str() != Some(actual.as_str()) {
                bail!("layer contents failed integrity verification");
            }
            let temp = tempfile::Builder::new()
                .prefix(".graft-restore-")
                .tempdir_in(&worktree)?;
            let copy = temp.path().join("node_modules");
            fs::create_dir(&copy)?;
            clone_tree(&tree, &copy, force_copy, &mut transfer)?;
            if inspect(&copy, None)? != actual {
                bail!("layer changed during restore");
            }
            if fs::symlink_metadata(&modules).is_ok() {
                bail!("node_modules appeared during restore");
            }
            fs::rename(copy, modules)?;
        }
        "forget" => {
            let metadata = fs::symlink_metadata(&layer).context("layer not found")?;
            if !metadata.file_type().is_dir() {
                bail!("invalid layer directory");
            }
            let path = layer.join("provenance.json");
            if !fs::symlink_metadata(&path)?.file_type().is_file() {
                bail!("invalid layer provenance file");
            }
            let provenance: Value = serde_json::from_slice(&fs::read(path)?)?;
            if provenance["version"] != 1 || provenance["compatibility"] != digest {
                bail!("invalid layer provenance");
            }
            saved_backend = Some(
                provenance["backend"]
                    .as_str()
                    .unwrap_or("copy-on-write")
                    .to_owned(),
            );
            fs::remove_dir_all(&layer)?;
        }
        _ => unreachable!(),
    }
    Ok(
        json!({"action":action,"digest":digest,"backend":saved_backend.as_deref().unwrap_or_else(|| transfer.backend(force_copy)),
        "cloned_bytes":transfer.cloned_bytes,"copied_bytes":transfer.copied_bytes,
        "logical_bytes":transfer.cloned_bytes + transfer.copied_bytes}),
    )
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
            &["worktree", "add", "--detach", b.to_str().unwrap()],
        )?;
        let install = crate::runtime::dependency(&a, &state, 1234, "npm", &[], true)?;
        assert_eq!(install["success"], true, "{install}");
        let source = a.join("node_modules/graft-layer-fixture/index.js");
        let unsafe_link = a.join("node_modules/external");
        symlink("../../package", &unsafe_link)?;
        assert!(run(&a, &state, "save", "node-toolchain", true).is_err());
        fs::remove_file(unsafe_link)?;
        let embedded = a.join("node_modules/absolute-path");
        fs::write(&embedded, a.canonicalize()?.to_string_lossy().as_bytes())?;
        assert!(run(&a, &state, "save", "node-toolchain", true).is_err());
        fs::remove_file(embedded)?;
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
        assert!(run(&b, &state, "forget", "node-toolchain", false).is_err());
        let forgotten = run(&a, &state, "forget", "node-toolchain", false)?;
        assert_eq!(forgotten["backend"], "copy");
        assert!(run(&a, &state, "forget", "node-toolchain", false).is_err());
        Ok(())
    }

    #[test]
    fn only_unsupported_clone_errors_fall_back() {
        assert!(clone_unsupported(&io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported"
        )));
        assert!(clone_unsupported(&io::Error::from_raw_os_error(
            libc::EXDEV
        )));
        assert!(clone_unsupported(&io::Error::from_raw_os_error(
            libc::EOPNOTSUPP
        )));
        assert!(clone_unsupported(&io::Error::from_raw_os_error(
            libc::ENOTTY
        )));
        assert!(!clone_unsupported(&io::Error::from_raw_os_error(
            libc::EACCES
        )));
        assert!(!clone_unsupported(&io::Error::from_raw_os_error(
            libc::ENOSPC
        )));
    }

    #[test]
    fn unsupported_clone_copies_but_permission_failure_does_not() -> Result<()> {
        let dir = tempdir()?;
        let source = dir.path().join("source");
        let copied = dir.path().join("copied");
        let denied = dir.path().join("denied");
        fs::write(&source, "original")?;
        let mut transfer = Transfer::default();
        transfer_file(&source, &copied, 8, false, &mut transfer, |_, _| {
            Err(io::Error::from_raw_os_error(libc::EXDEV))
        })?;
        assert_eq!(transfer.backend(false), "copy");
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
