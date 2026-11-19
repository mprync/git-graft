//! Prepare checkout from a small independently indexed store while Git builds
//! the complete single-commit pack. Only the complete pack is installed.
use super::{
    Boundary, DonorReuse, ObjectId, Pack, Source, bootstrap, cache, checked, output_text,
    pack_with_donors, rename_exclusive, transfer_git, verify_membership,
};
use crate::workspace::objects::{Primary, finish_head};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::{Seek, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
};

// Larger manifests or attribute files take the ordinary verified checkout.
const PROOF_LIMIT: u64 = 16 * 1024 * 1024;

type ParentPacks = (Vec<Pack>, PathBuf, DonorReuse);

pub(super) fn stage_parent(
    source: Source<'_>,
    root: &Path,
    stage: &Path,
    tip: &str,
    boundary: &Path,
    expected: &[ObjectId],
    width: usize,
) -> Result<Option<ParentPacks>> {
    let format = if width == 20 { "sha1" } else { "sha256" };
    let cache = cache::Cache::new(source.state, format, tip);
    if !cache.has_root()
        || fs::metadata(boundary)?.len() != (tip.len() + 1) as u64
        || fs::read(boundary)? != format!("{tip}\n").as_bytes()
    {
        return Ok(None);
    }
    let mut query = transfer_git(source.primary);
    query.args(["rev-parse", "--verify", &format!("{tip}^1")]);
    let parent = checked(query)?;
    let parent = output_text(&parent)?;
    ObjectId::from_hex(parent.as_bytes(), width)?;
    let key = crate::workspace::objects::shallow::cache_key(parent, &format!("{parent}\n"));
    let generation = cache.for_base(&key);
    if !generation.has_entry() {
        return Ok(None);
    }
    let primary_objects = super::repository_path(source.primary, "objects")?;
    let primary_packs = primary_objects.join("pack");
    ensure!(
        fs::symlink_metadata(&primary_objects)?.is_dir()
            && fs::symlink_metadata(&primary_packs)?.is_dir(),
        "invalid primary Git object directory"
    );
    let original = cache::temporary_pack_dir(&primary_packs)?;
    let original_packs = original.path().join("pack");
    fs::create_dir(&original_packs)?;
    // A small new selection can follow a very large deletion. Bound the
    // optional parent read before cloning or hashing its pack files.
    let budget = super::mixed::MIN_PACK_BYTES..=(192 << 20);
    if generation
        .restore_bounded(&original_packs, width, Some(budget))?
        .is_none()
    {
        return Ok(None);
    }
    // Even an interrupted verification must leave unverified parent bytes
    // in primary scratch. Move only individually verified pack pairs.
    let selection = original.path().join("selection");
    fs::create_dir(&selection)?;
    let (mut packs, reuse) = pack_with_donors(
        source,
        root,
        &selection,
        expected,
        width,
        Some(original.path()),
    )?;
    verify_membership(&packs, expected, width)?;
    let objects = stage.join("parent-selection");
    let pack_dir = objects.join("pack");
    fs::create_dir_all(&pack_dir)?;
    for pack in &mut packs {
        for file in [&mut pack.pack, &mut pack.index] {
            let to = pack_dir.join(file.file_name().context("missing Git pack filename")?);
            rename_exclusive(file, &to)?;
            *file = to;
        }
    }
    Ok(Some((packs, objects, reuse)))
}

pub(in crate::workspace) fn needs_tip_proof(state: &Path, base: &str) -> bool {
    let key = crate::workspace::objects::shallow::cache_key(base, &format!("{base}\n"));
    let format = if base.len() == 40 { "sha1" } else { "sha256" };
    cfg!(any(target_os = "macos", target_os = "linux"))
        && !cache::Cache::new(state, format, &key).has_entry()
}

pub(in crate::workspace::objects) fn checkout(
    source: &Primary<'_>,
    root: &Path,
    state: &Path,
    base: &str,
    reference: &str,
    boundary: &Boundary<'_>,
    bytes: Vec<u8>,
) -> Result<(Value, Option<Value>)> {
    thread::scope(|scope| {
        let worker = thread::Builder::new().spawn_scoped(scope, || {
            bootstrap(
                source.path,
                root,
                state,
                source.format,
                base,
                Some(*boundary),
            )
        })?;
        let wait = || {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Git history worker failed"))?
        };
        // This optional proof may miss when the primary is repacked. A full
        // verified pack is sufficient to use the normal checkout in that case.
        let Ok(proof) = checkout_proof(root, base, &bytes) else {
            let receipt = wait()?;
            finish_head(source.path, root, reference, base, None)?;
            return Ok((receipt, None));
        };
        finish_head(source.path, root, reference, base, Some(proof.path()))?;
        let mut receipt = None;
        let checkout =
            crate::storage::materialize_tip(root, state, base, source.path, proof.path(), || {
                receipt = Some(wait()?);
                Ok(())
            })?;
        Ok((
            receipt.context("checkout did not verify the complete Git history")?,
            Some(checkout),
        ))
    })
}

fn git_objects(repo: &Path, objects: Option<&Path>) -> Command {
    let mut command = transfer_git(repo);
    if let Some(objects) = objects {
        command.env("GIT_OBJECT_DIRECTORY", objects);
    }
    command
}

// Trees authenticate the mapping from path to blob. Attribute blobs are the
// only file contents Git needs before it decides which files can be cloned.
fn metadata_ids(repo: &Path, objects: Option<&Path>, base: &str) -> Result<Vec<ObjectId>> {
    let width = base.len() / 2;
    let mut root = git_objects(repo, objects);
    root.args(["rev-parse", "--verify", &format!("{base}^{{tree}}")]);
    let tree = checked(root)?;
    let mut ids = HashSet::from([
        ObjectId::from_hex(base.as_bytes(), width)?,
        ObjectId::from_hex(output_text(&tree)?.as_bytes(), width)?,
    ]);
    let rows = crate::git::bounded_stdout(
        git_objects(repo, objects).args(["ls-tree", "-r", "-t", "-z", base]),
        PROOF_LIMIT,
    )?;
    for row in rows.split(|&b| b == 0).filter(|row| !row.is_empty()) {
        let tab = row
            .iter()
            .position(|&b| b == b'\t')
            .context("invalid Git tree entry")?;
        let name = row[tab + 1..]
            .rsplit(|&b| b == b'/')
            .next()
            .unwrap_or_default();
        let header = &row[..tab];
        if header.starts_with(b"040000 ") || name.eq_ignore_ascii_case(b".gitattributes") {
            let id = header
                .rsplit(|&b| b == b' ')
                .next()
                .context("missing tree object")?;
            ids.insert(ObjectId::from_hex(id, width)?);
        }
    }
    Ok(ids.into_iter().collect())
}

/// Read from the selected donor during reservation. The only named proof
/// files are created later, under the recorded pending workspace.
pub(in crate::workspace) fn prepare_tip_proof(primary: &Path, base: &str) -> Result<Vec<u8>> {
    let width = base.len() / 2;
    let hints = metadata_ids(primary, None, base)?;
    let mut ids = Vec::with_capacity(hints.len() * (2 * width + 1));
    for id in &hints {
        id.write_hex(width, &mut ids);
    }
    let mut input = tempfile::tempfile()?;
    input.write_all(&ids)?;
    input.rewind()?;
    let mut size = transfer_git(primary);
    size.arg("cat-file")
        .arg("--batch-check=%(objectsize)")
        .stdin(input.try_clone()?);
    let sizes = checked(size)?;
    let mut total = 0u64;
    for line in output_text(&sizes)?.lines() {
        total = total.saturating_add(line.parse::<u64>()?);
        ensure!(
            total <= PROOF_LIMIT,
            "checkout metadata exceeds proof limit"
        );
    }
    input.rewind()?;
    crate::git::bounded_stdout(
        transfer_git(primary)
            .args([
                "pack-objects",
                "--quiet",
                "--stdout",
                "--delta-base-offset",
                "--window=0",
            ])
            .stdin(input),
        PROOF_LIMIT,
    )
}

fn checkout_proof(root: &Path, base: &str, bytes: &[u8]) -> Result<tempfile::TempDir> {
    let width = base.len() / 2;
    let stage = cache::private_tempdir_in(&root.join(".git"))?;
    let dir = stage.path().join("pack");
    fs::create_dir(&dir)?;
    let mut index = git_objects(root, Some(stage.path()))
        .args(["index-pack", "--stdin", "--index-version=2", "--threads=2"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let write = index
        .stdin
        .take()
        .context("missing metadata pack input")?
        .write_all(bytes);
    let output = index.wait_with_output()?;
    write?;
    ensure!(output.status.success(), "cannot index metadata pack");
    let hash = output_text(&output.stdout)?
        .strip_prefix("pack\t")
        .context("invalid index-pack receipt")?;
    let pack = Pack::at(&dir, hash, width)?;
    // Git's index-pack rehashed the objects. Walk the pinned commit in that
    // isolated store, so a dishonest donor index cannot introduce arbitrary
    // path associations or omit a committed attributes file.
    let expected = metadata_ids(root, Some(stage.path()), base)?;
    verify_membership(&[pack], &expected, width)?;
    Ok(stage)
}

#[cfg(test)]
mod tests {
    use super::super::{PackInput, export_pack};
    use super::*;
    use crate::workspace::objects::run;

    #[test]
    fn a_readable_tree_does_not_prove_its_attributes_are_present() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path();
        run(
            repo,
            &["init", "--quiet", "--template=", "--object-format=sha1"],
        )?;
        fs::write(repo.join("source"), b"committed bytes\n")?;
        fs::write(repo.join(".gitattributes"), b"source text eol=crlf\n")?;
        run(repo, &["add", "--", "source", ".gitattributes"])?;
        run(
            repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.org",
                "commit",
                "-qm",
                "base",
            ],
        )?;
        let base = run(repo, &["rev-parse", "HEAD"])?;
        let base = output_text(&base)?;
        let id = |name| -> Result<_> {
            let bytes = run(repo, &["rev-parse", name])?;
            ObjectId::from_hex(output_text(&bytes)?.as_bytes(), 20)
        };
        let attr = id("HEAD:.gitattributes")?;
        let source = id("HEAD:source")?;
        let mut bytes = prepare_tip_proof(repo, base)?;
        let proof = checkout_proof(repo, base, &bytes)?;
        let expected = metadata_ids(repo, Some(proof.path()), base)?;
        assert!(expected.contains(&attr) && !expected.contains(&source));
        // Both malformed packs have a readable selected commit and root tree.
        // One omits a conversion rule; the other adds ordinary file content.
        for extra in [false, true] {
            let stage = tempfile::tempdir_in(repo.join(".git"))?;
            let dir = stage.path().join("pack");
            fs::create_dir(&dir)?;
            let ids: Vec<_> = if extra {
                expected.iter().copied().chain([source]).collect()
            } else {
                expected.iter().copied().filter(|id| *id != attr).collect()
            };
            let pack = export_pack(repo, repo, &dir, 20, 0, PackInput::Ids(&ids), true)?;
            let from_tree = metadata_ids(repo, Some(stage.path()), base)?;
            assert!(verify_membership(&[pack], &from_tree, 20).is_err());
        }
        // Reservation and indexing are separate phases. A truncated export
        // must never become the object store used for a checkout.
        bytes.pop();
        assert!(checkout_proof(repo, base, &bytes).is_err());
        Ok(())
    }
}
