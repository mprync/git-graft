//! Repository-local ownership, locks and durable lifecycle records. A record is
//! written before its corresponding Git mutation and replaced atomically.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

use crate::{api::fault, git, landing::Plan};

pub const LEASE_SECS: u64 = 24 * 60 * 60;
pub const RECEIPT_SECS: u64 = 7 * LEASE_SECS;
pub const CACHE_OWNER: &str = ".git-graft-id";
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn validate_name(name: &str) -> Result<()> {
    let reserved = [
        "aux", "con", "nul", "prn", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    if name.len() > 48
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || reserved.contains(&name)
    {
        return Err(fault(
            "invalid_name",
            "Use a portable workspace name of 1–48 lowercase letters, numbers or hyphens, starting with a letter",
            Value::Null,
        ));
    }
    Ok(())
}
pub fn dir(path: &Path) -> Result<()> {
    match create_private_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => owned_directory(path).map(|_| ()),
        Err(e) => Err(e).with_context(|| format!("create directory {}", path.display())),
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    fs::create_dir(path)
}

#[cfg(unix)]
fn require_private_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    ensure!(
        owned_directory(path)?.permissions().mode() & 0o077 == 0,
        "directory is accessible to other users: {}. Restrict this Graft-owned directory to mode 0700 (chmod 700) before making changes",
        path.display()
    );
    Ok(())
}

pub fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// Inspect the endpoint without following symlinks or Windows junctions.
pub fn owned_directory(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !is_reparse_point(&metadata),
        "expected an ordinary directory at {}",
        path.display()
    );
    Ok(metadata)
}

fn check_unmounted_directory(path: &Path, recursive: bool) -> Result<fs::Metadata> {
    let root = owned_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = path.parent().context("tree has no parent directory")?;
        ensure!(
            root.dev() == fs::metadata(parent)?.dev(),
            "mounted tree: {}",
            path.display()
        );
    }
    #[cfg(target_os = "linux")]
    check_mounts(path, recursive)?;
    #[cfg(not(target_os = "linux"))]
    let _ = recursive;
    Ok(root)
}

/// Check the contents of a tree before recursive removal. Ordinary symlinks
/// are leaves. Nested repositories and mounted filesystems are retained.
pub fn check_owned_tree(path: &Path, allow_root_git: bool) -> Result<()> {
    let root = check_unmounted_directory(path, true)?;
    #[cfg(unix)]
    let device = {
        use std::os::unix::fs::MetadataExt;
        root.dev()
    };
    #[cfg(not(unix))]
    let _ = root;

    let mut pending = vec![path.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let mut head = false;
        let mut objects = false;
        let mut refs = false;
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let name = entry.file_name();
            head |= name.eq_ignore_ascii_case("HEAD");
            objects |= name.eq_ignore_ascii_case("objects");
            refs |= name.eq_ignore_ascii_case("refs") || name.eq_ignore_ascii_case("reftable");
            let child = entry.path();
            let metadata = fs::symlink_metadata(&child)?;
            if name.eq_ignore_ascii_case(".git") {
                ensure!(
                    allow_root_git
                        && directory == path
                        && metadata.is_file()
                        && !is_reparse_point(&metadata),
                    "nested Git metadata in {}",
                    directory.display()
                );
            }
            if metadata.file_type().is_symlink() {
                continue;
            }
            ensure!(
                !is_reparse_point(&metadata) && (metadata.is_file() || metadata.is_dir()),
                "unexpected filesystem entry at {}",
                child.display()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(
                    metadata.dev() == device,
                    "mounted entry: {}",
                    child.display()
                );
            }
            if metadata.is_dir() {
                pending.push(child);
            }
        }
        ensure!(
            !(head && objects && refs),
            "nested bare Git repository in {}",
            directory.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn check_mounts(path: &Path, recursive: bool) -> Result<()> {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let root = dunce::canonicalize(path)?;
    let mountinfo = fs::read("/proc/self/mountinfo").context("read Linux mount table")?;
    for line in mountinfo
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let encoded = line
            .split(|&byte| byte == b' ')
            .nth(4)
            .context("invalid mount table")?;
        // The kernel escapes whitespace and literal backslashes with three octal digits.
        let mut decoded = Vec::with_capacity(encoded.len());
        let mut remaining = encoded;
        while let [first, rest @ ..] = remaining {
            if let [
                b'\\',
                a @ b'0'..=b'3',
                b @ b'0'..=b'7',
                c @ b'0'..=b'7',
                tail @ ..,
            ] = remaining
            {
                decoded.push((a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'));
                remaining = tail;
            } else {
                decoded.push(*first);
                remaining = rest;
            }
        }
        let mount = Path::new(OsStr::from_bytes(&decoded));
        ensure!(
            mount != root && (!recursive || !mount.starts_with(&root)),
            "mounted entry: {}",
            mount.display()
        );
    }
    Ok(())
}

pub fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn open_lock(path: &Path) -> Result<File> {
    if exists(path)? {
        let metadata = fs::symlink_metadata(path)?;
        ensure!(
            metadata.is_file() && !is_reparse_point(&metadata),
            "lock is not a regular file"
        );
    }
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}
pub(crate) fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("missing state directory")?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn regular(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !is_reparse_point(&metadata),
        "expected regular file at {}",
        path.display()
    );
    Ok(fs::read(path)?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    Creating,
    Ready,
    Landing {
        plan: Plan,
        discard_ignored: bool,
    },
    Removing {
        tip: String,
        discard: bool,
        discard_ignored: bool,
        landed: Option<Plan>,
    },
}
impl Phase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Ready => "ready",
            Self::Landing { .. } => "landing",
            Self::Removing { .. } => "removing",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    pub lease: String,
    pub base: String,
    #[serde(default)]
    pub requested_base: String,
    #[serde(default)]
    pub sparse: Vec<String>,
    pub port: u16,
    pub created: u64,
    pub heartbeat: u64,
    /// Git's per-worktree administrative directory, verified before retirement.
    pub admin: Option<PathBuf>,
    pub state: Phase,
    pub storage: Value,
}
impl Record {
    pub fn branch(&self) -> String {
        format!("graft/{}", self.name)
    }
    pub fn branch_ref(&self) -> String {
        format!("refs/heads/{}", self.branch())
    }
    pub fn authorize(&self, token: &str) -> Result<()> {
        if self.lease != token {
            return Err(fault(
                "lease_mismatch",
                "The lease does not identify this workspace instance",
                json!({"name":self.name}),
            ));
        }
        Ok(())
    }
    pub fn ready(&self) -> Result<()> {
        if !matches!(self.state, Phase::Ready) {
            return Err(fault(
                "operation_in_progress",
                "Inspect or retry the pending lifecycle operation",
                json!({"phase":self.state.label()}),
            ));
        }
        Ok(())
    }
}

pub struct Project {
    /// Main checkout or bare repository. It outlives a retiring linked tree.
    pub repo: PathBuf,
    pub invocation: PathBuf,
    pub common: PathBuf,
    pub state: PathBuf,
    pub root: PathBuf,
    /// Git's inventory when this command acquired the project lock.
    pub registered: Vec<git::Worktree>,
    id: String,
}
impl Project {
    /// The returned file holds the project lock. Long-running child commands
    /// release it after taking their workspace lock.
    pub fn open(repo: &Path) -> Result<(Self, File)> {
        let repo = dunce::canonicalize(repo)?;
        let common = dunce::canonicalize(git::text(
            &repo,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?)?;
        let state = common.join("graft");
        dir(&state)?;
        let lock_path = state.join("project.lock");
        let id_file = state.join("project-id");
        if !exists(&id_file)? && !exists(&lock_path)? {
            // Leave a preexisting foreign registry untouched, even before locking.
            for entry in fs::read_dir(&state)? {
                let name = entry?.file_name();
                ensure!(
                    name == "project.lock" || exists(&lock_path)?,
                    "unrecognized graft data in {}",
                    state.display()
                );
            }
        }
        let lock = open_lock(&lock_path)?;
        lock.lock().context("lock workspace registry")?;
        let id = if exists(&id_file)? {
            let bytes = regular(&id_file)?;
            Uuid::parse_str(std::str::from_utf8(&bytes)?)?.to_string()
        } else {
            // A nonempty foreign or future-version store must never be adopted.
            for entry in fs::read_dir(&state)? {
                ensure!(
                    entry?.file_name() == "project.lock",
                    "unrecognized graft data in {}",
                    state.display()
                );
            }
            let id = Uuid::new_v4().to_string();
            atomic(&id_file, id.as_bytes())?;
            id
        };
        for sub in ["records", "locks", "bases", "runtime", "completed"] {
            dir(&state.join(sub))?;
        }
        let registered = git::worktrees(&repo)?;
        let primary = &registered[0].path;
        let name = primary
            .file_name()
            .context("cannot place workspaces adjacent to filesystem root")?;
        let mut sibling = name.to_os_string();
        sibling.push(".graft");
        let root = primary.with_file_name(sibling);
        git::path_str(&root)?;
        let project = Self {
            repo: primary.clone(),
            invocation: repo,
            common,
            state,
            root,
            id,
            registered,
        };
        for name in ["cache", "layers"] {
            project.init_cache_directory(&project.state.join(name))?;
        }
        Ok((project, lock))
    }
    /// Mark only directories Graft creates. Legacy unmarked caches remain usable.
    fn init_cache_directory(&self, path: &Path) -> Result<()> {
        match create_private_dir(path) {
            Ok(()) => atomic(&path.join(CACHE_OWNER), self.id.as_bytes()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                owned_directory(path).map(|_| ())
            }
            Err(error) => Err(error).with_context(|| format!("create cache {}", path.display())),
        }
    }
    pub fn check_cache_owner(&self, path: &Path) -> Result<()> {
        check_unmounted_directory(path, false)?;
        let marker = path.join(CACHE_OWNER);
        ensure!(
            exists(&marker)? && regular(&marker)? == self.id.as_bytes(),
            "cache ownership is unverified at {}",
            path.display()
        );
        Ok(())
    }
    pub fn workspace(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
    pub fn require_private_state(&self) -> Result<()> {
        #[cfg(unix)]
        require_private_mode(&self.state)?;
        Ok(())
    }
    pub fn require_private_workspaces(&self) -> Result<()> {
        #[cfg(unix)]
        if exists(&self.root)? {
            self.root(false)?;
            require_private_mode(&self.root)?;
        }
        Ok(())
    }
    /// Only a directory created for this repository may contain managed trees.
    pub fn root(&self, create: bool) -> Result<()> {
        if create && !exists(&self.root)? {
            create_private_dir(&self.root)?;
            atomic(&self.root.join(".git-graft-id"), self.id.as_bytes())?;
        }
        owned_directory(&self.root).context("workspace root is not an ordinary directory")?;
        ensure!(
            regular(&self.root.join(".git-graft-id"))? == self.id.as_bytes(),
            "workspace root belongs to another repository: {}",
            self.root.display()
        );
        #[cfg(unix)]
        if create {
            require_private_mode(&self.root)?;
        }
        Ok(())
    }
    fn record_path(&self, name: &str) -> PathBuf {
        self.state.join("records").join(format!("{name}.json"))
    }
    pub fn record(&self, name: &str) -> Result<Record> {
        validate_name(name)?;
        let path = self.record_path(name);
        if !exists(&path)? {
            return Err(fault(
                "not_found",
                format!("No managed workspace named {name}"),
                Value::Null,
            ));
        }
        let r: Record =
            serde_json::from_slice(&regular(&path)?).context("invalid workspace record")?;
        ensure!(r.name == name, "workspace record name mismatch");
        ensure!(
            Uuid::parse_str(&r.lease)?.to_string() == r.lease,
            "workspace lease must be a canonical UUID"
        );
        git::checked_oid(&r.base)?;
        if let Some(admin) = &r.admin {
            ensure!(
                admin.parent() == Some(self.common.join("worktrees").as_path())
                    && admin.file_name().is_some(),
                "invalid workspace administrative directory"
            );
        }
        let plan = match &r.state {
            Phase::Creating | Phase::Ready => None,
            Phase::Landing { plan, .. } => Some(plan),
            Phase::Removing { tip, landed, .. } => {
                git::checked_oid(tip)?;
                landed.as_ref()
            }
        };
        if let Some(plan) = plan {
            git::checked_oid(&plan.old)?;
            git::checked_oid(&plan.new)?;
            git::checked_oid(&plan.tip)?;
        }
        Ok(r)
    }
    pub fn records(&self) -> Result<Vec<Record>> {
        let mut result = Vec::new();
        for entry in fs::read_dir(self.state.join("records"))? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                result.push(
                    self.record(
                        path.file_stem()
                            .and_then(|s| s.to_str())
                            .context("invalid record filename")?,
                    )?,
                );
            }
        }
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(result)
    }
    pub fn save(&self, r: &Record) -> Result<()> {
        self.require_private_state()?;
        validate_name(&r.name)?;
        atomic(&self.record_path(&r.name), &serde_json::to_vec(r)?)
    }
    pub fn forget(&self, r: &Record) -> Result<()> {
        fs::remove_file(self.record_path(&r.name))?;
        Ok(())
    }
    fn receipt_path(&self, lease: Uuid) -> PathBuf {
        self.state.join("completed").join(format!("{lease}.json"))
    }
    /// Persist the terminal result before forgetting the live record. A lost
    /// response can then be replayed without repeating a merge or deletion.
    pub fn complete(&self, r: &Record, result: &Value) -> Result<()> {
        self.require_private_state()?;
        let path = self.receipt_path(Uuid::parse_str(&r.lease)?);
        atomic(
            &path,
            &serde_json::to_vec(&json!({"at":now(),"result":result}))?,
        )?;
        self.forget(r)
    }
    pub fn receipt(&self, token: &str) -> Result<Option<Value>> {
        let Ok(id) = Uuid::parse_str(token) else {
            return Ok(None);
        };
        let path = self.receipt_path(id);
        if !exists(&path)? {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&regular(&path)?)?))
    }
    pub fn work_lock(&self, name: &str) -> Result<File> {
        validate_name(name)?;
        let file = open_lock(&self.state.join("locks").join(name))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(fs::TryLockError::WouldBlock) => Err(fault(
                "workspace_busy",
                "A command is running in this workspace",
                json!({"name":name}),
            )),
            Err(e) => Err(io::Error::from(e).into()),
        }
    }
    pub fn summary(&self, r: &Record) -> Value {
        json!({"name":r.name,"worktree_path":self.workspace(&r.name),"branch":r.branch(),
            "base":r.base,"port":r.port,"created_at":r.created,"last_heartbeat":r.heartbeat,
            "expires_at":r.heartbeat.saturating_add(LEASE_SECS),"phase":r.state.label(),"storage":r.storage})
    }
    pub fn identify(&self, r: &Record) -> Result<PathBuf> {
        self.root(false)?;
        let path = self.workspace(&r.name);
        owned_directory(&path).context("workspace path was replaced")?;
        let gitfile = path.join(".git");
        let metadata = fs::symlink_metadata(&gitfile)?;
        ensure!(
            metadata.is_file() && !is_reparse_point(&metadata),
            "expected linked worktree .git file"
        );
        let admin = dunce::canonicalize(git::text(&path, &["rev-parse", "--absolute-git-dir"])?)?;
        ensure!(
            admin.parent() == Some(self.common.join("worktrees").as_path()),
            "unexpected Git administrative directory"
        );
        // Linked checkouts name their shared store in an admin-relative commondir file.
        let common = regular(&admin.join("commondir"))?;
        let common = std::str::from_utf8(&common)?.trim_end_matches(['\n', '\r']);
        ensure!(
            dunce::canonicalize(admin.join(common))? == self.common,
            "workspace belongs to another repository"
        );
        let gitdir = regular(&admin.join("gitdir"))?;
        let backlink = Path::new(std::str::from_utf8(&gitdir)?.trim_end_matches(['\n', '\r']));
        ensure!(
            dunce::canonicalize(admin.join(backlink))? == gitfile,
            "Git administrative directory points to a different checkout"
        );
        let branch = r.branch_ref();
        // Loose HEAD is the normal files-backend representation. Ask Git for
        // other representations (including the reftable backend).
        let direct_head = regular(&admin.join("HEAD"))? == format!("ref: {branch}\n").as_bytes()
            || git::symbolic_ref(&path, "HEAD")?.as_deref() == Some(&branch);
        ensure!(
            direct_head && git::symbolic_ref(&path, &branch)?.is_none(),
            "workspace was detached, switched or uses a symbolic branch"
        );
        ensure!(
            git::worktrees(&self.repo)?
                .iter()
                .any(
                    |w| dunce::canonicalize(&w.path).ok().as_ref() == Some(&path)
                        && w.branch.as_deref() == Some(&branch)
                ),
            "workspace was moved, detached or switched to another branch"
        );
        Ok(admin)
    }
    pub fn verify(&self, r: &Record) -> Result<PathBuf> {
        let admin = self.identify(r)?;
        ensure!(
            Some(&admin) == r.admin.as_ref()
                && regular(&admin.join("graft-owner"))? == r.lease.as_bytes(),
            "workspace registration has changed; inspect it with git worktree list"
        );
        Ok(self.workspace(&r.name))
    }
    pub fn mark(&self, r: &mut Record) -> Result<()> {
        let admin = self.identify(r)?;
        ensure!(
            !exists(&admin.join("graft-owner"))?,
            "worktree already marked"
        );
        atomic(&admin.join("graft-owner"), r.lease.as_bytes())?;
        r.admin = Some(admin);
        self.save(r)
    }
}

/// Select an unused project slot. Ports are suggestions for local services and
/// are unique among this project's live records. Sockets are opened by the child.
pub fn port_for(records: &[Record], name: &str) -> Result<u16> {
    const SLOTS: u32 = 20000;
    let hash = name
        .bytes()
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b)));
    for offset in 0..SLOTS {
        let port = (10000 + (hash % SLOTS + offset) % SLOTS) as u16;
        if !records.iter().any(|r| r.port == port) {
            return Ok(port);
        }
    }
    bail!("all workspace port slots are in use")
}
