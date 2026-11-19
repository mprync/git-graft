//! Preserve CoW extents when a Git pack contains a small amount of unrelated
//! history. Git indexes the finished pack from its actual compressed bytes;
//! the donor index only suggests which records to keep.

use super::{
    ObjectId, Pack, checked, clone_file, index_count, open_regular, output_text, transfer_git,
    verify_membership,
};
use anyhow::{Context, Result, bail, ensure};
use sha2::Digest;
use std::{
    collections::HashSet,
    io::{Read, Seek, SeekFrom, Write},
    ops::Range,
    path::Path,
    process::Stdio,
};

mod scratch;

pub(super) const MIN_PACK_BYTES: u64 = 1 << 20;
const MAX_PACK_BYTES: u64 = 256 << 20;
const MAX_INDEX_BYTES: u64 = 40 << 20;
const MAX_WORKING_BYTES: u64 = 192 << 20;
// A large run of empty blocks would make subsequent reads of the preceding
// object expensive. Let Git export these layouts in its usual compact form.
const MAX_ALIGNMENT_BYTES: usize = 64 << 10;

pub(super) struct Filtered {
    pub(super) pack: Pack,
    pub(super) hash: String,
    pub(super) ids: Vec<ObjectId>,
    pub(super) unchanged_bytes: u64,
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

// v2 indexes put sorted IDs, CRCs and 32-bit pack offsets after the fanout
// table. The size limit makes large-offset entries unnecessary here.
fn offsets(index: &[u8], width: usize) -> Result<Vec<(usize, ObjectId)>> {
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
    let mut records = Vec::new();
    records.try_reserve_exact(count)?;
    for (id, start) in ids.chunks_exact(width).zip(starts.as_chunks::<4>().0) {
        let mut value = ObjectId([0; 32]);
        value.0[..width].copy_from_slice(id);
        records.push((u32::from_be_bytes(*start) as usize, value));
    }
    records.sort_unstable_by_key(|r| r.0);
    Ok(records)
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
    ids: Vec<ObjectId>,
    // Compressed bytes to move right, followed by the end of the removed run.
    gaps: Vec<(Range<usize>, usize)>,
    end: usize,
}

fn select(
    bytes: &[u8],
    index: &[u8],
    width: usize,
    wanted: &HashSet<ObjectId>,
) -> Result<Selection> {
    let records = offsets(index, width)?;
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
    let mut ids = Vec::new();
    ids.try_reserve_exact(records.len())?;
    let mut gaps = Vec::new();
    gaps.try_reserve_exact(records.len() / 2)?;
    let mut previous = 12..12;
    let mut rewrite = 0;
    for (i, &(start, id)) in records.iter().enumerate() {
        let end = records.get(i + 1).map_or(payload_end, |next| next.0);
        ensure!(
            start < end && end <= payload_end,
            "invalid pack record bounds"
        );
        if !wanted.contains(&id) {
            continue;
        }
        let Some(stream) = stream_start(&bytes[start..end], start, &kept)? else {
            continue;
        };
        if previous.end != start {
            ensure!(
                !ids.is_empty(),
                "first pack record is outside the selected history"
            );
            ensure!(
                start - previous.end <= MAX_ALIGNMENT_BYTES,
                "mixed pack requires excessive alignment"
            );
            rewrite += start - previous.start;
            gaps.push((previous.clone(), start));
        }
        kept.insert(start);
        ids.push(id);
        previous = stream + 2..end;
    }
    ensure!(
        !ids.is_empty() && ids.len() < records.len() && rewrite < previous.end / 4,
        "mixed pack has insufficient reusable data"
    );
    Ok(Selection {
        ids,
        gaps,
        end: previous.end,
    })
}

pub(super) fn filter(
    root: &Path,
    scratch: &Path,
    donor: &Pack,
    wanted: &HashSet<ObjectId>,
    width: usize,
) -> Result<Filtered> {
    let input = open_regular(&donor.pack)?;
    let input_size = input.metadata()?.len();
    let index = open_regular(&donor.index)?;
    let index_size = index.metadata()?.len();
    check_budget(input_size, index_size, width)?;
    let pack = Pack {
        pack: scratch.join("filtered.pack"),
        index: scratch.join("filtered.idx"),
    };
    let mut file = scratch::create(&input, &donor.pack, scratch)?;
    let mut bytes = bounded_bytes(&file, input_size)?;
    let index = bounded_bytes(index, index_size)?;
    let plan = select(&bytes, &index, width, wanted)?;
    drop(index);

    // Edit the unlinked clone. Only the sanitized bytes get an output name.
    let count = u32::try_from(plan.ids.len())?.to_be_bytes();
    bytes[8..12].copy_from_slice(&count);
    file.seek(SeekFrom::Start(8))?;
    file.write_all(&count)?;
    let mut unchanged_bytes = plan.end - 4;
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
    let trailer = match width {
        20 => sha1::Sha1::digest(payload).to_vec(),
        32 => sha2::Sha256::digest(payload).to_vec(),
        _ => bail!("unsupported Git object format"),
    };
    file.seek(SeekFrom::Start(plan.end as u64))?;
    file.write_all(&trailer)?;
    drop(bytes);
    file.set_len((plan.end + width) as u64)?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;

    // Git verifies the anonymous pack as a regular file and writes only the
    // index in private staging. Name the pack after its membership is proven.
    file.rewind()?;
    let mut command = transfer_git(root);
    command
        .args(["index-pack", "--index-version=2", "--threads=4", "-o"])
        .arg(&pack.index)
        .arg("/dev/fd/0")
        .stdin(Stdio::from(file.try_clone()?));
    let output = checked(command).context("index filtered Git pack")?;
    verify_membership(std::slice::from_ref(&pack), &plan.ids, width)?;
    clone_file(&file, &pack.pack)?;
    Ok(Filtered {
        hash: output_text(&output)?.into(),
        pack,
        ids: plan.ids,
        unchanged_bytes: unchanged_bytes as u64,
    })
}
