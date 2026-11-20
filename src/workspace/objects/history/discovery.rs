//! Enumerate the selected commits and walk batches of their root trees.
use super::{ObjectId, set_shallow, transfer_git};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashSet,
    fs,
    io::{Seek, Write},
    path::Path,
    thread,
};

const WORKERS: usize = 4;
// Each of the four tree walks and the initial commit list has its own cap.
const OUTPUT_LIMIT: u64 = 8 << 20;

// A reachability bitmap can answer the ordinary Git walk directly.
fn eligible(primary: &Path) -> bool {
    let objects = primary.join(".git/objects");
    if thread::available_parallelism().map_or(0, |n| n.get()) < WORKERS
        || objects.join("info/alternates").exists()
    {
        return false;
    }
    let Ok(packs) = fs::read_dir(objects.join("pack")) else {
        return false;
    };
    let mut large = false;
    for (i, entry) in packs.enumerate() {
        let Ok(entry) = entry else { return false };
        if i >= 64 {
            return false;
        }
        let path = entry.path();
        if entry.file_name() == "multi-pack-index.d"
            || path.extension().is_some_and(|ext| ext == "bitmap")
        {
            return false;
        }
        if path.extension().is_some_and(|ext| ext == "idx") {
            large |= fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() >= 3 << 20);
        }
    }
    large
}

pub(super) fn parallel(
    primary: &Path,
    scratch: &Path,
    oid: &str,
    width: usize,
) -> Option<Vec<ObjectId>> {
    if !eligible(primary) {
        return None;
    }
    discover(primary, scratch, oid, width).ok()
}

fn discover(primary: &Path, scratch: &Path, oid: &str, width: usize) -> Result<Vec<ObjectId>> {
    let mut cmd = transfer_git(primary);
    set_shallow(&mut cmd, None);
    cmd.args([
        "-c",
        "core.commitGraph=true",
        "rev-list",
        "--no-commit-header",
        "--format=%H%T",
        oid,
        "--",
    ]);
    let pairs = crate::git::bounded_stdout(&mut cmd, OUTPUT_LIMIT)?;
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    let mut roots = Vec::new();
    let mut seen_roots = HashSet::new();
    for row in lines(&pairs)? {
        let (commit, tree) = row
            .split_at_checked(width * 2)
            .context("missing root tree")?;
        let commit = ObjectId::from_hex(commit, width)?;
        ensure!(seen.insert(commit), "duplicate commit");
        ids.push(commit);
        let tree = ObjectId::from_hex(tree, width)?;
        if seen_roots.insert(tree) {
            roots.push(tree);
        }
    }
    ensure!(
        roots.len() >= 4096 && seen.contains(&ObjectId::from_hex(oid.as_bytes(), width)?),
        "small or incomplete Git history"
    );
    drop(seen_roots);
    drop(pairs);
    let batches = thread::scope(|scope| -> Result<Vec<_>> {
        let mut jobs = Vec::new();
        for batch in roots.chunks(roots.len().div_ceil(WORKERS)) {
            jobs.push(
                thread::Builder::new()
                    .spawn_scoped(scope, move || tree_objects(primary, scratch, batch, width))?,
            );
        }
        jobs.into_iter()
            .map(|j| {
                j.join()
                    .map_err(|_| anyhow::anyhow!("Git tree discovery worker panicked"))?
            })
            .collect()
    })?;
    // Adjacent history slices share subtrees and blobs. Keep the first
    // occurrence in Git's order for deterministic residual pack exports.
    for batch in batches {
        for id in batch {
            if seen.insert(id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

fn lines(bytes: &[u8]) -> Result<impl Iterator<Item = &[u8]>> {
    let body = bytes
        .strip_suffix(b"\n")
        .context("incomplete Git history output")?;
    Ok(body
        .split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line)))
}

fn tree_objects(
    primary: &Path,
    scratch: &Path,
    roots: &[ObjectId],
    width: usize,
) -> Result<Vec<ObjectId>> {
    let mut text = Vec::with_capacity(roots.len() * (width * 2 + 1));
    for tree in roots {
        tree.write_hex(width, &mut text);
    }
    let mut input = tempfile::tempfile_in(scratch)?;
    input.write_all(&text)?;
    input.rewind()?;
    let mut cmd = transfer_git(primary);
    set_shallow(&mut cmd, None);
    cmd.args([
        "rev-list",
        "--objects",
        "--no-object-names",
        "--missing=error",
        "--stdin",
        "--",
    ])
    .stdin(input);
    let output = crate::git::bounded_stdout(&mut cmd, OUTPUT_LIMIT)?;
    lines(&output)?
        .map(|row| ObjectId::from_hex(row, width))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::objects::{checked, output_text, run};

    #[test]
    fn parallel_roots_include_historical_blobs_and_exclude_an_unrelated_branch() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path();
        run(
            repo,
            &["init", "-q", "--template=", "--object-format=sha256"],
        )?;
        let mut stream = tempfile::tempfile_in(repo)?;
        for i in 0..4096 {
            writeln!(
                stream,
                "commit refs/heads/main\nmark :{}\ncommitter Test <test@example.org> 1000000000 +0000\ndata 1\nx\nM 100644 inline version\ndata 4\n{i:04}\n",
                i + 1
            )?;
        }
        // Two selected commits share a root. The other branch adds a private blob.
        stream.write_all(b"commit refs/heads/main\ncommitter Test <test@example.org> 1000000001 +0000\ndata 1\ny\n\nreset refs/heads/other\nfrom :1\n\ncommit refs/heads/other\ncommitter Test <test@example.org> 1000000002 +0000\ndata 1\nz\nM 100644 inline foreign\ndata 7\nprivate\n\ndone\n")?;
        stream.rewind()?;
        let mut import = transfer_git(repo);
        import.args(["fast-import", "--quiet"]).stdin(stream);
        checked(import)?;
        let head = run(repo, &["rev-parse", "refs/heads/main"])?;
        let head = output_text(&head)?;
        let actual = discover(repo, repo, head, 32)?;
        let expected = super::super::reachable(transfer_git(repo), head, 32, None, None, false)?;
        assert_eq!(actual.len(), 4097 + 4096 + 4096);
        assert_eq!(actual.len(), expected.len());
        assert!(actual.into_iter().collect::<HashSet<_>>() == expected.into_iter().collect());
        Ok(())
    }
}
