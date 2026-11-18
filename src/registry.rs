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
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e)
            if e.kind() == io::ErrorKind::AlreadyExists && fs::symlink_metadata(path)?.is_dir() =>
        {
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("create directory {}", path.display())),
    }
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
        ensure!(
            fs::symlink_metadata(path)?.is_file(),
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
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
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
    ensure!(
        fs::symlink_metadata(path)?.is_file(),
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
        let lock = open_lock(&state.join("project.lock"))?;
        lock.lock().context("lock workspace registry")?;
        let id_file = state.join("project-id");
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
        for sub in [
            "records",
            "locks",
            "bases",
            "runtime",
            "cache",
            "layers",
            "completed",
        ] {
            dir(&state.join(sub))?;
        }
        let primary = &git::worktrees(&repo)?[0].path;
        let name = primary
            .file_name()
            .context("cannot place workspaces adjacent to filesystem root")?;
        let mut sibling = name.to_os_string();
        sibling.push(".graft");
        let root = primary.with_file_name(sibling);
        git::path_str(&root)?;
        Ok((
            Self {
                repo: primary.clone(),
                invocation: repo,
                common,
                state,
                root,
                id,
            },
            lock,
        ))
    }
    pub fn workspace(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
    /// Only a directory created for this repository may contain managed trees.
    pub fn root(&self, create: bool) -> Result<()> {
        if create && !exists(&self.root)? {
            fs::create_dir(&self.root)?;
            atomic(&self.root.join(".git-graft-id"), self.id.as_bytes())?;
        }
        ensure!(
            fs::symlink_metadata(&self.root)?.is_dir(),
            "workspace root is not an ordinary directory"
        );
        ensure!(
            regular(&self.root.join(".git-graft-id"))? == self.id.as_bytes(),
            "workspace root belongs to another repository: {}",
            self.root.display()
        );
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
        ensure!(
            fs::symlink_metadata(&path)?.is_dir(),
            "workspace path was replaced"
        );
        ensure!(
            fs::symlink_metadata(path.join(".git"))?.is_file(),
            "expected linked worktree .git file"
        );
        let common = dunce::canonicalize(git::text(
            &path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?)?;
        ensure!(
            common == self.common,
            "workspace belongs to another repository"
        );
        let admin = dunce::canonicalize(git::text(&path, &["rev-parse", "--absolute-git-dir"])?)?;
        ensure!(
            admin.parent() == Some(self.common.join("worktrees").as_path()),
            "unexpected Git administrative directory"
        );
        ensure!(
            git::worktrees(&self.repo)?
                .iter()
                .any(
                    |w| dunce::canonicalize(&w.path).ok().as_ref() == Some(&path)
                        && w.branch.as_deref() == Some(&r.branch_ref())
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
