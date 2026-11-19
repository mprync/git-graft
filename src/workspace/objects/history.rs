//! Verify exactly one commit closure and install only self-contained packs.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs,
    io::{BufReader, Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Instant,
};

use super::{
    checked, clone_file, graph_count, open_regular, output_text, repository_path, run,
    sync_directory, sync_pack_file, transfer_git,
};
use crate::{git, recovery::rename_exclusive};

pub(super) mod cache;
mod mixed;
pub(crate) use cache::{
    DIR as HISTORY_CACHE_DIR, MARKER as HISTORY_CACHE_MARKER, history_cache_root,
    verify_history_cache_generation,
};

pub(super) struct Boundary<'a> {
    pub(super) file: &'a Path,
    pub(super) cache_key: &'a str,
}

/// Clone only an existing, verified exact generation into private staging.
pub(super) fn restore_exact(
    state: &Path,
    format: &str,
    oid: &str,
    stage: &Path,
) -> Result<Option<Vec<(PathBuf, PathBuf)>>> {
    let width = if format == "sha1" { 20 } else { 32 };
    Ok(cache::Cache::new(state, format, oid)
        .restore(stage, width)?
        .map(|packs| {
            packs
                .into_iter()
                .map(|pack| (pack.pack, pack.index))
                .collect()
        }))
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct ObjectId([u8; 32]);

impl ObjectId {
    fn from_hex(input: &[u8], width: usize) -> Result<Self> {
        ensure!(input.len() == width * 2, "invalid Git object ID length");
        let mut id = [0; 32];
        fn digit(byte: u8) -> Result<u8> {
            match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => bail!("invalid Git object ID"),
            }
        }
        for (bytes, value) in input.as_chunks::<2>().0.iter().zip(&mut id) {
            *value = digit(bytes[0])? * 16 + digit(bytes[1])?;
        }
        Ok(Self(id))
    }

    fn write_hex(&self, width: usize, buffer: &mut Vec<u8>) {
        const HEX: &[u8] = b"0123456789abcdef";
        for &byte in &self.0[..width] {
            buffer.extend([HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)]]);
        }
        buffer.push(b'\n');
    }
}

struct Pack {
    pack: PathBuf,
    index: PathBuf,
}

#[derive(Default)]
struct DonorReuse {
    whole: usize,
    filtered: usize,
    unchanged_bytes: u64,
}

impl Pack {
    fn at(dir: &Path, hash: &str, width: usize) -> Result<Self> {
        git::checked_oid(hash)?;
        ObjectId::from_hex(hash.as_bytes(), width)?;
        let pack = dir.join(format!("pack-{hash}.pack"));
        let index = pack.with_extension("idx");
        Ok(Self { pack, index })
    }
}

pub(super) fn bootstrap(
    primary: &Path,
    root: &Path,
    state: &Path,
    format: &str,
    oid: &str,
    boundary: Option<Boundary<'_>>,
) -> Result<Value> {
    let started = Instant::now();
    let destination = root.join(".git/objects/pack");
    ensure!(
        fs::symlink_metadata(&destination)?.is_dir()
            && fs::read_dir(&destination)?.next().is_none(),
        "workspace Git pack directory is occupied or has been replaced"
    );
    let width = if format == "sha1" { 20 } else { 32 };
    let cache_key = boundary.as_ref().map_or(oid, |shallow| shallow.cache_key);
    let cache = cache::Cache::new(state, format, cache_key);
    let mut timings = json!({});
    let mut cache_errors = json!({});
    let stage = cache::private_tempdir_in(&destination)?;
    let cached = stage.path().join("cached");
    fs::create_dir(&cached)?;
    let mut ancestor = None;
    let mut donor_reuse = DonorReuse::default();
    let (packs, source, shared_count) = match cache.restore(&cached, width) {
        Ok(Some(packs)) => {
            timings["cache_seconds"] = json!(started.elapsed().as_secs_f64());
            let count = packs.len();
            (packs, "cache", count)
        }
        miss => {
            if let Err(error) = miss {
                cache_errors["restore"] = json!(format!("{error:#}"));
            }
            let result = if boundary.is_none() && cache.has_root() {
                let assemble = Instant::now();
                let next = extend_generation(primary, root, stage.path(), &cache, oid, width);
                timings["ancestor_seconds"] = json!(assemble.elapsed().as_secs_f64());
                match next {
                    Ok(next) => next,
                    Err(error) => {
                        cache_errors["ancestor"] = json!(format!("{error:#}"));
                        None
                    }
                }
            } else {
                None
            };
            if let Some((packs, base, reused)) = result {
                ancestor = Some(base);
                (packs, "ancestor-cache", reused)
            } else {
                let (packs, source, reuse) = cold_transfer(
                    Source { primary, state },
                    root,
                    stage.path(),
                    oid,
                    width,
                    boundary,
                    &mut timings,
                )?;
                donor_reuse = reuse;
                (packs, source, donor_reuse.whole)
            }
        }
    };

    let cache_bytes = if source == "cache" {
        0
    } else {
        let publish = Instant::now();
        let size = match cache.publish(&packs) {
            Ok(size) => size,
            Err(error) => {
                cache_errors["publish"] = json!(format!("{error:#}"));
                0
            }
        };
        timings["cache_publish_seconds"] = json!(publish.elapsed().as_secs_f64());
        size
    };
    let (mut pack_bytes, mut index_bytes, mut reused_bytes) = (0, 0, 0);
    for (index, pack) in packs.iter().enumerate() {
        let written_pack = install(&pack.pack, &destination)?;
        let written_index = install(&pack.index, &destination)?;
        pack_bytes += written_pack;
        index_bytes += written_index;
        if index < shared_count {
            reused_bytes += written_pack + written_index;
        }
    }
    sync_directory(&destination).context("sync workspace Git pack directory")?;
    sync_directory(
        destination
            .parent()
            .context("missing workspace Git object directory")?,
    )?;
    let bytes = pack_bytes + index_bytes;
    timings["total_seconds"] = json!(started.elapsed().as_secs_f64());
    let mut receipt = json!({
        "method": if shared_count == 0 && donor_reuse.filtered == 0 { "local-pack" } else if cfg!(target_os = "macos") { "clonefile" } else { "reflink" },
        "source": source,
        "bytes": bytes,
        "pack_bytes": pack_bytes,
        "index_bytes": index_bytes,
        "shared_bytes": reused_bytes.max(cache_bytes),
        "cache_bytes": cache_bytes,
        "pack_count": packs.len(),
        "object_format": format,
        "timings": timings
    });
    if let Some(ancestor) = ancestor {
        receipt["ancestor"] = json!(ancestor);
        receipt["new_pack_count"] = json!(packs.len() - shared_count);
    } else if source != "cache" && shared_count != 0 {
        receipt["reused_primary_packs"] = json!(shared_count);
        receipt["reused_primary_bytes"] = json!(reused_bytes);
    }
    if donor_reuse.filtered > 0 {
        receipt["filtered_primary_packs"] = json!(donor_reuse.filtered);
        // Logical ranges retained at their original offsets. Physical savings
        // depend on allocation-block boundaries and are measured separately.
        receipt["filtered_unchanged_bytes"] = json!(donor_reuse.unchanged_bytes);
    }
    if !cache_errors
        .as_object()
        .is_none_or(|errors| errors.is_empty())
    {
        receipt["cache_errors"] = cache_errors;
    }
    Ok(receipt)
}

#[derive(Clone, Copy)]
struct Source<'a> {
    primary: &'a Path,
    state: &'a Path,
}

fn cold_transfer(
    source: Source<'_>,
    root: &Path,
    stage: &Path,
    oid: &str,
    width: usize,
    boundary: Option<Boundary<'_>>,
    timings: &mut Value,
) -> Result<(Vec<Pack>, &'static str, DonorReuse)> {
    let shallow = boundary.as_ref().map(|boundary| boundary.file);
    let mut attempt = |stage: &Path, accelerated: bool| -> Result<_> {
        let reachability = Instant::now();
        let mut discovery = transfer_git(source.primary);
        if accelerated {
            discovery.args(["-c", "core.commitGraph=true"]);
        }
        let expected = reachable(discovery, oid, width, None, shallow, accelerated)?;
        timings["reachability_seconds"] = json!(reachability.elapsed().as_secs_f64());
        let transfer = Instant::now();
        let (packs, source, objects, shared) =
            stage_cold_packs(source, root, stage, &expected, width)?;
        timings["transfer_seconds"] = json!(transfer.elapsed().as_secs_f64());
        let verify = Instant::now();
        verify_staged_closure(root, &objects, &packs, oid, shallow)?;
        timings["verify_seconds"] = json!(verify.elapsed().as_secs_f64());
        Ok((packs, source, shared))
    };
    if shallow.is_none() {
        let accelerated = Instant::now();
        match attempt(&stage.join("accelerated"), true) {
            Ok(packs) => return Ok(packs),
            Err(error) => {
                // Discovery indexes are hints. A damaged hint or a concurrent
                // repack can invalidate the candidate. Retry from raw objects.
                let failed = accelerated.elapsed().as_secs_f64();
                let result = attempt(&stage.join("plain"), false);
                timings["accelerated_failure_seconds"] = json!(failed);
                return result
                    .with_context(|| format!("accelerated Git discovery failed: {error:#}"));
            }
        }
    }
    attempt(stage, false)
}

fn stage_cold_packs(
    source: Source<'_>,
    root: &Path,
    stage: &Path,
    expected: &[ObjectId],
    width: usize,
) -> Result<(Vec<Pack>, &'static str, PathBuf, DonorReuse)> {
    // Separate staging directories ensure a partial failed attempt cannot
    // interfere with the selective export.
    let donor = stage.join("donor");
    let exported = stage.join("exported");
    fs::create_dir_all(donor.join("pack"))?;
    match pack_with_donors(source, root, &donor.join("pack"), expected, width) {
        Ok((packs, reused)) => {
            let source = if reused.whole == packs.len() {
                "primary-pack"
            } else if reused.whole + reused.filtered == packs.len() {
                "filtered-primary"
            } else {
                "local-export"
            };
            Ok((packs, source, donor, reused))
        }
        Err(_) => {
            fs::create_dir_all(exported.join("pack"))?;
            let packs = export(
                source.primary,
                root,
                &exported.join("pack"),
                expected,
                width,
            )?;
            Ok((packs, "local-export", exported, DonorReuse::default()))
        }
    }
}

fn verify_staged_closure(
    root: &Path,
    objects: &Path,
    packs: &[Pack],
    oid: &str,
    shallow: Option<&Path>,
) -> Result<()> {
    let pack_dir = objects.join("pack");
    ensure!(
        fs::read_dir(objects)?.count() == 1
            && fs::symlink_metadata(&pack_dir)?.is_dir()
            && fs::read_dir(&pack_dir)?.count() == packs.len() * 2,
        "unexpected entries in the staged Git object directory"
    );
    let packed = packs.iter().try_fold(0u64, |total, pack| {
        ensure!(
            pack.pack.parent() == Some(pack_dir.as_path())
                && pack.index.parent() == Some(pack_dir.as_path()),
            "verified Git pack is outside staging"
        );
        let count = index_count(&mut open_regular(&pack.index)?)?;
        total
            .checked_add(count as u64)
            .context("too many packed Git objects")
    })?;
    let mut isolated = transfer_git(root);
    isolated.env("GIT_OBJECT_DIRECTORY", objects);
    set_shallow(&mut isolated, shallow);
    // Git already rehashed each staged pack and checked its index. Every
    // reachable ID must exist here, and the walk counts each ID once. Matching
    // the sum of physical index entries rules out extra or duplicate objects
    // even when the source traversal supplied an incorrect set.
    ensure!(
        graph_count(isolated, oid)? == packed,
        "Git pack set does not exactly cover the requested history"
    );
    Ok(())
}

fn install(file: &Path, destination: &Path) -> Result<u64> {
    let name = file.file_name().context("missing Git pack filename")?;
    let bytes = fs::metadata(file)?.len();
    sync_pack_file(file)?;
    rename_exclusive(file, &destination.join(name))?;
    Ok(bytes)
}

fn set_shallow(command: &mut Command, boundary: Option<&Path>) {
    // Full history must remain full even if a shallow boundary appears while
    // spawning. Shallow initialization supplies its own pinned boundary.
    command.env(
        "GIT_SHALLOW_FILE",
        boundary.unwrap_or_else(|| Path::new("")),
    );
}

fn reachable(
    mut command: Command,
    oid: &str,
    width: usize,
    exclude: Option<&str>,
    shallow: Option<&Path>,
    use_bitmap: bool,
) -> Result<Vec<ObjectId>> {
    set_shallow(&mut command, shallow);
    command.args([
        "rev-list",
        "--objects",
        "--no-object-names",
        "--missing=error",
    ]);
    if use_bitmap {
        command.arg("--use-bitmap-index");
    }
    command.arg(oid);
    if let Some(base) = exclude {
        command.arg(format!("^{base}"));
    }
    command.arg("--");
    let list = checked(command).context("walk the requested Git history")?;
    let mut ids = Vec::with_capacity(list.len() / (2 * width + 1));
    for line in list.split_inclusive(|&byte| byte == b'\n') {
        let hex = line
            .strip_suffix(b"\n")
            .context("invalid Git object list")?;
        ids.push(ObjectId::from_hex(
            hex.strip_suffix(b"\r").unwrap_or(hex),
            width,
        )?);
    }
    ensure!(
        ids.contains(&ObjectId::from_hex(oid.as_bytes(), width)?),
        "Git object list does not include the requested commit"
    );
    Ok(ids)
}

type AncestorTransfer = (Vec<Pack>, String, usize);

fn extend_generation(
    primary: &Path,
    root: &Path,
    stage: &Path,
    cache: &cache::Cache<'_>,
    oid: &str,
    width: usize,
) -> Result<Option<AncestorTransfer>> {
    let mut first_parent = transfer_git(primary);
    set_shallow(&mut first_parent, None);
    first_parent.args(["rev-list", "--first-parent", "--max-count=17", oid, "--"]);
    let lineage = checked(first_parent).context("inspect nearby Git ancestors")?;
    for ancestor in output_text(&lineage)?.lines().skip(1) {
        ObjectId::from_hex(ancestor.as_bytes(), width)?;
        let cached = cache.for_base(ancestor);
        if !cached.has_entry() {
            continue;
        }
        let objects = stage.join("ancestor");
        let pack_dir = objects.join("pack");
        fs::create_dir_all(&pack_dir)?;
        let Some(mut packs) = cached.restore(&pack_dir, width)? else {
            return Ok(None);
        };
        const MAX_GENERATION_PACKS: usize = 16;
        if packs.len() >= MAX_GENERATION_PACKS {
            return Ok(None);
        }
        let reused = packs.len();
        let delta = reachable(
            transfer_git(primary),
            oid,
            width,
            Some(ancestor),
            None,
            false,
        )?;
        if delta.len() >= 85_000 {
            return Ok(None);
        }

        // A historical blob can reappear after the cached tip deleted it.
        // The negative revision may still list it. Omit objects already
        // proved and retained by that generation.
        let mut new: HashSet<_> = delta.iter().copied().collect();
        for pack in &packs {
            index_ids(&pack.index, width, |id| {
                new.remove(&id);
                Ok(())
            })?;
        }
        let delta: Vec<_> = delta.into_iter().filter(|id| new.contains(id)).collect();
        ensure!(
            !delta.is_empty(),
            "new Git tip is missing from its history delta"
        );
        let added = export(primary, root, &pack_dir, &delta, width)?;
        verify_membership(&added, &delta, width)?;
        packs.extend(added);

        // Prove the ancestor relationship and the delta against the staged
        // bytes. The ancestor pack set already proves its own full closure.
        let mut ancestry = transfer_git(root);
        ancestry.env("GIT_OBJECT_DIRECTORY", &objects);
        set_shallow(&mut ancestry, None);
        ancestry.args(["merge-base", "--is-ancestor", ancestor, oid]);
        checked(ancestry).context("verify cached base is a Git ancestor")?;
        let mut combined = transfer_git(root);
        combined.env("GIT_OBJECT_DIRECTORY", &objects);
        for id in reachable(combined, oid, width, Some(ancestor), None, false)? {
            new.remove(&id);
        }
        ensure!(
            new.is_empty(),
            "Git pack delta contains objects outside the requested history"
        );
        return Ok(Some((packs, ancestor.into(), reused)));
    }
    Ok(None)
}

fn index_count(index: &mut impl Read) -> Result<usize> {
    let mut header = [0; 1032];
    index.read_exact(&mut header)?;
    ensure!(
        &header[..8] == b"\xfftOc\0\0\0\x02",
        "unsupported Git pack index version"
    );
    Ok(u32::from_be_bytes(header[1028..].try_into()?) as usize)
}

fn index_ids(index: &Path, width: usize, visit: impl FnMut(ObjectId) -> Result<()>) -> Result<()> {
    read_index_ids(&mut BufReader::new(open_regular(index)?), width, visit)
}

fn read_index_ids(
    index: &mut impl Read,
    width: usize,
    mut visit: impl FnMut(ObjectId) -> Result<()>,
) -> Result<()> {
    for _ in 0..index_count(index)? {
        let mut id = ObjectId([0; 32]);
        index.read_exact(&mut id.0[..width])?;
        visit(id)?;
    }
    Ok(())
}

fn verify_membership(packs: &[Pack], expected: &[ObjectId], width: usize) -> Result<()> {
    let mut remaining = HashSet::new();
    remaining.try_reserve(expected.len())?;
    remaining.extend(expected.iter().copied());
    ensure!(
        remaining.len() == expected.len(),
        "Git returned duplicate reachable objects"
    );
    for pack in packs {
        index_ids(&pack.index, width, |id| {
            ensure!(
                remaining.remove(&id),
                "Git pack contains an object outside the requested history or a duplicate"
            );
            Ok(())
        })?;
    }
    ensure!(
        remaining.is_empty(),
        "Git pack is missing reachable objects"
    );
    Ok(())
}

fn pack_with_donors(
    source: Source<'_>,
    root: &Path,
    stage: &Path,
    expected: &[ObjectId],
    width: usize,
) -> Result<(Vec<Pack>, DonorReuse)> {
    ensure!(
        cfg!(any(target_os = "macos", target_os = "linux")),
        "CoW unavailable"
    );
    let objects = repository_path(source.primary, "objects")?;
    let dir = objects.join("pack");
    ensure!(
        fs::symlink_metadata(&objects)?.is_dir() && fs::symlink_metadata(&dir)?.is_dir(),
        "invalid primary pack directory"
    );
    let mut candidates = fs::read_dir(&dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    candidates.retain(|path| path.extension().is_some_and(|ext| ext == "pack"));
    candidates.sort_unstable();
    let mut remaining: HashSet<_> = expected.iter().copied().collect();
    let mut packs = Vec::new();
    let mut candidates_to_filter = Vec::new();
    for path in candidates {
        if remaining.is_empty() {
            break;
        }
        let hash = path
            .file_stem()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("pack-"))
            .context("invalid primary Git pack filename")?;
        let donor = Pack::at(&dir, hash, width)?;
        let pack = open_regular(&donor.pack)?;
        let index = open_regular(&donor.index)?;
        let mut ids = Vec::new();
        let eligible = read_index_ids(&mut BufReader::new(&index), width, |id| {
            ensure!(
                remaining.contains(&id),
                "primary pack contains an unrelated or already covered object"
            );
            ids.push(id);
            Ok(())
        });
        if eligible.is_err() || ids.is_empty() {
            if pack.metadata()?.len() >= mixed::MIN_PACK_BYTES {
                candidates_to_filter.push(donor);
            }
            continue;
        }
        let staged = Pack::at(stage, hash, width)?;
        for (file, destination) in [(&pack, &staged.pack), (&index, &staged.index)] {
            // A local clone may make donor files owner-writable. Seal the
            // independent copies and verify their actual bytes below.
            clone_file(file, destination)?;
            let mut permissions = fs::metadata(destination)?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(destination, permissions)?;
        }
        for id in ids {
            ensure!(
                remaining.remove(&id),
                "primary pack contains a duplicate object"
            );
        }
        packs.push(staged);
    }
    let mut reuse = DonorReuse {
        whole: packs.len(),
        ..Default::default()
    };
    for donor in candidates_to_filter {
        if remaining.is_empty() {
            break;
        }
        // A crash before filtering finishes must never leave mixed primary
        // history in the task or its archive. Stage under the primary registry.
        let scratch = cache::private_tempdir_in(source.state)?;
        if let Ok(filtered) = mixed::filter(root, scratch.path(), &donor, &remaining, width) {
            let pack = Pack::at(stage, &filtered.hash, width)?;
            rename_exclusive(&filtered.pack.pack, &pack.pack)?;
            rename_exclusive(&filtered.pack.index, &pack.index)?;
            for id in filtered.ids {
                ensure!(remaining.remove(&id), "duplicate filtered Git object");
            }
            reuse.filtered += 1;
            reuse.unchanged_bytes += filtered.unchanged_bytes;
            packs.push(pack);
        }
    }
    ensure!(
        !packs.is_empty(),
        "no complete primary pack belongs to the requested history"
    );
    let residual: Vec<_> = expected
        .iter()
        .copied()
        .filter(|id| remaining.contains(id))
        .collect();
    // Source indexes describe which packs may be reused. Independently
    // verify their packed contents and index mappings, in parallel with any
    // residual export. A failed verification discards this whole staging set.
    let new = thread::scope(|scope| {
        let verify = scope.spawn(|| {
            for pack in &packs[..reuse.whole] {
                let mut verify = transfer_git(root);
                verify.args(["index-pack", "--verify"]).arg(&pack.pack);
                checked(verify).context("verify cloned primary Git pack")?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let new = if residual.is_empty() {
            Ok(Vec::new())
        } else {
            export(source.primary, root, stage, &residual, width)
        };
        verify
            .join()
            .map_err(|_| anyhow::anyhow!("Git pack verification worker panicked"))??;
        new
    })?;
    packs.extend(new);
    Ok((packs, reuse))
}

fn export(
    primary: &Path,
    root: &Path,
    stage: &Path,
    expected: &[ObjectId],
    width: usize,
) -> Result<Vec<Pack>> {
    // Native index-pack does its initial inflate pass serially. Bound
    // parallelism and leave smaller histories in a single compact pack.
    let workers = thread::available_parallelism().map_or(1, usize::from);
    let shards = expected.len().div_ceil(100_000).clamp(1, workers.min(4));
    let chunks = if shards == 1 {
        vec![expected.to_vec()]
    } else {
        ordered_chunks(primary, expected, width, shards).unwrap_or_else(|_| {
            expected
                .chunks(expected.len().div_ceil(shards))
                .map(<[ObjectId]>::to_vec)
                .collect()
        })
    };
    let parallel = chunks.len() > 1;
    thread::scope(|scope| {
        let jobs: Vec<_> = chunks
            .iter()
            .enumerate()
            .map(|(i, chunk)| {
                scope.spawn(move || export_pack(primary, root, stage, width, i, chunk, parallel))
            })
            .collect();
        jobs.into_iter()
            .map(|job| {
                job.join()
                    .map_err(|_| anyhow::anyhow!("Git pack worker panicked"))?
            })
            .collect::<Result<Vec<_>>>()
    })
}

fn ordered_chunks(
    primary: &Path,
    expected: &[ObjectId],
    width: usize,
    shards: usize,
) -> Result<Vec<Vec<ObjectId>>> {
    let rows = run(
        primary,
        &[
            "cat-file",
            "--batch-all-objects",
            "--unordered",
            "--batch-check=%(objectname) %(objectsize:disk)",
        ],
    )?;
    let mut remaining: HashSet<_> = expected.iter().copied().collect();
    let mut ordered = Vec::with_capacity(expected.len());
    let mut bytes = 0u64;
    for row in rows
        .split(|&byte| byte == b'\n')
        .filter(|row| !row.is_empty())
    {
        let (id, size) = row
            .split_at_checked(width * 2)
            .context("invalid Git object size report")?;
        let id = ObjectId::from_hex(id, width)?;
        if remaining.remove(&id) {
            let size: u64 = output_text(size)?.trim_ascii_start().parse()?;
            bytes = bytes
                .checked_add(size)
                .context("Git object size overflow")?;
            ordered.push((id, size));
        }
    }
    ensure!(
        remaining.is_empty(),
        "Git object inventory changed during export"
    );
    let mut chunks = Vec::with_capacity(shards);
    let stride = bytes.div_ceil(shards as u64);
    let mut chunk = Vec::new();
    let mut seen = 0u64;
    for (id, size) in ordered {
        chunk.push(id);
        seen += size;
        if chunks.len() + 1 < shards && seen >= (chunks.len() as u64 + 1).saturating_mul(stride) {
            chunks.push(std::mem::take(&mut chunk));
        }
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    Ok(chunks)
}

fn export_pack(
    primary: &Path,
    root: &Path,
    stage: &Path,
    width: usize,
    shard: usize,
    objects: &[ObjectId],
    parallel: bool,
) -> Result<Pack> {
    let mut input = tempfile::tempfile_in(stage)?;
    let mut text = Vec::with_capacity(objects.len() * (width * 2 + 1));
    for id in objects {
        id.write_hex(width, &mut text);
    }
    input.write_all(&text)?;
    input.rewind()?;
    let mut errors = tempfile::tempfile_in(stage)?;
    let mut export = transfer_git(primary);
    export.args(["pack-objects", "--quiet", "--stdout", "--delta-base-offset"]);
    if parallel {
        export.arg("--window=0");
    }
    let mut export = export
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(errors.try_clone()?)
        .spawn()
        .context("start selective Git pack export")?;
    let incoming = stage.join(format!("shard-{shard}.pack"));
    let mut index = transfer_git(root);
    index.args(["index-pack", "--stdin", "--index-version=2"]);
    if parallel {
        index.arg("--threads=2");
    }
    index
        .arg(&incoming)
        .stdin(export.stdout.take().context("open Git pack output")?);
    let output = checked(index).context("index selective Git pack");
    if !export.wait()?.success() {
        errors.rewind()?;
        let mut detail = Vec::new();
        errors.take(8192).read_to_end(&mut detail)?;
        let detail = String::from_utf8_lossy(&detail);
        if !detail.trim().is_empty() {
            bail!("export selective Git pack: {}", detail.trim());
        }
        output?;
        bail!("export selective Git pack failed");
    }
    let output = output?;
    let hash = output_text(&output)?
        .strip_prefix("pack\t")
        .context("invalid Git index-pack receipt")?;
    let pack = Pack::at(stage, hash, width)?;
    rename_exclusive(&incoming, &pack.pack)?;
    rename_exclusive(&incoming.with_extension("idx"), &pack.index)?;
    Ok(pack)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_closure_rejects_bad_discovery_and_duplicate_pack_entries() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let root = temporary.path().join("workspace");
        for repo in [&source, &root] {
            fs::create_dir(repo)?;
            run(repo, &["init", "-q", "--template=", "--object-format=sha1"])?;
        }
        run(&source, &["config", "user.name", "Test"])?;
        run(&source, &["config", "user.email", "test@example.test"])?;
        fs::write(source.join("tracked"), b"reachable")?;
        run(&source, &["add", "tracked"])?;
        run(&source, &["commit", "-qm", "base"])?;
        let tip = run(&source, &["rev-parse", "HEAD"])?;
        let tip = output_text(&tip)?;
        let expected = reachable(transfer_git(&source), tip, 20, None, None, false)?;
        let blob = run(&source, &["rev-parse", "HEAD:tracked"])?;
        let blob = ObjectId::from_hex(output_text(&blob)?.as_bytes(), 20)?;
        fs::write(source.join("private"), b"unreachable")?;
        let private = run(&source, &["hash-object", "-w", "private"])?;
        let private = ObjectId::from_hex(output_text(&private)?.as_bytes(), 20)?;

        let mut broadened = expected.clone();
        broadened.push(private);
        let mut substituted = broadened.clone();
        substituted.retain(|&id| id != blob);
        assert_eq!(substituted.len(), expected.len());

        for (name, input) in [
            ("exact", vec![expected.clone()]),
            ("broadened", vec![broadened]),
            ("substituted", vec![substituted]),
            ("duplicate", vec![expected, vec![blob]]),
        ] {
            let objects = temporary.path().join(name);
            let packs = objects.join("pack");
            fs::create_dir_all(&packs)?;
            let mut staged = Vec::new();
            for ids in input {
                staged.extend(export(&source, &root, &packs, &ids, 20)?);
            }
            let result = verify_staged_closure(&root, &objects, &staged, tip, None);
            assert_eq!(result.is_ok(), name == "exact", "{name}: {result:?}");
        }
        Ok(())
    }
}
