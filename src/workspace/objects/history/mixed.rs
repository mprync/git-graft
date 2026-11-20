//! Preserve CoW extents when a Git pack contains a small amount of unrelated
//! history. Git indexes the finished pack from its actual compressed bytes;
//! the donor index only suggests which records to keep.

use super::{
    ObjectId, Pack, cache, checked, clone_file, index_count, index_ids, open_regular, output_text,
    rename_exclusive, transfer_git,
};
use anyhow::{Context, Result, bail, ensure};
use sha2::Digest;
use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    ops::Range,
    path::Path,
    process::Stdio,
};

#[cfg(target_os = "macos")]
mod preview;
mod scratch;
#[cfg(target_os = "macos")]
pub(super) use preview::Preview;

pub(super) const MIN_PACK_BYTES: u64 = 1 << 20;
const MAX_PACK_BYTES: u64 = 256 << 20;
const MAX_INDEX_BYTES: u64 = 40 << 20;
const MAX_WORKING_BYTES: u64 = 192 << 20;
// A large run of empty blocks would make subsequent reads of the preceding
// object expensive. Let Git export these layouts in its usual compact form.
const MAX_ALIGNMENT_BYTES: usize = 64 << 10;

pub(super) struct Prepared {
    #[cfg(target_os = "macos")]
    pub(super) preview: Option<Preview>,
    file: File,
    scratch: tempfile::TempDir,
    pub(super) ids: Vec<ObjectId>,
    unchanged_bytes: u64,
}

fn check_budget(pack: u64, index: u64, width: usize) -> Result<()> {
    ensure!(
        (MIN_PACK_BYTES..=MAX_PACK_BYTES).contains(&pack) && index <= MAX_INDEX_BYTES,
        "unsupported donor size"
    );
    let max_records = index / (width as u64 + 8);
    // The offset/ID table, selection, occupancy set (including spare hash
    // buckets) and gap list together require less than 128 bytes per record.
    ensure!(
        pack + index + max_records * 128 <= MAX_WORKING_BYTES,
        "pack and index exceed the filter working memory budget"
    );
    Ok(())
}

fn bounded_bytes(mut file: impl Read, size: u64) -> Result<Vec<u8>> {
    let size = usize::try_from(size)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size)?;
    bytes.resize(size, 0);
    file.read_exact(&mut bytes)?;
    ensure!(file.read(&mut [0])? == 0, "Git file changed size");
    Ok(bytes)
}

type OffsetAndIndexRow = (usize, usize);
// v2 indexes put sorted IDs, CRCs and 32-bit pack offsets after the fanout
// table. The size limit makes large-offset entries unnecessary here.
fn offsets<'a>(
    index: &'a [u8],
    width: usize,
    wanted: &HashSet<ObjectId>,
    allow_prefix: bool,
) -> Result<(&'a [u8], Vec<OffsetAndIndexRow>)> {
    let mut table = index;
    let count = index_count(&mut table)?;
    ensure!(
        count > 0
            && table.len()
                == count
                    .checked_mul(width + 8)
                    .and_then(|n| n.checked_add(width * 2))
                    .context("invalid pack index size")?,
        "unsupported pack index layout"
    );
    let (ids, rest) = table.split_at(count * width);
    let starts = &rest[count * 4..count * 8];
    // Most primary history lies outside a single-tip selection. Reject an
    // ineligible first record cheaply unless inspecting a nearby tip pack.
    let first = starts
        .as_chunks::<4>()
        .0
        .iter()
        .position(|start| *start == 12u32.to_be_bytes())
        .context("missing first pack record")?;
    ensure!(
        allow_prefix
            || wanted.contains(&object_id(&ids[first * width..(first + 1) * width], width)),
        "first pack record is outside the selected history"
    );
    let mut records = Vec::new();
    records.try_reserve_exact(count)?;
    for (row, start) in starts.as_chunks::<4>().0.iter().enumerate() {
        records.push((u32::from_be_bytes(*start) as usize, row));
    }
    records.sort_unstable_by_key(|r| r.0);
    Ok((ids, records))
}

fn object_id(row: &[u8], width: usize) -> ObjectId {
    let mut id = ObjectId([0; 32]);
    id.0[..width].copy_from_slice(row);
    id
}

// Return the zlib offset for a full object or a delta whose earlier base
// stays at its original offset. Git exports other delta chains separately.
fn stream_start(record: &[u8], start: usize, kept: &HashSet<usize>) -> Result<Option<usize>> {
    let mut header = record.iter().copied();
    let first = header.next().context("empty pack record")?;
    let mut byte = first;
    while byte & 0x80 != 0 {
        byte = header.next().context("invalid pack record size")?;
    }
    match (first >> 4) & 7 {
        1..=4 => {}
        6 => {
            byte = header.next().context("missing delta offset")?;
            let mut distance = usize::from(byte & 127);
            while byte & 128 != 0 {
                byte = header.next().context("incomplete delta offset")?;
                distance = distance
                    .checked_add(1)
                    .and_then(|n| n.checked_mul(128))
                    .and_then(|n| n.checked_add(usize::from(byte & 127)))
                    .context("invalid delta offset")?;
            }
            if !start
                .checked_sub(distance)
                .is_some_and(|base| kept.contains(&base))
            {
                return Ok(None);
            }
        }
        7 => return Ok(None),
        _ => bail!("unsupported pack record type"),
    }
    ensure!(header.len() >= 6, "truncated zlib stream");
    Ok(Some(start + record.len() - header.len()))
}

// An empty non-final DEFLATE stored block occupies five bytes. Preceding it
// with an empty fixed-Huffman block takes six bytes total. Both end on a byte
// boundary and produce no data. Inserting them after the zlib header retains
// the original compressed payload, checksum and Git object identity.
fn clear_gap(bytes: &mut [u8]) -> Result<()> {
    const STORED: &[u8; 5] = &[0, 0, 0, 255, 255];
    const FIXED_STORED: &[u8; 6] = &[2, 0, 0, 0, 255, 255];
    let sixes = bytes.len() % 5;
    ensure!(sixes * 6 <= bytes.len(), "pack gap is too short to align");
    let (six, five) = bytes.split_at_mut(sixes * 6);
    for block in six.as_chunks_mut::<6>().0 {
        *block = *FIXED_STORED;
    }
    for block in five.as_chunks_mut::<5>().0 {
        *block = *STORED;
    }
    Ok(())
}

struct Selection {
    #[cfg(target_os = "macos")]
    offsets: Vec<u32>,
    ids: Vec<ObjectId>,
    first_header: Option<Range<usize>>,
    // Compressed bytes to move right, followed by the end of the removed run.
    gaps: Vec<(Range<usize>, usize)>,
    end: usize,
}

fn select(
    bytes: &[u8],
    table: &[u8],
    records: &[OffsetAndIndexRow],
    width: usize,
    wanted: &HashSet<ObjectId>,
    allow_prefix: bool,
    early_walk: bool,
) -> Result<Selection> {
    #[cfg(not(target_os = "macos"))]
    let _ = early_walk;
    ensure!(
        bytes.len() >= 12 + width
            && &bytes[..4] == b"PACK"
            && matches!(u32::from_be_bytes(bytes[4..8].try_into()?), 2 | 3)
            && u32::from_be_bytes(bytes[8..12].try_into()?) as usize == records.len()
            && records[0].0 == 12,
        "pack and index disagree"
    );
    let payload_end = bytes.len() - width;
    let mut kept = HashSet::new();
    kept.try_reserve(records.len())?;
    let mut selected = Vec::new();
    selected.try_reserve_exact(records.len())?;
    selected.resize(records.len(), false);
    let mut count = 0;
    let mut gaps = Vec::new();
    gaps.try_reserve_exact(records.len() / 2)?;
    let mut previous = 12..12;
    let mut rewrite = 0;
    let mut first_header = None;
    for (i, &(start, row)) in records.iter().enumerate() {
        let end = records.get(i + 1).map_or(payload_end, |next| next.0);
        ensure!(
            start < end && end <= payload_end,
            "invalid pack record bounds"
        );
        if !wanted.contains(&object_id(&table[row * width..(row + 1) * width], width)) {
            continue;
        }
        let Some(stream) = stream_start(&bytes[start..end], start, &kept)? else {
            continue;
        };
        if previous.end != start {
            ensure!(
                start - previous.end <= MAX_ALIGNMENT_BYTES,
                "mixed pack requires excessive alignment"
            );
            if count == 0 {
                ensure!(
                    allow_prefix,
                    "first pack record is outside the selected history"
                );
                // Move only the first surviving full object's header to byte
                // 12. Its compressed payload stays in place behind padding.
                let end = stream + 2;
                rewrite += end - 12;
                first_header = Some(start..end);
            } else {
                rewrite += start - previous.start;
                gaps.push((previous.clone(), start));
            }
        }
        // OFS_DELTA addresses its base by offset. Export deltas referring
        // to the moved first header separately, including their dependants.
        if count > 0 || start == 12 {
            kept.insert(start);
        }
        selected[row] = true;
        count += 1;
        previous = stream + 2..end;
    }
    ensure!(
        count > 0 && count < records.len() && rewrite < previous.end / 4,
        "mixed pack has insufficient reusable data"
    );
    // Git writes index rows in ID order. Keep the selected rows in that
    // same order so the rebuilt index can be compared in one pass.
    let mut ids = Vec::new();
    ids.try_reserve_exact(count)?;
    #[cfg(target_os = "macos")]
    let offsets = if early_walk && !allow_prefix && count >= 100_000 {
        let mut by_row = vec![0u32; records.len()];
        for &(offset, row) in records {
            by_row[row] = u32::try_from(offset)?;
        }
        by_row
            .into_iter()
            .zip(&selected)
            .filter_map(|(offset, keep)| keep.then_some(offset))
            .collect()
    } else {
        Vec::new()
    };
    for (row, keep) in table.chunks_exact(width).zip(selected) {
        if keep {
            ids.push(object_id(row, width));
        }
    }
    Ok(Selection {
        #[cfg(target_os = "macos")]
        offsets,
        ids,
        first_header,
        gaps,
        end: previous.end,
    })
}

pub(super) fn prepare(
    state: &Path,
    donor: &Pack,
    wanted: &HashSet<ObjectId>,
    width: usize,
    allow_prefix: bool,
    early_walk: bool,
) -> Result<Prepared> {
    let input = open_regular(&donor.pack)?;
    let input_size = input.metadata()?.len();
    let index = open_regular(&donor.index)?;
    let index_size = index.metadata()?.len();
    check_budget(input_size, index_size, width)?;
    let index = bounded_bytes(index, index_size)?;
    let (table, records) = offsets(&index, width, wanted, allow_prefix)?;
    let scratch = cache::private_tempdir_in(state)?;
    let (mut file, reader) = scratch::create(&input, &donor.pack, scratch.path(), early_walk)?;
    #[cfg(not(target_os = "macos"))]
    let _ = reader;
    let mut bytes = bounded_bytes(&file, input_size)?;
    let plan = select(
        &bytes,
        table,
        &records,
        width,
        wanted,
        allow_prefix,
        early_walk,
    )?;
    drop(records);
    drop(index);

    // Edit the unlinked clone. Only the sanitized bytes get an output name.
    let count = u32::try_from(plan.ids.len())?.to_be_bytes();
    bytes[8..12].copy_from_slice(&count);
    file.seek(SeekFrom::Start(8))?;
    file.write_all(&count)?;
    let mut unchanged_bytes = plan.end - 4;
    if let Some(header) = plan.first_header {
        let end = header.end;
        let gap = 12 + header.len()..end;
        bytes.copy_within(header, 12);
        clear_gap(&mut bytes[gap])?;
        file.seek(SeekFrom::Start(12))?;
        file.write_all(&bytes[12..end])?;
        unchanged_bytes -= end - 12;
    }
    for (previous, gap_end) in plan.gaps {
        let length = gap_end - previous.end;
        let start = previous.start;
        bytes.copy_within(previous, start + length);
        clear_gap(&mut bytes[start..start + length])?;
        file.seek(SeekFrom::Start(start as u64))?;
        file.write_all(&bytes[start..gap_end])?;
        unchanged_bytes -= gap_end - start;
    }
    let payload = &bytes[..plan.end];
    let trailer = trailer_digest(width, payload)?;
    file.seek(SeekFrom::Start(plan.end as u64))?;
    file.write_all(&trailer)?;
    file.set_len((plan.end + width) as u64)?;
    #[cfg(target_os = "macos")]
    let preview = reader
        .filter(|_| !plan.offsets.is_empty())
        .and_then(|input| Preview::new(input, &plan.ids, plan.offsets, payload, &trailer).ok());
    drop(bytes);
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;

    Ok(Prepared {
        #[cfg(target_os = "macos")]
        preview,
        file,
        scratch,
        ids: plan.ids,
        unchanged_bytes: unchanged_bytes as u64,
    })
}

/// Git's pack or index trailer: the object-format hash of the preceding bytes.
fn trailer_digest(width: usize, bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(match width {
        20 => sha1::Sha1::digest(bytes).to_vec(),
        32 => sha2::Sha256::digest(bytes).to_vec(),
        _ => bail!("unsupported Git object format"),
    })
}

pub(super) fn default_delta_cache(root: &Path) -> bool {
    transfer_git(root)
        .args(["config", "--get", "core.deltaBaseCacheLimit"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.code() == Some(1))
}

impl Prepared {
    fn index_threads(&self, root: &Path, command: &mut std::process::Command) -> &'static str {
        // Git multiplies its delta cache limit by the worker count. Keep the
        // four-worker default budget and respect an explicitly configured limit.
        if self.ids.len() >= 100_000
            && std::thread::available_parallelism().is_ok_and(|n| n.get() >= 8)
            && default_delta_cache(root)
        {
            command.args(["-c", "core.deltaBaseCacheLimit=48m"]);
            "--threads=8"
        } else {
            "--threads=4"
        }
    }

    pub(super) fn verify(mut self, root: &Path, stage: &Path, width: usize) -> Result<(Pack, u64)> {
        // Git verifies the anonymous pack as a regular file and writes only the
        // index in private staging. Name the pack after its membership is proven.
        let index = self.scratch.path().join("filtered.idx");
        self.file.rewind()?;
        let mut command = transfer_git(root);
        let threads = self.index_threads(root, &mut command);
        command
            .args(["index-pack", "--index-version=2", threads, "-o"])
            .arg(&index)
            .arg("/dev/fd/0")
            .stdin(Stdio::from(self.file.try_clone()?));
        let output = checked(command).context("index filtered Git pack")?;
        let mut expected = self.ids.iter();
        index_ids(&index, width, |id| {
            ensure!(
                expected.next() == Some(&id),
                "filtered Git pack contains an unexpected object"
            );
            Ok(())
        })?;
        ensure!(
            expected.next().is_none(),
            "filtered Git pack is missing objects"
        );
        let pack = Pack::at(stage, output_text(&output)?, width)?;
        clone_file(&self.file, &pack.pack)?;
        rename_exclusive(&index, &pack.index)?;
        Ok((pack, self.unchanged_bytes))
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use crate::workspace::objects::run;
    use std::fs;

    #[test]
    fn relocated_base_and_its_dependents_are_not_delta_targets() -> Result<()> {
        // Only the compressed-stream offsets matter to the selection pass.
        // The small OFS_DELTA records point to the moved blob and each other.
        let records = [(12, 0), (52, 1), (100, 2), (120, 3), (140, 4)];
        let mut pack = vec![0; 4096];
        pack[..12].copy_from_slice(b"PACK\0\0\0\x02\0\0\0\x05");
        pack[52] = 0x30;
        pack[100..102].copy_from_slice(&[0x60, 48]);
        pack[120..122].copy_from_slice(&[0x60, 20]);
        pack[140] = 0x30;
        let table: Vec<_> = (0..5).flat_map(|i| [i; 20]).collect();
        let wanted = (1..5).map(|id| object_id(&[id; 20], 20)).collect();
        let selection = select(&pack, &table, &records, 20, &wanted, true, false)?;
        assert_eq!(selection.first_header, Some(52..55));
        assert_eq!(selection.ids.len(), 2);
        assert!(selection.ids.contains(&object_id(&[1; 20], 20)));
        assert!(selection.ids.contains(&object_id(&[4; 20], 20)));
        Ok(())
    }

    #[test]
    fn mislabeled_object_stays_unpublished() -> Result<()> {
        let root = tempfile::tempdir()?;
        run(
            root.path(),
            &["init", "-q", "--template=", "--object-format=sha1"],
        )?;
        fs::write(root.path().join("object"), b"unrelated object")?;
        let id = run(root.path(), &["hash-object", "-w", "object"])?;
        let mut ids = tempfile::tempfile_in(root.path())?;
        ids.write_all(&id)?;
        ids.rewind()?;
        let mut command = transfer_git(root.path());
        command.args(["pack-objects", "--stdout"]).stdin(ids);
        let bytes = checked(command)?;

        let mut file = tempfile::tempfile_in(root.path())?;
        file.write_all(&bytes)?;
        let scratch = cache::private_tempdir_in(root.path())?;
        let stage = tempfile::tempdir_in(root.path())?;
        let prepared = Prepared {
            #[cfg(target_os = "macos")]
            preview: None,
            file,
            scratch,
            // Model a donor index mapping a wanted ID to an unrelated record.
            ids: vec![ObjectId([0; 32])],
            unchanged_bytes: 0,
        };
        let error = prepared
            .verify(root.path(), stage.path(), 20)
            .err()
            .context("mislabeled object was accepted")?;
        assert_eq!(
            error.to_string(),
            "filtered Git pack contains an unexpected object"
        );
        assert_eq!(fs::read_dir(stage.path())?.count(), 0);
        Ok(())
    }
}
