//! Prepare checkout from a small independently indexed store while Git builds
//! the complete single-commit pack. Only the complete pack is installed.
use super::{
    Boundary, ObjectId, PackInput, bootstrap, cache, checked, export_pack, output_text,
    transfer_git, verify_membership,
};
use crate::workspace::objects::{Primary, finish_head};
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::{Read, Seek, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
};

// Larger manifests or attribute files take the ordinary verified checkout.
const PROOF_LIMIT: u64 = 16 * 1024 * 1024;

pub(in crate::workspace::objects) fn eligible(
    state: &Path,
    format: &str,
    boundary: &Boundary<'_>,
) -> bool {
    cfg!(any(target_os = "macos", target_os = "linux"))
        && !cache::Cache::new(state, format, boundary.cache_key).has_entry()
}

pub(in crate::workspace::objects) fn checkout(
    source: &Primary<'_>,
    root: &Path,
    state: &Path,
    base: &str,
    reference: &str,
    boundary: &Boundary<'_>,
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
        let Ok(proof) = checkout_proof(source.path, root, base) else {
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
    let mut listing = git_objects(repo, objects)
        .args(["ls-tree", "-r", "-t", "-z", base])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut rows = Vec::new();
    let read = listing
        .stdout
        .take()
        .context("missing Git tree listing")?
        .take(PROOF_LIMIT + 1)
        .read_to_end(&mut rows);
    let status = listing.wait()?;
    ensure!(
        read? as u64 <= PROOF_LIMIT && status.success(),
        "cannot bound checkout tree listing"
    );
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

fn checkout_proof(primary: &Path, root: &Path, base: &str) -> Result<tempfile::TempDir> {
    let width = base.len() / 2;
    let hints = metadata_ids(primary, None, base)?;
    let stage = cache::private_tempdir_in(&root.join(".git"))?;
    let dir = stage.path().join("pack");
    fs::create_dir(&dir)?;
    let mut ids = Vec::with_capacity(hints.len() * (2 * width + 1));
    for id in &hints {
        id.write_hex(width, &mut ids);
    }
    let mut input = tempfile::tempfile_in(stage.path())?;
    input.write_all(&ids)?;
    input.rewind()?;
    let mut size = transfer_git(primary);
    size.arg("cat-file")
        .arg("--batch-check=%(objectsize)")
        .stdin(input);
    let sizes = checked(size)?;
    let mut total = 0u64;
    for line in output_text(&sizes)?.lines() {
        total = total.saturating_add(line.parse::<u64>()?);
        ensure!(
            total <= PROOF_LIMIT,
            "checkout metadata exceeds proof limit"
        );
    }
    let pack = export_pack(primary, root, &dir, width, 0, PackInput::Ids(&hints), true)?;
    // Git's index-pack rehashed the objects. Walk the pinned commit in that
    // isolated store, so a dishonest donor index cannot introduce arbitrary
    // path associations or omit a committed attributes file.
    let expected = metadata_ids(root, Some(stage.path()), base)?;
    verify_membership(&[pack], &expected, width)?;
    Ok(stage)
}

#[cfg(test)]
mod tests {
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
        let proof = checkout_proof(repo, repo, base)?;
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
        Ok(())
    }
}
