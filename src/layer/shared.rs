//! A single project-wide enrollment for a caller-attested, source-independent npm install.
//! The install must match the lock and be relocatable. The caller accounts for
//! lifecycle inputs, registry settings outside the checkout, ABI and toolchain.

use super::registered::{INSTALL_PATH_NOT_IGNORED, require_ignored};
use super::{
    LOCKS, TRACKED_STATUS, Transfer, check_metadata, entry_exists, exact_digest, field,
    layer_provenance, restore_layer, safe_link, save_layer, small_file, source_contents,
    valid_digest, verify_layer_contents,
};
use crate::{api::Fault, git, registry, storage};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{Seek, Write},
    path::{Path, PathBuf},
    process::Stdio,
};
use uuid::Uuid;

mod npm;

const MARKER: &str = "shared-layer.json";
const MANIFEST: &str = "package.json";
// Pin project configuration and common toolchain selectors. The caller attests
// to other installation inputs, including environment and user/global config.
const CONFIGS: &[&str] = &[
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pnpmfile.cjs",
    "pnpmfile.cjs",
    "bunfig.toml",
    ".nvmrc",
    ".node-version",
    ".tool-versions",
    ".mise.toml",
    ".mise.local.toml",
    "mise.toml",
    "mise.local.toml",
    ".corepack.env",
];

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Active {
    version: u8,
    digest: String,
    generation: String,
    inputs: String,
    contents: String,
}

impl Active {
    fn existing(path: &Path) -> Result<Option<Self>> {
        entry_exists(path)?.then(|| Self::read(path)).transpose()
    }

    fn read(path: &Path) -> Result<Self> {
        let bytes = small_regular_file(path)?;
        let metadata = fs::symlink_metadata(path)?;
        check_metadata(path, &metadata)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                metadata.mode() & 0o077 == 0
                    && metadata.nlink() == 1
                    && metadata.uid() == unsafe { libc::geteuid() },
                "shared layer registration is not private"
            );
        }
        let active: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid shared layer registration"))?;
        ensure!(
            active.version == 1
                && valid_digest(&active.digest)
                && valid_digest(&active.inputs)
                && valid_digest(&active.contents)
                && Uuid::parse_str(&active.generation)
                    .is_ok_and(|id| id.to_string() == active.generation)
                && serde_json::to_vec(&active)? == bytes,
            "invalid shared layer registration"
        );
        Ok(active)
    }

    fn provenance(&self, layer: &Path) -> Result<Value> {
        let provenance = layer_provenance(layer, &self.digest)?;
        ensure!(
            provenance["version"] == 2
                && provenance["generation"] == self.generation
                && provenance["contents"] == self.contents,
            "registered layer generation is unavailable"
        );
        Ok(provenance)
    }
}

fn small_regular_file(path: &Path) -> Result<Vec<u8>> {
    small_file(path, "invalid shared layer state file", |_, _| Ok(()))
}

fn private_state(state: &Path) -> Result<()> {
    let metadata = registry::owned_directory(state)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() },
            "shared layers require private project state owned by the current user"
        );
    }
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

fn owned_layers(state: &Path) -> Result<PathBuf> {
    let layers = state.join("layers");
    registry::owned_directory(&layers)?;
    let id = small_regular_file(&state.join("project-id"))?;
    let text = std::str::from_utf8(&id)?;
    ensure!(
        Uuid::parse_str(text)?.to_string() == text
            && small_regular_file(&layers.join(registry::CACHE_OWNER))? == id,
        "shared layer cache ownership is unverified"
    );
    Ok(layers)
}

struct Inputs {
    head: String,
    tree: String,
    manifest: Vec<u8>,
    lock_name: &'static str,
    lock: Vec<u8>,
    digest: String,
}

impl Inputs {
    fn read(checkout: &Path, state: &Path, primary: &Path) -> Result<Self> {
        let refs = git::text(
            checkout,
            &[
                "rev-parse",
                "--show-prefix",
                "--path-format=absolute",
                "--git-common-dir",
                "HEAD",
                "HEAD^{tree}",
            ],
        )?;
        #[cfg(windows)]
        let refs = refs.replace("\r\n", "\n");
        let refs = refs
            .strip_prefix('\n')
            .context("shared layers require a checkout root")?;
        let (rest, tree) = refs.rsplit_once('\n').context("missing Git tree")?;
        let (common, head) = rest.rsplit_once('\n').context("missing Git checkout")?;
        git::checked_oid(head)?;
        git::checked_oid(tree)?;
        let project_common = if checkout == primary {
            common.into()
        } else {
            storage::validate_workspace(checkout, common)?;
            ensure!(
                checkout.join(".git").join("graft") != state,
                "independent checkout must have separate project state"
            );
            git::text(
                primary,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?
        };
        ensure!(
            dunce::canonicalize(project_common)?.join("graft") == state,
            "shared layers require registered project state"
        );
        ensure!(
            !entry_exists(&checkout.join("pnpm-workspace.yaml"))?,
            "workspace projects are ineligible for shared layers"
        );
        let mut names = vec![MANIFEST];
        for name in LOCKS {
            if entry_exists(&checkout.join(name))? {
                names.push(name);
            }
        }
        ensure!(
            names.len() == 2 && matches!(names[1], "package-lock.json" | "npm-shrinkwrap.json"),
            "shared layers require one root npm package-lock.json or npm-shrinkwrap.json"
        );
        for name in CONFIGS {
            if entry_exists(&checkout.join(name))? {
                names.push(name);
            }
        }

        // Capture the index as well as status. Git deliberately conceals some
        // tracked changes marked assume-unchanged or skip-worktree.
        let listing = git::bytes(checkout, &["ls-files", "-v", "--stage", "-z"])?;
        let mut blobs = BTreeMap::new();
        for record in listing.split(|byte| *byte == 0).filter(|r| !r.is_empty()) {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid index entry")?;
            let path = &record[tab + 1..];
            let mut fields = std::str::from_utf8(&record[..tab])?.split(' ');
            ensure!(
                fields.next() == Some("H"),
                "shared layers require a complete, visible index"
            );
            let mode = fields.next().context("missing index mode")?;
            let blob = fields.next().context("missing index object")?;
            ensure!(
                fields.next() == Some("0") && mode != "160000",
                "submodules and conflicted indexes are ineligible for shared layers"
            );
            ensure!(
                !path
                    .split(|byte| *byte == b'/')
                    .next()
                    .unwrap_or_default()
                    .eq_ignore_ascii_case(b"node_modules"),
                "tracked node_modules is ineligible for shared layers"
            );
            if let Some(name) = names.iter().find(|name| name.as_bytes() == path) {
                ensure!(
                    matches!(mode, "100644" | "100755"),
                    "install inputs must be regular files"
                );
                blobs.insert(*name, git::checked_oid(blob)?);
            }
        }
        ensure!(blobs.len() == names.len(), "install inputs must be tracked");
        ensure!(
            git::bytes(checkout, TRACKED_STATUS)?.is_empty(),
            "shared layers require clean tracked source and index"
        );

        // Read the committed blob bytes in one Git call. Comparing raw bytes
        // also rejects checkout filters that change install inputs.
        let mut requests = tempfile::tempfile()?;
        for name in &names {
            writeln!(requests, "{head}:{name}")?;
        }
        requests.rewind()?;
        let output = git::command(checkout)
            .args(["--no-replace-objects", "cat-file", "--batch"])
            .stdin(Stdio::from(requests))
            .output()?;
        ensure!(
            output.status.success(),
            "cannot read committed install inputs"
        );

        let mut data = output.stdout.as_slice();
        let mut digest = blake3::Hasher::new();
        for domain in [
            b"git-graft-shared-npm-v1".as_slice(),
            std::env::consts::OS.as_bytes(),
            std::env::consts::ARCH.as_bytes(),
        ] {
            field(&mut digest, domain);
        }
        let mut inputs = Vec::with_capacity(names.len());
        for name in names {
            let (header, rest) = split_line(data)?;
            let len: usize = std::str::from_utf8(header)?
                .strip_prefix(blobs[name])
                .and_then(|header| header.strip_prefix(" blob "))
                .context("install inputs must match the committed index")?
                .parse()?;
            let (committed, rest) = rest.split_at_checked(len).context("incomplete Git blob")?;
            data = rest
                .strip_prefix(b"\n")
                .context("invalid Git blob terminator")?;

            let path = checkout.join(name);
            let metadata = fs::symlink_metadata(&path)?;
            ensure!(
                metadata.is_file()
                    && !registry::is_reparse_point(&metadata)
                    && metadata.len() == len as u64
                    && fs::read(&path)? == committed,
                "install inputs must match committed files"
            );
            field(&mut digest, name.as_bytes());
            field(&mut digest, committed);
            if inputs.len() < 2 {
                inputs.push((name, committed.to_vec()));
            }
        }
        ensure!(data.is_empty(), "unexpected Git batch output");
        let (lock_name, lock) = inputs.pop().context("missing npm lock")?;
        let (_, manifest) = inputs.pop().context("missing npm manifest")?;
        Ok(Self {
            head: head.to_owned(),
            tree: format!("{tree}\n"),
            manifest,
            lock_name,
            lock,
            digest: digest.finalize().to_hex().to_string(),
        })
    }

    fn recheck(&self, checkout: &Path, state: &Path, primary: &Path) -> Result<()> {
        let after = Self::read(checkout, state, primary)?;
        ensure!(
            after.head == self.head
                && after.tree == self.tree
                && after.digest == self.digest
                && git::text(checkout, &["rev-parse", "HEAD"])? == self.head,
            "checkout changed during shared layer transfer"
        );
        Ok(())
    }
}

fn split_line(data: &[u8]) -> Result<(&[u8], &[u8])> {
    let stop = data
        .iter()
        .position(|byte| *byte == b'\n')
        .context("incomplete Git blob header")?;
    Ok((&data[..stop], &data[stop + 1..]))
}

/// Register an already installed, source-independent npm tree. Caller holds
/// the project lock and attests to the lockfile, toolchain and install inputs.
pub fn seed_shared(checkout: &Path, state: &Path, key: &str) -> Result<Value> {
    ensure!(
        !key.trim().is_empty(),
        "a nonempty toolchain and environment key is required"
    );
    let checkout = dunce::canonicalize(checkout)?;
    private_state(state)?;
    let layers = owned_layers(state)?;
    let marker = state.join(MARKER);
    let previous =
        Active::existing(&marker).context("cannot replace invalid shared layer registration")?;
    let inputs = Inputs::read(&checkout, state, &checkout)?;
    npm::validate(&inputs.manifest, &inputs.lock)?;
    let digest = exact_digest(
        key,
        inputs.tree.as_bytes(),
        &inputs.manifest,
        inputs.lock_name,
        &inputs.lock,
    );
    let layer = layers.join(&digest);
    let mut receipt = if entry_exists(&layer)? {
        let provenance = verify_layer_contents(&layer, &digest)?;
        ensure!(
            provenance["version"] == 2,
            "an explicit layer already exists; remove it before registering a shared layer with this key"
        );
        ensure!(
            source_contents(&checkout)? == provenance["contents"],
            "installed tree differs from the saved layer; remove it before replacing it"
        );
        Transfer::default().receipt("save", &digest, "copy-on-write")
    } else {
        save_layer(
            &checkout,
            &layer,
            &digest,
            Some(Uuid::new_v4()),
            false,
            || inputs.recheck(&checkout, state, &checkout),
        )?
    };

    let provenance = layer_provenance(&layer, &digest)?;
    let active = Active {
        version: 1,
        digest: digest.clone(),
        generation: provenance["generation"]
            .as_str()
            .context("missing layer generation")?
            .into(),
        inputs: inputs.digest.clone(),
        contents: provenance["contents"]
            .as_str()
            .context("missing layer contents")?
            .into(),
    };
    active.provenance(&layer)?;
    inputs.recheck(&checkout, state, &checkout)?;
    ensure!(
        Active::existing(&marker)? == previous,
        "shared layer registration changed during capture"
    );
    registry::atomic(&marker, &serde_json::to_vec(&active)?)?;
    receipt["status"] = json!("registered");
    receipt["manager"] = json!("npm");
    Ok(receipt)
}

/// Attach the primary's registered layer to an authenticated standalone checkout.
pub fn inherit_shared(checkout: &Path, state: &Path, primary: &Path) -> Result<Option<Value>> {
    let status = |status: &str, reason: &str| {
        Ok(Some(
            json!({"status": status, "reason": reason, "manager": "npm"}),
        ))
    };
    if private_state(state).is_err() {
        return status("unavailable", "invalid_project_state");
    }
    let marker = state.join(MARKER);
    let active = match Active::existing(&marker) {
        Ok(Some(active)) => active,
        Ok(None) => return Ok(None),
        Err(_) => return status("unavailable", "invalid_registration"),
    };
    let Ok(layers) = owned_layers(state) else {
        return status("unavailable", "invalid_layer_cache");
    };
    let layer = layers.join(&active.digest);
    let Ok(provenance) = active.provenance(&layer) else {
        return status("unavailable", "layer_generation_unavailable");
    };
    let Ok(checkout) = dunce::canonicalize(checkout) else {
        return status("inputs_mismatch", "ineligible_checkout");
    };
    if checkout == primary {
        return status("inputs_mismatch", "ineligible_checkout");
    }
    match entry_exists(&checkout.join("node_modules")) {
        Ok(false) => (),
        Ok(true) => return status("unavailable", "node_modules_exists"),
        Err(_) => return status("unavailable", "node_modules_unavailable"),
    }
    let Ok(inputs) = Inputs::read(&checkout, state, primary) else {
        return status("inputs_mismatch", "ineligible_checkout");
    };
    if inputs.digest != active.inputs {
        return status("inputs_mismatch", "dependency_inputs_changed");
    }
    if require_ignored(&checkout, "node_modules").is_err() {
        return status("unavailable", INSTALL_PATH_NOT_IGNORED);
    }
    let result = restore_layer(
        &checkout,
        &layer,
        &active.digest,
        &provenance,
        false,
        || {
            ensure!(
                Active::read(&marker)? == active,
                "shared layer registration changed during transfer"
            );
            inputs.recheck(&checkout, state, primary)?;
            require_ignored(&checkout, "node_modules")
        },
    );
    match result {
        Ok(mut receipt) => {
            receipt["status"] = json!("attached");
            receipt["manager"] = json!("npm");
            Ok(Some(receipt))
        }
        Err(error) => match error.downcast_ref::<Fault>().map(|fault| fault.code) {
            Some(reason @ ("cow_unavailable" | INSTALL_PATH_NOT_IGNORED)) => {
                status("unavailable", reason)
            }
            _ => status("unavailable", "layer_restore_failed"),
        },
    }
}

/// Check the attached checkout's current write targets before invoking npm.
/// Caller holds the workspace lock. These checks cover static filesystem
/// links. npm scripts and concurrent edits run in the caller's trust domain.
pub fn check_attached_for_add(checkout: &Path) -> Result<()> {
    let manifest = checkout.join(MANIFEST);
    exclusive_regular_file(&manifest, &fs::symlink_metadata(&manifest)?)?;
    let manifest: Value =
        serde_json::from_slice(&fs::read(manifest)?).context("invalid npm manifest")?;
    ensure!(
        manifest.is_object() && manifest.get("workspaces").is_none(),
        "adding to a shared install requires a single-package npm project"
    );
    for name in ["package-lock.json", "npm-shrinkwrap.json"] {
        let lock = checkout.join(name);
        if entry_exists(&lock)? {
            exclusive_regular_file(&lock, &fs::symlink_metadata(&lock)?)?;
        }
    }
    let root = checkout.join("node_modules");
    ensure!(
        entry_exists(&root)?,
        "the attached node_modules is missing; restore the install before adding a package"
    );
    registry::check_owned_tree(&root)?;
    let root = dunce::canonicalize(root)?;
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                safe_link(&root, &path)?;
            } else if metadata.is_dir() && !registry::is_reparse_point(&metadata) {
                pending.push(path);
            } else {
                exclusive_regular_file(&path, &metadata)?;
            }
        }
    }
    Ok(())
}

fn exclusive_regular_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    ensure!(
        metadata.is_file() && !registry::is_reparse_point(metadata),
        "dependency write target must be a regular file: {}",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.nlink() == 1,
            "dependency write target is shared by a hardlink: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::registry::Project;
    use std::fs::File;
    use std::os::unix::fs::{MetadataExt, symlink};
    use tempfile::{TempDir, tempdir};

    const KEY: &str = "private-toolchain-attestation";

    fn fixture() -> Result<(TempDir, Project, File)> {
        let root = tempdir()?;
        let repo = root.path().join("repo ");
        fs::create_dir(&repo)?;
        let repo = dunce::canonicalize(repo)?;
        git::bytes(&repo, &["init", "-q"])?;
        git::bytes(&repo, &["config", "user.email", "fixture@example.test"])?;
        git::bytes(&repo, &["config", "user.name", "Fixture"])?;
        fs::write(repo.join("src.js"), "original source")?;
        fs::write(repo.join(".gitignore"), "/node_modules\n/.npmrc\n")?;
        fs::write(
            repo.join(MANIFEST),
            r#"{"name":"fixture","version":"1.0.0"}"#,
        )?;
        fs::write(
            repo.join("package-lock.json"),
            r#"{"lockfileVersion":3,"packages":{"":{"name":"fixture","version":"1.0.0"}}}"#,
        )?;
        git::bytes(&repo, &["add", "--all"])?;
        git::bytes(&repo, &["commit", "-qm", "fixture"])?;
        let (project, lock) = Project::open(&repo)?;
        Ok((root, project, lock))
    }

    /// Seed the primary's shared layer, or return None without native copy-on-write.
    fn seed_unless_cow_unavailable(project: &Project) -> Result<Option<Value>> {
        match seed_shared(&project.repo, &project.state, KEY) {
            Ok(receipt) => Ok(Some(receipt)),
            Err(error)
                if error
                    .downcast_ref::<Fault>()
                    .is_some_and(|e| e.code == "cow_unavailable") =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn clone_checkout(repo: &Path, state: &Path, checkout: &Path) -> Result<()> {
        git::bytes(
            repo,
            &[
                "clone",
                "-q",
                "--no-local",
                "--no-checkout",
                ".",
                git::path_str(checkout)?,
            ],
        )?;
        let oid = git::oid(repo, "HEAD")?;
        storage::materialize(checkout, state, &oid, &[], repo)?;
        for (key, value) in [
            ("user.email", "fixture@example.test"),
            ("user.name", "Fixture"),
        ] {
            git::bytes(checkout, &["config", key, value])?;
        }
        Ok(())
    }

    #[test]
    fn dependency_inputs_pin_project_configs_and_transfer_commit() -> Result<()> {
        let (_root, project, _lock) = fixture()?;
        let repo = &project.repo;
        let state = &project.state;
        assert!(inherit_shared(repo, state, repo)?.is_none());
        let original = Inputs::read(repo, state, repo)?;

        git::bytes(repo, &["commit", "--allow-empty", "-qm", "different base"])?;
        let other = Inputs::read(repo, state, repo)?;
        assert_eq!(other.digest, original.digest);
        assert_eq!(other.tree, original.tree);
        assert!(original.recheck(repo, state, repo).is_err());

        fs::write(
            repo.join(".npmrc"),
            "registry=https://different.example.test\n",
        )?;
        assert!(Inputs::read(repo, state, repo).is_err());
        git::bytes(repo, &["add", "-f", ".npmrc"])?;
        git::bytes(repo, &["commit", "-qm", "new project config"])?;
        let configured = Inputs::read(repo, state, repo)?;
        assert_ne!(configured.digest, original.digest);

        fs::write(repo.join("src.js"), "dirty source")?;
        assert!(Inputs::read(repo, state, repo).is_err());
        git::bytes(repo, &["add", "src.js"])?;
        git::bytes(repo, &["commit", "-qm", "source only"])?;
        assert_eq!(Inputs::read(repo, state, repo)?.digest, configured.digest);
        git::bytes(repo, &["update-index", "--assume-unchanged", "src.js"])?;
        assert!(Inputs::read(repo, state, repo).is_err());
        Ok(())
    }

    #[test]
    fn invalid_registration_is_preserved() -> Result<()> {
        let (root, project, _lock) = fixture()?;
        let marker = project.state.join(MARKER);
        let foreign = root.path().join("foreign");
        fs::write(&foreign, b"keep")?;
        symlink(&foreign, &marker)?;
        assert!(seed_shared(&project.repo, &project.state, KEY).is_err());
        assert_eq!(fs::read_link(&marker)?, foreign);
        assert_eq!(
            inherit_shared(&project.repo, &project.state, &project.repo)?.unwrap()["reason"],
            "invalid_registration"
        );
        fs::remove_file(&marker)?;
        registry::atomic(&marker, b"foreign state")?;
        assert!(seed_shared(&project.repo, &project.state, KEY).is_err());
        assert_eq!(fs::read(&marker)?, b"foreign state");
        assert_eq!(fs::read(&foreign)?, b"keep");
        Ok(())
    }

    #[test]
    fn independent_checkout_inherits_only_its_projects_layer() -> Result<()> {
        let (tmp, project, _lock) = fixture()?;
        let repo = &project.repo;
        let state = &project.state;
        let modules = repo.join("node_modules");
        fs::create_dir(&modules)?;
        fs::write(modules.join("payload"), b"installed")?;
        let manual = super::super::run(repo, state, "save", "manual", true)?;

        let workspace = tmp.path().join("independent");
        clone_checkout(repo, state, &workspace)?;
        let restored = super::super::run(&workspace, state, "restore", "manual", true)?;
        assert_eq!(restored["digest"], manual["digest"]);
        assert_eq!(restored["backend"], "copy");
        let installed = workspace.join("node_modules");
        assert_eq!(fs::read(installed.join("payload"))?, b"installed");
        fs::remove_dir_all(&installed)?;

        let Some(registered) = seed_unless_cow_unavailable(&project)? else {
            assert!(inherit_shared(&workspace, state, repo)?.is_none());
            assert!(!installed.exists());
            return Ok(());
        };
        assert_eq!(
            inherit_shared(&workspace, state, &workspace)?.unwrap()["reason"],
            "ineligible_checkout"
        );
        let other = tmp.path().join("other");
        fs::create_dir(&other)?;
        git::bytes(&other, &["init", "--bare", "-q"])?;
        assert_eq!(
            inherit_shared(&workspace, state, &other)?.unwrap()["reason"],
            "ineligible_checkout"
        );
        assert!(!installed.exists());

        git::bytes(&workspace, &["config", "core.excludesFile", "/dev/null"])?;
        fs::write(workspace.join(".gitignore"), "/node_modules/*\n/.npmrc\n")?;
        git::bytes(&workspace, &["add", ".gitignore"])?;
        git::bytes(&workspace, &["commit", "-qm", "ignore children only"])?;
        let exposed = inherit_shared(&workspace, state, repo)?.unwrap();
        assert_eq!(exposed["status"], "unavailable");
        assert_eq!(exposed["reason"], INSTALL_PATH_NOT_IGNORED);
        assert!(!installed.exists());

        let exclude = workspace.join(".git/info/exclude");
        fs::create_dir_all(exclude.parent().unwrap())?;
        fs::write(exclude, "/node_modules/\n")?;
        let receipt = inherit_shared(&workspace, state, repo)?.unwrap();
        assert_eq!(receipt["status"], "attached", "{receipt}");
        assert_eq!(receipt["digest"], registered["digest"]);
        assert_eq!(receipt["copied_bytes"], 0);
        assert_eq!(receipt["cloned_bytes"], 9);
        let cached = state
            .join("layers")
            .join(receipt["digest"].as_str().unwrap())
            .join("node_modules/payload");
        assert_ne!(
            fs::metadata(installed.join("payload"))?.ino(),
            fs::metadata(&cached)?.ino()
        );
        fs::write(installed.join("payload"), b"edited in workspace")?;
        assert_eq!(fs::read(cached)?, b"installed");
        assert_eq!(fs::read(modules.join("payload"))?, b"installed");
        Ok(())
    }

    #[test]
    fn shared_generation_is_isolated_and_ordinary_save_cannot_reactivate_it() -> Result<()> {
        let (tmp, project, _lock) = fixture()?;
        let repo = &project.repo;
        let state = &project.state;
        let primary_modules = repo.join("node_modules");
        fs::create_dir(&primary_modules)?;
        fs::write(primary_modules.join("payload"), b"installed")?;
        symlink("payload", primary_modules.join("local-link"))?;

        let Some(first) = seed_unless_cow_unavailable(&project)? else {
            assert!(!entry_exists(&state.join(MARKER))?);
            assert_eq!(fs::read_dir(state.join("layers"))?.count(), 1);
            return Ok(());
        };
        assert_eq!(first["status"], "registered");
        assert_eq!(first["copied_bytes"], 0);
        let digest = first["digest"].as_str().unwrap();
        let marker = state.join(MARKER);
        let registered = fs::read(&marker)?;
        assert!(!String::from_utf8_lossy(&registered).contains(KEY));
        let cached = state.join("layers").join(digest);
        super::super::verify_layer(&cached, digest)?;
        assert_eq!(seed_shared(repo, state, KEY)?["digest"], digest);
        assert_eq!(fs::read(&marker)?, registered);

        let checkout = tmp.path().join("target");
        clone_checkout(repo, state, &checkout)?;
        fs::write(checkout.join("src.js"), "different source")?;
        git::bytes(&checkout, &["add", "src.js"])?;
        git::bytes(&checkout, &["commit", "-qm", "source only"])?;

        let installed = checkout.join("node_modules");
        assert!(super::super::run(&checkout, state, "restore", KEY, true).is_err());
        assert!(!installed.exists());
        fs::write(cached.join("node_modules/payload"), b"cache corrupt")?;
        assert_eq!(
            inherit_shared(&checkout, state, repo)?.unwrap()["reason"],
            "layer_restore_failed"
        );
        assert!(!installed.exists());
        fs::write(cached.join("node_modules/payload"), b"installed")?;

        let receipt = inherit_shared(&checkout, state, repo)?.unwrap();
        assert_eq!(receipt["action"], "restore");
        assert_eq!(receipt["status"], "attached");
        assert_eq!(receipt["copied_bytes"], 0);
        assert_eq!(receipt["cloned_bytes"], 9);
        assert_eq!(receipt["digest"], digest);
        assert_ne!(
            fs::metadata(installed.join("payload"))?.ino(),
            fs::metadata(cached.join("node_modules/payload"))?.ino()
        );
        fs::write(installed.join("local-link"), b"changed in workspace")?;
        assert_eq!(fs::read(primary_modules.join("payload"))?, b"installed");
        assert_eq!(fs::read(cached.join("node_modules/payload"))?, b"installed");
        assert_eq!(
            inherit_shared(&checkout, state, repo)?.unwrap()["reason"],
            "node_modules_exists"
        );
        assert_eq!(
            fs::read(installed.join("payload"))?,
            b"changed in workspace"
        );
        fs::remove_dir_all(&installed)?;
        fs::write(
            checkout.join(MANIFEST),
            r#"{"name":"different","version":"1.0.0"}"#,
        )?;
        git::bytes(&checkout, &["add", MANIFEST])?;
        git::bytes(&checkout, &["commit", "-qm", "changed dependency inputs"])?;
        assert_eq!(
            inherit_shared(&checkout, state, repo)?.unwrap()["status"],
            "inputs_mismatch"
        );
        assert!(!installed.exists());

        super::super::run(repo, state, "forget", KEY, false)?;
        assert_eq!(
            inherit_shared(&checkout, state, repo)?.unwrap()["reason"],
            "layer_generation_unavailable"
        );
        assert_eq!(
            super::super::run(repo, state, "save", KEY, true)?["digest"],
            digest
        );
        assert_eq!(
            inherit_shared(&checkout, state, repo)?.unwrap()["reason"],
            "layer_generation_unavailable"
        );
        assert!(!installed.exists());
        Ok(())
    }

    #[test]
    fn attached_add_rejects_external_links_and_hardlinks() -> Result<()> {
        let (tmp, project, _lock) = fixture()?;
        let repo = &project.repo;
        let modules = repo.join("node_modules");
        let external = tmp.path().join("external");
        fs::write(&external, b"untouched")?;
        symlink(&external, &modules)?;
        assert!(check_attached_for_add(repo).is_err());
        fs::remove_file(&modules)?;
        assert!(check_attached_for_add(repo).is_err());

        fs::create_dir(&modules)?;
        fs::write(modules.join("package"), b"package")?;
        symlink("package", modules.join("bin"))?;
        check_attached_for_add(repo)?;
        symlink(&external, modules.join("external-link"))?;
        assert!(check_attached_for_add(repo).is_err());
        fs::remove_file(modules.join("external-link"))?;
        fs::hard_link(&external, modules.join("hardlink"))?;
        assert!(check_attached_for_add(repo).is_err());
        fs::remove_file(modules.join("hardlink"))?;

        let manifest = repo.join(MANIFEST);
        fs::write(&manifest, br#"{"workspaces":["../external"]}"#)?;
        assert!(check_attached_for_add(repo).is_err());
        fs::write(&manifest, b"{}")?;

        let lock = repo.join("package-lock.json");
        fs::remove_file(&lock)?;
        symlink(&external, &lock)?;
        assert!(check_attached_for_add(repo).is_err());
        fs::remove_file(&lock)?;
        fs::remove_file(&manifest)?;
        fs::hard_link(&external, &manifest)?;
        assert!(check_attached_for_add(repo).is_err());
        assert_eq!(fs::read(&external)?, b"untouched");
        Ok(())
    }
}
