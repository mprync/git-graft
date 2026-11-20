//! Plan verified file checkout while Git installs the complete selected pack.
use super::{
    DonorReuse, ObjectId, Pack, Source, cache, checked, output_text, pack_with_donors,
    rename_exclusive, transfer_git, verify_membership,
};
use crate::workspace::objects::{Primary, finish_head};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    thread,
};
mod metadata;
pub(in crate::workspace) use metadata::CheckoutDiscovery;
use metadata::LoosePlan;
#[cfg(test)]
use metadata::prepare_checkout_proof;

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
        None,
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
    plan: LoosePlan,
    transfer: impl FnOnce() -> Result<Value> + Send,
) -> Result<(Value, Option<Value>)> {
    thread::scope(|scope| {
        let worker = thread::Builder::new().spawn_scoped(scope, transfer)?;
        let wait = || {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Git history worker failed"))?
        };
        // This optional proof may miss when the primary is repacked. A full
        // verified pack is sufficient to use the normal checkout in that case.
        let Ok(proof) = plan.install(root, base) else {
            let receipt = wait()?;
            finish_head(source.path, root, reference, base, None)?;
            return Ok((receipt, None));
        };
        finish_head(source.path, root, reference, base, Some(proof.path()))?;
        let mut receipt = None;
        let checkout = crate::storage::materialize_planned(
            root,
            state,
            base,
            source.path,
            Some(proof.path()),
            Some(&plan.index),
            || {
                receipt = Some(wait()?);
                Ok(())
            },
        )?;
        Ok((
            receipt.context("checkout did not verify the complete Git history")?,
            Some(checkout),
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::objects::run;

    #[test]
    fn checkout_plan_verifies_attribute_bytes_and_binds_the_selected_commit() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let repo = tmp.path();
        run(
            repo,
            &["init", "--quiet", "--template=", "--object-format=sha1"],
        )?;
        fs::write(repo.join("source"), b"committed bytes\n")?;
        fs::create_dir(repo.join("part"))?;
        for path in ["part.ext", "part/entrée", "part0"] {
            fs::write(repo.join(path), path)?;
        }
        // Exercise the concurrent installed-store checks when a selected
        // subtree or blob disappears. These entries share one Git blob.
        #[cfg(target_os = "macos")]
        for n in 0..128 {
            fs::write(repo.join("part").join(n.to_string()), b"repeated")?;
        }
        fs::write(repo.join(".gitattributes"), b"source text eol=crlf\n")?;
        run(repo, &["add", "--", "."])?;
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
        let attr = run(repo, &["rev-parse", "HEAD:.gitattributes"])?;
        let id = output_text(&attr)?;
        let plan = prepare_checkout_proof(repo, base, None)?;
        let proof = plan.install(repo, base)?;
        assert!(proof.path().join(&id[..2]).join(&id[2..]).is_file());
        assert!(plan.install(repo, &"0".repeat(base.len())).is_err());
        let other = run(repo, &["rev-parse", "HEAD:source"])?;
        let other = output_text(&other)?;
        let objects = repo.join(".git/objects");
        #[cfg(target_os = "macos")]
        {
            plan.verify_local(repo)?;
            let subtree = run(repo, &["rev-parse", "HEAD:part"])?;
            for object in [other, output_text(&subtree)?] {
                let source = objects.join(&object[..2]).join(&object[2..]);
                let held = tmp.path().join("held-object");
                fs::rename(&source, &held)?;
                assert!(plan.verify_local(repo).is_err());
                fs::rename(held, source)?;
            }
        }
        let attribute_object = objects.join(&id[..2]).join(&id[2..]);
        fs::remove_file(&attribute_object)?;
        fs::copy(
            objects.join(&other[..2]).join(&other[2..]),
            attribute_object,
        )?;
        assert!(prepare_checkout_proof(repo, base, None).is_err());
        #[cfg(target_os = "macos")]
        assert!(plan.verify_local(repo).is_err());
        Ok(())
    }
}
