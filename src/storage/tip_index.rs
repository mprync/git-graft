//! Authenticate a flattened checkout and require Git to accept every index entry.
use super::{Entry, local::LocalCheckout, parse_entries};
use anyhow::{Context, Result, ensure};
use sha2::Digest;
use std::{
    fmt::Write as _,
    io::{Seek, Write},
    process::Stdio,
};

pub(crate) struct TipIndex {
    pub(super) entries: Vec<Entry>,
    rows: Vec<u8>,
    pub(crate) tree: String,
}

impl TipIndex {
    pub(crate) fn new(rows: Vec<u8>, width: usize) -> Result<Self> {
        let entries = parse_entries(&rows)?;
        ensure!(
            entries.len() <= 131_072 && entries.windows(2).all(|p| p[0].name < p[1].name),
            "unsorted or excessive checkout entries"
        );
        let digest = tree_hash(&entries, 0, 0, width)?;
        let mut tree = String::with_capacity(width * 2);
        for byte in digest {
            write!(tree, "{byte:02x}")?;
        }
        Ok(Self {
            entries,
            rows,
            tree,
        })
    }

    pub(crate) fn attributes(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|e| e.attributes())
            .map(|e| e.blob.as_str())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn matches_rows(&self, rows: &[u8]) -> bool {
        self.rows == rows
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn blobs(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|entry| entry.mode != 0o160000)
            .map(|entry| entry.blob.as_str())
    }

    pub(super) fn prepare(&self, checkout: &LocalCheckout<'_>) -> Result<()> {
        let mut input = tempfile::tempfile()?;
        input.write_all(&self.rows)?;
        input.rewind()?;
        let mut command = checkout.proof_command();
        command
            .args([
                "-c",
                "core.protectHFS=true",
                "-c",
                "core.protectNTFS=true",
                "update-index",
                "--info-only",
                "-z",
                "--index-info",
            ])
            .stdin(Stdio::from(input));
        super::local::run(command)?;
        // update-index may skip an invalid name and still exit successfully.
        let actual = crate::git::bounded_stdout(
            checkout
                .proof_command()
                .args(["ls-files", "--stage", "--full-name", "-z"]),
            16 * 1024 * 1024,
        )?;
        let mut records = actual.split(|&b| b == 0).filter(|r| !r.is_empty());
        for entry in &self.entries {
            let row = records
                .next()
                .context("Git omitted a selected index entry")?;
            let tab = row
                .iter()
                .position(|&b| b == b'\t')
                .context("missing index name")?;
            ensure!(
                row[..tab] == *format!("{:06o} {} 0", entry.mode, entry.blob).as_bytes()
                    && row[tab + 1..] == entry.name,
                "Git index differs from selected tree"
            );
        }
        ensure!(records.next().is_none(), "unexpected private index entry");
        Ok(())
    }
}

// Git orders a subtree as if its name ends with a slash. In a sorted
// recursive listing the complete subtree occupies one contiguous range.
fn tree_hash(entries: &[Entry], prefix: usize, depth: usize, width: usize) -> Result<Vec<u8>> {
    ensure!(depth <= 128, "checkout tree is too deep");
    let mut data = Vec::new();
    let mut rest = entries;
    while let Some(entry) = rest.first() {
        let suffix = &entry.name[prefix..];
        if let Some(slash) = suffix.iter().position(|&b| b == b'/') {
            let end = prefix + slash + 1;
            let count = rest.partition_point(|e| e.name.starts_with(&entry.name[..end]));
            data.extend_from_slice(b"40000 ");
            data.extend_from_slice(&suffix[..slash]);
            data.push(0);
            data.extend_from_slice(&tree_hash(&rest[..count], end, depth + 1, width)?);
            rest = &rest[count..];
        } else {
            ensure!(entry.blob.len() == 2 * width, "invalid object ID length");
            data.extend_from_slice(format!("{:o} ", entry.mode).as_bytes());
            data.extend_from_slice(suffix);
            data.push(0);
            for pair in entry.blob.as_bytes().as_chunks::<2>().0 {
                data.push(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?);
            }
            rest = &rest[1..];
        }
    }
    object_digest("tree", &data, width)
}

pub(crate) fn object_digest(kind: &str, bytes: &[u8], width: usize) -> Result<Vec<u8>> {
    let header = format!("{kind} {}\0", bytes.len());
    match width {
        20 => {
            let mut hash = sha1_checked::Sha1::new();
            sha1_checked::Digest::update(&mut hash, header.as_bytes());
            sha1_checked::Digest::update(&mut hash, bytes);
            let checked = hash.try_finalize();
            ensure!(!checked.has_collision(), "Git object has a SHA-1 collision");
            Ok(checked.hash().to_vec())
        }
        32 => Ok(sha2::Sha256::new()
            .chain_update(header)
            .chain_update(bytes)
            .finalize()
            .to_vec()),
        _ => anyhow::bail!("unsupported Git object ID size"),
    }
}
