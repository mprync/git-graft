//! Clone fully tracked donor directories with one APFS call each.
//!
//! Cloning 3,000 files one by one costs about 160 ms on APFS; cloning the
//! directories that hold them takes about 30 ms. A directory qualifies only
//! when every entry the selected tree places under it is an eligible regular
//! file or symbolic link and the donor holds nothing else there, so untracked
//! and ignored files are never copied by design. Each clone is listed again
//! afterwards; a clone that picked up a concurrently created entry is discarded
//! and its files take the per-file path. Every cloned entry is then verified
//! before its stat data is recorded: a regular file by Git's own stat test on
//! an unchanged donor or by hashing its bytes, a link by hashing its target. An entry that
//! fails verification is removed and left for Git to check out.
use super::{link_matches, verified_blob};
use crate::storage::{Counts, Entry, cache::Stamp, index_stat, native::Root};
use anyhow::{Context, Result};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    ffi::OsStr,
    io,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::Path,
};

/// Smaller directories gain nothing over per-file clones.
const MIN_FILES: usize = 8;
const WORKERS: usize = 4;
const LINK: u32 = 0o120000;

/// The outcome of cloning directories ahead of the per-file workers.
#[derive(Default)]
pub(super) struct Cloned<'a> {
    /// Entries installed here; the per-file workers and Git skip them.
    pub(super) covered: Vec<bool>,
    pub(super) counts: Counts,
    pub(super) installed: Vec<(&'a Entry, index_stat::Stat)>,
    /// Cloned entries that did not verify; Git checks them out.
    pub(super) missing: Vec<&'a Entry>,
}

/// What a checkout may install by directory clone.
#[derive(Clone, Copy)]
pub(super) struct Policy<'t> {
    /// Stat data from the primary's index, trusted as `git status` trusts it.
    pub(super) trusted: Option<&'t index_stat::Trusted>,
    /// Git checks symbolic links out as links.
    pub(super) links: bool,
    /// Permission bits Git gives a new non-executable file.
    pub(super) mode: u32,
    pub(super) sha256: bool,
    /// Workers may cache open ancestor directories.
    pub(super) reuse_ancestors: bool,
}

/// Directories and the names the selected tree places in each, by relative path.
type Shape = BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>;

fn path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

fn parent(name: &[u8]) -> Option<&[u8]> {
    name.iter().rposition(|&b| b == b'/').map(|at| &name[..at])
}

fn inside(name: &[u8], directory: &[u8]) -> bool {
    name.starts_with(directory) && name.get(directory.len()) == Some(&b'/')
}

/// Every directory whose whole subtree holds only clonable entries, keeping
/// only the outermost ones. The repository root never qualifies.
fn clean_directories(files: &[Entry], candidates: &[bool], links: bool) -> Vec<Vec<u8>> {
    let mut totals: BTreeMap<&[u8], (usize, usize)> = BTreeMap::new();
    for (entry, &eligible) in files.iter().zip(candidates) {
        let clonable = eligible && entry.regular() || links && entry.mode == LINK;
        let mut name = entry.name.as_slice();
        while let Some(directory) = parent(name) {
            let count = totals.entry(directory).or_default();
            count.0 += 1;
            count.1 += usize::from(clonable);
            name = directory;
        }
    }
    let clean = |directory: &[u8]| {
        totals
            .get(directory)
            .is_some_and(|&(total, clonable)| total == clonable && total >= MIN_FILES)
    };
    totals
        .keys()
        .filter(|directory| clean(directory) && parent(directory).is_none_or(|up| !clean(up)))
        .map(|directory| directory.to_vec())
        .collect()
}

/// The names the selected tree places in `root` and each directory below it.
fn shape(files: &[Entry], indexes: &[usize], root: &[u8]) -> Shape {
    let mut shape = Shape::new();
    shape.entry(root.to_vec()).or_default();
    for &index in indexes {
        let mut name = files[index].name.as_slice();
        while let Some(directory) = parent(name) {
            let child = &name[directory.len() + 1..];
            shape
                .entry(directory.to_vec())
                .or_default()
                .insert(child.to_vec());
            if directory == root {
                break;
            }
            name = directory;
        }
    }
    shape
}

/// Whether `directory` under `root` lists exactly the expected names.
fn listed_exactly(root: &Root, directory: &[u8], expected: &BTreeSet<Vec<u8>>) -> bool {
    root.names(path(directory)).is_ok_and(|names| {
        names.len() == expected.len() && names.iter().all(|name| expected.contains(name.as_bytes()))
    })
}

/// Directories in `shape` whose listing under `root` differs from the tree.
/// Large shapes are listed by several workers at once.
fn mismatched(root: &Root, shape: &Shape, reuse_ancestors: bool) -> io::Result<HashSet<Vec<u8>>> {
    let directories: Vec<_> = shape.iter().collect();
    if directories.len() < 64 {
        return Ok(directories
            .into_iter()
            .filter(|(directory, names)| !listed_exactly(root, directory, names))
            .map(|(directory, _)| directory.clone())
            .collect());
    }
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for chunk in directories.chunks(directories.len().div_ceil(WORKERS)) {
            let root = root.checkout_worker(reuse_ancestors)?;
            workers.push(scope.spawn(move || {
                chunk
                    .iter()
                    .filter(|(directory, names)| !listed_exactly(&root, directory, names))
                    .map(|(directory, _)| (*directory).clone())
                    .collect::<Vec<_>>()
            }));
        }
        Ok(workers
            .into_iter()
            .flat_map(crate::workspace::joined)
            .collect())
    })
}

/// The outermost directories under `root` with no mismatched directory below.
/// Entries directly inside a mismatched directory take the per-file path.
fn clonable(root: &[u8], shape: &Shape, bad: &HashSet<Vec<u8>>, out: &mut Vec<Vec<u8>>) {
    if !bad.iter().any(|bad| bad == root || inside(bad, root)) {
        out.push(root.to_vec());
        return;
    }
    for child in shape.get(root).into_iter().flatten() {
        let mut directory = root.to_vec();
        directory.push(b'/');
        directory.extend_from_slice(child);
        if shape.contains_key(&directory) {
            clonable(&directory, shape, bad, out);
        }
    }
}

/// Clone each clean directory, then verify and stat every entry inside it.
///
/// Directories are cloned by lane, but one directory can hold most of a
/// repository, so the per-entry checks afterwards are spread across workers.
pub(super) fn clone_directories<'a>(
    files: &'a [Entry],
    candidates: &[bool],
    donor: &Root,
    destination: &Root,
    policy: Policy<'_>,
) -> Result<Cloned<'a>> {
    let mut cloned = Cloned {
        covered: vec![false; files.len()],
        ..Cloned::default()
    };
    let roots: HashSet<Vec<u8>> = clean_directories(files, candidates, policy.links)
        .into_iter()
        .collect();
    if roots.is_empty() {
        return Ok(cloned);
    }
    // Assign every entry to the clean directory that contains it.
    let mut members: BTreeMap<&[u8], Vec<usize>> = BTreeMap::new();
    for (index, entry) in files.iter().enumerate() {
        let mut name = entry.name.as_slice();
        while let Some(directory) = parent(name) {
            if let Some(root) = roots.get(directory) {
                members.entry(root.as_slice()).or_default().push(index);
                break;
            }
            name = directory;
        }
    }
    let clones = std::thread::scope(|scope| -> Result<Vec<Clone>> {
        let mut workers = Vec::new();
        for lane in lanes(members.into_iter().collect(), |(_, indexes)| indexes.len()) {
            let donor = donor.checkout_worker(policy.reuse_ancestors)?;
            let destination = destination.checkout_worker(policy.reuse_ancestors)?;
            workers.push(scope.spawn(move || -> Result<Vec<Clone>> {
                let mut clones = Vec::new();
                for (root, indexes) in lane {
                    // Narrow to the subtrees whose donor listing matches exactly.
                    let shape = shape(files, &indexes, root);
                    // A listing that cannot be read leaves the root per-file.
                    let Ok(bad) = mismatched(&donor, &shape, policy.reuse_ancestors) else {
                        continue;
                    };
                    let mut chosen = Vec::new();
                    clonable(root, &shape, &bad, &mut chosen);
                    for name in chosen {
                        let members: Vec<usize> = indexes
                            .iter()
                            .copied()
                            .filter(|&index| inside(&files[index].name, &name))
                            .collect();
                        if members.len() < MIN_FILES {
                            continue;
                        }
                        // Stamp donor files before cloning them.
                        let before = stat_donors(files, &members, &donor, policy.reuse_ancestors)?;
                        let shape = self::shape(files, &members, &name);
                        if clone_one(&name, &shape, &donor, &destination, policy.reuse_ancestors)? {
                            clones.push(Clone {
                                name,
                                members,
                                before,
                            });
                        }
                    }
                }
                Ok(clones)
            }));
        }
        let mut clones = Vec::new();
        for worker in workers {
            clones.extend(crate::workspace::joined(worker)?);
        }
        Ok(clones)
    })?;
    crate::trace::mark("checkout: clone directory trees");
    // Verify every cloned entry, spread evenly across workers.
    let work: Vec<(usize, usize)> = clones
        .iter()
        .enumerate()
        .flat_map(|(clone, item)| (0..item.members.len()).map(move |at| (clone, at)))
        .collect();
    let outcomes = std::thread::scope(|scope| -> Result<Vec<_>> {
        let mut workers = Vec::new();
        for chunk in work.chunks(work.len().div_ceil(WORKERS).max(1)) {
            let donor = donor.checkout_worker(policy.reuse_ancestors)?;
            let destination = destination.checkout_worker(policy.reuse_ancestors)?;
            let clones = &clones;
            workers.push(scope.spawn(move || -> Result<Vec<_>> {
                let mut buffer = [0; 64 * 1024];
                chunk
                    .iter()
                    .map(|&(clone, at)| {
                        let item = &clones[clone];
                        let index = item.members[at];
                        let checked = Checked {
                            entry: &files[index],
                            before: item.before[at].as_ref(),
                        };
                        let outcome = verify(&checked, &donor, &destination, policy, &mut buffer)?;
                        Ok((clone, index, outcome))
                    })
                    .collect()
            }));
        }
        let mut outcomes = Vec::new();
        for worker in workers {
            outcomes.extend(crate::workspace::joined(worker)?);
        }
        Ok(outcomes)
    })?;
    // A clone with an entry of the wrong kind is discarded whole.
    let mut broken = vec![false; clones.len()];
    for (clone, _, outcome) in &outcomes {
        broken[*clone] |= matches!(outcome, Outcome::Broken);
    }
    for (item, broken) in clones.iter().zip(&broken) {
        if *broken {
            let target = path(&item.name);
            destination
                .discard_tree(target)
                .with_context(|| format!("discard mismatched clone {}", target.display()))?;
        }
    }
    for (clone, index, outcome) in outcomes {
        if broken[clone] {
            continue;
        }
        cloned.covered[index] = true;
        let entry = &files[index];
        match outcome {
            Outcome::Installed(stat, size) => {
                if let Some(size) = size {
                    cloned.counts.add(size);
                }
                cloned.installed.push((entry, stat));
            }
            Outcome::Missing => cloned.missing.push(entry),
            Outcome::Broken => unreachable!("broken clones were skipped"),
        }
    }
    Ok(cloned)
}

/// `lstat` each regular donor file, using several workers for large sets.
fn stat_donors(
    files: &[Entry],
    members: &[usize],
    donor: &Root,
    reuse_ancestors: bool,
) -> Result<Vec<Option<libc::stat>>> {
    let stat = |root: &Root, index: usize| {
        let entry = &files[index];
        if entry.mode == LINK {
            return None;
        }
        root.regular_stat(entry.path().ok()?).ok()
    };
    if members.len() < 512 {
        return Ok(members.iter().map(|&index| stat(donor, index)).collect());
    }
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for chunk in members.chunks(members.len().div_ceil(WORKERS)) {
            let root = donor.checkout_worker(reuse_ancestors)?;
            workers.push(scope.spawn(move || {
                chunk
                    .iter()
                    .map(|&index| stat(&root, index))
                    .collect::<Vec<_>>()
            }));
        }
        Ok(workers
            .into_iter()
            .flat_map(crate::workspace::joined)
            .collect())
    })
}

/// Split jobs across at most [`WORKERS`] lanes, largest first, by `weight`.
fn lanes<T>(mut jobs: Vec<T>, weight: impl Fn(&T) -> usize) -> Vec<Vec<T>> {
    jobs.sort_by_key(|job| std::cmp::Reverse(weight(job)));
    let mut lanes: Vec<Vec<T>> = (0..WORKERS.min(jobs.len())).map(|_| Vec::new()).collect();
    let mut loads = vec![0; lanes.len()];
    for job in jobs {
        let lane = (0..loads.len())
            .min_by_key(|&lane| loads[lane])
            .unwrap_or(0);
        loads[lane] += weight(&job);
        lanes[lane].push(job);
    }
    lanes
}

/// A directory cloned into the destination, and its donors' stats from before.
struct Clone {
    name: Vec<u8>,
    members: Vec<usize>,
    before: Vec<Option<libc::stat>>,
}

/// Clone one directory and confirm it holds exactly the tree's names.
/// `false` leaves its entries for the per-file workers.
fn clone_one(
    name: &[u8],
    shape: &Shape,
    donor: &Root,
    destination: &Root,
    reuse_ancestors: bool,
) -> Result<bool> {
    let target = path(name);
    destination.create_parent(target)?;
    match destination.clone_directory(target, donor) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(error)
                .with_context(|| format!("checkout destination exists: {}", target.display()));
        }
        Err(_) => return Ok(false),
    }
    // An entry created in the donor between its listing and the clone would
    // have been copied too. Such a clone is ours to discard.
    if mismatched(destination, shape, reuse_ancestors).is_ok_and(|bad| bad.is_empty()) {
        return Ok(true);
    }
    destination
        .discard_tree(target)
        .with_context(|| format!("discard mismatched clone {}", target.display()))?;
    Ok(false)
}

/// One cloned entry and what is known about its donor.
struct Checked<'a> {
    entry: &'a Entry,
    before: Option<&'a libc::stat>,
}

enum Outcome {
    /// Verified; regular files report their size.
    Installed(index_stat::Stat, Option<u64>),
    /// Removed for Git to check out.
    Missing,
    /// Not the kind of entry the tree names; the whole clone is discarded.
    Broken,
}

fn verify(
    checked: &Checked<'_>,
    donor: &Root,
    destination: &Root,
    policy: Policy<'_>,
    buffer: &mut [u8; 64 * 1024],
) -> Result<Outcome> {
    let entry = checked.entry;
    let file_path = entry.path()?;
    if entry.mode == LINK {
        let Ok(target) = destination.link(file_path) else {
            return Ok(Outcome::Broken);
        };
        if !link_matches(target.as_bytes(), &entry.blob, policy.sha256) {
            destination.discard(file_path)?;
            return Ok(Outcome::Missing);
        }
        let info = destination.link_stat(file_path)?;
        return Ok(Outcome::Installed(index_stat::Stat::from_libc(&info), None));
    }
    let Ok(info) = destination.regular_stat(file_path) else {
        return Ok(Outcome::Broken);
    };
    // A donor whose stat Git itself trusts, unchanged across the clone, needs
    // no hashing.
    let verified = checked.before.is_some_and(|before| {
        trusted_donor(donor, file_path, before, entry, policy.trusted)
            || donor_unchanged(donor, file_path, before, entry, buffer, policy.sha256)
    }) || destination.regular_metadata(file_path).is_ok_and(
        |(mut file, metadata)| {
            verified_blob(&mut file, &metadata, buffer, &entry.blob, policy.sha256)
        },
    );
    if !verified {
        destination.discard(file_path)?;
        return Ok(Outcome::Missing);
    }
    let wanted = entry.creation_mode(policy.mode);
    let info = if info.st_mode as u32 & 0o7777 != wanted {
        let (file, _) = destination.regular_metadata(file_path)?;
        file.set_permissions(std::fs::Permissions::from_mode(wanted))?;
        destination.regular_stat(file_path)?
    } else {
        info
    };
    Ok(Outcome::Installed(
        index_stat::Stat::from_libc(&info),
        Some(info.st_size as u64),
    ))
}

/// Whether the donor kept the stat it had before the clone and Git's index
/// vouches for that stat holding the staged blob.
fn trusted_donor(
    donor: &Root,
    path: &Path,
    before: &libc::stat,
    entry: &Entry,
    trusted: Option<&index_stat::Trusted>,
) -> bool {
    let Some(trusted) = trusted else {
        return false;
    };
    let stat = index_stat::Stat::from_libc(before);
    trusted.holds(
        &entry.name,
        &entry.blob,
        entry.mode,
        &stat,
        before.st_size as u64,
    ) && donor
        .regular_stat(path)
        .is_ok_and(|now| index_stat::Stat::from_libc(&now) == stat && now.st_mode == before.st_mode)
}

/// Hash the donor's bytes, accepting them only if the donor kept the stamp it
/// had before the clone until after hashing.
fn donor_unchanged(
    donor: &Root,
    path: &Path,
    before: &libc::stat,
    entry: &Entry,
    buffer: &mut [u8; 64 * 1024],
    sha256: bool,
) -> bool {
    let Ok((mut file, metadata)) = donor.regular_metadata(path) else {
        return false;
    };
    let stamp = Stamp::from(&metadata);
    stamp.matches_stat(before)
        && verified_blob(&mut file, &metadata, buffer, &entry.blob, sha256)
        && file.metadata().is_ok_and(|now| Stamp::from(&now) == stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, mode: u32) -> Entry {
        Entry {
            name: name.as_bytes().to_vec(),
            blob: "0".repeat(40),
            mode,
        }
    }

    #[test]
    fn selects_outermost_directories_of_only_eligible_regular_files() {
        let mut rows = Vec::new();
        for i in 0..MIN_FILES {
            rows.push((entry(&format!("clean/a/{i}"), 0o100644), true));
            rows.push((entry(&format!("clean/b/{i}"), 0o100755), true));
            rows.push((entry(&format!("mixed/ok/{i}"), 0o100644), true));
            rows.push((entry(&format!("blocked/{i}"), 0o100644), true));
        }
        rows.push((entry("mixed/link", 0o120000), true));
        rows.push((entry("mixed/few/x", 0o100644), true));
        rows.push((entry("top", 0o100644), true));
        // An ineligible file blocks its directory and every ancestor.
        rows.push((entry("blocked/sub/x", 0o100644), false));
        rows.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        let (files, candidates): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        let selected = clean_directories(&files, &candidates, false);
        assert_eq!(
            selected,
            vec![b"clean".to_vec(), b"mixed/ok".to_vec()],
            "symlinks, ineligible files and small directories stay per-file"
        );
    }

    fn blob(bytes: &[u8]) -> String {
        use sha1::Digest;
        let mut hasher = sha1::Sha1::new();
        hasher.update(format!("blob {}\0", bytes.len()));
        hasher.update(bytes);
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// Clone a real donor tree; an untracked file narrows its directory to
    /// per-file handling and never reaches the destination.
    #[test]
    fn clones_matching_directories_and_never_copies_untracked_files() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let (donor_path, destination_path) = (root.join("donor"), root.join("dest"));
        std::fs::create_dir_all(&destination_path)?;
        let mut rows = Vec::new();
        for directory in ["src/a", "src/b", "docs"] {
            std::fs::create_dir_all(donor_path.join(directory))?;
            for i in 0..MIN_FILES {
                let name = format!("{directory}/{i}.txt");
                let bytes = format!("{name}\n").into_bytes();
                std::fs::write(donor_path.join(&name), &bytes)?;
                rows.push(Entry {
                    name: name.into_bytes(),
                    blob: blob(&bytes),
                    mode: 0o100644,
                });
            }
        }
        std::os::unix::fs::symlink("0.txt", donor_path.join("src/b/link"))?;
        rows.push(Entry {
            name: b"src/b/link".to_vec(),
            blob: blob(b"0.txt"),
            mode: 0o120000,
        });
        // Untracked in docs, and one tracked file whose donor bytes changed.
        std::fs::write(donor_path.join("docs/.env"), b"SECRET=1\n")?;
        std::fs::write(donor_path.join("src/a/0.txt"), b"edited\n")?;
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        let candidates = vec![true; rows.len()];
        let donor = Root::open(&donor_path)?;
        let destination = Root::open(&destination_path)?;
        let policy = Policy {
            trusted: None,
            links: true,
            mode: 0o644,
            sha256: false,
            reuse_ancestors: false,
        };
        let cloned = clone_directories(&rows, &candidates, &donor, &destination, policy)?;
        assert!(!destination_path.join("docs/.env").exists());
        assert!(
            !destination_path.join("docs").exists(),
            "docs stays per-file"
        );
        let covered: Vec<_> = rows
            .iter()
            .zip(&cloned.covered)
            .filter(|(_, covered)| **covered)
            .map(|(entry, _)| String::from_utf8_lossy(&entry.name).into_owned())
            .collect();
        assert_eq!(covered.len(), 2 * MIN_FILES + 1);
        assert!(covered.iter().all(|name| name.starts_with("src/")));
        // The edited file failed verification: removed and left for Git.
        assert_eq!(cloned.missing.len(), 1);
        assert_eq!(cloned.missing[0].name, b"src/a/0.txt");
        assert!(!destination_path.join("src/a/0.txt").exists());
        assert_eq!(cloned.installed.len(), 2 * MIN_FILES);
        assert_eq!(
            std::fs::read_link(destination_path.join("src/b/link"))?,
            Path::new("0.txt")
        );
        // Symbolic links are installed but not counted as cloned file data.
        assert_eq!(cloned.counts.files, 2 * MIN_FILES as u64 - 1);
        assert_eq!(
            std::fs::read(destination_path.join("src/b/3.txt"))?,
            b"src/b/3.txt\n"
        );
        Ok(())
    }

    #[test]
    fn mismatched_listings_narrow_cloning_to_matching_subtrees() {
        let mut shape = Shape::new();
        shape.insert(
            b"d".to_vec(),
            [b"a".to_vec(), b"b".to_vec(), b"f".to_vec()].into(),
        );
        shape.insert(b"d/a".to_vec(), [b"x".to_vec()].into());
        shape.insert(b"d/b".to_vec(), [b"y".to_vec()].into());
        let mut out = Vec::new();
        clonable(b"d", &shape, &HashSet::new(), &mut out);
        assert_eq!(out, vec![b"d".to_vec()]);
        let mut out = Vec::new();
        clonable(b"d", &shape, &[b"d/b".to_vec()].into(), &mut out);
        assert_eq!(out, vec![b"d/a".to_vec()]);
        let mut out = Vec::new();
        clonable(b"d", &shape, &[b"d".to_vec()].into(), &mut out);
        assert_eq!(out, vec![b"d/a".to_vec(), b"d/b".to_vec()]);
    }
}
