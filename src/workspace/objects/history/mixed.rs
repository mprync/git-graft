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
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    ops::Range,
    path::Path,
};

pub(super) const MIN_PACK_BYTES: u64 = 1 << 20;
const MAX_PACK_BYTES: u64 = 256 << 20;
const MAX_INDEX_BYTES: u64 = 40 << 20;
// A large run of empty blocks would make subsequent reads of the preceding
// object expensive. Let Git export these layouts in its usual compact form.
const MAX_ALIGNMENT_BYTES: usize = 64 << 10;

pub(super) struct Filtered {
    pub(super) pack: Pack,
    pub(super) hash: String,
    pub(super) ids: Vec<ObjectId>,
    pub(super) unchanged_bytes: u64,
}

fn bounded_bytes(file: File, max: u64) -> Result<Vec<u8>> {
    let size = file.metadata()?.len();
    ensure!(size <= max, "pack exceeds the in-memory filter limit");
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(size + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 == size, "Git file changed size");
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
    let mut records = Vec::with_capacity(count);
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
    let mut kept = HashSet::with_capacity(records.len());
    let mut ids = Vec::new();
    let mut gaps = Vec::new();
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
        !ids.is_empty() && ids.len() < records.len() && rewrite < bytes.len() / 4,
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
    ensure!(
        (MIN_PACK_BYTES..=MAX_PACK_BYTES).contains(&input.metadata()?.len()),
        "unsupported donor size"
    );
    let pack = Pack {
        pack: scratch.join("filtered.pack"),
        index: scratch.join("filtered.idx"),
    };
    clone_file(&input, &pack.pack)?;
    let mut bytes = bounded_bytes(open_regular(&pack.pack)?, MAX_PACK_BYTES)?;
    let index = bounded_bytes(open_regular(&donor.index)?, MAX_INDEX_BYTES)?;
    let plan = select(&bytes, &index, width, wanted)?;

    // Only this CoW clone is writable. It stays in the primary's private
    // registry until Git has independently confirmed every output object ID.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&pack.pack, fs::Permissions::from_mode(0o600))?;
    }
    let mut file = File::options().write(true).open(&pack.pack)?;
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
    file.set_len((plan.end + width) as u64)?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)?;
    drop(file);

    let mut command = transfer_git(root);
    command
        .args(["index-pack", "--index-version=2", "--threads=4"])
        .arg(&pack.pack);
    let output = checked(command).context("index filtered Git pack")?;
    verify_membership(std::slice::from_ref(&pack), &plan.ids, width)?;
    Ok(Filtered {
        hash: output_text(&output)?.into(),
        pack,
        ids: plan.ids,
        unchanged_bytes: unchanged_bytes as u64,
    })
}
