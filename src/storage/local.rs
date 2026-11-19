//! Direct CoW reuse from a trusted local checkout. Git still owns the tree and
//! index, and supplies files whose checkout bytes cannot come from the donor.
use super::*;
use std::path::PathBuf;

pub(crate) fn materialize_local(
    workspace: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: &Path,
) -> Result<Value> {
    let workspace = dunce::canonicalize(workspace)?;
    materialize(&workspace, state, oid, sparse, primary, None, || Ok(()))
}

/// Prepare verified donor files while the complete tip pack is installed.
/// Git uses the proof store only for planning, never for file checkout.
pub(crate) fn materialize_tip<F: FnOnce() -> Result<()>>(
    workspace: &Path,
    state: &Path,
    oid: &str,
    primary: &Path,
    proof_objects: &Path,
    ready: F,
) -> Result<Value> {
    let mut ready = Some(ready);
    let result = (|| {
        let workspace = dunce::canonicalize(workspace)?;
        materialize(
            &workspace,
            state,
            oid,
            &[],
            primary,
            Some(proof_objects),
            || {
                ready.take().expect("checkout readiness called once")()?;
                checkout_identity(&workspace, oid)?;
                Ok(())
            },
        )
    })();
    // Always join the pack installation, including when planning fails early.
    if let Some(ready) = ready {
        let joined = ready();
        if result.is_ok() {
            joined?;
        }
    }
    result
}

fn materialize<F: FnOnce() -> Result<()>>(
    workspace: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: &Path,
    proof_objects: Option<&Path>,
    ready: F,
) -> Result<Value> {
    let oid = checkout_identity_with_objects(workspace, oid, proof_objects)?;
    validate_sparse(workspace, &oid, sparse)?;
    let files = entries_with_objects(workspace, &oid, proof_objects)?;
    let checkout = LocalCheckout::reserve(workspace, proof_objects)?;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let receipt = match direct::plan(&checkout, &oid, sparse, &files, state, primary)? {
        Ok(plan) => direct::install(&checkout, &oid, &files, plan, ready)?,
        Err(reason) => {
            ready()?;
            checkout.native(&oid, sparse, &files, reason)?
        }
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let receipt = {
        let _ = (state, primary);
        ready()?;
        checkout.native(&oid, sparse, &files, "native clone unavailable")?
    };
    checkout.publish()?;
    Ok(receipt)
}

struct LocalCheckout<'a> {
    worktree: &'a Path,
    scratch: tempfile::TempDir,
    index: PathBuf,
    index_lock: tempfile::TempPath,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    proof_objects: Option<&'a Path>,
}

impl<'a> LocalCheckout<'a> {
    fn reserve(worktree: &'a Path, proof_objects: Option<&'a Path>) -> Result<Self> {
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = proof_objects;
        ensure_empty(worktree)?;
        let admin = worktree.join(".git");
        let index_lock = admin.join("index.lock");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&index_lock)
            .context("reserve workspace Git index")?;
        let index_lock = tempfile::TempPath::try_from_path(index_lock)?;
        ensure!(
            matches!(fs::symlink_metadata(admin.join("index")), Err(error) if error.kind() == io::ErrorKind::NotFound),
            "workspace Git index already exists"
        );
        let scratch = tempfile::Builder::new()
            .prefix(".graft-local-")
            .tempdir_in(&admin)?;
        let index = scratch.path().join("index");
        Ok(Self {
            worktree,
            scratch,
            index,
            index_lock,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            proof_objects,
        })
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn proof_command(&self) -> Command {
        let mut command = self.command();
        if let Some(objects) = self.proof_objects {
            command.env("GIT_OBJECT_DIRECTORY", objects);
        }
        command
    }

    fn command(&self) -> Command {
        let mut command = git_command(self.worktree);
        // Let Git compress common path prefixes in the new private index.
        command.env("GIT_INDEX_FILE", &self.index).args([
            "-c",
            "index.version=4",
            "-c",
            "core.splitIndex=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.ignorestat=false",
        ]);
        command
    }

    fn publish(self) -> Result<()> {
        let destination = self.worktree.join(".git/index");
        crate::recovery::rename_exclusive(&self.index, &destination)
            .context("publish workspace Git index")?;
        self.index_lock.close()?;
        Ok(())
    }

    fn native(&self, oid: &str, sparse: &[String], files: &[Entry], reason: &str) -> Result<Value> {
        // read-tree verifies untracked paths before using Git's forced
        // checkout. Run it off the live tree so a path created in that gap
        // cannot be overwritten.
        let staged = self.scratch.path().join("tree");
        fs::create_dir(&staged)?;
        // Git resolves relative core.attributesFile paths from GIT_WORK_TREE.
        // Keep the workspace's effective attributes when using the scratch tree.
        let attributes = crate::git::raw(
            self.worktree,
            &["config", "--null", "--path", "--get", "core.attributesFile"],
        )?;
        let relative_attributes = if attributes.status.success() {
            let path = attributes
                .stdout
                .strip_suffix(b"\0")
                .context("invalid Git attributes file path")?;
            let path = Path::new(std::str::from_utf8(path)?);
            (path.is_relative() && !path.as_os_str().is_empty()).then(|| self.worktree.join(path))
        } else {
            ensure!(
                attributes.status.code() == Some(1),
                "read checkout attributes: {}",
                String::from_utf8_lossy(&attributes.stderr).trim()
            );
            None
        };
        let staged_command = || {
            let mut command = self.command();
            command.env("GIT_WORK_TREE", &staged);
            if let Some(path) = &relative_attributes {
                command
                    .env("GRAFT_CHECKOUT_ATTRIBUTES", path)
                    .arg("--config-env=core.attributesFile=GRAFT_CHECKOUT_ATTRIBUTES");
            }
            command
        };
        if !sparse.is_empty() {
            let mut command = staged_command();
            command
                .args(["sparse-checkout", "set", "--cone", "--"])
                .args(sparse);
            run(command)?;
        }
        let mut command = parallel_checkout(staged_command());
        command.args(["read-tree", "-m", "-u", oid]);
        run(command)?;
        let mut receipt = Receipt::default();
        count_native(&staged, files.iter(), &mut receipt.private);
        for entry in fs::read_dir(&staged)? {
            let entry = entry?;
            let destination = self.worktree.join(entry.file_name());
            crate::recovery::rename_exclusive(&entry.path(), &destination)
                .with_context(|| format!("publish checkout path {}", destination.display()))?;
        }
        Ok(receipt.json("git", Some(reason)))
    }
}

fn run(mut command: Command) -> Result<()> {
    let output = command.output().context("run Git checkout")?;
    ensure!(
        output.status.success(),
        "Git checkout: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

fn count_native<'a>(worktree: &Path, files: impl Iterator<Item = &'a Entry>, counts: &mut Counts) {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Ok(root) = native::Root::open(worktree) {
        for entry in files {
            if let Ok(size) = entry.path().and_then(|path| Ok(root.regular_size(path)?)) {
                counts.add(size);
            }
        }
        return;
    }
    for entry in files {
        count_private(worktree, entry, counts);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod direct {
    use super::*;
    use sha1::Digest;
    use std::{collections::HashMap, fs::File, io::Read, os::unix::fs::MetadataExt};

    pub(super) struct Plan {
        donor: native::Root,
        destination: native::Root,
        candidates: Vec<bool>,
        mode: u32,
    }

    pub(super) fn plan(
        checkout: &LocalCheckout<'_>,
        oid: &str,
        sparse: &[String],
        files: &[Entry],
        state: &Path,
        primary: &Path,
    ) -> Result<std::result::Result<Plan, &'static str>> {
        if !sparse.is_empty() {
            return Ok(Err("sparse checkout"));
        }
        if files.is_empty() {
            return Ok(Err("empty tree"));
        }
        if files.iter().any(|entry| entry.mode == 0o160000) {
            return Ok(Err("submodules"));
        }
        if !unambiguous_paths(files) {
            return Ok(Err("filesystem-dependent Git paths"));
        }
        if state.parent() == Some(primary) {
            return Ok(Err("primary checkout unavailable"));
        }
        let Ok(primary) = dunce::canonicalize(primary) else {
            return Ok(Err("primary checkout unavailable"));
        };
        let (Ok(donor), Ok(destination)) = (
            native::Root::open(&primary),
            native::Root::open(checkout.worktree),
        ) else {
            return Ok(Err("native clone unavailable"));
        };
        if !donor.same_device(&destination)
            || !probe(checkout.scratch.path(), checkout.scratch.path())?
        {
            return Ok(Err("native clone unavailable"));
        }

        // These read-only queries use independent Git processes. Join all
        // workers before using their results to select donor files.
        let (tracked, reason, conversions) = std::thread::scope(|scope| -> Result<_> {
            let config = scope.spawn(|| native_reason(checkout.worktree));
            let attributes = files.iter().any(Entry::attributes).then(|| {
                scope.spawn(|| checkout_conversions(checkout.proof_command(), oid, files))
            });
            let tracked = donor_index(&primary)?;
            let reason = config
                .join()
                .map_err(|_| anyhow::anyhow!("checkout configuration worker failed"))??;
            let conversions = attributes
                .map(|task| task.join())
                .transpose()
                .map_err(|_| anyhow::anyhow!("checkout attribute worker failed"))?;
            Ok((tracked, reason, conversions))
        })?;
        if let Some(reason) = reason {
            return Ok(Err(reason));
        }
        let Some(tracked) = tracked else {
            return Ok(Err("primary checkout unavailable"));
        };
        let tracked = donor_entries(&tracked);
        let mut candidates: Vec<_> = files
            .iter()
            .map(|entry| {
                let mode: &[u8] = if entry.mode == 0o100755 {
                    b"100755"
                } else {
                    b"100644"
                };
                entry.regular()
                    && tracked.get(entry.name.as_slice()) == Some(&(mode, entry.blob.as_bytes()))
            })
            .collect();
        if !candidates.contains(&true) {
            return Ok(Err("no eligible local donor files"));
        }

        if let Some(result) = conversions {
            let Ok(conversions) = result else {
                return Ok(Err("cannot inspect committed attributes"));
            };
            for (eligible, conversion) in candidates.iter_mut().zip(conversions) {
                *eligible &= conversion == Conversion::Raw;
            }
        }
        if !candidates.contains(&true) {
            return Ok(Err("path-dependent checkout conversion"));
        }
        let mode = native::creation_mode(checkout.scratch.path())?;
        Ok(Ok(Plan {
            donor,
            destination,
            candidates,
            mode,
        }))
    }

    // Decline aliases before either Git or clonefile creates their names.
    fn unambiguous_paths(files: &[Entry]) -> bool {
        let mut spellings = HashMap::new();
        for entry in files {
            if !entry.name.is_ascii() {
                return false;
            }
            let lowercase = entry.name.to_ascii_lowercase();
            for end in entry
                .name
                .iter()
                .enumerate()
                .filter_map(|(i, &byte)| (byte == b'/').then_some(i))
                .chain([entry.name.len()])
            {
                let original = &entry.name[..end];
                let directory = end < entry.name.len();
                let key = &lowercase[..end];
                match spellings.get(key) {
                    Some(&(previous, true)) if directory && previous == original => (),
                    Some(_) => return false,
                    None => {
                        spellings.insert(key.to_vec(), (original, directory));
                    }
                }
            }
        }
        true
    }

    fn donor_entries(output: &[u8]) -> HashMap<&[u8], (&[u8], &[u8])> {
        output
            .split(|&byte| byte == 0)
            .filter_map(|record| {
                let tab = record.iter().position(|&byte| byte == b'\t')?;
                let header = record[..tab].strip_suffix(b" 0")?;
                let mode = header.get(..6)?;
                let oid = header.get(7..)?;
                Some((&record[tab + 1..], (mode, oid)))
            })
            .collect()
    }

    fn donor_index(primary: &Path) -> Result<Option<Vec<u8>>> {
        // Git can discover a surrounding repository even if the original one
        // moved. Only use the index belonging to the selected checkout root.
        let location = git_command(primary)
            .args(["rev-parse", "--show-prefix"])
            .output()?;
        if !location.status.success() || location.stdout != b"\n" {
            return Ok(None);
        }
        let output = git_command(primary)
            .args([
                "-c",
                "core.fsmonitor=false",
                "ls-files",
                "--stage",
                "--full-name",
                "-z",
            ])
            .output()?;
        Ok(output.status.success().then_some(output.stdout))
    }

    fn native_reason(worktree: &Path) -> Result<Option<&'static str>> {
        let config = git_command(worktree)
            .args([
                "config",
                "--null",
                "--type=bool",
                "--get-regexp",
                r"^core\.(autocrlf|sparsecheckout)$",
            ])
            .output()?;
        if !matches!(config.status.code(), Some(0 | 1)) {
            // A valid core.autocrlf=input is deliberately delegated to Git.
            return Ok(Some("Git checkout configuration"));
        }
        let enabled = |key: &[u8]| {
            config
                .stdout
                .split(|&byte| byte == 0)
                .filter_map(|field| field.strip_prefix(key))
                .next_back()
                == Some(b"true")
        };
        if enabled(b"core.sparsecheckout\n") || enabled(b"core.autocrlf\n") {
            return Ok(Some("Git checkout configuration"));
        }
        for variable in ["GIT_ATTR_SYSTEM", "GIT_ATTR_GLOBAL"] {
            if variable == "GIT_ATTR_SYSTEM"
                && std::env::var_os("GIT_ATTR_NOSYSTEM").as_deref() == Some(OsStr::new("1"))
            {
                continue;
            }
            let Ok(output) = git(worktree, &["var", variable]) else {
                return Ok(Some("cannot determine external attributes"));
            };
            if let Some(reason) = external_attributes(&output.stdout) {
                return Ok(Some(reason));
            }
        }
        if let Some(reason) = existing_attributes(&worktree.join(".git/info/attributes")) {
            return Ok(Some(reason));
        }
        if !git(
            worktree,
            &[
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "refs/replace",
            ],
        )?
        .stdout
        .is_empty()
        {
            return Ok(Some("Git replacement objects"));
        }
        Ok(None)
    }

    pub(super) fn install<F: FnOnce() -> Result<()>>(
        checkout: &LocalCheckout<'_>,
        oid: &str,
        files: &[Entry],
        plan: Plan,
        ready: F,
    ) -> Result<Value> {
        let mut command = checkout.proof_command();
        command.env("GIT_NO_REPLACE_OBJECTS", "1").args([
            "-c",
            "core.protectHFS=true",
            "-c",
            "core.protectNTFS=true",
            "read-tree",
            oid,
        ]);
        run(command)?;

        let mut receipt = Receipt::default();
        let sha256 = oid.len() == 64;
        let batches = std::thread::scope(|scope| -> Result<Vec<_>> {
            let mut tasks = Vec::new();
            let mut flags = plan.candidates.as_slice();
            for chunk in source_chunks(files) {
                let (eligible, remaining) = flags.split_at(chunk.len());
                flags = remaining;
                let donor = plan.donor.fork()?;
                let destination = plan.destination.fork()?;
                let mode = plan.mode;
                #[cfg(target_os = "linux")]
                let scratch = tempfile::tempdir_in(checkout.scratch.path())?;
                tasks.push(scope.spawn(move || {
                    #[cfg(target_os = "linux")]
                    let temporary = native::Root::open(scratch.path())?;
                    clone_batch(
                        chunk,
                        eligible,
                        &donor,
                        &destination,
                        mode,
                        sha256,
                        #[cfg(target_os = "linux")]
                        &temporary,
                    )
                }));
            }
            tasks
                .into_iter()
                .map(|task| {
                    task.join()
                        .map_err(|_| anyhow::anyhow!("local clone worker failed"))?
                })
                .collect()
        })?;
        let mut missing = Vec::new();
        for (cloned, paths) in batches {
            receipt.cloned.merge(cloned);
            missing.extend(paths);
        }
        // The private index is not published and Git cannot materialize any
        // missing paths until the full pack and live workspace are verified.
        ready()?;
        if !missing.is_empty() {
            let mut command = parallel_checkout(checkout.command());
            command
                .env("GIT_ATTR_SOURCE", oid)
                .env("GIT_NO_REPLACE_OBJECTS", "1")
                .args(["checkout-index", "--stdin", "-z"])
                .stdin(Stdio::from(path_input(missing.iter().copied())?));
            run(command)?;
            count_native(checkout.worktree, missing.into_iter(), &mut receipt.private);
        }
        // Cloned entries retain read-tree's empty stat fields. No later stat
        // refresh can bless an edit that arrived after a path was installed.
        receipt.primary.files = receipt.cloned.files;
        receipt.primary.bytes = receipt.cloned.bytes;
        let backend = match (receipt.cloned.files, cfg!(target_os = "macos")) {
            (0, _) => "git",
            (_, true) => "clonefile",
            (_, false) => "reflink",
        };
        Ok(receipt.json(backend, None))
    }

    fn clone_batch<'a>(
        files: &'a [Entry],
        candidates: &[bool],
        donor: &native::Root,
        destination: &native::Root,
        mode: u32,
        sha256: bool,
        #[cfg(target_os = "linux")] scratch: &native::Root,
    ) -> Result<(Counts, Vec<&'a Entry>)> {
        let mut counts = Counts::default();
        let mut missing = Vec::new();
        let mut buffer = [0; 64 * 1024];
        for (entry, &eligible) in files.iter().zip(candidates) {
            if !eligible {
                missing.push(entry);
                continue;
            }
            let path = entry.path()?;
            let Ok((mut file, metadata)) = donor.regular_metadata(path) else {
                missing.push(entry);
                continue;
            };
            let mode = entry.creation_mode(mode);
            let same_mode = if cfg!(target_os = "macos") {
                metadata.mode() & 0o7777 == mode
            } else {
                metadata.mode() & 0o100 == mode & 0o100
            };
            // A tracked index entry can still name a staged change, an
            // assume-unchanged file or a checkout produced with old filters.
            // Hash the source bytes independently, including same-size edits.
            if !same_mode
                || !native::plain(&file, &metadata).unwrap_or(false)
                || !matches_blob(&mut file, metadata.len(), &mut buffer, &entry.blob, sha256)
                    .unwrap_or(false)
            {
                missing.push(entry);
                continue;
            }
            destination.create_parent(path)?;
            let stamp = cache::Stamp::from(&metadata);
            if cache::Stamp::from(&file.metadata()?) != stamp {
                missing.push(entry);
                continue;
            }
            #[cfg(target_os = "macos")]
            let cloned = destination.clone_regular(path, &file, &metadata, mode, true);
            #[cfg(target_os = "linux")]
            let cloned = scratch.clone_regular(Path::new("clone"), &file, &metadata, mode, true);
            if let Err(error) = cloned {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    return Err(error).with_context(|| {
                        format!("checkout destination exists: {}", path.display())
                    });
                }
                missing.push(entry);
                continue;
            }
            let stable = cache::Stamp::from(&file.metadata()?) == stamp;
            #[cfg(target_os = "macos")]
            ensure!(
                stable,
                "local donor changed during clone: {}",
                path.display()
            );
            #[cfg(target_os = "linux")]
            {
                if !stable {
                    scratch.discard(Path::new("clone"))?;
                    missing.push(entry);
                    continue;
                }
                destination.install_local_clone(path, scratch)?;
            }
            counts.add(metadata.len());
        }
        Ok((counts, missing))
    }

    fn matches_blob(
        file: &mut File,
        size: u64,
        buffer: &mut [u8],
        oid: &str,
        sha256: bool,
    ) -> io::Result<bool> {
        fn hash_matches(
            mut hasher: impl Digest,
            mut file: impl Read,
            size: u64,
            buffer: &mut [u8],
            oid: &str,
        ) -> io::Result<bool> {
            hasher.update(format!("blob {size}\0"));
            let mut remaining = size;
            while remaining > 0 {
                let length = remaining.min(buffer.len() as u64) as usize;
                file.read_exact(&mut buffer[..length])?;
                hasher.update(&buffer[..length]);
                remaining -= length as u64;
            }

            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            let bytes = hasher.finalize();
            Ok(oid.len() == bytes.len() * 2
                && bytes
                    .into_iter()
                    .zip(oid.as_bytes().chunks(2))
                    .all(|(byte, hex)| {
                        hex == [
                            DIGITS[usize::from(byte >> 4)],
                            DIGITS[usize::from(byte & 15)],
                        ]
                    }))
        }
        if sha256 {
            hash_matches(sha2::Sha256::new(), file, size, buffer, oid)
        } else {
            hash_matches(sha1::Sha1::new(), file, size, buffer, oid)
        }
    }
}
