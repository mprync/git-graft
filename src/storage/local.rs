//! Direct CoW reuse from a trusted local checkout. Git still owns the tree and
//! index, and supplies files whose checkout bytes cannot come from the donor.
use super::*;
use std::path::PathBuf;

#[derive(Default)]
struct CheckoutMetadata<'a> {
    objects: Option<&'a Path>,
    index: Option<&'a TipIndex>,
}

pub(crate) fn materialize_local(
    workspace: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: &Path,
) -> Result<Value> {
    let workspace = dunce::canonicalize(workspace)?;
    materialize(
        &workspace,
        state,
        oid,
        sparse,
        primary,
        CheckoutMetadata::default(),
        || Ok(()),
    )
}

/// Prepare files from a verified manifest and join history checks before index publication.
pub(crate) fn materialize_planned<F: FnOnce() -> Result<()>>(
    workspace: &Path,
    state: &Path,
    oid: &str,
    primary: &Path,
    proof_objects: Option<&Path>,
    manifest: Option<&TipIndex>,
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
            CheckoutMetadata {
                objects: proof_objects,
                index: manifest,
            },
            || {
                ready.take().expect("checkout readiness called once")()?;
                checkout_identity(&workspace, oid)?;
                Ok(())
            },
        )
    })();
    // Always join the history checks, including when planning fails early.
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
    metadata: CheckoutMetadata<'_>,
    ready: F,
) -> Result<Value> {
    // Blob sizes only depend on object IDs, so the primary can report them
    // while the workspace index is prepared.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return std::thread::scope(|scope| {
        let sizes = metadata.index.map(|manifest| {
            scope.spawn(move || direct::blob_sizes(|| git_command(primary), &manifest.entries))
        });
        materialize_with(
            workspace, state, oid, sparse, primary, metadata, ready, sizes,
        )
    });
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    materialize_with(workspace, state, oid, sparse, primary, metadata, ready, ())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
type Sizes<'s> =
    Option<std::thread::ScopedJoinHandle<'s, Option<std::collections::HashMap<String, u64>>>>;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
type Sizes<'s> = ();

#[allow(clippy::too_many_arguments)]
fn materialize_with<F: FnOnce() -> Result<()>>(
    workspace: &Path,
    state: &Path,
    oid: &str,
    sparse: &[String],
    primary: &Path,
    metadata: CheckoutMetadata<'_>,
    ready: F,
    sizes: Sizes<'_>,
) -> Result<Value> {
    let proof_objects = metadata.objects;
    let oid = checkout_identity_with_objects(workspace, oid, proof_objects)?;
    validate_sparse(workspace, &oid, sparse)?;
    let mut checkout = LocalCheckout::reserve(workspace, proof_objects)?;
    crate::trace::mark("checkout: identity and reserve");
    let listed;
    let files = if let Some(manifest) = metadata.index {
        if manifest.prepare(&checkout).is_err() {
            ready()?;
            drop(checkout);
            return materialize_local(workspace, state, &oid, sparse, primary);
        }
        checkout.planned = true;
        crate::trace::mark("checkout: prepare index");
        &manifest.entries
    } else {
        listed = entries_with_objects(workspace, &oid, proof_objects)?;
        &listed
    };

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let receipt = {
        let planned = direct::plan(&checkout, &oid, sparse, files, state, primary, sizes)?;
        crate::trace::mark("checkout: plan donors");
        match planned {
            Ok(plan) => direct::install(&checkout, &oid, files, plan, ready)?,
            Err(reason) => {
                ready()?;
                checkout.native(&oid, sparse, files, reason)?
            }
        }
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let receipt = {
        let _ = (state, primary, sizes);
        ready()?;
        // Report the same reason as the cloning platforms for a sparse request.
        let reason = if sparse.is_empty() {
            "native clone unavailable"
        } else {
            "sparse checkout"
        };
        checkout.native(&oid, sparse, files, reason)?
    };
    checkout.publish()?;
    Ok(receipt)
}

pub(super) struct LocalCheckout<'a> {
    planned: bool,
    worktree: &'a Path,
    scratch: tempfile::TempDir,
    index: PathBuf,
    index_lock: tempfile::TempPath,
    proof_objects: Option<&'a Path>,
}

impl<'a> LocalCheckout<'a> {
    fn reserve(worktree: &'a Path, proof_objects: Option<&'a Path>) -> Result<Self> {
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
            planned: false,
            worktree,
            scratch,
            index,
            index_lock,
            proof_objects,
        })
    }

    pub(super) fn proof_command(&self) -> Command {
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

    fn native(
        &mut self,
        oid: &str,
        sparse: &[String],
        files: &[Entry],
        reason: &str,
    ) -> Result<Value> {
        if self.planned {
            self.index = self.scratch.path().join("fallback-index");
        }
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

pub(super) fn run(mut command: Command) -> Result<()> {
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
pub(super) mod direct {
    use super::*;
    use sha1::Digest;
    use std::{collections::HashMap, fs::File, io::Read, os::unix::fs::MetadataExt};

    #[cfg(target_os = "macos")]
    mod tree;

    pub(super) struct Plan {
        donor: native::Root,
        destination: native::Root,
        candidates: Vec<bool>,
        /// Entries Git checks out with every LF expanded to CRLF. These are
        /// proven only by the primary's stat data at the expanded size; their
        /// raw hash can never match what Git writes.
        crlf: Vec<bool>,
        mode: u32,
        /// Symbolic links may be installed as links.
        #[cfg(target_os = "macos")]
        links: bool,
        /// The primary's index stat data, trusted as `git status` trusts it.
        trusted: Option<index_stat::Trusted>,
    }

    pub(super) fn plan(
        checkout: &LocalCheckout<'_>,
        oid: &str,
        sparse: &[String],
        files: &[Entry],
        state: &Path,
        primary: &Path,
        sizes: super::Sizes<'_>,
    ) -> Result<std::result::Result<Plan, &'static str>> {
        if !sparse.is_empty() {
            return Ok(Err("sparse checkout"));
        }
        if files.is_empty() {
            return Ok(Err("empty tree"));
        }
        let gitlinks = files.iter().any(|entry| entry.mode == 0o160000);
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
        let (tracked, policy, conversions, sizes) = std::thread::scope(|scope| -> Result<_> {
            let config = scope.spawn(|| raw_file_policy(checkout.worktree, gitlinks));
            let sizes = match sizes {
                Some(early) => early,
                None => scope.spawn(|| blob_sizes(|| checkout.proof_command(), files)),
            };
            // Git also applies info/attributes and user and system rules when
            // the selected tree has no .gitattributes of its own.
            let attributes = scope.spawn(|| {
                let source = if checkout.planned {
                    AttributeSource::Index
                } else {
                    AttributeSource::Commit(oid)
                };
                checkout_conversions(checkout.proof_command(), source, files.iter())
            });
            let tracked = donor_index(&primary)?;
            let policy = config
                .join()
                .map_err(|_| anyhow::anyhow!("checkout configuration worker failed"))??;
            let conversions = attributes
                .join()
                .map_err(|_| anyhow::anyhow!("checkout attribute worker failed"))?;
            Ok((
                tracked,
                policy,
                conversions,
                crate::workspace::joined(sizes),
            ))
        })?;
        let endings = match policy {
            Ok(value) => value,
            Err(reason) => return Ok(Err(reason)),
        };
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

        let Ok(conversions) = conversions else {
            return Ok(Err("cannot inspect checkout attributes"));
        };
        // Paths without line-ending attributes that core.autocrlf=true may expand.
        let mut automatic = Vec::new();
        for (index, (eligible, conversion)) in candidates.iter_mut().zip(conversions).enumerate() {
            *eligible &= match conversion {
                Conversion::Raw if endings.auto_crlf => {
                    if *eligible {
                        automatic.push(index);
                    }
                    true
                }
                Conversion::Raw => true,
                Conversion::Text => endings.text_uses_lf,
                Conversion::Lf => true,
                Conversion::Eol | Conversion::Git => false,
            };
        }
        let mut trusted = sizes.and_then(|sizes| {
            index_stat::Trusted::read(&primary.join(".git/index"), files[0].blob.len() / 2, sizes)
        });
        let mut crlf = vec![false; files.len()];
        if !automatic.is_empty() {
            let blobs = automatic.iter().map(|&index| files[index].blob.as_str());
            let written = auto_crlf_checkouts(|| checkout.proof_command(), blobs);
            crate::trace::mark("checkout: classify automatic line endings");
            let mut expanded = HashMap::new();
            for index in automatic {
                let blob = &files[index].blob;
                match written.as_ref().and_then(|written| written.get(blob)) {
                    Some(Written::Raw) => {}
                    // Without the primary's stat data nothing can prove CRLF bytes.
                    Some(&Written::Crlf(size)) if trusted.is_some() => {
                        crlf[index] = true;
                        expanded.insert(blob.clone(), size);
                    }
                    _ => candidates[index] = false,
                }
            }
            if !expanded.is_empty() {
                // The size proof assumes the primary's own attributes, which
                // may differ from the selected commit's, convert only line
                // endings. One batched query covers every expanded path.
                let indices: Vec<usize> = (0..files.len()).filter(|&index| crlf[index]).collect();
                let primary_conversions = checkout_conversions(
                    git_command(&primary),
                    AttributeSource::Worktree,
                    indices.iter().map(|&index| &files[index]),
                );
                crate::trace::mark("checkout: primary attributes of expanded paths");
                // Any failure leaves every expanded path to Git.
                let primary_conversions = primary_conversions.unwrap_or_default();
                for (position, index) in indices.into_iter().enumerate() {
                    if primary_conversions.get(position) != Some(&Conversion::Raw) {
                        crlf[index] = false;
                        candidates[index] = false;
                    }
                }
            }
            if let Some(trusted) = trusted.as_mut() {
                trusted.expand(expanded);
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
            crlf,
            mode,
            #[cfg(target_os = "macos")]
            links: endings.links,
            trusted,
        }))
    }

    /// What Git writes at checkout for a blob on a path with no `text`, `eol`
    /// or `crlf` attribute under `core.autocrlf=true` (convert.c's
    /// `CRLF_AUTO_CRLF` action).
    ///
    /// Git expands the blob only when `will_convert_lf_to_crlf` holds: it has
    /// at least one LF, no CR at all (neither a lone CR nor CRLF), and is not
    /// binary by `convert_is_binary`. The expansion inserts one CR before
    /// every LF, so the checkout is the blob's size plus its LF count.
    /// Otherwise Git writes the blob's bytes unchanged.
    ///
    /// # Why a stat-clean file of the expected size holds exactly these bytes
    ///
    /// [`index_stat::Trusted::holds`] accepts a donor file F only when Git's
    /// own stat test on a non-racy index entry vouches that cleaning F gives
    /// the staged blob B. Git's line-ending clean (`crlf_to_git`, the same
    /// premise the raw path relies on: no filter, `ident` or encoding) never
    /// adds or changes bytes; it only removes CRs that immediately precede an
    /// LF. So F is B with `size(F) - size(B)` CRs inserted, each directly
    /// before a distinct LF of B (a second CR before the same LF would remain
    /// after cleaning, but B has no CR when Git expands it).
    ///
    /// - When Git expands B, the expected size is `size(B) + LFs(B)`: a CR
    ///   was inserted before every LF and nowhere else, which is exactly
    ///   Git's expanded output.
    /// - When Git writes B unchanged, the expected size is `size(B)`: no CR
    ///   was removed, so F == B.
    ///
    /// A file Git calls clean but with LF endings (rewritten after checkout)
    /// is as large as B, not the expansion, and goes to Git. The raw hash of
    /// an expanded path can never prove Git's bytes, so those paths are never
    /// hashed.
    ///
    /// ## The primary's own attributes
    ///
    /// The premise that only Git's line-ending clean stands between F and B
    /// holds for the primary's attributes, not the selected commit's, which
    /// decide what the graft gets. The primary's working tree can carry an
    /// older or uncommitted `.gitattributes`, `.git/info/attributes` or
    /// external rules with `ident`, a `filter`, `working-tree-encoding`, or
    /// `text`, `eol` or `crlf` settings. Those smudge or clean more than CRs,
    /// and `ident` adds bytes, so a file they wrote could reach the expanded
    /// size by coincidence while Git's stat test still vouches for it. So
    /// `plan` also runs one batched `git check-attr --all --stdin` in the
    /// primary, reading its working tree as its own clean does, and expands
    /// a path only when no such attribute applies there. Any other answer,
    /// or no answer, leaves the path to Git.
    ///
    /// [`Written::Raw`] paths do not take this check: like every path on the
    /// default checkout, they are proven by the blob's own size with the
    /// stat test, or by hashing F against B, and a primary conversion that
    /// changes the size fails the size test. They share the default path's
    /// remaining premise that the primary applies no size-preserving filter.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Written {
        Raw,
        /// Expanded to CRLF; the checkout's size in bytes.
        Crlf(u64),
    }

    /// Git's `gather_stats` over one blob, as a histogram of byte values.
    struct TextStats {
        counts: [u64; 256],
        size: u64,
        last: Option<u8>,
    }

    impl TextStats {
        fn new() -> Self {
            Self {
                counts: [0; 256],
                size: 0,
                last: None,
            }
        }

        fn update(&mut self, bytes: &[u8]) {
            for &byte in bytes {
                self.counts[usize::from(byte)] += 1;
            }
            self.size += bytes.len() as u64;
            if let Some(&byte) = bytes.last() {
                self.last = Some(byte);
            }
        }

        /// `will_convert_lf_to_crlf` for `CRLF_AUTO_CRLF` (convert.c, Git 2.46).
        fn written(&self) -> Written {
            let lf = self.counts[usize::from(b'\n')];
            // Any CR, lone or in CRLF, leaves the blob untouched, and a NUL is binary.
            if lf == 0 || self.counts[usize::from(b'\r')] > 0 || self.counts[0] > 0 {
                return Written::Raw;
            }
            // DEL and control bytes other than BS, HT, ESC and FF are
            // non-printable; CR and LF count as neither.
            let mut nonprintable = self.counts[127]
                + (0..32u8)
                    .filter(|byte| {
                        !matches!(byte, b'\x08' | b'\t' | b'\x1b' | b'\x0c' | b'\n' | b'\r')
                    })
                    .map(|byte| self.counts[usize::from(byte)])
                    .sum::<u64>();
            let printable = self.size - lf - nonprintable;
            // A trailing Ctrl-Z (DOS end of file) does not count.
            if self.last == Some(0x1a) {
                nonprintable -= 1;
            }
            if (printable >> 7) < nonprintable {
                Written::Raw
            } else {
                Written::Crlf(self.size + lf)
            }
        }
    }

    /// What Git writes for each of `blobs` under `core.autocrlf=true` on a
    /// path without line-ending attributes, or `None` if Git cannot supply
    /// them all. Blobs too large for Git's 32-bit counters are left out.
    /// Reading blob contents is costly, so large sets use several processes.
    fn auto_crlf_checkouts<'b>(
        command: impl Fn() -> Command + Sync,
        blobs: impl Iterator<Item = &'b str>,
    ) -> Option<HashMap<String, Written>> {
        let mut seen = std::collections::HashSet::new();
        let blobs: Vec<&str> = blobs.filter(|blob| seen.insert(*blob)).collect();
        let workers = if blobs.len() >= 1024 { 4 } else { 1 };
        let shards = std::thread::scope(|scope| {
            let handles: Vec<_> = blobs
                .chunks(blobs.len().div_ceil(workers).max(1))
                .map(|shard| scope.spawn(|| shard_checkouts(command(), shard)))
                .collect();
            handles
                .into_iter()
                .map(crate::workspace::joined)
                .collect::<Option<Vec<_>>>()
        })?;
        Some(shards.into_iter().flatten().collect())
    }

    /// Stream `blobs` from `git cat-file --batch`, whose headers give raw
    /// sizes (unlike `--filters`, which reports the unconverted size).
    fn shard_checkouts(mut command: Command, blobs: &[&str]) -> Option<Vec<(String, Written)>> {
        use std::io::BufRead;
        let mut stdin = tempfile::tempfile().ok()?;
        for blob in blobs {
            std::io::Write::write_all(&mut stdin, blob.as_bytes()).ok()?;
            std::io::Write::write_all(&mut stdin, b"\n").ok()?;
        }
        std::io::Seek::rewind(&mut stdin).ok()?;
        let mut child = command
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .args(["cat-file", "--batch", "--buffer"])
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let read = (|| {
            let mut out = io::BufReader::with_capacity(256 * 1024, child.stdout.take()?);
            let mut written = Vec::with_capacity(blobs.len());
            let mut header = Vec::new();
            for blob in blobs {
                header.clear();
                out.read_until(b'\n', &mut header).ok()?;
                let line = std::str::from_utf8(header.strip_suffix(b"\n")?).ok()?;
                let mut fields = line.split(' ');
                let (Some(oid), Some("blob"), Some(size), None) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    return None;
                };
                let size: u64 = size.parse().ok()?;
                if oid != *blob {
                    return None;
                }
                let mut stats = TextStats::new();
                let mut remaining = size;
                while remaining > 0 {
                    let available = out.fill_buf().ok()?;
                    if available.is_empty() {
                        return None;
                    }
                    let length = available.len().min(remaining as usize);
                    stats.update(&available[..length]);
                    out.consume(length);
                    remaining -= length as u64;
                }
                let mut end = [0];
                io::Read::read_exact(&mut out, &mut end).ok()?;
                if end != *b"\n" {
                    return None;
                }
                if size <= u64::from(u32::MAX) {
                    written.push(((*blob).to_owned(), stats.written()));
                }
            }
            let mut rest = [0];
            (io::Read::read(&mut out, &mut rest).ok()? == 0).then_some(written)
        })();
        let status = child.wait().ok()?;
        read.filter(|_| status.success())
    }

    /// Each regular file blob's size, or `None` if Git cannot report them all.
    /// Reading a loose object's size inflates its header, so large sets are
    /// split across several Git processes made by `command`.
    pub(super) fn blob_sizes(
        command: impl Fn() -> Command + Sync,
        files: &[Entry],
    ) -> Option<HashMap<String, u64>> {
        let mut seen = std::collections::HashSet::new();
        let blobs: Vec<&str> = files
            .iter()
            .filter(|entry| entry.regular())
            .map(|entry| entry.blob.as_str())
            .filter(|blob| seen.insert(*blob))
            .collect();
        let workers = if blobs.len() >= 1024 { 4 } else { 1 };
        let shards = std::thread::scope(|scope| {
            let handles: Vec<_> = blobs
                .chunks(blobs.len().div_ceil(workers).max(1))
                .map(|shard| scope.spawn(|| shard_sizes(command(), shard)))
                .collect();
            handles
                .into_iter()
                .map(crate::workspace::joined)
                .collect::<Option<Vec<_>>>()
        })?;
        let sizes: HashMap<String, u64> = shards.into_iter().flatten().collect();
        (sizes.len() == blobs.len()).then_some(sizes)
    }

    fn shard_sizes(mut command: Command, blobs: &[&str]) -> Option<Vec<(String, u64)>> {
        let mut input = Vec::with_capacity(blobs.len() * 65);
        for blob in blobs {
            input.extend_from_slice(blob.as_bytes());
            input.push(b'\n');
        }
        let mut stdin = tempfile::tempfile().ok()?;
        std::io::Write::write_all(&mut stdin, &input).ok()?;
        std::io::Seek::rewind(&mut stdin).ok()?;
        command
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .args([
                "cat-file",
                "--batch-check=%(objectname) %(objecttype) %(objectsize)",
                "--buffer",
            ])
            .stdin(Stdio::from(stdin));
        let output = crate::git::bounded_stdout(&mut command, 64 << 20).ok()?;
        let mut sizes = Vec::with_capacity(blobs.len());
        for line in output
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
        {
            let line = std::str::from_utf8(line).ok()?;
            let mut fields = line.split(' ');
            let (Some(oid), Some("blob"), Some(size), None) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return None;
            };
            sizes.push((oid.to_owned(), size.parse().ok()?));
        }
        Some(sizes)
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
        // The listing is only used once the prefix proves it belongs to this
        // checkout root, so both read-only queries can run at once.
        let (location, output) = std::thread::scope(|scope| {
            let output = scope.spawn(|| donor_listing(primary).output());
            (
                donor_prefix(primary).output(),
                crate::workspace::joined(output),
            )
        });
        let location = location?;
        if !location.status.success() || location.stdout != b"\n" {
            return Ok(None);
        }
        let output = output?;
        Ok(output.status.success().then_some(output.stdout))
    }

    fn donor_prefix(primary: &Path) -> Command {
        // Git can discover a surrounding repository even if the original one
        // moved. Only use the index belonging to the selected checkout root.
        let mut location = git_command(primary);
        location.args(["rev-parse", "--show-prefix"]);
        location
    }

    fn donor_listing(primary: &Path) -> Command {
        let mut listing = git_command(primary);
        listing.args([
            "-c",
            "core.fsmonitor=false",
            "ls-files",
            "--stage",
            "--full-name",
            "-z",
        ]);
        listing
    }

    struct LineEndings {
        /// `core.autocrlf=true`: Git may expand LF to CRLF on paths without
        /// line-ending attributes, depending on each blob's contents.
        auto_crlf: bool,
        text_uses_lf: bool,
        /// Whether Git checks symbolic links out as links (`core.symlinks`).
        /// Only macOS installs links by directory clone.
        #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
        links: bool,
    }

    fn raw_file_policy(
        worktree: &Path,
        gitlinks: bool,
    ) -> Result<Result<LineEndings, &'static str>> {
        let settings = if gitlinks {
            r"^(core\.(autocrlf|eol|sparsecheckout|symlinks)|submodule\.recurse)$"
        } else {
            r"^core\.(autocrlf|eol|sparsecheckout|symlinks)$"
        };
        let config = git_command(worktree)
            .args([
                "config",
                "--null",
                "--type=bool-or-str",
                "--get-regexp",
                settings,
            ])
            .output()?;
        if !matches!(config.status.code(), Some(0 | 1)) {
            return Ok(Err("Git checkout configuration"));
        }
        let mut autocrlf: &[u8] = b"false";
        let mut eol_crlf = false;
        let mut sparse = false;
        let mut recurse = false;
        // Git creates symbolic links unless core.symlinks is false.
        let mut links = true;
        for field in config.stdout.split(|&b| b == 0).filter(|f| !f.is_empty()) {
            let mut parts = field.splitn(2, |&b| b == b'\n');
            let key = parts.next().unwrap_or_default();
            let value = parts.next().unwrap_or_default();
            let boolean = matches!(value, b"true" | b"false");
            match key {
                b"core.autocrlf" if boolean || value.eq_ignore_ascii_case(b"input") => {
                    autocrlf = value
                }
                b"core.eol" => eol_crlf = value.eq_ignore_ascii_case(b"crlf"),
                b"core.sparsecheckout" if boolean => sparse = value == b"true",
                b"submodule.recurse" if boolean => recurse = value == b"true",
                b"core.symlinks" if boolean => links = value == b"true",
                _ => return Ok(Err("Git checkout configuration")),
            }
        }
        if sparse {
            return Ok(Err("Git checkout configuration"));
        }
        if recurse {
            return Ok(Err("submodules"));
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
            return Ok(Err("Git replacement objects"));
        }
        // Both direct-clone platforms use LF natively. Git's input mode also
        // suppresses checkout expansion when core.eol requests CRLF.
        Ok(Ok(LineEndings {
            auto_crlf: autocrlf == b"true",
            text_uses_lf: autocrlf.eq_ignore_ascii_case(b"input")
                || (autocrlf == b"false" && !eol_crlf),
            links,
        }))
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
        if !checkout.planned {
            run(command)?;
            // read-tree can create an index even when a referenced blob is missing.
            // Check the object store before a donor working file can mask that loss.
            crate::git::require_blobs(
                checkout.proof_command(),
                files
                    .iter()
                    .filter(|entry| entry.mode != 0o160000)
                    .map(|entry| entry.blob.as_str()),
            )?;
        }

        let mut receipt = Receipt::default();
        let sha256 = oid.len() == 64;
        #[cfg(target_os = "macos")]
        let reuse_ancestors = native::parents::available();
        #[cfg(not(target_os = "macos"))]
        let reuse_ancestors = false;
        // Leave conversions and submodule directory creation in Git's original order.
        let overlap_links = checkout.planned
            && files.len() >= PARALLEL_THRESHOLD
            && files.iter().any(|entry| entry.mode == 0o120000)
            && files
                .iter()
                .zip(&plan.candidates)
                .all(|(entry, eligible)| *eligible || entry.mode == 0o120000);
        let checkout_paths = |input: File| -> Result<()> {
            let mut command = parallel_checkout(checkout.command());
            command
                .env("GIT_ATTR_SOURCE", oid)
                .env("GIT_NO_REPLACE_OBJECTS", "1")
                .args(["checkout-index", "--stdin", "-z"])
                .stdin(Stdio::from(input));
            run(command)?;
            Ok(())
        };
        let mut ready = Some(ready);
        // Whole clean directories first; the per-file workers skip their entries.
        #[cfg(target_os = "macos")]
        let directories = tree::clone_directories(
            files,
            &plan.candidates,
            &plan.donor,
            &plan.destination,
            tree::Policy {
                trusted: plan.trusted.as_ref(),
                crlf: &plan.crlf,
                links: plan.links,
                mode: plan.mode,
                sha256,
                reuse_ancestors,
            },
        )?;
        #[cfg(target_os = "macos")]
        let covered = directories.covered.as_slice();
        #[cfg(target_os = "linux")]
        let covered = vec![false; files.len()];
        #[cfg(target_os = "linux")]
        let covered = covered.as_slice();
        crate::trace::mark(&format!(
            "checkout: clone directories ({} of {} entries)",
            covered.iter().filter(|covered| **covered).count(),
            files.len()
        ));
        let batches = std::thread::scope(|scope| -> Result<Vec<_>> {
            let mut tasks = Vec::new();
            let mut flags = plan.candidates.as_slice();
            let mut expanded = plan.crlf.as_slice();
            let mut skipped = covered;
            for chunk in source_chunks(files) {
                let (eligible, remaining) = flags.split_at(chunk.len());
                flags = remaining;
                let (crlf, remaining) = expanded.split_at(chunk.len());
                expanded = remaining;
                let (skip, remaining) = skipped.split_at(chunk.len());
                skipped = remaining;
                let donor = plan.donor.checkout_worker(reuse_ancestors)?;
                let destination = plan.destination.checkout_worker(reuse_ancestors)?;
                let mode = plan.mode;
                let trusted = plan.trusted.as_ref();
                #[cfg(target_os = "linux")]
                let scratch = tempfile::tempdir_in(checkout.scratch.path())?;
                tasks.push(scope.spawn(move || {
                    #[cfg(target_os = "linux")]
                    let temporary = native::Root::open(scratch.path())?;
                    clone_batch(
                        chunk,
                        eligible,
                        crlf,
                        skip,
                        &donor,
                        &destination,
                        mode,
                        sha256,
                        trusted,
                        #[cfg(target_os = "linux")]
                        &temporary,
                    )
                }));
            }
            if overlap_links {
                ready.take().expect("history readiness is checked once")()?;
                let links = files
                    .iter()
                    .zip(covered)
                    .filter(|(entry, covered)| entry.mode == 0o120000 && !**covered)
                    .map(|(entry, _)| entry);
                // Git treats EEXIST as a checkout failure if a clone worker
                // creates a missing directory between Git's lookup and mkdir.
                for link in links.clone() {
                    plan.destination.create_parent(link.path()?)?;
                }
                checkout_paths(path_input(links)?)?;
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
        let mut installed = HashMap::new();
        #[cfg(target_os = "macos")]
        let batches = batches.into_iter().chain(std::iter::once((
            directories.counts,
            directories.missing,
            directories.installed,
        )));
        for (cloned, paths, stats) in batches {
            receipt.cloned.merge(cloned);
            missing.extend(paths);
            installed.extend(stats.into_iter().map(|(entry, stat)| {
                let record = index_stat::Installed {
                    blob: entry.blob.clone(),
                    mode: entry.mode,
                    stat,
                };
                (entry.name.clone(), record)
            }));
        }
        if let Some(ready) = ready {
            ready()?;
        } else {
            // HEAD can change after native link checkout while regular files are still cloning.
            checkout_identity(checkout.worktree, oid)?;
        }
        if !missing.is_empty() {
            // checkout-index accepts an existing gitlink directory, including
            // a populated repository. Reserve each one exclusively first.
            for entry in missing.iter().filter(|entry| entry.mode == 0o160000) {
                let path = entry.path()?;
                plan.destination
                    .create_empty_directory(path)
                    .with_context(|| format!("reserve submodule directory {}", path.display()))?;
            }
            let mut remaining = missing
                .iter()
                .copied()
                .filter(|e| !overlap_links || e.mode != 0o120000)
                .peekable();
            if remaining.peek().is_some() {
                checkout_paths(path_input(remaining)?)?;
            }
            count_native(checkout.worktree, missing.into_iter(), &mut receipt.private);
        }
        // Like Git's own checkout, record each cloned file's stat as captured
        // immediately after installation; no later refresh blesses an edit.
        crate::trace::mark("checkout: clone files");
        index_stat::record(&checkout.index, &installed)?;
        receipt.primary.files = receipt.cloned.files;
        receipt.primary.bytes = receipt.cloned.bytes;
        let backend = match (receipt.cloned.files, cfg!(target_os = "macos")) {
            (0, _) => "git",
            (_, true) => "clonefile",
            (_, false) => "reflink",
        };
        Ok(receipt.json(backend, None))
    }

    /// Cloned bytes, entries Git must check out, and cloned entries' stat data.
    type Batch<'a> = (Counts, Vec<&'a Entry>, Vec<(&'a Entry, index_stat::Stat)>);

    /// Clone `files` one by one. Entries flagged in `skip` were already
    /// installed; entries flagged in `crlf` are proven only by stat data.
    #[allow(clippy::too_many_arguments)]
    fn clone_batch<'a>(
        files: &'a [Entry],
        candidates: &[bool],
        crlf: &[bool],
        skip: &[bool],
        donor: &native::Root,
        destination: &native::Root,
        mode: u32,
        sha256: bool,
        trusted: Option<&index_stat::Trusted>,
        #[cfg(target_os = "linux")] scratch: &native::Root,
    ) -> Result<Batch<'a>> {
        let mut counts = Counts::default();
        let mut missing = Vec::new();
        let mut installed = Vec::new();
        let mut buffer = [0; 64 * 1024];
        for (((entry, &eligible), &crlf), &done) in files.iter().zip(candidates).zip(crlf).zip(skip)
        {
            if done {
                continue;
            }
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
            // Git's own stat test, unchanged across the clone below, proves
            // the bytes; otherwise hash them, catching same-size edits. A
            // blob's hash cannot prove the CRLF bytes Git expands it to.
            let trusted = native::plain(&file, &metadata).unwrap_or(false)
                && trusted.is_some_and(|trusted| {
                    trusted.holds(
                        &entry.name,
                        &entry.blob,
                        entry.mode,
                        &index_stat::Stat::from_metadata(&metadata),
                        metadata.len(),
                        crlf,
                    )
                });
            if !same_mode
                || (!trusted
                    && (crlf
                        || !verified_blob(&mut file, &metadata, &mut buffer, &entry.blob, sha256)))
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
            // Capture the installed inode right away, as Git's checkout does.
            // A file Graft cannot stat keeps empty stat data and is rehashed.
            if let Ok(info) = destination.regular_stat(path) {
                installed.push((entry, index_stat::Stat::from_libc(&info)));
            }
            counts.add(metadata.len());
        }
        Ok((counts, missing, installed))
    }

    fn verified_blob(
        file: &mut File,
        metadata: &fs::Metadata,
        buffer: &mut [u8],
        oid: &str,
        sha256: bool,
    ) -> bool {
        native::plain(file, metadata).unwrap_or(false)
            && matches_blob(file, metadata.len(), buffer, oid, sha256).unwrap_or(false)
    }

    /// Whether a symbolic link's target bytes are exactly the Git blob `oid`.
    #[cfg(target_os = "macos")]
    fn link_matches(target: &[u8], oid: &str, sha256: bool) -> bool {
        let mut buffer = [0; 4096];
        matches_blob(target, target.len() as u64, &mut buffer, oid, sha256).unwrap_or(false)
    }

    fn matches_blob(
        file: impl Read,
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

    #[cfg(test)]
    mod tests {
        use super::*;

        fn git(repo: &Path) -> Command {
            let mut command = Command::new("git");
            command
                .arg("-C")
                .arg(repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1");
            command
        }

        fn run(repo: &Path, args: &[&str]) -> String {
            let output = git(repo).args(args).output().unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }

        /// Predicted checkout sizes match what Git itself writes under
        /// core.autocrlf=true, across every rule in convert.c's heuristics.
        #[test]
        fn auto_crlf_predictions_match_git_checkouts() {
            let text = |prefix: &[u8], count: usize, suffix: &[u8]| {
                [prefix, &vec![b'a'; count], suffix].concat()
            };
            let mut long = Vec::new();
            for line in 0..40_000 {
                long.extend_from_slice(format!("line {line}\n").as_bytes());
            }
            let samples: Vec<(&str, Vec<u8>)> = vec![
                ("empty", b"".to_vec()),
                ("no-lf", b"no line feed".to_vec()),
                ("one-lf", b"one\n".to_vec()),
                ("lines", b"two\nlines\n".to_vec()),
                ("crlf", b"crlf\r\nend\n".to_vec()),
                ("only-crlf", b"crlf\r\n".to_vec()),
                ("lone-cr", b"lone\rcr\n".to_vec()),
                ("trailing-cr", b"trailing\n\r".to_vec()),
                ("nul", b"\0nul\n".to_vec()),
                ("dos-eof", b"dos\n\x1a".to_vec()),
                ("control", b"\x01ctl\n".to_vec()),
                ("printable-controls", b"\t\x08\x1b\x0c\n".to_vec()),
                ("utf8", "caf\u{e9}\n".as_bytes().to_vec()),
                ("threshold-text", text(b"", 128, b"\x01\n")),
                ("threshold-binary", text(b"", 127, b"\x01\n")),
                ("delete-binary", text(b"", 127, b"\x7f\n")),
                ("eof-text", text(b"", 128, b"\x01\n\x1a")),
                ("eof-binary", b"a\n\x1a\x1a".to_vec()),
                ("long", long),
            ];
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path().join("repo");
            fs::create_dir(&repo).unwrap();
            run(&repo, &["init", "-q"]);
            run(&repo, &["config", "core.autocrlf", "false"]);
            for (name, bytes) in &samples {
                fs::write(repo.join(name), bytes).unwrap();
            }
            run(&repo, &["add", "."]);
            let checkout = temp.path().join("checkout");
            fs::create_dir(&checkout).unwrap();
            let work_tree = format!("--work-tree={}", checkout.display());
            run(
                &repo,
                &[
                    "-c",
                    "core.autocrlf=true",
                    &work_tree,
                    "checkout-index",
                    "-a",
                ],
            );
            let blobs: Vec<String> = samples
                .iter()
                .map(|(name, _)| run(&repo, &["rev-parse", &format!(":{name}")]))
                .collect();
            let written =
                auto_crlf_checkouts(|| git(&repo), blobs.iter().map(String::as_str)).unwrap();
            for ((name, bytes), blob) in samples.iter().zip(&blobs) {
                let actual = fs::read(checkout.join(name)).unwrap();
                let expected = if actual == *bytes {
                    Written::Raw
                } else {
                    Written::Crlf(actual.len() as u64)
                };
                assert_eq!(written[blob], expected, "{name}");
            }
            // A missing object fails the whole set.
            assert!(
                auto_crlf_checkouts(|| git(&repo), ["0".repeat(40).as_str()].into_iter()).is_none()
            );
        }
    }
}
