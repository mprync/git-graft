use super::{ObjectId, PROOF_LIMIT, cache, transfer_git};
use crate::{
    git,
    storage::{TipIndex, object_digest},
};
use anyhow::{Context, Result, ensure};
use flate2::{Compression, write::ZlibEncoder};
use std::{
    fs,
    io::{Seek, Write},
    path::Path,
    process::Command,
    sync::atomic::AtomicBool,
};

pub(in crate::workspace) struct LoosePlan {
    base: String,
    pub(in crate::workspace::objects) index: TipIndex,
    objects: Vec<(String, Vec<u8>)>,
}

pub(in crate::workspace) struct CheckoutDiscovery {
    stop: std::sync::Arc<AtomicBool>,
    task: Option<std::thread::JoinHandle<Result<LoosePlan>>>,
}

impl CheckoutDiscovery {
    pub(in crate::workspace) fn start(primary: &Path, oid: &str, local: bool) -> Option<Self> {
        let primary = primary.to_owned();
        let oid = oid.to_owned();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let task = std::thread::Builder::new()
            .spawn(move || prepare_checkout_proof(&primary, &oid, local.then_some(&*worker_stop)))
            .ok()?;
        Some(Self {
            stop,
            task: Some(task),
        })
    }

    fn cancel(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub(in crate::workspace::objects) fn finish(mut self) -> Option<LoosePlan> {
        self.cancel();
        self.task.take()?.join().ok()?.ok()
    }
}

impl Drop for CheckoutDiscovery {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            self.cancel();
            let _ = task.join();
        }
    }
}

impl LoosePlan {
    #[cfg(target_os = "macos")]
    pub(in crate::workspace::objects) fn verify_local(&self, root: &Path) -> Result<()> {
        let metadata = || read_metadata(root, &self.base, &self.index, None, |_, _, _| Ok(()));
        if self.index.blobs().nth(127).is_none() {
            self.verify_tree_rows(root)?;
            self.verify_blob_types(root)?;
            return metadata();
        }

        // Both queries read the installed store. Join the type check even if
        // the tree walk or the commit and attribute checks fail.
        std::thread::scope(|scope| {
            let worker =
                std::thread::Builder::new().spawn_scoped(scope, || self.verify_blob_types(root));
            let tree = self.verify_tree_rows(root).and_then(|()| metadata());
            let blobs = match worker {
                Ok(worker) => worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("Git blob check worker failed"))?,
                Err(_) => self.verify_blob_types(root),
            };
            tree.and(blobs)
        })
    }

    #[cfg(target_os = "macos")]
    fn verify_tree_rows(&self, root: &Path) -> Result<()> {
        let rows = git::bounded_stdout(
            transfer_git(root).args(["ls-tree", "-r", "-z", &self.base]),
            PROOF_LIMIT,
        )?;
        ensure!(
            self.index.matches_rows(&rows),
            "installed Git tree differs from checkout plan"
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn verify_blob_types(&self, root: &Path) -> Result<()> {
        git::require_blobs(transfer_git(root), self.index.blobs())
    }

    pub(super) fn install(&self, root: &Path, base: &str) -> Result<tempfile::TempDir> {
        ensure!(base == self.base, "checkout plan has a different commit");
        let stage = cache::private_tempdir_in(&root.join(".git"))?;
        for (id, bytes) in &self.objects {
            let (prefix, name) = id.split_at(2);
            let parent = stage.path().join(prefix);
            fs::create_dir_all(&parent)?;
            fs::File::create_new(parent.join(name))?.write_all(bytes)?;
        }
        Ok(stage)
    }
}

/// Prepare a bounded checkout plan without writing to the primary object store.
pub(in crate::workspace) fn prepare_checkout_proof(
    primary: &Path,
    base: &str,
    cancelled: Option<&AtomicBool>,
) -> Result<LoosePlan> {
    git::checked_oid(base)?;
    let width = base.len() / 2;
    let rows = metadata_output(
        transfer_git(primary).args(["ls-tree", "-r", "-z", base]),
        PROOF_LIMIT,
        cancelled,
    )?;
    let index = TipIndex::new(rows, width)?;
    let mut objects = Vec::new();
    read_metadata(primary, base, &index, cancelled, |id, kind, bytes| {
        objects.push((id.to_owned(), loose(kind, bytes)?));
        Ok(())
    })?;
    Ok(LoosePlan {
        base: base.to_owned(),
        index,
        objects,
    })
}

fn metadata_output(
    command: &mut Command,
    limit: u64,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<u8>> {
    #[cfg(target_os = "macos")]
    if let Some(stop) = cancelled {
        return git::optional::collect(command, usize::try_from(limit)?, stop)
            .context("optional Git metadata unavailable");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = cancelled;
    git::bounded_stdout(command, limit)
}

fn read_metadata(
    primary: &Path,
    base: &str,
    index: &TipIndex,
    cancelled: Option<&AtomicBool>,
    mut visit: impl FnMut(&str, &str, &[u8]) -> Result<()>,
) -> Result<()> {
    let width = base.len() / 2;
    let mut attributes: Vec<_> = index.attributes().collect();
    attributes.sort_unstable();
    attributes.dedup();
    ensure!(
        attributes.len() <= 1024,
        "too many checkout attribute blobs"
    );
    let mut names = tempfile::tempfile()?;
    writeln!(names, "{base}")?;
    for attr in &attributes {
        writeln!(names, "{attr}")?;
    }
    names.rewind()?;
    // Check the declared sizes before asking Git to materialize object bodies.
    let headers = metadata_output(
        transfer_git(primary)
            .args(["cat-file", "--batch-check"])
            .stdin(names.try_clone()?),
        128 * 1024,
        cancelled,
    )?;
    let mut headers = std::str::from_utf8(&headers)?.lines();
    let mut remaining = PROOF_LIMIT;
    for (id, kind) in
        std::iter::once((base, "commit")).chain(attributes.iter().map(|&id| (id, "blob")))
    {
        let line = headers.next().context("missing checkout object size")?;
        let size: u64 = line
            .strip_prefix(&format!("{id} {kind} "))
            .context("unexpected checkout object type")?
            .parse()?;
        remaining = remaining
            .checked_sub(size)
            .context("checkout metadata exceeds size limit")?;
    }
    ensure!(
        headers.next().is_none(),
        "unexpected extra checkout object size"
    );
    names.rewind()?;
    let raw = metadata_output(
        transfer_git(primary)
            .args(["cat-file", "--batch"])
            .stdin(names),
        PROOF_LIMIT,
        cancelled,
    )?;
    let mut input = raw.as_slice();
    let selected = object(&mut input, base, "commit", width)?;
    // A flattened listing can omit empty subtrees. Use it only when its
    // reconstructed root matches the tree in the authenticated commit.
    ensure!(
        selected.starts_with(format!("tree {}\n", index.tree).as_bytes()),
        "checkout paths differ from the selected commit"
    );
    visit(base, "commit", selected)?;
    for attr in attributes {
        let bytes = object(&mut input, attr, "blob", width)?;
        visit(attr, "blob", bytes)?;
    }
    ensure!(input.is_empty(), "unexpected extra checkout metadata");
    Ok(())
}

fn object<'a>(input: &mut &'a [u8], id: &str, kind: &str, width: usize) -> Result<&'a [u8]> {
    let end = input
        .iter()
        .position(|&b| b == b'\n')
        .context("missing Git object header")?;
    let header = std::str::from_utf8(&input[..end])?;
    let size: usize = header
        .strip_prefix(&format!("{id} {kind} "))
        .context("unexpected checkout metadata")?
        .parse()?;
    let body = &input[end + 1..];
    let data = body.get(..size).context("truncated checkout metadata")?;
    *input = body[size..]
        .strip_prefix(b"\n")
        .context("missing Git object terminator")?;
    let expected = ObjectId::from_hex(id.as_bytes(), width)?;
    ensure!(
        object_digest(kind, data, width)? == expected.0[..width],
        "checkout metadata hash mismatch"
    );
    Ok(data)
}

fn loose(kind: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    write!(encoder, "{kind} {}\0", bytes.len())?;
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}
