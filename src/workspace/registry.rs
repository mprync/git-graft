//! Identity and crash-recoverable directory transitions for independent Git
//! repositories. Callers hold the project lock while reading or changing records.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
};
use uuid::Uuid;

use crate::{
    api::fault,
    cache::private_stage,
    git,
    landing::Plan,
    recovery::rename_exclusive,
    registry::{
        CACHE_OWNER, LEASE_SECS, Project, atomic, check_relocation_tree, dir, exists,
        is_reparse_point, now, open_lock, owned_directory, regular, validate_name,
    },
};

pub(super) const MARKER: &str = "graft-workspace.json";
const NAMESPACE: &str = "workspaces";
const LEGACY_MARKER: &str = "graft-independent.json";
const ARCHIVES: &str = ".retired";
/// Prefix of the private stage a new workspace or namespace is built in.
const PENDING: &str = ".pending-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Active,
    Dropped,
    Landed,
}

impl Status {
    fn retired(self) -> bool {
        matches!(self, Self::Dropped | Self::Landed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub name: String,
    pub lease: String,
    pub base: Option<String>,
    pub branch: Option<String>,
    pub path: PathBuf,
    pub status: Status,
    pub plan: Option<Plan>,
    pub archive: Option<PathBuf>,
    pub locked: Option<String>,
    pub storage: Value,
    pub request: Value,
    pub port: u16,
    pub created: u64,
    pub heartbeat: u64,
    project: PathBuf,
    identity: Identity,
    // A failed spawn can be archived too. Its Git directory may be incomplete.
    initialized: bool,
    transition: Option<Transition>,
    previous: Vec<Retirement>,
    /// The graft HEAD that `remove --discard` archived after a merge had
    /// already landed an earlier tip: its later commits were never merged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmerged_head: Option<String>,
}

impl Record {
    pub fn placing(&self) -> bool {
        matches!(self.transition, Some(Transition::Place { .. }))
    }

    pub fn retiring_to(&self) -> Option<Status> {
        match self.transition {
            Some(Transition::Retire { status, .. }) => Some(status),
            _ => None,
        }
    }

    /// A live record, or a retired one being restored, owns its name.
    fn holds_name(&self) -> bool {
        !self.status.retired() || matches!(self.transition, Some(Transition::Restore { .. }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Transition {
    Place {
        from: PathBuf,
    },
    Move {
        to: PathBuf,
    },
    Retire {
        status: Status,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<PathBuf>,
    },
    Restore {
        lease: String,
        port: u16,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Retirement {
    lease: String,
    status: Status,
    path: PathBuf,
    plan: Option<Plan>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unmerged_head: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    workspace: FileId,
    git: FileId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileId {
    volume: u64,
    file: u64,
}

#[cfg(unix)]
fn file_id(path: &Path) -> Result<FileId> {
    Ok(unix_file_id(&owned_directory(path)?))
}

#[cfg(unix)]
fn unix_file_id(meta: &fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    FileId {
        volume: meta.dev(),
        file: meta.ino(),
    }
}

#[cfg(windows)]
fn file_id(path: &Path) -> Result<FileId> {
    let (volume, file) = crate::cache::windows_directory_id(path, "directory")?;
    Ok(FileId { volume, file })
}

#[cfg(not(any(unix, windows)))]
fn file_id(_path: &Path) -> Result<FileId> {
    bail!("independent workspace identity is unsupported on this platform");
}

impl Identity {
    fn read(path: &Path) -> Result<Self> {
        Ok(Self {
            workspace: file_id(path)?,
            git: file_id(&path.join(".git"))?,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    project_id: String,
    project: PathBuf,
    common: PathBuf,
    id: String,
    name: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct NamespaceOwner {
    project_id: String,
    directory: FileId,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub name: String,
    pub id: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub base: Option<String>,
    pub status: Status,
    pub phase: &'static str,
    pub port: u16,
    pub created_at: u64,
    pub last_heartbeat: u64,
    pub expires_at: u64,
    pub locked: bool,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    pub storage: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// Why prune could not check this graft. It was left as it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn summary(r: &Record, available: bool, reason: Option<&'static str>) -> Summary {
    let phase = match (&r.transition, r.status, r.plan.is_some()) {
        (Some(Transition::Move { .. }), _, _) => "moving",
        (Some(Transition::Retire { .. }), _, _) => "retiring",
        (Some(Transition::Restore { .. }), _, _) => "restoring",
        (_, Status::Pending, _) => "creating",
        (_, Status::Active, true) => "landing",
        (_, Status::Active, false) => "ready",
        (_, Status::Dropped, _) => "dropped",
        (_, Status::Landed, _) => "landed",
    };
    Summary {
        name: r.name.clone(),
        id: r.id.clone(),
        path: r.path.clone(),
        branch: r.branch.clone(),
        base: r.base.clone(),
        status: r.status,
        phase,
        port: r.port,
        created_at: r.created,
        last_heartbeat: r.heartbeat,
        expires_at: r.heartbeat.saturating_add(LEASE_SECS),
        locked: r.locked.is_some(),
        available,
        archive_path: r.archive.clone(),
        receipt_id: r
            .status
            .retired()
            .then(|| format!("{}:{}", r.id, r.previous.len() + 1)),
        storage: r.storage.clone(),
        reason,
        error: None,
    }
}

fn canonical_uuid(s: &str) -> Result<&str> {
    ensure!(
        Uuid::parse_str(s)?.to_string() == s,
        "expected a canonical UUID"
    );
    Ok(s)
}

fn project_id(common: &Path) -> Result<String> {
    let state = common.join("graft");
    owned_directory(&state)?;
    let id = String::from_utf8(regular(&state.join("project-id"))?)?;
    canonical_uuid(&id)?;
    Ok(id)
}

pub(super) fn marked_dir(path: &Path, id: &str, create: bool) -> Result<()> {
    if create && !exists(path)? {
        let parent = path.parent().context("namespace has no parent")?;
        let stage = private_stage(parent, PENDING)?;
        let owner = NamespaceOwner {
            project_id: id.into(),
            directory: file_id(stage.path())?,
        };
        crate::registry::publish_marked_directory(stage, path, &serde_json::to_vec(&owner)?)?;
    }
    let metadata = owned_directory(path)?;
    #[cfg(unix)]
    let directory = {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "shared directory at {}",
            path.display()
        );
        unix_file_id(&metadata)
    };
    #[cfg(not(unix))]
    let directory = {
        let _ = metadata;
        file_id(path)?
    };
    let owner: NamespaceOwner = serde_json::from_slice(&regular(&path.join(CACHE_OWNER))?)
        .with_context(|| format!("invalid workspace namespace marker at {}", path.display()))?;
    ensure!(
        owner.project_id == id && owner.directory == directory,
        "graft namespace ownership is unverified at {}",
        path.display()
    );
    Ok(())
}

fn state_dir(p: &Project, create: bool) -> Result<PathBuf> {
    p.require_private_state()?;
    let state = p.state.join(NAMESPACE);
    marked_dir(&state, p.id(), create)?;
    Ok(state)
}

fn workspaces(p: &Project, create: bool) -> Result<PathBuf> {
    p.root(create)?;
    ensure!(
        !exists(&p.root.join(".independent"))?,
        "legacy workspace data at {}",
        p.root.display()
    );
    Ok(p.root.clone())
}

fn validate_fresh_root(p: &Project) -> Result<()> {
    if exists(&p.root)? {
        workspaces(p, false)?;
        for entry in fs::read_dir(&p.root)? {
            ensure!(
                entry?.file_name() == CACHE_OWNER,
                "graft root contains preexisting data at {}",
                p.root.display()
            );
        }
    }
    Ok(())
}

fn record_directory(p: &Project, create: bool) -> Result<PathBuf> {
    let state = state_dir(p, create)?;
    let path = state.join("records");
    if create && !exists(&path)? {
        dir(&path)?;
        sync_directory(&state)?;
    } else {
        owned_directory(&path)?;
    }
    Ok(path)
}

pub(super) struct WorkspaceLock(File);

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        // A concurrent fork can share the open file description until exec
        // closes its descriptor. Release the lock when the caller is done.
        let _ = self.0.unlock();
    }
}

fn lock_in(state: &Path, name: &str) -> Result<WorkspaceLock> {
    let path = state.join("locks");
    dir(&path)?;
    try_lock(open_lock(&path.join(name))?, name)
}

/// Serialize workspace subprocesses with lifecycle operations, without
/// holding the project lock throughout a subprocess.
pub(super) fn workspace_lock(p: &Project, name: &str) -> Result<WorkspaceLock> {
    validate_name(name)?;
    lock_in(&state_dir(p, false)?, name)
}

/// Hold the graft lock shared for `exec`. The caller passes the open file to
/// the process it starts, so the hold lasts while that process or any
/// descendant that kept the file open is running, even after `exec` returns.
/// Other `exec` calls share it; lifecycle operations, which need the lock
/// exclusively, report `workspace_busy` until the last holder exits.
pub(super) fn exec_lock(p: &Project, name: &str) -> Result<File> {
    validate_name(name)?;
    let path = state_dir(p, false)?.join("locks");
    dir(&path)?;
    let file = open_lock(&path.join(name))?;
    match file.try_lock_shared() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(fault(
            "workspace_busy",
            "A command is running in this workspace",
            json!({"name":name}),
        )),
        Err(error) => Err(io::Error::from(error).into()),
    }
}

fn try_lock(file: File, name: &str) -> Result<WorkspaceLock> {
    match file.try_lock() {
        Ok(()) => Ok(WorkspaceLock(file)),
        Err(fs::TryLockError::WouldBlock) => Err(fault(
            "workspace_busy",
            "A command is running in this workspace",
            json!({"name":name}),
        )),
        Err(error) => Err(io::Error::from(error).into()),
    }
}

fn marker(p: &Project, id: &str, name: &str) -> Marker {
    Marker {
        project_id: p.id().into(),
        project: p.repo.clone(),
        common: p.common.clone(),
        id: id.into(),
        name: name.into(),
    }
}

fn valid_branch(branch: &[u8]) -> bool {
    !branch.is_empty()
        && !branch.ends_with(b".")
        && !branch.windows(2).any(|pair| pair == b".." || pair == b"@{")
        && !branch
            .iter()
            .any(|byte| *byte <= b' ' || *byte == 0x7f || b"~^:?*[\\".contains(byte))
        && branch
            .split(|byte| *byte == b'/')
            .all(|part| !part.is_empty() && !part.starts_with(b".") && !part.ends_with(b".lock"))
}

fn validate_plan(plan: &Plan) -> Result<()> {
    ensure!(
        valid_branch(plan.target.as_bytes())
            && !plan.target.starts_with('-')
            && !plan.target.starts_with("refs/")
            && !plan.target.starts_with("graft/")
            && plan.target != "HEAD",
        "invalid workspace merge target"
    );
    git::checked_oid(&plan.old)?;
    git::checked_oid(&plan.new)?;
    git::checked_oid(&plan.tip)?;
    Ok(())
}

fn validate_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && path.file_name().is_some()
            && !path.components().any(|part| {
                part == Component::ParentDir || part.as_os_str().eq_ignore_ascii_case(".git")
            }),
        "invalid workspace path"
    );
    ensure!(
        !git::path_str(path)?.contains('\0'),
        "invalid workspace path"
    );
    Ok(())
}

/// Unmerged commits are only recorded beside the merge that did land.
fn validate_unmerged_head(head: Option<&str>, plan: Option<&Plan>) -> Result<()> {
    if let Some(head) = head {
        git::checked_oid(head)?;
        ensure!(
            plan.is_some_and(|plan| plan.tip != head),
            "invalid unmerged graft head"
        );
    }
    Ok(())
}

fn validate_record(r: &Record) -> Result<()> {
    canonical_uuid(&r.id)?;
    validate_name(&r.name)?;
    canonical_uuid(&r.lease)?;
    ensure!(r.project.is_absolute(), "invalid workspace primary path");
    validate_path(&r.path)?;
    if let Some(archive) = &r.archive {
        validate_path(archive)?;
    }
    if let Some(base) = &r.base {
        git::checked_oid(base)?;
    }
    if let Some(branch) = &r.branch {
        ensure!(valid_branch(branch.as_bytes()), "invalid workspace branch");
    }
    ensure!(
        r.branch.is_some() || r.base.is_some(),
        "graft has no starting ref"
    );
    if let Some(plan) = &r.plan {
        validate_plan(plan)?;
    }
    validate_unmerged_head(r.unmerged_head.as_deref(), r.plan.as_ref())?;
    ensure!(valid_port(r.port), "invalid workspace port");
    ensure!(
        match r.status {
            Status::Pending => !r.initialized && r.plan.is_none() && r.locked.is_none(),
            Status::Active => r.initialized,
            Status::Dropped => r.plan.is_none() && r.locked.is_none(),
            Status::Landed => r.initialized && r.plan.is_some() && r.locked.is_none(),
        },
        "invalid workspace lifecycle state"
    );
    let mut leases = HashSet::new();
    for prior in &r.previous {
        validate_path(&prior.path)?;
        ensure!(
            prior.status.retired() && (prior.status == Status::Landed) == prior.plan.is_some(),
            "invalid workspace retirement receipt"
        );
        ensure!(
            prior.lease != r.lease && leases.insert(canonical_uuid(&prior.lease)?),
            "duplicate workspace lease"
        );
        if let Some(plan) = &prior.plan {
            validate_plan(plan)?;
        }
        validate_unmerged_head(prior.unmerged_head.as_deref(), prior.plan.as_ref())?;
    }
    ensure!(
        r.archive.is_some()
            == (r.status.retired() || matches!(r.transition, Some(Transition::Retire { .. }))),
        "graft archive does not match its journal phase"
    );
    match &r.transition {
        Some(Transition::Place { from }) => {
            validate_path(from)?;
            ensure!(
                r.status == Status::Pending && from != &r.path,
                "invalid placement journal"
            );
        }
        Some(Transition::Move { to }) => {
            validate_path(to)?;
            ensure!(
                !r.status.retired()
                    && r.plan.is_none()
                    && r.locked.is_none()
                    && !overlaps(&r.path, to),
                "invalid move journal"
            );
        }
        Some(Transition::Retire { status, from }) => {
            if let Some(from) = from {
                validate_path(from)?;
            }
            ensure!(
                !r.status.retired()
                    && status.retired()
                    && r.locked.is_none()
                    && (*status == Status::Landed) == r.plan.is_some()
                    && (from.is_none() || r.status == Status::Pending),
                "invalid retirement journal"
            );
        }
        Some(Transition::Restore { lease, port }) => {
            ensure!(
                r.status.retired()
                    && valid_port(*port)
                    && lease != &r.lease
                    && leases.insert(canonical_uuid(lease)?),
                "invalid restore journal"
            );
        }
        None => {}
    }
    Ok(())
}

fn read_record(directory: &Path, id: &str) -> Result<Record> {
    canonical_uuid(id)?;
    let r: Record = serde_json::from_slice(&regular(&directory.join(format!("{id}.json")))?)
        .context("invalid workspace record")?;
    ensure!(r.id == id, "graft record ID mismatch");
    validate_record(&r)?;
    Ok(r)
}

/// A record file Graft cannot use. Graft never rewrites, moves or deletes it:
/// commands report it, keep it out of prune and refuse to act on its name,
/// and every other graft keeps working.
#[derive(Debug, Clone, Serialize)]
pub struct DamagedRecord {
    pub file: PathBuf,
    /// The graft name the file claims, when it can be read at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub error: String,
    /// Leases and ports the file claims, so a new graft never reuses them.
    #[serde(skip)]
    leases: Vec<String>,
    #[serde(skip)]
    ports: Vec<u16>,
}

impl DamagedRecord {
    fn new(file: PathBuf, error: &anyhow::Error) -> Self {
        // Read the claims leniently: the file failed validation as a whole.
        let value = fs::read(&file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .unwrap_or(Value::Null);
        let text = |value: &Value| value.as_str().map(str::to_owned);
        let mut leases: Vec<String> = [&value["lease"], &value["transition"]["lease"]]
            .into_iter()
            .filter_map(text)
            .collect();
        if let Some(previous) = value["previous"].as_array() {
            leases.extend(previous.iter().filter_map(|old| text(&old["lease"])));
        }
        let ports = [&value["port"], &value["transition"]["port"]]
            .into_iter()
            .filter_map(|port| port.as_u64().and_then(|port| u16::try_from(port).ok()))
            .collect();
        Self {
            file,
            name: text(&value["name"]),
            error: format!("{error:#}"),
            leases,
            ports,
        }
    }

    fn fault(&self, name: &str) -> anyhow::Error {
        let owner = match &self.name {
            Some(_) => format!("The record of graft {name} is damaged"),
            None => format!("A damaged graft record may belong to graft {name}"),
        };
        fault(
            "record_damaged",
            format!(
                "{owner}: {}. Graft left {} and the graft's files unchanged; repair or move that file to continue. Other grafts are unaffected",
                self.error,
                self.file.display()
            ),
            json!({"name":name,"file":self.file,"error":self.error}),
        )
    }
}

/// Every record of a project: the usable ones and the damaged files.
struct Registry {
    records: Vec<Record>,
    damaged: Vec<DamagedRecord>,
}

impl Registry {
    /// A damaged record that names this graft. Its state is unknown, so
    /// nothing may act on the name until someone repairs the file.
    fn damaged_name(&self, name: &str) -> Result<()> {
        match self
            .damaged
            .iter()
            .find(|d| d.name.as_deref() == Some(name))
        {
            Some(damaged) => Err(damaged.fault(name)),
            None => Ok(()),
        }
    }

    /// The error for a name no usable record holds. A damaged file whose
    /// name cannot be read may be the graft that was asked for.
    fn missing(&self, name: &str, otherwise: anyhow::Error) -> anyhow::Error {
        match self.damaged.iter().find(|d| d.name.is_none()) {
            Some(damaged) => damaged.fault(name),
            None => otherwise,
        }
    }

    fn current(&self, name: &str) -> Option<&Record> {
        current(&self.records, name)
    }
}

fn records(p: &Project) -> Result<Vec<Record>> {
    Ok(scan(p)?.records)
}

/// Read every record. A file that cannot be read or validated, and every
/// record in a conflict between files, is set aside as damaged instead of
/// failing the whole project.
fn scan(p: &Project) -> Result<Registry> {
    let mut registry = Registry {
        records: vec![],
        damaged: vec![],
    };
    if !exists(&p.state.join(NAMESPACE))? {
        ensure!(
            !p.has_legacy_workspace_records()?,
            "legacy workspace records at {}",
            p.state.display()
        );
        return Ok(registry);
    }
    let directory = state_dir(p, false)?.join("records");
    if !exists(&directory)? {
        return Ok(registry);
    }
    owned_directory(&directory)?;
    let mut found = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".json"))
            && canonical_uuid(id).is_ok()
        {
            let file = directory.join(&name);
            match read_record(&directory, id).and_then(|r| check_record(p, &r).map(|()| r)) {
                Ok(r) => found.push((file, r)),
                Err(error) => registry.damaged.push(DamagedRecord::new(file, &error)),
            }
        }
    }
    // Leases, live ports and live names must be unique across files. When
    // two files disagree, neither can be trusted, so both are set aside.
    let mut leases: std::collections::HashMap<&str, Vec<usize>> = Default::default();
    let mut ports: std::collections::HashMap<u16, Vec<usize>> = Default::default();
    let mut names: std::collections::HashMap<&str, Vec<usize>> = Default::default();
    for (index, (_, r)) in found.iter().enumerate() {
        let restore = match &r.transition {
            Some(Transition::Restore { lease, port }) => Some((lease.as_str(), *port)),
            _ => None,
        };
        let claimed = std::iter::once(r.lease.as_str())
            .chain(r.previous.iter().map(|old| old.lease.as_str()))
            .chain(restore.map(|(lease, _)| lease));
        for lease in claimed {
            leases.entry(lease).or_default().push(index);
        }
        if let Some((_, port)) = restore {
            ports.entry(port).or_default().push(index);
        } else if !r.status.retired() {
            ports.entry(r.port).or_default().push(index);
        }
        if r.holds_name() {
            names.entry(r.name.as_str()).or_default().push(index);
        }
    }
    let damaged_leases: HashSet<&str> = registry
        .damaged
        .iter()
        .flat_map(|d| d.leases.iter().map(String::as_str))
        .collect();
    let mut conflicts: std::collections::BTreeMap<usize, &'static str> = Default::default();
    for (lease, holders) in &leases {
        if holders.len() > 1 || damaged_leases.contains(lease) {
            for &index in holders {
                conflicts.insert(index, "duplicate workspace lease");
            }
        }
    }
    for holders in ports.values().filter(|holders| holders.len() > 1) {
        for &index in holders {
            conflicts
                .entry(index)
                .or_insert("duplicate live workspace port");
        }
    }
    for holders in names.values().filter(|holders| holders.len() > 1) {
        for &index in holders {
            conflicts
                .entry(index)
                .or_insert("multiple live records for one graft name");
        }
    }
    for (index, (file, r)) in found.into_iter().enumerate() {
        match conflicts.get(&index) {
            Some(reason) => {
                let mut damaged = DamagedRecord::new(file, &anyhow::anyhow!(*reason));
                damaged.name = Some(r.name);
                registry.damaged.push(damaged);
            }
            None => registry.records.push(r),
        }
    }
    registry
        .records
        .sort_by(|a, b| (&a.name, a.created, &a.id).cmp(&(&b.name, b.created, &b.id)));
    registry.damaged.sort_by(|a, b| a.file.cmp(&b.file));
    Ok(registry)
}

/// Checks of one record against its project, beyond its own format.
fn check_record(p: &Project, r: &Record) -> Result<()> {
    ensure!(
        r.project == p.repo,
        "graft record belongs to a different primary"
    );
    if let Some(path) = &r.archive {
        ensure!(
            path == &p.root.join(ARCHIVES).join(&r.id),
            "invalid workspace archive path"
        );
    }
    if let Some(
        Transition::Place { from }
        | Transition::Retire {
            from: Some(from), ..
        },
    ) = &r.transition
    {
        placement_source(&p.root, r, from)?;
    }
    Ok(())
}

/// Every damaged record file of the project, for `list` and `prune`.
pub fn damaged(p: &Project) -> Result<Vec<DamagedRecord>> {
    Ok(scan(p)?.damaged)
}

fn current<'a>(records: &'a [Record], name: &str) -> Option<&'a Record> {
    records.iter().find(|r| r.name == name && r.holds_name())
}

fn not_found(name: &str) -> anyhow::Error {
    fault("not_found", format!("No graft named {name}"), Value::Null)
}

fn bad_lease(name: &str) -> anyhow::Error {
    fault(
        "lease_mismatch",
        "The workspace lease does not match",
        json!({"name":name}),
    )
}

/// The lease belongs to a graft that is now archived. Say so, and how to get
/// the work back, so an agent does not abandon it. The lease is not repeated.
fn archived(r: &Record, name_in_use: bool) -> anyhow::Error {
    let (by, archived_by) = match (r.status, &r.unmerged_head) {
        (Status::Landed, None) => ("merge", "merge"),
        (Status::Landed, Some(_)) => ("remove --discard after an earlier merge", "remove"),
        _ => (
            "remove, or by prune after its lease expired",
            "remove_or_prune",
        ),
    };
    let next = if name_in_use {
        format!(
            "Another graft now uses the name {0}; archive that graft first, then run `git graft restore {0} --lease <this lease>` (MCP: graft_restore) to reopen this one",
            r.name
        )
    } else {
        format!(
            "Run `git graft restore {} --lease <this lease>` (MCP: graft_restore) to reopen it",
            r.name
        )
    };
    fault(
        "graft_archived",
        format!(
            "Graft {} was archived by {by}. Its files, commits and stashes are kept in the archive. {next}",
            r.name
        ),
        json!({"name":r.name,"archived_by":archived_by,"archive_path":r.archive,"name_in_use":name_in_use}),
    )
}

pub fn load(p: &Project, name: &str) -> Result<Record> {
    validate_name(name)?;
    let registry = scan(p)?;
    registry.damaged_name(name)?;
    registry
        .current(name)
        .cloned()
        .ok_or_else(|| registry.missing(name, not_found(name)))
}

pub fn authenticate(p: &Project, name: &str, lease: &str) -> Result<Record> {
    validate_name(name)?;
    let registry = scan(p)?;
    registry.damaged_name(name)?;
    let live = registry.current(name);
    if let Some(r) = live
        && r.lease == lease
        && !r.status.retired()
    {
        return Ok(r.clone());
    }
    if let Some(r) = registry
        .records
        .iter()
        .find(|r| r.name == name && r.lease == lease && r.status.retired())
    {
        return Err(archived(r, live.is_some_and(|live| live.id != r.id)));
    }
    Err(match live {
        Some(_) => bad_lease(name),
        None => registry.missing(name, not_found(name)),
    })
}

pub fn load_retired(p: &Project, name: &str, lease: &str) -> Result<Record> {
    validate_name(name)?;
    let registry = scan(p)?;
    registry.damaged_name(name)?;
    for mut r in registry.records.iter().filter(|r| r.name == name).cloned() {
        if r.lease == lease && r.status.retired() {
            return Ok(r);
        }
        if let Some(receipt) = r.previous.iter().find(|old| old.lease == lease) {
            r.lease = receipt.lease.clone();
            r.status = receipt.status;
            r.path = receipt.path.clone();
            r.plan = receipt.plan.clone();
            r.unmerged_head = receipt.unmerged_head.clone();
            r.archive = None;
            r.transition = None;
            return Ok(r);
        }
    }
    Err(registry.missing(name, bad_lease(name)))
}

fn persist(p: &Project, r: &Record) -> Result<()> {
    write_record(&record_directory(p, false)?, r)
}

fn write_record(directory: &Path, r: &Record) -> Result<()> {
    validate_record(r)?;
    let path = directory.join(format!("{}.json", r.id));
    ensure!(absent_or_regular(&path)?, "graft record path is occupied");
    atomic(&path, &serde_json::to_vec(r)?)
}

/// Whether `path` is missing or an ordinary file reached without a link.
fn absent_or_regular(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_file() && !is_reparse_point(&metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

/// Persist runtime data or a merge plan on the current generation.
pub fn save(p: &Project, r: &Record) -> Result<()> {
    let directory = record_directory(p, false)?;
    let mut live = read_record(&directory, &r.id)?;
    ensure!(
        live.name == r.name
            && live.lease == r.lease
            && live.path == r.path
            && live.status == r.status
            && live.project == p.repo
            && live.transition.is_none()
            && !live.status.retired(),
        "stale independent workspace record"
    );
    live.plan.clone_from(&r.plan);
    live.storage.clone_from(&r.storage);
    live.heartbeat = live.heartbeat.max(r.heartbeat);
    write_record(&directory, &live)
}

fn verify_at(
    path: &Path,
    identity: &Identity,
    expected: &Marker,
    initialized: bool,
) -> Result<Option<usize>> {
    ensure!(
        dunce::canonicalize(path)? == path,
        "graft path traverses an alias at {}",
        path.display()
    );
    ensure!(
        Identity::read(path)? == *identity,
        "graft directory was replaced at {}",
        path.display()
    );
    let admin = path.join(".git");
    let actual: Marker = serde_json::from_slice(&regular(&admin.join(MARKER))?)?;
    ensure!(actual == *expected, "graft ownership marker does not match");
    ensure!(
        !exists(&admin.join("commondir"))?,
        "graft uses an external Git common directory"
    );
    if !initialized {
        return Ok(None);
    }
    let objects = admin.join("objects");
    owned_directory(&objects)?;
    for name in ["objects/info", "objects/pack", "refs", "logs"] {
        let directory = admin.join(name);
        if exists(&directory)? {
            owned_directory(&directory)?;
        }
    }
    for name in ["config", "config.worktree", "index", "packed-refs"] {
        let file = admin.join(name);
        ensure!(
            absent_or_regular(&file)?,
            "workspace Git metadata is redirected at {}",
            file.display()
        );
    }
    ensure!(
        !exists(&objects.join("info/alternates"))?,
        "graft depends on an external Git object store"
    );
    let raw = regular(&admin.join("HEAD"))?;
    let head = without_line_ending(&raw);
    if head == b"ref: refs/heads/.invalid" {
        return verify_reftable_head(path, &admin);
    }
    if let Some(branch) = head.strip_prefix(b"ref: refs/heads/") {
        ensure!(valid_branch(branch), "graft has an invalid Git HEAD");
        Ok(None)
    } else {
        ensure!(
            matches!(head.len(), 40 | 64)
                && head.iter().all(u8::is_ascii_hexdigit)
                && head.iter().any(|byte| *byte != b'0'),
            "graft has an invalid Git HEAD"
        );
        Ok(Some(head.len()))
    }
}

fn without_line_ending(bytes: &[u8]) -> &[u8] {
    bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(bytes)
}

fn verify_reftable_head(path: &Path, admin: &Path) -> Result<Option<usize>> {
    // Reftable keeps the real HEAD in the ref store and leaves this sentinel
    // in the file where older Git versions expect the files-backend HEAD.
    let table = admin.join("reftable");
    owned_directory(&table)?;
    regular(&table.join("tables.list"))?;
    let head = git::raw(
        path,
        &[
            "--git-dir=.git",
            "symbolic-ref",
            "--quiet",
            "--no-recurse",
            "HEAD",
        ],
    )?;
    match head.status.code() {
        Some(0) => {
            ensure!(
                without_line_ending(&head.stdout)
                    .strip_prefix(b"refs/heads/")
                    .is_some_and(valid_branch),
                "graft has an invalid reftable Git HEAD"
            );
            Ok(None)
        }
        Some(1) => {
            let oid = git::text(
                path,
                &["--git-dir=.git", "rev-parse", "--verify", "HEAD^{commit}"],
            )?;
            Ok(Some(git::checked_oid(&oid)?.len()))
        }
        _ => bail!("cannot verify reftable Git HEAD"),
    }
}

fn verify_endpoint(p: &Project, r: &Record, path: &Path) -> Result<Option<usize>> {
    verify_at(path, &r.identity, &marker(p, &r.id, &r.name), r.initialized)
}

/// Verify the workspace identity at `path` and, once Git is initialized,
/// that Git resolves its working directory there.
fn verify_checkout(p: &Project, r: &Record, path: &Path) -> Result<()> {
    let detached = verify_endpoint(p, r, path)?;
    if r.initialized {
        verify_git_location(path, detached)?;
    }
    Ok(())
}

fn registered_path(p: &Project, name: &str, path: &Path) -> Result<()> {
    validate_path(path)?;
    ensure!(
        !overlaps(path, &p.repo) && !overlaps(path, &p.common),
        "graft path overlaps the primary repository"
    );
    if overlaps(path, &p.root) {
        ensure!(
            path == workspaces(p, false)?.join(name),
            "invalid managed workspace path"
        );
    }
    Ok(())
}

fn verify_current(p: &Project, r: &Record) -> Result<()> {
    ensure!(
        r.project == p.repo && !r.status.retired() && r.transition.is_none(),
        "graft lifecycle transition requires recovery"
    );
    registered_path(p, &r.name, &r.path)?;
    verify_checkout(p, r, &r.path)
}

fn matching_record(directory: &Path, r: &Record) -> Result<Record> {
    let stored = read_record(directory, &r.id)?;
    ensure!(
        stored.name == r.name
            && stored.lease == r.lease
            && stored.path == r.path
            && stored.status == r.status,
        "stale workspace record"
    );
    Ok(stored)
}

/// Verify the authoritative record and its Git directory without rescanning history.
pub fn verify(p: &Project, r: &Record) -> Result<()> {
    let stored = matching_record(&record_directory(p, false)?, r)?;
    verify_current(p, &stored)
}

fn verify_git_location(path: &Path, detached: Option<usize>) -> Result<()> {
    // Explicitly select the already verified .git directory. This prevents
    // discovery from substituting an enclosing repository and still permits
    // Git to report the effective working directory from its own config.
    let output = git::text(
        path,
        &[
            "--git-dir=.git",
            "rev-parse",
            "--show-object-format",
            "--show-toplevel",
        ],
    )?;
    let (format, worktree) = output
        .split_once('\n')
        .context("incomplete Git workspace location")?;
    ensure!(
        dunce::canonicalize(worktree)? == path,
        "Git working directory differs from the registered workspace"
    );
    let oid_len = match format.trim_end_matches('\r') {
        "sha1" => 40,
        "sha256" => 64,
        _ => bail!("graft has an unsupported Git object format"),
    };
    ensure!(
        detached.is_none_or(|len| len == oid_len),
        "graft has an invalid Git HEAD"
    );
    Ok(())
}

pub struct Location {
    pub project: PathBuf,
    pub name: String,
    pub id: String,
}

/// Discover the primary and workspace identity from a checkout or its child.
pub fn discover(path: &Path) -> Result<Option<Location>> {
    let canonical = dunce::canonicalize(path)?;
    for path in canonical.ancestors() {
        let admin = path.join(".git");
        let metadata = match fs::symlink_metadata(&admin) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() || is_reparse_point(&metadata) {
            return Ok(None);
        }
        ensure!(
            !exists(&admin.join(LEGACY_MARKER))?,
            "legacy workspace marker at {}",
            path.display()
        );
        let marker_path = admin.join(MARKER);
        if !exists(&marker_path)? {
            return Ok(None);
        }
        let m: Marker = serde_json::from_slice(&regular(&marker_path)?)?;
        ensure!(
            dunce::canonicalize(&m.project)? == m.project
                && dunce::canonicalize(&m.common)? == m.common
                && project_id(&m.common)? == m.project_id,
            "independent workspace primary identity does not match"
        );
        let state = m.common.join("graft").join(NAMESPACE);
        marked_dir(&state, &m.project_id, false)?;
        let directory = state.join("records");
        owned_directory(&directory)?;
        let r = read_record(&directory, &m.id)?;
        ensure!(
            r.name == m.name && r.project == m.project,
            "graft marker does not match the registry"
        );
        // After a filesystem rename the invoking shell can already be in the
        // new location. Route it to the project so the operation can replay.
        let transition_path = match &r.transition {
            Some(Transition::Move { to }) => Some(to),
            Some(Transition::Retire { .. }) => r.archive.as_ref(),
            _ => None,
        };
        ensure!(
            r.path == path
                || transition_path.is_some_and(|destination| destination == path)
                || r.archive.as_deref() == Some(path),
            "graft path does not match the registry; run repair from the primary"
        );
        if r.archive.as_deref() == Some(path) {
            let archives = path
                .parent()
                .context("missing workspace archive directory")?;
            ensure!(
                archives.file_name().is_some_and(|name| name == ARCHIVES)
                    && path.file_name().is_some_and(|id| id == r.id.as_str()),
                "invalid workspace archive path"
            );
            marked_dir(archives, &m.project_id, false)?;
        }
        verify_at(path, &r.identity, &m, r.initialized)?;
        return Ok(Some(Location {
            project: m.project,
            name: m.name,
            id: m.id,
        }));
    }
    Ok(None)
}

fn candidate_path(path: &Path) -> Result<PathBuf> {
    validate_path(path)?;
    let name = path
        .file_name()
        .context("graft destination must name a directory")?;
    let parent = path
        .parent()
        .context("missing workspace destination parent")?;
    let result = dunce::canonicalize(parent)?.join(name);
    ensure!(
        result == path,
        "graft destination must use its canonical parent"
    );
    #[cfg(windows)]
    {
        let name = name.to_str().context("graft destination must be UTF-8")?;
        ensure!(
            !name.ends_with([' ', '.'])
                && !name
                    .chars()
                    .any(|c| c.is_control() || "<>:\"|?*".contains(c)),
            "invalid Windows workspace destination"
        );
    }
    Ok(result)
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn safe_destination(p: &Project, r: &Record, path: &Path) -> Result<PathBuf> {
    let dest = candidate_path(path)?;
    registered_path(p, &r.name, &dest)?;
    for other in records(p)?.into_iter().filter(|other| other.id != r.id) {
        let other_target = match &other.transition {
            Some(Transition::Move { to }) => Some(to.as_path()),
            _ => None,
        };
        if other.holds_name() {
            ensure!(
                !overlaps(&dest, &other.path)
                    && !other_target.is_some_and(|to| overlaps(&dest, to)),
                "destination overlaps another managed workspace"
            );
        }
    }
    Ok(dest)
}

fn ensure_unoccupied(path: &Path) -> Result<()> {
    if exists(path)? {
        return Err(fault(
            "already_exists",
            "Workspace destination is occupied",
            json!({"path":path}),
        ));
    }
    Ok(())
}

fn archive_path(p: &Project, r: &Record, create: bool) -> Result<PathBuf> {
    let root = workspaces(p, false)?.join(ARCHIVES);
    marked_dir(&root, p.id(), create)?;
    Ok(root.join(&r.id))
}

/// The archive path, checked against the one the record journaled.
fn recorded_archive(p: &Project, r: &Record) -> Result<PathBuf> {
    let archive = archive_path(p, r, false)?;
    ensure!(
        r.archive.as_ref() == Some(&archive),
        "invalid workspace archive path"
    );
    Ok(archive)
}

pub(super) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn same_filesystem(source: &Path, destination: &Path) -> Result<bool> {
    let parent = destination.parent().context("destination has no parent")?;
    Ok(file_id(source)?.volume == file_id(parent)?.volume)
}

/// Resume an exclusive rename using directory identity to distinguish the
/// operation's result from preexisting or replacement filesystem entries.
fn relocate(p: &Project, r: &Record, from: &Path, to: &Path, fresh_placement: bool) -> Result<()> {
    ensure!(from != to, "source and destination must differ");
    if exists(to)? {
        verify_checkout(p, r, to)
            .with_context(|| format!("destination {} is occupied", to.display()))?;
        ensure!(
            !exists(from)? || verify_endpoint(p, r, from).is_err(),
            "both relocation endpoints claim workspace ownership"
        );
    } else {
        candidate_path(to)?;
        verify_endpoint(p, r, from)?;
        if !fresh_placement {
            require_movable_git(r, from)?;
        }
        rename_exclusive(from, to).with_context(|| {
            format!("move workspace from {} to {}", from.display(), to.display())
        })?;
        if let Err(error) = verify_checkout(p, r, to) {
            rename_exclusive(to, from).context(
                "graft changed during rename and could not be returned to its original path",
            )?;
            return Err(error).context("graft changed during rename");
        }
    }
    // Persist the directory entries before committing the journal record.
    let from_parent = from.parent().context("source has no parent")?;
    let to_parent = to.parent().context("destination has no parent")?;
    sync_directory(to_parent)?;
    if from_parent != to_parent && exists(from_parent)? {
        sync_directory(from_parent)?;
    }
    // An external Git command can run independently of our locks. If a
    // nested checkout appeared during the rename, retain both the data and
    // journal at the destination for recovery.
    if !fresh_placement {
        require_movable_git(r, to)?;
    }
    Ok(())
}

fn placement_source(root: &Path, r: &Record, from: &Path) -> Result<()> {
    ensure!(
        from.parent() == Some(root)
            && r.path == root.join(&r.name)
            && from
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(PENDING)),
        "invalid pending workspace location"
    );
    Ok(())
}

fn resume(p: &Project, r: &mut Record) -> Result<()> {
    ensure!(
        r.project == p.repo,
        "graft record belongs to a different primary"
    );
    registered_path(p, &r.name, &r.path)?;
    match r
        .transition
        .clone()
        .context("no workspace transition to resume")?
    {
        Transition::Place { from } => {
            let parent = workspaces(p, false)?;
            placement_source(&parent, r, &from)?;
            relocate(p, r, &from, &r.path, false)?;
        }
        Transition::Move { to } => {
            safe_destination(p, r, &to)?;
            relocate(p, r, &r.path, &to, false)?;
            r.path = to;
        }
        Transition::Retire { status, from } => {
            let archive = recorded_archive(p, r)?;
            if let Some(from) = &from {
                placement_source(&p.root, r, from)?;
            }
            relocate(p, r, from.as_deref().unwrap_or(&r.path), &archive, false)?;
            r.status = status;
        }
        Transition::Restore { lease, port } => {
            let archive = recorded_archive(p, r)?;
            safe_destination(p, r, &r.path)?;
            relocate(p, r, &archive, &r.path, false)?;
            r.previous.push(Retirement {
                lease: std::mem::replace(&mut r.lease, lease),
                status: r.status,
                path: r.path.clone(),
                plan: r.plan.take(),
                unmerged_head: r.unmerged_head.take(),
            });
            r.archive = None;
            r.status = if r.initialized {
                Status::Active
            } else {
                Status::Pending
            };
            r.port = port;
            r.heartbeat = now();
        }
    }
    r.transition = None;
    persist(p, r)
}

fn port_for(registry: &Registry, name: &str) -> Result<u16> {
    let slots = u32::from(PORTS.end - PORTS.start);
    let hash = name
        .bytes()
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b)));
    let used = registry
        .records
        .iter()
        .filter_map(|r| match r.transition {
            Some(Transition::Restore { port, .. }) => Some(port),
            _ if !r.status.retired() => Some(r.port),
            _ => None,
        })
        .chain(
            registry
                .damaged
                .iter()
                .flat_map(|d| d.ports.iter().copied()),
        )
        .collect::<HashSet<_>>();
    for offset in 0..slots {
        let port = PORTS.start + ((hash % slots + offset) % slots) as u16;
        if !used.contains(&port) {
            return Ok(port);
        }
    }
    bail!("all workspace port slots are in use")
}

const PORTS: std::ops::Range<u16> = 30000..50000;

fn valid_port(port: u16) -> bool {
    PORTS.contains(&port)
}

pub fn reserve(
    p: &Project,
    name: &str,
    request_id: Option<&str>,
    base: Option<&str>,
    branch: Option<&str>,
    request: Value,
) -> Result<Record> {
    validate_name(name)?;
    if let Some(base) = base {
        git::checked_oid(base)?;
    }
    let requested = request_id
        .map(Uuid::parse_str)
        .transpose()?
        .map(|id| id.to_string());
    let registry = scan(p)?;
    crate::trace::mark("reserve: read records");
    registry.damaged_name(name)?;
    let known = &registry.records;
    if let Some(mut r) = current(known, name).cloned() {
        if requested.as_ref() == Some(&r.lease)
            && r.base.as_deref() == base
            && r.branch.as_deref() == branch
            && r.request == request
            && !r.status.retired()
        {
            if r.placing() {
                resume(p, &mut r)?;
            }
            verify_current(p, &r)?;
            return Ok(r);
        }
        return Err(fault(
            "already_exists",
            "Workspace name is already in use",
            json!({"name":name}),
        ));
    }
    if let Some(lease) = &requested {
        let id = Uuid::parse_str(lease)?;
        if id.get_version() != Some(uuid::Version::Random)
            || id.get_variant() != uuid::Variant::RFC4122
        {
            return Err(fault(
                "invalid_request_id",
                "Use a randomly generated UUIDv4 for request_id",
                Value::Null,
            ));
        }
        ensure!(
            !known
                .iter()
                .any(|r| r.lease == *lease
                    || r.previous.iter().any(|old| old.lease == *lease)
                    || matches!(&r.transition, Some(Transition::Restore { lease: next, .. }) if next == lease))
                && !registry.damaged.iter().any(|d| d.leases.contains(lease)),
            "request_id has already been used"
        );
    }
    if !exists(&p.state.join(NAMESPACE))? {
        validate_fresh_root(p)?;
    }
    crate::trace::mark("reserve: validate fresh root");
    let directory = record_directory(p, true)?;
    crate::trace::mark("reserve: record directory");
    let root = workspaces(p, true)?;
    crate::trace::mark("reserve: workspace root");
    let path = root.join(name);
    ensure_unoccupied(&path)?;
    let namespace = directory.parent().context("missing workspace namespace")?;
    let _lock = lock_in(namespace, name)?;
    // Assemble the identity before exposing the workspace at its named path.
    let stage = private_stage(&root, PENDING)?;
    let admin = stage.path().join(".git");
    dir(&admin)?;
    let id = Uuid::new_v4().to_string();
    atomic(
        &admin.join(MARKER),
        &serde_json::to_vec(&marker(p, &id, name))?,
    )?;
    sync_directory(stage.path())?;
    sync_directory(&root)?;
    crate::trace::mark("reserve: stage marker");
    let identity = Identity::read(stage.path())?;
    crate::trace::mark("reserve: identity");
    let port = port_for(&registry, name)?;
    let started = now();
    let from = stage.keep();
    let mut r = Record {
        id,
        name: name.into(),
        lease: requested.unwrap_or_else(|| Uuid::new_v4().to_string()),
        base: base.map(str::to_owned),
        branch: branch.map(str::to_owned),
        path,
        status: Status::Pending,
        plan: None,
        archive: None,
        locked: None,
        storage: Value::Null,
        request,
        port,
        created: started,
        heartbeat: started,
        project: p.repo.clone(),
        identity,
        initialized: false,
        transition: Some(Transition::Place { from: from.clone() }),
        previous: Vec::new(),
        unmerged_head: None,
    };
    let result = (|| {
        write_record(&directory, &r)?;
        crate::trace::mark("reserve: write placement");
        // The private stage was just created with only our .git marker.
        relocate(p, &r, &from, &r.path, true)?;
        crate::trace::mark("reserve: relocate");
        r.transition = None;
        write_record(&directory, &r)
    })();
    if let Err(error) = result {
        return Err(fault(
            "spawn_incomplete",
            format!("Workspace reservation could not be placed: {error:#}"),
            json!({"name":r.name,"lease":r.lease,"path":r.path}),
        ));
    }
    Ok(r)
}

pub fn finish(p: &Project, r: &mut Record) -> Result<()> {
    let directory = record_directory(p, false)?;
    let mut live = matching_record(&directory, r)?;
    ensure!(
        live.project == p.repo && live.status == Status::Pending && live.transition.is_none(),
        "graft is not pending or placement requires recovery"
    );
    registered_path(p, &live.name, &live.path)?;
    let detached = verify_at(
        &live.path,
        &live.identity,
        &marker(p, &live.id, &live.name),
        true,
    )?;
    verify_git_location(&live.path, detached)?;
    live.initialized = true;
    live.status = Status::Active;
    live.heartbeat = now();
    live.storage.clone_from(&r.storage);
    live.locked.clone_from(&r.locked);
    write_record(&directory, &live)?;
    *r = live;
    Ok(())
}

fn require_unlocked(r: &Record) -> Result<()> {
    if r.locked.is_some() {
        return Err(fault(
            "workspace_locked",
            "The workspace is locked",
            json!({"name":r.name}),
        ));
    }
    Ok(())
}

pub fn move_workspace(p: &Project, name: &str, lease: &str, path: &Path) -> Result<Record> {
    let mut r = authenticate(p, name, lease)?;
    let _lock = workspace_lock(p, name)?;
    require_unlocked(&r)?;
    ensure!(r.plan.is_none(), "graft has a merge in progress");
    let to = safe_destination(p, &r, path)?;
    if let Some(Transition::Move { to: pending }) = &r.transition {
        ensure!(*pending == to, "graft already has a move in progress");
    } else {
        verify(p, &r)?;
        if r.path == to {
            return Ok(r);
        }
        require_movable_git(&r, &r.path)?;
        ensure!(
            !overlaps(&r.path, &to),
            "destination overlaps the workspace"
        );
        ensure_unoccupied(&to)?;
        ensure!(
            same_filesystem(&r.path, &to)?,
            "graft moves must stay on the same filesystem"
        );
        r.transition = Some(Transition::Move { to });
        persist(p, &r)?;
    }
    resume(p, &mut r)?;
    Ok(r)
}

pub fn repair(p: &Project, name: &str, lease: &str, path: &Path) -> Result<Record> {
    let mut r = authenticate(p, name, lease)?;
    let _lock = workspace_lock(p, name)?;
    let path = safe_destination(p, &r, path)?;
    verify_checkout(p, &r, &path)?;
    require_movable_git(&r, &path)?;
    match &r.transition {
        Some(Transition::Move { to }) if *to == path => resume(p, &mut r)?,
        None | Some(Transition::Move { .. }) => {
            ensure!(
                r.path == path || !exists(&r.path)? || verify_endpoint(p, &r, &r.path).is_err(),
                "the workspace still exists at its registered path"
            );
            r.path = path;
            r.transition = None;
            persist(p, &r)?;
        }
        Some(_) => bail!("graft has a lifecycle transition in progress"),
    }
    Ok(r)
}

pub fn lock(p: &Project, name: &str, lease: &str, reason: Option<&str>) -> Result<Record> {
    let mut r = authenticate(p, name, lease)?;
    let _lock = workspace_lock(p, name)?;
    verify(p, &r)?;
    ensure!(r.plan.is_none(), "graft has a merge in progress");
    r.locked = Some(reason.unwrap_or_default().into());
    persist(p, &r)?;
    Ok(r)
}

pub fn unlock(p: &Project, name: &str, lease: &str) -> Result<Record> {
    let mut r = authenticate(p, name, lease)?;
    let _lock = workspace_lock(p, name)?;
    verify(p, &r)?;
    r.locked = None;
    persist(p, &r)?;
    Ok(r)
}

fn begin_retirement(
    p: &Project,
    r: &mut Record,
    status: Status,
    unmerged_head: Option<&str>,
) -> Result<()> {
    require_unlocked(r)?;
    ensure!(
        (status == Status::Landed) == r.plan.is_some(),
        "retirement does not match the workspace merge plan"
    );
    match &r.transition {
        Some(Transition::Retire {
            status: pending, ..
        }) => {
            ensure!(
                *pending == status,
                "graft already has a retirement in progress"
            );
        }
        _ => {
            let (archive, from) = retirement_destination(p, r)?;
            r.archive = Some(archive);
            r.unmerged_head = unmerged_head.map(str::to_owned);
            r.transition = Some(Transition::Retire { status, from });
            persist(p, r)?;
        }
    }
    resume(p, r)
}

/// Check before applying mutations to the primary. A merge already
/// interrupted during its archive move can proceed directly to replay.
pub fn preflight_retire(p: &Project, r: &Record) -> Result<()> {
    if r.retiring_to().is_none() {
        retirement_destination(p, r)?;
    }
    Ok(())
}

fn has_linked_checkouts(path: &Path) -> Result<bool> {
    let linked = path.join(".git").join("worktrees");
    if exists(&linked)? {
        owned_directory(&linked)?;
        return Ok(fs::read_dir(linked)?.next().transpose()?.is_some());
    }
    Ok(false)
}

fn require_movable_git(r: &Record, path: &Path) -> Result<()> {
    check_relocation_tree(path)?;
    ensure!(
        !has_linked_checkouts(path)?,
        "graft has external Git linked checkouts"
    );
    if r.initialized {
        // A configured worktree path can change meaning after an otherwise
        // successful directory rename. Preserve that configuration in place.
        ensure_relocatable_worktree(path)?;
    }
    Ok(())
}

fn ensure_relocatable_worktree(path: &Path) -> Result<()> {
    let config = git::raw(path, &["config", "--get", "core.worktree"])?;
    ensure!(
        config.status.code() == Some(1)
            || (config.status.success()
                && std::str::from_utf8(&config.stdout).is_ok_and(|value| Path::new(
                    value.trim_end_matches(['\r', '\n'])
                ) == Path::new(".."))),
        "graft has a configured or unverified Git working directory"
    );
    Ok(())
}

fn retirement_destination(p: &Project, r: &Record) -> Result<(PathBuf, Option<PathBuf>)> {
    let from = if let Some(Transition::Place { from }) = &r.transition {
        let root = workspaces(p, false)?;
        placement_source(&root, r, from)?;
        // The rename may already have completed while the record still says
        // Place. If a foreign path won the destination, keep that path intact.
        if verify_endpoint(p, r, &r.path).is_ok() {
            None
        } else {
            verify_endpoint(p, r, from)?;
            Some(from.clone())
        }
    } else {
        verify_current(p, r)?;
        None
    };
    require_unlocked(r)?;
    let source = from.as_deref().unwrap_or(&r.path);
    require_movable_git(r, source)?;
    let archive = archive_path(p, r, true)?;
    ensure_unoccupied(&archive)?;
    ensure!(
        same_filesystem(source, &archive)?,
        "archives must be on the same filesystem as the workspace"
    );
    Ok((archive, from))
}

/// The caller holds the workspace lock until the Git operation and archive
/// transition have both completed. `unmerged_head` is journaled with a new
/// retirement only; a replay keeps what the first attempt recorded.
pub fn retire(
    p: &Project,
    name: &str,
    lease: &str,
    status: Status,
    unmerged_head: Option<&str>,
) -> Result<Record> {
    ensure!(status.retired(), "expected terminal workspace status");
    if let Ok(completed) = load_retired(p, name, lease) {
        ensure!(
            completed.status == status,
            "graft was retired by another operation"
        );
        return Ok(completed);
    }
    let mut r = authenticate(p, name, lease)?;
    begin_retirement(p, &mut r, status, unmerged_head)?;
    Ok(r)
}

pub fn restore(p: &Project, name: &str, lease: &str) -> Result<Record> {
    validate_name(name)?;
    let registry = scan(p)?;
    registry.damaged_name(name)?;
    let known = &registry.records;
    let active = current(known, name);
    if let Some(r) = active
        && !r.status.retired()
        && r.previous.last().is_some_and(|old| old.lease == lease)
    {
        let _lock = workspace_lock(p, name)?;
        verify(p, r)?;
        return Ok(r.clone());
    }
    let mut r = known
        .iter()
        .find(|r| r.name == name && r.lease == lease && r.status.retired())
        .cloned()
        .ok_or_else(|| registry.missing(name, bad_lease(name)))?;
    let _lock = workspace_lock(p, name)?;
    if active.as_ref().is_some_and(|active| active.id != r.id) {
        return Err(fault(
            "already_exists",
            "Another graft is using this name; merge or remove it before restoring the archive",
            json!({"name": name, "archive_path": r.archive}),
        ));
    }
    if r.transition.is_none() {
        // A missing or hand-edited archive is left exactly as found.
        let archive = recorded_archive(p, &r)
            .and_then(|archive| verify_checkout(p, &r, &archive).map(|()| archive))
            .map_err(|error| {
                fault(
                    "archive_damaged",
                    format!(
                        "The graft archive is missing or damaged, so restore left it untouched: {error:#}"
                    ),
                    json!({"name": name, "archive_path": r.archive}),
                )
            })?;
        require_movable_git(&r, &archive)?;
        safe_destination(p, &r, &r.path)?;
        ensure_unoccupied(&r.path)?;
        ensure!(
            same_filesystem(&archive, &r.path)?,
            "graft restores must stay on the same filesystem"
        );
        r.transition = Some(Transition::Restore {
            lease: Uuid::new_v4().to_string(),
            port: port_for(&registry, name)?,
        });
        persist(p, &r)?;
    }
    resume(p, &mut r)?;
    Ok(r)
}

/// Whether any graft may still be live. A damaged record counts as live:
/// its state is unknown, so caches it might use must be kept.
pub fn has_live(p: &Project) -> Result<bool> {
    let registry = scan(p)?;
    Ok(!registry.damaged.is_empty()
        || registry
            .records
            .iter()
            .any(|r| !r.status.retired() || r.transition.is_some()))
}

pub fn list(p: &Project) -> Result<Vec<Summary>> {
    Ok(records(p)?
        .into_iter()
        .filter(|r| !r.status.retired() || r.transition.is_some())
        .map(|r| {
            let available = registered_path(p, &r.name, &r.path).is_ok()
                && verify_endpoint(p, &r, &r.path).is_ok();
            summary(&r, available, None)
        })
        .collect())
}

pub fn recoverable(p: &Project) -> Result<Vec<Summary>> {
    let mut result = Vec::new();
    for mut r in records(p)? {
        if r.status.retired() {
            let available = r.archive.as_ref().is_some_and(|path| {
                archive_path(p, &r, false).is_ok_and(|expected| expected == *path)
                    && verify_endpoint(p, &r, path).is_ok()
            });
            result.push(summary(&r, available, None));
        }
        r.archive = None;
        r.transition = None;
        for (index, old) in std::mem::take(&mut r.previous).into_iter().enumerate() {
            r.lease = old.lease;
            r.path = old.path;
            r.status = old.status;
            r.plan = old.plan;
            r.unmerged_head = old.unmerged_head;
            let mut prior = summary(&r, false, Some("restored"));
            prior.receipt_id = Some(format!("{}:{}", r.id, index + 1));
            result.push(prior);
        }
    }
    Ok(result)
}

/// Only an idle checkout at its original local refs is eligible for expiry.
fn prune_eligibility(r: &Record, detached: Option<usize>) -> Result<Option<&'static str>> {
    let path = &r.path;
    if let Err(error) = check_relocation_tree(path) {
        return if is_nested_git_error(&error) {
            Ok(Some("nested_git_metadata"))
        } else {
            Err(error)
        };
    }
    verify_git_location(path, detached)?;
    if has_linked_checkouts(path)? {
        return Ok(Some("linked_checkouts"));
    }
    ensure_relocatable_worktree(path)?;
    if git::check_idle(path).is_err() || !git::changes(path)?.is_empty() {
        return Ok(Some("dirty"));
    }
    if !unmerged_refs(r)?.is_empty() {
        return Ok(Some("changed"));
    }
    Ok(None)
}

/// Every way the graft's refs differ from how it was created: commits on its
/// branch or a moved HEAD, other branches, tags, a stash or any other ref.
/// Each is work that a merge never took, described for the person.
fn unmerged_refs(r: &Record) -> Result<Vec<String>> {
    let path = &r.path;
    let expected = r
        .branch
        .as_ref()
        .map(|branch| format!("refs/heads/{branch}"));
    let mut found = Vec::new();
    let head = git::symbolic_ref(path, "HEAD")?;
    if head != expected {
        found.push(match &head {
            Some(other) => format!("HEAD is on {other}"),
            None => "HEAD is detached".to_owned(),
        });
    }
    let refs = git::text(path, &["for-each-ref", "--format=%(refname) %(objectname)"])?;
    let mut has_branch = false;
    for line in refs.lines() {
        let (name, oid) = line.split_once(' ').context("malformed Git ref listing")?;
        if Some(name) == expected.as_deref() {
            has_branch = true;
            if r.base.as_deref() != Some(oid) {
                found.push(format!("commits on branch {}", short_ref(name)));
            }
        } else if name == "refs/stash" {
            found.push("a stash".to_owned());
        } else if let Some(tag) = name.strip_prefix("refs/tags/") {
            found.push(format!("tag {tag}"));
        } else if let Some(branch) = name.strip_prefix("refs/heads/") {
            found.push(format!("branch {branch}"));
        } else {
            found.push(format!("ref {name}"));
        }
    }
    match (&expected, &r.base) {
        (Some(branch), Some(_)) if !has_branch => {
            found.push(format!("branch {} was deleted", short_ref(branch)));
        }
        _ => {}
    }
    if let Some(base) = &r.base
        && head.is_none()
        && git::oid(path, "HEAD")? != *base
    {
        found.push("commits on the detached HEAD".to_owned());
    }
    Ok(found)
}

fn short_ref(name: &str) -> &str {
    name.strip_prefix("refs/heads/").unwrap_or(name)
}

/// `remove` without `--discard` archives only a graft whose refs are as
/// they were created. Otherwise it names the work it found.
pub fn require_merged(r: &Record) -> Result<()> {
    let found = unmerged_refs(r)?;
    if found.is_empty() {
        return Ok(());
    }
    Err(fault(
        "unmerged_work",
        format!(
            "Graft {} has work that is not merged: {}. Merge it, or archive it with --discard and keep the recovery lease",
            r.name,
            found.join(", ")
        ),
        json!({"name":r.name,"found":found}),
    ))
}

fn is_nested_git_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::api::Fault>()
        .is_some_and(|fault| fault.code == "nested_git_metadata")
}

fn open_existing_lock(p: &Project, name: &str) -> Result<WorkspaceLock> {
    let path = state_dir(p, false)?.join("locks");
    owned_directory(&path)?;
    let lock = path.join(name);
    let metadata = fs::symlink_metadata(&lock)?;
    ensure!(
        metadata.is_file() && !is_reparse_point(&metadata),
        "graft lock is not an ordinary file"
    );
    try_lock(
        fs::OpenOptions::new().read(true).write(true).open(lock)?,
        name,
    )
}

/// Archive eligible expired workspaces. Archives, foreign paths and refs are
/// retained. A preview performs the same eligibility checks without writing.
/// An error on one graft is reported on that graft, and prune goes on.
pub fn prune(p: &Project, apply: bool) -> Result<Vec<Summary>> {
    let mut result = Vec::new();
    for mut r in scan(p)?.records {
        if r.status.retired() || now().saturating_sub(r.heartbeat) < LEASE_SECS {
            continue;
        }
        result.push(match prune_one(p, &mut r, apply) {
            Ok((reason, available)) => summary(&r, available, Some(reason)),
            Err(error) => Summary {
                error: Some(format!("{error:#}")),
                ..summary(&r, false, Some("error"))
            },
        });
    }
    Ok(result)
}

/// The prune reason for one expired graft, and whether it is available.
fn prune_one(p: &Project, r: &mut Record, apply: bool) -> Result<(&'static str, bool)> {
    if apply && r.retiring_to() == Some(Status::Dropped) {
        let _lock = open_existing_lock(p, &r.name)?;
        return match resume(p, r) {
            Ok(()) => Ok(("archived", false)),
            Err(error) if is_nested_git_error(&error) => Ok(("nested_git_metadata", false)),
            Err(error) => Err(error),
        };
    }
    let endpoint =
        registered_path(p, &r.name, &r.path).and_then(|()| verify_endpoint(p, r, &r.path));
    let available = endpoint.is_ok();
    let reason = if r.locked.is_some() {
        "locked"
    } else if r.plan.is_some() || r.transition.is_some() || r.status == Status::Pending {
        "transition_in_progress"
    } else if !available {
        "missing_or_replaced"
    } else {
        match open_existing_lock(p, &r.name) {
            Ok(_lock) => {
                let root = workspaces(p, false)?;
                let archives = root.join(ARCHIVES);
                let archive = if exists(&archives)? {
                    archive_path(p, r, false)?
                } else {
                    archives
                };
                if !same_filesystem(&r.path, &archive)? {
                    "different_filesystem"
                } else if exists(&archive)? {
                    "archive_occupied"
                } else if let Some(reason) =
                    prune_eligibility(r, endpoint?).unwrap_or(Some("git_state_unverified"))
                {
                    reason
                } else if apply {
                    begin_retirement(p, r, Status::Dropped, None)?;
                    "archived"
                } else {
                    "eligible"
                }
            }
            Err(error) if crate::api::error_json(&error)["code"] == "workspace_busy" => "busy",
            Err(error) => return Err(error),
        }
    };
    Ok((reason, available && !r.status.retired()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        temp: tempfile::TempDir,
        project: Project,
        _lock: File,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let primary = temp.path().join("primary");
            fs::create_dir(&primary)?;
            git::bytes(&primary, &["init", "-q", "--template=", "-b", "main"])?;
            let (project, _lock) = Project::open(&primary)?;
            Ok(Self {
                temp,
                project,
                _lock,
            })
        }

        fn workspace(&self, name: &str) -> Result<Record> {
            let mut record = reserve(&self.project, name, None, None, Some("agent"), Value::Null)?;
            git::bytes(&record.path, &["init", "-q", "--template=", "-b", "agent"])?;
            for (key, value) in [
                ("user.name", "Registry Fixture"),
                ("user.email", "registry@example.invalid"),
                ("commit.gpgsign", "false"),
                ("core.hooksPath", "disabled-hooks"),
            ] {
                git::bytes(&record.path, &["config", key, value])?;
            }
            finish(&self.project, &mut record)?;
            Ok(record)
        }

        /// A graft whose branch starts at a commit of the primary, as `add`
        /// creates it, with its heartbeat expired so prune considers it.
        fn expired_at_base(&self, name: &str) -> Result<Record> {
            let primary = &self.project.repo;
            if git::raw(primary, &["rev-parse", "--verify", "--quiet", "HEAD"])?
                .status
                .code()
                == Some(1)
            {
                fs::write(primary.join("tracked"), "base\n")?;
                git::bytes(primary, &["add", "tracked"])?;
                git::bytes(
                    primary,
                    &[
                        "-c",
                        "user.name=Registry Fixture",
                        "-c",
                        "user.email=registry@example.invalid",
                        "commit",
                        "-qm",
                        "base",
                    ],
                )?;
            }
            let base = git::oid(primary, "HEAD")?;
            let mut record = reserve(
                &self.project,
                name,
                None,
                Some(&base),
                Some("agent"),
                Value::Null,
            )?;
            let path = record.path.clone();
            git::bytes(&path, &["init", "-q", "--template=", "-b", "agent"])?;
            for (key, value) in [
                ("user.name", "Registry Fixture"),
                ("user.email", "registry@example.invalid"),
                ("commit.gpgsign", "false"),
                ("core.hooksPath", "disabled-hooks"),
            ] {
                git::bytes(&path, &["config", key, value])?;
            }
            git::bytes(&path, &["fetch", "-q", git::path_str(primary)?, "HEAD"])?;
            git::bytes(&path, &["reset", "-q", "--hard", &base])?;
            finish(&self.project, &mut record)?;
            record.heartbeat = 0;
            persist(&self.project, &record)?;
            Ok(record)
        }
    }

    /// Everything a person could lose in a graft: every ref with its target,
    /// the stash, the index and the status of every file, ignored included.
    fn work_in(path: &Path) -> Result<String> {
        Ok([
            git::text(path, &["for-each-ref", "--format=%(refname) %(objectname)"])?,
            git::text(path, &["rev-parse", "HEAD"]).unwrap_or_default(),
            git::text(path, &["ls-files", "--stage", "--debug"])?,
            git::text(path, &["status", "--porcelain=v1", "--ignored=matching"])?,
        ]
        .join("\n--\n"))
    }

    fn code(error: &anyhow::Error) -> String {
        crate::api::error_json(error)["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    fn record_file(p: &Project, r: &Record) -> Result<PathBuf> {
        Ok(state_dir(p, false)?
            .join("records")
            .join(format!("{}.json", r.id)))
    }

    #[test]
    fn prune_and_remove_keep_every_kind_of_unmerged_work() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let clean = f.expired_at_base("clean")?;
        let mut cases = Vec::new();
        let mut case = |name: &str, setup: &dyn Fn(&Path) -> Result<()>| -> Result<()> {
            let r = f.expired_at_base(name)?;
            setup(&r.path)?;
            cases.push((r.clone(), work_in(&r.path)?));
            Ok(())
        };
        let commit = |path: &Path| -> Result<()> {
            fs::write(path.join("tracked"), "unmerged\n")?;
            git::bytes(path, &["commit", "-qam", "unmerged"])?;
            Ok(())
        };
        case("commit", &commit)?;
        case("branch", &|path| {
            git::bytes(path, &["branch", "extra"])?;
            Ok(())
        })?;
        case("tag", &|path| {
            git::bytes(path, &["tag", "v1"])?;
            Ok(())
        })?;
        case("stash", &|path| {
            fs::write(path.join("tracked"), "stashed edit\n")?;
            git::bytes(path, &["stash", "push", "-q"])?;
            Ok(())
        })?;
        case("detached", &|path| {
            git::bytes(path, &["checkout", "-q", "--detach"])?;
            commit(path)
        })?;
        case("ignored", &|path| {
            fs::create_dir_all(path.join(".git/info"))?;
            fs::write(path.join(".git/info/exclude"), "build/\n")?;
            fs::create_dir(path.join("build"))?;
            fs::write(path.join("build/output"), "build output")?;
            Ok(())
        })?;
        case("staged", &|path| {
            fs::write(path.join("new"), "staged only")?;
            git::bytes(path, &["add", "new"])?;
            Ok(())
        })?;
        case("untracked", &|path| {
            fs::write(path.join("notes"), "untracked")?;
            Ok(())
        })?;
        case("gitbusy", &|path| {
            fs::write(path.join(".git/index.lock"), "")?;
            Ok(())
        })?;
        let locked = f.expired_at_base("locked")?;
        lock(p, &locked.name, &locked.lease, Some("keep"))?;
        cases.push((load(p, "locked")?, work_in(&locked.path)?));
        let busy = f.expired_at_base("busy")?;
        cases.push((busy.clone(), work_in(&busy.path)?));

        let expected = [
            ("branch", "changed"),
            ("busy", "busy"),
            ("clean", "eligible"),
            ("commit", "changed"),
            ("detached", "changed"),
            ("gitbusy", "dirty"),
            ("ignored", "dirty"),
            ("locked", "locked"),
            ("staged", "dirty"),
            ("stash", "changed"),
            ("tag", "changed"),
            ("untracked", "dirty"),
        ];
        // `exec` holds the graft lock for as long as its program runs.
        let held = workspace_lock(p, &busy.name)?;
        for apply in [false, true] {
            let reasons = prune(p, apply)?;
            let reasons: Vec<_> = reasons
                .iter()
                .map(|s| (s.name.as_str(), s.reason.unwrap_or_default()))
                .collect();
            let mut wanted = expected.to_vec();
            if apply {
                wanted[2].1 = "archived";
            }
            assert_eq!(reasons, wanted);
            for (r, before) in &cases {
                assert_eq!(&work_in(&r.path)?, before, "prune changed {}", r.name);
                assert!(authenticate(p, &r.name, &r.lease).is_ok());
            }
        }
        drop(held);
        assert!(!clean.path.exists());

        // `remove` without --discard refuses the same grafts and names the work.
        for (name, found) in [
            ("commit", "commits on branch agent"),
            ("branch", "branch extra"),
            ("tag", "tag v1"),
            ("stash", "a stash"),
            ("detached", "HEAD is detached, commits on the detached HEAD"),
        ] {
            let r = load(p, name)?;
            let error = require_merged(&r).unwrap_err();
            assert_eq!(code(&error), "unmerged_work");
            assert!(format!("{error:#}").contains(found), "{name}: {error:#}");
            assert!(!format!("{error:#}").contains(&r.lease));
        }
        require_merged(&load(p, "busy")?)?;
        Ok(())
    }

    #[test]
    fn prune_reports_occupied_archives_and_keeps_going_after_one_graft_fails() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let first = f.expired_at_base("a-first")?;
        let broken = f.expired_at_base("b-broken")?;
        let occupied = f.expired_at_base("c-occupied")?;
        let last = f.expired_at_base("d-last")?;
        // A foreign directory already holds this graft's archive slot.
        let slot = archive_path(p, &occupied, true)?;
        fs::create_dir(&slot)?;
        fs::write(slot.join("foreign"), "keep")?;
        // The graft lock is not a file, so prune cannot check this graft.
        let lock = state_dir(p, false)?.join("locks").join(&broken.name);
        fs::remove_file(&lock)?;
        fs::create_dir(&lock)?;
        let before = work_in(&broken.path)?;

        let applied = prune(p, true)?;
        let reasons: Vec<_> = applied
            .iter()
            .map(|s| (s.name.as_str(), s.reason.unwrap_or_default()))
            .collect();
        assert_eq!(
            reasons,
            [
                ("a-first", "archived"),
                ("b-broken", "error"),
                ("c-occupied", "archive_occupied"),
                ("d-last", "archived"),
            ]
        );
        assert!(
            applied[1]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("not an ordinary file"))
        );
        assert!(!first.path.exists() && !last.path.exists());
        assert_eq!(work_in(&broken.path)?, before);
        assert!(occupied.path.exists());
        assert_eq!(fs::read_to_string(slot.join("foreign"))?, "keep");
        Ok(())
    }

    #[test]
    fn a_damaged_record_is_reported_kept_and_isolated_from_other_grafts() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let torn = f.expired_at_base("torn")?;
        let invalid = f.expired_at_base("invalid")?;
        let healthy = f.expired_at_base("healthy")?;
        let torn_file = record_file(p, &torn)?;
        let invalid_file = record_file(p, &invalid)?;
        let full = fs::read(&torn_file)?;
        fs::write(&torn_file, &full[..full.len() / 2])?;
        let mut value: Value = serde_json::from_slice(&fs::read(&invalid_file)?)?;
        value["base"] = json!("not-an-object-id");
        fs::write(&invalid_file, serde_json::to_vec(&value)?)?;
        let torn_bytes = fs::read(&torn_file)?;
        let invalid_bytes = fs::read(&invalid_file)?;

        let damaged = damaged(p)?;
        assert_eq!(damaged.len(), 2);
        let report = serde_json::to_string(&damaged)?;
        assert!(report.contains("full Git object ID"));
        assert!(!report.contains(&torn.lease) && !report.contains(&invalid.lease));
        let named = damaged.iter().find(|d| d.file == invalid_file).unwrap();
        assert_eq!(named.name.as_deref(), Some("invalid"));

        // Other grafts keep working: list, prune, archive and restore.
        let listed: Vec<_> = list(p)?.into_iter().map(|s| s.name).collect();
        assert_eq!(listed, ["healthy"]);
        let pruned = prune(p, true)?;
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].name, "healthy");
        assert_eq!(pruned[0].reason, Some("archived"));
        assert!(torn.path.exists() && invalid.path.exists());
        // Every usable graft is archived, yet caches stay while a damaged
        // record might still use them.
        assert!(has_live(p)?);
        restore(p, "healthy", &healthy.lease)?;
        let other = f.expired_at_base("other")?;
        assert!(authenticate(p, &other.name, &other.lease).is_ok());

        // The damaged grafts' own names report the damage and are never reused.
        let error = authenticate(p, "invalid", &invalid.lease).unwrap_err();
        assert_eq!(code(&error), "record_damaged");
        assert!(format!("{error:#}").contains("full Git object ID"));
        let error = reserve(p, "invalid", None, None, Some("agent"), Value::Null).unwrap_err();
        assert_eq!(code(&error), "record_damaged");
        assert_eq!(code(&load(p, "torn").unwrap_err()), "record_damaged");
        assert_eq!(
            code(&restore(p, "torn", &torn.lease).unwrap_err()),
            "record_damaged"
        );
        assert_eq!(fs::read(&torn_file)?, torn_bytes);
        assert_eq!(fs::read(&invalid_file)?, invalid_bytes);

        // A copied record that claims another graft's lease taints both.
        let mut copy: Value = serde_json::from_slice(&fs::read(record_file(p, &other)?)?)?;
        let id = Uuid::new_v4().to_string();
        copy["id"] = json!(id);
        copy["name"] = json!("impostor");
        copy["port"] = json!(49999);
        let copy_file = record_file(p, &other)?.with_file_name(format!("{id}.json"));
        fs::write(&copy_file, serde_json::to_vec(&copy)?)?;
        assert_eq!(
            code(&authenticate(p, "other", &other.lease).unwrap_err()),
            "record_damaged"
        );
        assert_eq!(
            code(&authenticate(p, "impostor", &other.lease).unwrap_err()),
            "record_damaged"
        );
        assert!(authenticate(p, "healthy", &load(p, "healthy")?.lease).is_ok());
        assert!(other.path.exists());
        Ok(())
    }

    #[test]
    fn an_archived_lease_says_how_to_restore_without_showing_a_lease() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let r = f.expired_at_base("idle")?;
        assert_eq!(prune(p, true)?[0].reason, Some("archived"));
        let error = authenticate(p, &r.name, &r.lease).unwrap_err();
        let reported = crate::api::error_json(&error);
        assert_eq!(reported["code"], "graft_archived");
        assert_eq!(reported["details"]["archived_by"], "remove_or_prune");
        assert_eq!(reported["details"]["name_in_use"], false);
        let message = reported["message"].as_str().unwrap_or_default();
        assert!(message.contains("prune") && message.contains("git graft restore idle"));
        assert!(!reported.to_string().contains(&r.lease));
        // Another lease, or none, still learns nothing about the archive.
        let stranger = Uuid::new_v4().to_string();
        assert_eq!(
            code(&authenticate(p, &r.name, &stranger).unwrap_err()),
            "not_found"
        );

        // When the name is reused, the hint says to free it first.
        let next = f.expired_at_base("idle")?;
        let reported = crate::api::error_json(&authenticate(p, &r.name, &r.lease).unwrap_err());
        assert_eq!(reported["code"], "graft_archived");
        assert_eq!(reported["details"]["name_in_use"], true);
        assert!(!reported.to_string().contains(&next.lease));
        assert_eq!(
            code(&authenticate(p, &r.name, &stranger).unwrap_err()),
            "lease_mismatch"
        );
        {
            let _lock = workspace_lock(p, &next.name)?;
            retire(p, &next.name, &next.lease, Status::Dropped, None)?;
        }
        assert_eq!(restore(p, &r.name, &r.lease)?.status, Status::Active);
        Ok(())
    }

    #[test]
    fn verifies_identity_and_repairs_a_manually_moved_repository() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let r = f.workspace("agent")?;
        let manual = dunce::canonicalize(f.temp.path())?.join("manual checkout");
        rename_exclusive(&r.path, &manual)?;

        // Even a replacement carrying a copied marker cannot claim the lease.
        fs::create_dir(&r.path)?;
        fs::create_dir(r.path.join(".git"))?;
        fs::copy(
            manual.join(".git").join(MARKER),
            r.path.join(".git").join(MARKER),
        )?;
        fs::write(r.path.join("foreign"), "untouched")?;
        assert!(verify(p, &r).is_err());
        assert!(discover(&r.path).is_err());
        assert!(discover(&manual).is_err());
        let repaired = repair(p, &r.name, &r.lease, &manual)?;
        verify(p, &repaired)?;
        let found = discover(&manual)?.context("graft location missing")?;
        assert_eq!(found.project, p.repo);
        assert_eq!(found.name, r.name);
        assert_eq!(found.id, r.id);
        assert_eq!(fs::read_to_string(r.path.join("foreign"))?, "untouched");
        let head = repaired.path.join(".git/HEAD");
        let original = fs::read(&head)?;
        for corrupt in [
            "corrupt HEAD\n".into(),
            "ref: refs/heads/has..dots\n".into(),
            "ref: refs/heads/branch.lock\n".into(),
            "ref: refs/heads/agent\n\n".into(),
            format!("{}\n", "f".repeat(64)),
        ] {
            fs::write(&head, corrupt)?;
            assert!(verify(p, &repaired).is_err());
        }
        fs::write(&head, original)?;
        let marker_path = repaired.path.join(".git").join(MARKER);
        let original = fs::read(&marker_path)?;
        let mut changed: Marker = serde_json::from_slice(&original)?;
        changed.project = repaired.path.clone();
        fs::write(&marker_path, serde_json::to_vec(&changed)?)?;
        assert!(discover(&repaired.path).is_err());
        fs::write(marker_path, original)?;
        let common = repaired.path.join(".git/commondir");
        fs::write(&common, p.common.to_str().context("invalid fixture path")?)?;
        assert!(verify(p, &repaired).is_err());
        fs::remove_file(common)?;
        git::bytes(
            &repaired.path,
            &["config", "core.worktree", git::path_str(&p.repo)?],
        )?;
        assert!(verify(p, &repaired).is_err());
        git::bytes(&repaired.path, &["config", "--unset", "core.worktree"])?;
        #[cfg(unix)]
        {
            let refs = repaired.path.join(".git/refs");
            let original = refs.with_file_name("refs-saved");
            rename_exclusive(&refs, &original)?;
            std::os::unix::fs::symlink(p.common.join("refs"), &refs)?;
            assert!(verify(p, &repaired).is_err());
            fs::remove_file(&refs)?;
            rename_exclusive(&original, &refs)?;
        }

        let moved = manual.with_file_name("explicit checkout");
        lock(p, &r.name, &r.lease, Some("in use"))?;
        assert!(move_workspace(p, &r.name, &r.lease, &moved).is_err());
        assert!(preflight_retire(p, &load(p, &r.name)?).is_err());
        unlock(p, &r.name, &r.lease)?;
        assert_eq!(move_workspace(p, &r.name, &r.lease, &moved)?.path, moved);
        assert_eq!(move_workspace(p, &r.name, &r.lease, &moved)?.path, moved);
        assert!(!p.common.join("worktrees").exists());
        Ok(())
    }

    #[test]
    fn interrupted_rename_preserves_git_and_rotates_lease_once() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let mut r = f.workspace("agent")?;
        let old_lease = r.lease.clone();
        fs::write(r.path.join("tracked"), "base")?;
        git::bytes(&r.path, &["add", "tracked"])?;
        git::bytes(&r.path, &["commit", "-qm", "base"])?;
        let base = git::oid(&r.path, "HEAD")?;
        fs::write(r.path.join("tracked"), "saved edit")?;
        git::bytes(&r.path, &["stash", "push", "-q"])?;
        let stash = git::oid(&r.path, "refs/stash")?;
        fs::write(r.path.join("tracked"), "staged edit")?;
        git::bytes(&r.path, &["add", "tracked"])?;
        fs::write(r.path.join("tracked"), "unstaged edit")?;
        fs::create_dir_all(r.path.join(".git/info"))?;
        fs::write(r.path.join(".git/info/exclude"), "ignored\n")?;
        fs::write(r.path.join("ignored"), "private output")?;
        git::bytes(&r.path, &["tag", "private"])?;
        r.plan = Some(Plan {
            target: "main".into(),
            old: base.clone(),
            new: base.clone(),
            tip: base,
        });
        save(p, &r)?;

        // Crash after a successful rename but before committing retirement.
        let (archive, from) = retirement_destination(p, &r)?;
        r.archive = Some(archive.clone());
        r.transition = Some(Transition::Retire {
            status: Status::Landed,
            from,
        });
        persist(p, &r)?;
        rename_exclusive(&r.path, &archive)?;
        let retired = {
            let _lock = workspace_lock(p, &r.name)?;
            retire(p, &r.name, &old_lease, Status::Landed, None)?
        };
        assert_eq!(retired.status, Status::Landed);
        assert_eq!(git::oid(&archive, "refs/stash")?, stash);
        assert!(git::symbolic_ref(&archive, "HEAD")?.is_some());

        fs::create_dir(&r.path)?;
        fs::write(r.path.join("foreign"), "leave this")?;
        assert!(restore(p, &r.name, &old_lease).is_err());
        assert_eq!(fs::read_to_string(r.path.join("foreign"))?, "leave this");
        rename_exclusive(&r.path, &r.path.with_file_name("foreign"))?;

        // And crash after restoring the checkout but before writing its new lease.
        let mut retired = retired;
        let new_lease = Uuid::new_v4().to_string();
        retired.transition = Some(Transition::Restore {
            lease: new_lease.clone(),
            port: r.port,
        });
        persist(p, &retired)?;
        rename_exclusive(&archive, &r.path)?;
        assert_eq!(restore(p, &r.name, &old_lease)?.lease, new_lease);
        assert_eq!(restore(p, &r.name, &old_lease)?.lease, new_lease);
        assert!(authenticate(p, &r.name, &old_lease).is_err());
        assert_eq!(authenticate(p, &r.name, &new_lease)?.id, r.id);
        assert!(load_retired(p, &r.name, &old_lease)?.plan.is_some());
        assert_eq!(git::oid(&r.path, "refs/stash")?, stash);
        assert_eq!(git::text(&r.path, &["show", ":tracked"])?, "staged edit");
        assert_eq!(fs::read_to_string(r.path.join("tracked"))?, "unstaged edit");
        assert_eq!(
            fs::read_to_string(r.path.join("ignored"))?,
            "private output"
        );

        {
            let _lock = workspace_lock(p, &r.name)?;
            retire(p, &r.name, &new_lease, Status::Dropped, None)?;
        }
        let replacement = f.workspace(&r.name)?;
        assert_ne!(replacement.id, r.id);
        assert!(restore(p, &r.name, &old_lease).is_err());
        assert!(restore(p, &r.name, &new_lease).is_err());
        assert!(load_retired(p, &r.name, &old_lease)?.plan.is_some());
        let rows = serde_json::to_string(&recoverable(p)?)?;
        assert!(!rows.contains(&old_lease) && !rows.contains(&new_lease));
        assert!(!p.common.join("worktrees").exists());
        Ok(())
    }

    #[test]
    fn prune_preview_and_apply_preserve_dirty_and_replaced_paths() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let mut clean = f.workspace("clean")?;
        let mut dirty = f.workspace("dirty")?;
        let mut replaced = f.workspace("replaced")?;
        for r in [&mut clean, &mut dirty, &mut replaced] {
            r.heartbeat = 0;
            persist(p, r)?;
        }
        fs::write(dirty.path.join("private"), "contents")?;
        rename_exclusive(&replaced.path, &replaced.path.with_file_name("elsewhere"))?;
        fs::create_dir(&replaced.path)?;
        fs::write(replaced.path.join("foreign"), "contents")?;

        let preview = prune(p, false)?;
        assert_eq!(
            preview.iter().map(|r| r.reason).collect::<Vec<_>>(),
            [Some("eligible"), Some("dirty"), Some("missing_or_replaced")]
        );
        assert!(clean.path.exists());
        assert!(!workspaces(p, false)?.join(ARCHIVES).exists());
        let applied = prune(p, true)?;
        assert_eq!(applied[0].reason, Some("archived"));
        assert!(!clean.path.exists());
        assert!(dirty.path.join("private").exists() && replaced.path.join("foreign").exists());
        assert_eq!(
            restore(p, &clean.name, &clean.lease)?.status,
            Status::Active
        );
        Ok(())
    }

    #[test]
    fn pending_retry_and_explicit_archive_retain_partial_data() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let lease = Uuid::new_v4().to_string();
        let request = json!({"request_id":lease,"name":"pending"});
        let r = reserve(
            p,
            "pending",
            Some(&lease),
            None,
            Some("agent"),
            request.clone(),
        )?;
        assert_eq!(r.status, Status::Pending);
        assert_eq!(
            reserve(
                p,
                "pending",
                Some(&lease),
                None,
                Some("agent"),
                request.clone()
            )?
            .id,
            r.id
        );
        assert!(reserve(p, "pending", Some(&lease), None, Some("other"), request).is_err());
        assert!(reserve(p, "pending", Some(&lease), None, Some("agent"), Value::Null).is_err());
        assert!(reserve(p, "other", Some(&lease), None, Some("agent"), Value::Null).is_err());
        fs::write(r.path.join("partial"), "keep it")?;
        let retired = {
            let _lock = workspace_lock(p, &r.name)?;
            retire(p, &r.name, &lease, Status::Dropped, None)?
        };
        assert_eq!(retired.status, Status::Dropped);
        let restored = restore(p, &r.name, &lease)?;
        assert_eq!(restored.id, r.id);
        assert_ne!(restored.lease, lease);
        assert_eq!(restored.status, Status::Pending);
        assert_eq!(
            fs::read_to_string(restored.path.join("partial"))?,
            "keep it"
        );
        assert!(verify(p, &r).is_err());
        let public = serde_json::to_value(list(p)?)?;
        let public = public.as_array().context("expected list")?;
        assert_eq!(public[0]["path"], json!(r.path));
        assert!(public[0].get("worktree_path").is_none());
        assert!(!serde_json::to_string(public)?.contains(&lease));
        assert!(!serde_json::to_string(public)?.contains(&restored.lease));
        Ok(())
    }

    #[test]
    fn occupied_placement_can_be_archived_without_touching_the_occupant() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let lease = Uuid::new_v4().to_string();
        let mut r = reserve(p, "pending", Some(&lease), None, Some("agent"), Value::Null)?;
        let stage = p.root.join(".pending-interrupted");
        fs::write(r.path.join("partial"), "partial work")?;
        rename_exclusive(&r.path, &stage)?;
        r.transition = Some(Transition::Place {
            from: stage.clone(),
        });
        persist(p, &r)?;

        fs::create_dir(&r.path)?;
        fs::write(r.path.join("foreign"), "foreign contents")?;
        assert!(reserve(p, "pending", Some(&lease), None, Some("agent"), Value::Null).is_err());
        assert_eq!(fs::read_to_string(stage.join("partial"))?, "partial work");
        let retired = {
            let _lock = workspace_lock(p, &r.name)?;
            retire(p, &r.name, &r.lease, Status::Dropped, None)?
        };
        let archive = retired.archive.context("missing archive")?;
        assert_eq!(fs::read_to_string(archive.join("partial"))?, "partial work");
        assert!(restore(p, &r.name, &r.lease).is_err());
        assert_eq!(
            fs::read_to_string(r.path.join("foreign"))?,
            "foreign contents"
        );
        Ok(())
    }

    #[test]
    fn malformed_journals_cannot_claim_a_source_or_change_the_lifecycle() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let mut pending = reserve(p, "pending", None, None, Some("agent"), Value::Null)?;
        let directory = record_directory(p, false)?;
        let record = directory.join(format!("{}.json", pending.id));
        let before = fs::read(&record)?;
        let foreign = p.root.join("foreign");
        rename_exclusive(&pending.path, &foreign)?;
        fs::write(foreign.join("data"), "preserve")?;
        pending.transition = Some(Transition::Place {
            from: foreign.clone(),
        });
        atomic(&record, &serde_json::to_vec(&pending)?)?;
        assert!(
            reserve(
                p,
                "pending",
                Some(&pending.lease),
                None,
                Some("agent"),
                Value::Null
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(foreign.join("data"))?, "preserve");
        assert!(!pending.path.exists());

        let original: Value = serde_json::from_slice(&before)?;
        for transition in [
            json!({"action":"move","to": pending.path.join("descendant")}),
            json!({"action":"retire","status":"active"}),
            json!({"action":"restore","lease": pending.lease,"port": pending.port}),
        ] {
            let mut malformed = original.clone();
            malformed["transition"] = transition;
            if malformed["transition"]["action"] == "retire" {
                malformed["archive"] = json!(p.root.join(ARCHIVES).join(&pending.id));
            }
            atomic(&record, &serde_json::to_vec(&malformed)?)?;
            assert!(load(p, &pending.name).is_err());
        }
        assert_eq!(fs::read_to_string(foreign.join("data"))?, "preserve");
        Ok(())
    }

    #[test]
    fn replaying_move_and_prune_keeps_foreign_paths_and_archived_data() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let mut r = f.workspace("agent")?;
        let to = p.root.with_file_name("external checkout");
        let from = r.path.clone();
        r.transition = Some(Transition::Move { to: to.clone() });
        persist(p, &r)?;
        fs::create_dir(&to)?;
        fs::write(to.join("foreign"), "keep at destination")?;
        assert!(move_workspace(p, &r.name, &r.lease, &to).is_err());
        assert_eq!(
            fs::read_to_string(to.join("foreign"))?,
            "keep at destination"
        );
        assert!(from.join(".git/HEAD").is_file());
        rename_exclusive(&to, &to.with_extension("foreign"))?;
        rename_exclusive(&from, &to)?;
        fs::create_dir(&from)?;
        fs::write(from.join("foreign"), "keep at source")?;
        let mut r = move_workspace(p, &r.name, &r.lease, &to)?;
        assert_eq!(r.path, to);
        assert_eq!(fs::read_to_string(from.join("foreign"))?, "keep at source");

        r.heartbeat = 0;
        let (archive, from) = retirement_destination(p, &r)?;
        r.archive = Some(archive.clone());
        r.transition = Some(Transition::Retire {
            from,
            status: Status::Dropped,
        });
        persist(p, &r)?;
        rename_exclusive(&r.path, &archive)?;
        fs::write(archive.join("private"), "recovered")?;
        assert_eq!(prune(p, false)?[0].reason, Some("transition_in_progress"));
        assert_eq!(prune(p, true)?[0].reason, Some("archived"));
        let restored = restore(p, &r.name, &r.lease)?;
        assert_eq!(
            fs::read_to_string(restored.path.join("private"))?,
            "recovered"
        );
        Ok(())
    }

    #[test]
    fn interrupted_move_respects_a_native_linked_checkout_added_after_journaling() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let mut r = f.workspace("agent")?;
        git::bytes(&r.path, &["commit", "-qm", "base", "--allow-empty"])?;
        let base = git::oid(&r.path, "HEAD")?;
        let to = p.root.with_file_name("new location");
        r.transition = Some(Transition::Move { to: to.clone() });
        persist(p, &r)?;
        let linked = p.root.with_file_name("linked checkout");
        git::bytes(
            &r.path,
            &[
                "worktree",
                "add",
                "--detach",
                git::path_str(&linked)?,
                "HEAD",
            ],
        )?;
        let error = move_workspace(p, &r.name, &r.lease, &to).unwrap_err();
        assert!(error.to_string().contains("external Git linked checkouts"));
        assert_eq!(git::oid(&linked, "HEAD")?, base);
        assert_eq!(git::oid(&r.path, "HEAD")?, base);
        assert!(!to.exists());
        Ok(())
    }

    #[test]
    fn retains_inbound_native_checkout_and_dirty_data_on_refused_relocation() -> Result<()> {
        let expect_nested = |operation: &str, error: anyhow::Error| {
            assert!(
                is_nested_git_error(&error),
                "{operation}: expected nested_git_metadata, got {}",
                crate::api::error_json(&error)
            );
        };
        let f = Fixture::new()?;
        let p = &f.project;
        let external = f.workspace("external")?;
        git::bytes(&external.path, &["commit", "-qm", "base", "--allow-empty"])?;
        let mut r = f.workspace("agent")?;
        let nested = r.path.join("nested");
        git::bytes(
            &external.path,
            &[
                "worktree",
                "add",
                "--detach",
                git::path_str(&nested)?,
                "HEAD",
            ],
        )?;
        let dirty = nested.join("private");
        fs::write(&dirty, "preserve this")?;
        let gitfile = fs::read(nested.join(".git"))?;
        let admin = git::absolute_git_dir(&nested)?;
        let backpointer = fs::read(admin.join("gitdir"))?;

        let to = p.root.with_file_name("new workspace location");
        expect_nested(
            "move",
            move_workspace(p, &r.name, &r.lease, &to).unwrap_err(),
        );
        expect_nested("retirement preflight", preflight_retire(p, &r).unwrap_err());
        {
            let _lock = workspace_lock(p, &r.name)?;
            expect_nested(
                "drop",
                retire(p, &r.name, &r.lease, Status::Dropped, None).unwrap_err(),
            );
        }
        assert!(load(p, &r.name)?.transition.is_none());
        r.heartbeat = 0;
        persist(p, &r)?;
        assert_eq!(prune(p, false)?[0].reason, Some("nested_git_metadata"));
        assert_eq!(prune(p, true)?[0].reason, Some("nested_git_metadata"));
        assert_eq!(fs::read_to_string(&dirty)?, "preserve this");
        assert_eq!(fs::read(nested.join(".git"))?, gitfile);
        assert_eq!(fs::read(admin.join("gitdir"))?, backpointer);
        git::bytes(&nested, &["status", "--porcelain"])?;

        r.transition = Some(Transition::Move { to: to.clone() });
        persist(p, &r)?;
        expect_nested(
            "move replay at source",
            move_workspace(p, &r.name, &r.lease, &to).unwrap_err(),
        );
        assert!(!to.exists());
        // A manual rename is never adopted while it still contains gitfiles.
        rename_exclusive(&r.path, &to)?;
        expect_nested(
            "repair at destination",
            repair(p, &r.name, &r.lease, &to).unwrap_err(),
        );
        expect_nested(
            "move replay at destination",
            move_workspace(p, &r.name, &r.lease, &to).unwrap_err(),
        );
        assert_eq!(load(p, &r.name)?.path, r.path);
        rename_exclusive(&to, &r.path)?;
        assert_eq!(fs::read_to_string(dirty)?, "preserve this");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn workspace_lock_releases_even_when_its_descriptor_is_still_shared() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let r = f.workspace("agent")?;
        let lock = workspace_lock(p, &r.name)?;
        // A forked process briefly shares this open file description until
        // its exec closes the descriptor. dup reproduces that deterministically.
        let inherited = lock.0.try_clone()?;
        let blocked = workspace_lock(p, &r.name)
            .err()
            .context("lock was bypassed")?;
        assert_eq!(crate::api::error_json(&blocked)["code"], "workspace_busy");
        drop(lock);
        let next = workspace_lock(p, &r.name).context("released lock remained busy")?;
        drop(inherited);
        let blocked = workspace_lock(p, &r.name)
            .err()
            .context("new lock was bypassed")?;
        assert_eq!(crate::api::error_json(&blocked)["code"], "workspace_busy");
        drop(next);
        Ok(())
    }

    #[test]
    fn pending_archive_and_restore_replay_recheck_nested_gitfiles() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let mut r = reserve(p, "pending", None, None, Some("agent"), Value::Null)?;
        let nested = r.path.join("dir");
        fs::create_dir(&nested)?;
        fs::write(nested.join(".git"), "gitdir: missing-external-admin\n")?;
        r.heartbeat = 0;
        {
            let archive = archive_path(p, &r, true)?;
            r.archive = Some(archive.clone());
            r.transition = Some(Transition::Retire {
                status: Status::Dropped,
                from: None,
            });
            persist(p, &r)?;
            assert_eq!(prune(p, true)?[0].reason, Some("nested_git_metadata"));
            assert!(!archive.exists());
            assert!(nested.join(".git").exists());
            // Simulate a previous version that moved the shared directory
            // before completing its journal.
            rename_exclusive(&r.path, &archive)?;
            assert_eq!(prune(p, true)?[0].reason, Some("nested_git_metadata"));
            assert!(archive.join("dir/.git").exists());

            // Simulate the same previous version having committed the archive.
            r.status = Status::Dropped;
            r.transition = None;
            persist(p, &r)?;
            assert!(is_nested_git_error(
                &restore(p, &r.name, &r.lease).unwrap_err()
            ));
            assert!(archive.join("dir/.git").exists());
            assert!(load_retired(p, &r.name, &r.lease)?.transition.is_none());
            r.transition = Some(Transition::Restore {
                lease: Uuid::new_v4().to_string(),
                port: r.port,
            });
            persist(p, &r)?;
            assert!(is_nested_git_error(
                &restore(p, &r.name, &r.lease).unwrap_err()
            ));
            rename_exclusive(&archive, &r.path)?;
            assert!(is_nested_git_error(
                &restore(p, &r.name, &r.lease).unwrap_err()
            ));
            assert!(r.path.join("dir/.git").exists());
            assert!(load_retired(p, &r.name, &r.lease)?.transition.is_some());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn relocation_never_follows_payload_symlinks_or_opens_special_files() -> Result<()> {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt, fs::symlink},
        };
        let f = Fixture::new()?;
        let p = &f.project;
        let r = f.workspace("agent")?;
        let external = p.root.with_file_name("external");
        fs::create_dir(&external)?;
        fs::write(external.join(".git"), "gitdir: never read")?;
        symlink(&external, r.path.join("symlink"))?;
        let fifo = r.path.join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes())?;
        ensure!(
            unsafe { libc::mkfifo(name.as_ptr(), 0o600) } == 0,
            "create FIFO fixture"
        );
        let to = p.root.with_file_name("new workspace location");
        assert!(move_workspace(p, &r.name, &r.lease, &to).is_err());
        assert!(!to.exists());
        fs::remove_file(fifo)?;
        assert_eq!(move_workspace(p, &r.name, &r.lease, &to)?.path, to);
        assert_eq!(fs::read_link(to.join("symlink"))?, external);
        Ok(())
    }

    #[test]
    fn new_request_ids_require_uuid_v4() -> Result<()> {
        let f = Fixture::new()?;
        for request in [
            "00000000-0000-0000-0000-000000000000",
            "01900000-0000-7000-8000-000000000000",
            "01900000-0000-4000-0000-000000000000",
        ] {
            let error = reserve(
                &f.project,
                "agent",
                Some(request),
                None,
                Some("agent"),
                Value::Null,
            )
            .unwrap_err();
            assert_eq!(crate::api::error_json(&error)["code"], "invalid_request_id");
            assert!(!f.project.root.exists());
        }
        Ok(())
    }

    #[test]
    fn existing_registry_workspace_and_archive_namespaces_are_never_adopted() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        p.root(true)?;
        let foreign = p.root.join("foreign");
        fs::create_dir(&foreign)?;
        fs::write(foreign.join("data"), "preserve")?;
        assert!(reserve(p, "different", None, None, Some("agent"), Value::Null).is_err());
        assert_eq!(fs::read_to_string(foreign.join("data"))?, "preserve");
        assert!(!p.state.join(NAMESPACE).exists());
        rename_exclusive(&foreign, &p.root.with_extension("foreign"))?;

        let legacy = p.state.join("independent");
        fs::create_dir(&legacy)?;
        fs::write(legacy.join("data"), "preserve legacy")?;
        assert!(reserve(p, "different", None, None, Some("agent"), Value::Null).is_err());
        assert_eq!(fs::read_to_string(legacy.join("data"))?, "preserve legacy");
        rename_exclusive(&legacy, &p.state.join("fixture-legacy"))?;

        let r = f.workspace("agent")?;
        let archive = archive_path(p, &r, true)?;
        let archives = archive.parent().context("missing archive directory")?;
        let original = archives.with_file_name("original-archive-root");
        rename_exclusive(archives, &original)?;
        dir(archives)?;
        fs::copy(original.join(CACHE_OWNER), archives.join(CACHE_OWNER))?;
        fs::write(archives.join("foreign"), "preserve archive contents")?;
        assert!(preflight_retire(p, &r).is_err());
        assert!(r.path.join(".git/HEAD").is_file());
        assert_eq!(
            fs::read_to_string(archives.join("foreign"))?,
            "preserve archive contents"
        );
        Ok(())
    }

    #[test]
    fn verifies_reftable_head_through_git() -> Result<()> {
        let f = Fixture::new()?;
        let p = &f.project;
        let mut r = reserve(p, "reftable", None, None, Some("agent"), Value::Null)?;
        let init = git::raw(
            &r.path,
            &[
                "init",
                "-q",
                "--template=",
                "--ref-format=reftable",
                "-b",
                "agent",
            ],
        )?;
        if init.status.code() == Some(129)
            && init.stderr.windows(10).any(|word| word == b"ref-format")
        {
            // Git versions predating the reftable option still support the files backend.
            return Ok(());
        }
        ensure!(
            init.status.success(),
            "Git could not initialize the reftable fixture"
        );
        finish(p, &mut r)?;
        verify(p, &r)?;
        let tables = r.path.join(".git/reftable/tables.list");
        rename_exclusive(&tables, &tables.with_extension("saved"))?;
        assert!(verify(p, &r).is_err());
        Ok(())
    }
}
