//! Turns on a named shared resource: something a project has once and cannot
//! copy per graft, such as a development server on a fixed port. Each
//! resource has one holder and a first-come queue. A holder is whoever does
//! the work that needs the resource, such as an agent, under a name it
//! chooses and a lease that proves the name is its own. One holder can work
//! in grafts of several repositories and still hold one turn.
//!
//! A holder loses its turn and its place when its turn runs out, when the
//! process it named ends, or when none of the grafts it named is active any
//! more. Turn state is kept in a project's private state: the primary's, or,
//! for a resource the configuration of a directory above lists under
//! `turns`, that of the repository at that directory, which the
//! repositories below it share. Graft does nothing to the resource itself:
//! like a lease, a turn is a promise between cooperating clients. Callers
//! hold the lock of the project that keeps the state while reading or
//! changing it.
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    hook::{self, Event, Policy},
    links, registry,
};
use crate::{
    api, cache,
    config::{self, Config},
    registry::{Paths, Project, atomic, dir, exists, now, owned_directory},
};

const DIRECTORY: &str = "turns";
/// A resource may be named like a Windows device, such as `com3`, which
/// cannot be the stem of a file name there. The prefix keeps every stem legal.
const PREFIX: &str = "turn-";
const SUFFIX: &str = ".json";
const MAX_BYTES: usize = 1024 * 1024;
/// A queued holder keeps its place while it asks again within this long.
const QUEUE_SECS: u64 = 300;
const MAX_HOLDER_BYTES: usize = 64;
/// The most grafts one holder may name.
const MAX_GRAFTS: usize = 64;

pub(crate) fn validate_resource(resource: &str) -> Result<()> {
    if resource.is_empty()
        || resource.len() > 64
        || resource.starts_with('-')
        || !resource
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        return Err(api::fault(
            "invalid_resource",
            "Use a resource name of 1 to 64 lowercase letters, numbers or hyphens, starting with a letter or number",
            Value::Null,
        ));
    }
    Ok(())
}

fn invalid_holder(message: impl Into<String>) -> anyhow::Error {
    api::fault("invalid_holder", message.into(), Value::Null)
}

/// A holder is named by 1 to 64 letters, digits, `-`, `_` or `.`, and proves
/// the name with a random UUIDv4 of its own choosing.
fn validate_holder(holder: &str, lease: &str) -> Result<()> {
    if holder.is_empty()
        || holder.len() > MAX_HOLDER_BYTES
        || holder.starts_with('-')
        || !holder
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Err(invalid_holder(format!(
            "Use a holder name of 1 to {MAX_HOLDER_BYTES} letters, digits, '-', '_' or '.', not starting with '-'"
        )));
    }
    let random = Uuid::parse_str(lease).is_ok_and(|id| {
        id.get_version() == Some(uuid::Version::Random)
            && id.get_variant() == uuid::Variant::RFC4122
    });
    if !random {
        return Err(invalid_holder(
            "Use a randomly generated UUIDv4 as the holder's lease, and keep it for every later take and release",
        ));
    }
    Ok(())
}

/// What is stored of a lease: its digest, so the state file never holds it.
fn digest(lease: &str) -> String {
    Sha256::digest(lease.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A process whose life bounds a holder, by ID and start time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Process {
    pid: u32,
    start: String,
}

/// A graft a holder works in: the primary checkout of its repository, the
/// graft's ID there, and its name and path as they were when named.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Graft {
    project: PathBuf,
    id: String,
    name: String,
    path: PathBuf,
}

/// Who holds or waits for a turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Party {
    name: String,
    /// The digest of the holder's lease.
    lease: String,
    process: Option<Process>,
    grafts: Vec<Graft>,
}

impl Party {
    /// Take what a later ask says about the holder's process and grafts.
    fn update(&mut self, asked: &Party) {
        if asked.process.is_some() {
            self.process.clone_from(&asked.process);
        }
        if !asked.grafts.is_empty() {
            self.grafts.clone_from(&asked.grafts);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Holder {
    who: Party,
    /// When the turn was granted.
    since: u64,
    /// When the holder last took the turn again.
    renewed: u64,
    ttl: u64,
}

impl Holder {
    fn expires_at(&self) -> u64 {
        self.renewed.saturating_add(self.ttl)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Waiter {
    who: Party,
    joined: u64,
    /// When the holder last asked for the turn.
    asked: u64,
}

/// Who holds the turn on one resource and who waits for it, first come first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Turn {
    holder: Option<Holder>,
    queue: Vec<Waiter>,
}

fn lease_mismatch(holder: &str) -> anyhow::Error {
    api::fault(
        "lease_mismatch",
        format!(
            "Another holder holds or waits for this turn under the name {holder}. Use the lease that name was first given, or another name"
        ),
        json!({"holder": holder}),
    )
}

// The rules. They read no clock, file or process: `now`, and whether a
// party has gone, come from the caller.
impl Turn {
    /// Forget a holder that has gone or whose turn ran out, and every queued
    /// party that has gone or stopped asking.
    fn settle(&mut self, now: u64, gone: &dyn Fn(&Party) -> bool) {
        if self
            .holder
            .as_ref()
            .is_some_and(|holder| now >= holder.expires_at() || gone(&holder.who))
        {
            self.holder = None;
        }
        self.queue
            .retain(|waiter| now <= waiter.asked.saturating_add(QUEUE_SECS) && !gone(&waiter.who));
    }

    /// Ask for the turn, and report whether `who` now holds it. A holder
    /// renews its turn. Anyone else joins the queue, or keeps its place in
    /// it, and gets the turn once nobody holds it and nobody is ahead. A
    /// name held or queued under another lease is refused.
    fn take(
        &mut self,
        now: u64,
        who: &Party,
        ttl: Option<u64>,
        gone: &dyn Fn(&Party) -> bool,
    ) -> Result<bool> {
        self.settle(now, gone);
        if let Some(holder) = self.holder.as_mut()
            && holder.who.name == who.name
        {
            ensure!(holder.who.lease == who.lease, lease_mismatch(&who.name));
            holder.who.update(who);
            holder.renewed = now;
            holder.ttl = ttl.unwrap_or(holder.ttl);
            return Ok(true);
        }
        match self
            .queue
            .iter_mut()
            .find(|waiter| waiter.who.name == who.name)
        {
            Some(waiter) => {
                ensure!(waiter.who.lease == who.lease, lease_mismatch(&who.name));
                waiter.who.update(who);
                waiter.asked = now;
            }
            None => self.queue.push(Waiter {
                who: who.clone(),
                joined: now,
                asked: now,
            }),
        }
        if self.holder.is_some() || self.queue[0].who.name != who.name {
            return Ok(false);
        }
        let first = self.queue.remove(0);
        self.holder = Some(Holder {
            who: first.who,
            since: now,
            renewed: now,
            ttl: ttl.unwrap_or(api::TURN_TTL_DEFAULT),
        });
        Ok(true)
    }

    /// Give up the turn or leave the queue, and report whether the holder
    /// named `name` did either. The next in line gets the turn when it next
    /// asks. A name held or queued under another lease is refused.
    fn release(
        &mut self,
        now: u64,
        name: &str,
        lease: &str,
        gone: &dyn Fn(&Party) -> bool,
    ) -> Result<bool> {
        let mine = |who: &Party| -> Result<bool> {
            if who.name != name {
                return Ok(false);
            }
            ensure!(who.lease == lease, lease_mismatch(name));
            Ok(true)
        };
        let held = match &self.holder {
            Some(holder) => mine(&holder.who)?,
            None => false,
        };
        let mut queued = false;
        for waiter in &self.queue {
            queued |= mine(&waiter.who)?;
        }
        if held {
            self.holder = None;
        }
        self.queue.retain(|waiter| waiter.who.name != name);
        // Settled last: a holder whose turn has just run out still gave it up.
        self.settle(now, gone);
        Ok(held || queued)
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.queue
            .iter()
            .position(|waiter| waiter.who.name == name)
            .map(|index| index + 1)
    }

    fn idle(&self) -> bool {
        self.holder.is_none() && self.queue.is_empty()
    }

    /// What everyone may see: names, times and the holder's graft paths,
    /// never a lease or a graft ID.
    fn describe(&self, resource: &str) -> Value {
        let holder = self.holder.as_ref();
        json!({
            "resource": resource,
            "holder": holder.map(|holder| &holder.who.name),
            "since": holder.map(|holder| holder.since),
            "expires_at": holder.map(Holder::expires_at),
            "grafts": holder.map_or_else(Vec::new, |holder| {
                holder.who.grafts.iter().map(|graft| &graft.path).collect::<Vec<_>>()
            }),
            "queue": self.queue.iter().map(|waiter| &waiter.who.name).collect::<Vec<_>>(),
        })
    }
}

/// Tells whether a party has gone. Each repository's active grafts are read
/// once, without its lock, as another repository's lock may be held by a
/// command that is itself waiting for this one.
struct Liveness<'a> {
    here: &'a Project,
    live: RefCell<HashMap<PathBuf, Option<HashSet<String>>>>,
}

impl<'a> Liveness<'a> {
    fn new(here: &'a Project) -> Self {
        Self {
            here,
            live: RefCell::new(HashMap::new()),
        }
    }

    /// The active graft IDs of the repository whose primary is `project`,
    /// or `None` when they cannot be read.
    fn live_in(&self, project: &Path) -> Option<HashSet<String>> {
        if project == self.here.repo {
            return registry::live_ids(self.here).ok();
        }
        let paths = Project::paths(project).ok()?;
        if paths.0 != project {
            return None;
        }
        match Project::read_existing_at(paths).ok()? {
            Some(other) => registry::live_ids(&other).ok(),
            None => Some(HashSet::new()),
        }
    }

    /// Whether the graft `id` of the repository at `project` is active. A
    /// repository whose grafts cannot be read counts its grafts as active:
    /// they may still be using the resource.
    fn active(&self, project: &Path, id: &str) -> bool {
        let mut live = self.live.borrow_mut();
        let ids = live
            .entry(project.to_owned())
            .or_insert_with(|| self.live_in(project));
        ids.as_ref().is_none_or(|ids| ids.contains(id))
    }

    fn gone(&self, who: &Party) -> bool {
        if let Some(process) = &who.process
            && !crate::process::alive(process.pid, &process.start)
        {
            return true;
        }
        !who.grafts.is_empty()
            && !who
                .grafts
                .iter()
                .any(|graft| self.active(&graft.project, &graft.id))
    }
}

/// One resource's turn as stored, with the project and resource it is for.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u8,
    project: String,
    resource: String,
    holder: Option<Holder>,
    queue: Vec<Waiter>,
}

/// The version of the stored turn. Version 1 held grafts as holders.
const VERSION: u8 = 2;

fn file(p: &Project, resource: &str) -> PathBuf {
    p.state
        .join(DIRECTORY)
        .join(format!("{PREFIX}{resource}{SUFFIX}"))
}

/// Read the turn on `resource`: nobody holds or waits for one never taken.
/// A file that cannot be used is an error and stays as it is.
fn read(p: &Project, resource: &str) -> Result<Turn> {
    let directory = p.state.join(DIRECTORY);
    if !exists(&directory)? {
        return Ok(Turn::default());
    }
    owned_directory(&directory)?;
    let path = file(p, resource);
    if !exists(&path)? {
        return Ok(Turn::default());
    }
    (|| {
        let stored: Stored =
            serde_json::from_slice(&cache::read_private_file(&path, MAX_BYTES as u64)?)?;
        ensure!(
            stored.version == VERSION && stored.project == p.id() && stored.resource == resource,
            "it belongs to another project, resource or version"
        );
        Ok(Turn {
            holder: stored.holder,
            queue: stored.queue,
        })
    })()
    .with_context(|| {
        format!(
            "The turn state of resource {resource} at {} cannot be used. Graft left it unchanged; repair it or move it aside to start the resource's queue again",
            path.display()
        )
    })
}

/// Replace the stored turn on `resource`. Callers read it first, under the
/// same project lock, so the path holds a regular file or nothing.
fn write(p: &Project, resource: &str, turn: &Turn) -> Result<()> {
    let directory = p.state.join(DIRECTORY);
    if !exists(&directory)? {
        dir(&directory)?;
        registry::sync_directory(&p.state)?;
    }
    let bytes = serde_json::to_vec(&Stored {
        version: VERSION,
        project: p.id().into(),
        resource: resource.into(),
        holder: turn.holder.clone(),
        queue: turn.queue.clone(),
    })?;
    ensure!(
        bytes.len() <= MAX_BYTES,
        "too many holders wait for resource {resource}"
    );
    atomic(&file(p, resource), &bytes)
}

/// The project that keeps the turns the configuration shares, and their names.
type Shared = Option<(Paths, Vec<String>)>;

/// Where the turns of the primary at `primary` are kept: the paths of the
/// primary's own project, and, when a directory above it shares resources,
/// the paths of the project that keeps them with the resources it shares.
fn places(primary: &Path, own: Paths) -> Result<(Paths, Shared)> {
    let shared = match config::shared_turns(primary)? {
        Some((dir, turns)) => {
            let paths = Project::paths(&dir).ok().filter(|paths| paths.0 == dir);
            let Some(paths) = paths else {
                return Err(api::fault(
                    "invalid_config",
                    format!(
                        "{} lists turns to share, but the directory is not the root of a Git checkout, which would keep their state",
                        dir.display()
                    ),
                    json!({"file": dir}),
                ));
            };
            Some((paths, turns))
        }
        None => None,
    };
    Ok((own, shared))
}

/// The paths of the project that keeps the turn on `resource`.
fn place_of(primary: &Path, own: Paths, resource: &str) -> Result<Paths> {
    Ok(match places(primary, own)? {
        (_, Some((paths, shared))) if shared.iter().any(|known| known == resource) => paths,
        (own, _) => own,
    })
}

/// The grafts at `paths`, each of which must be an active graft.
fn grafts_at(paths: &[PathBuf], liveness: &Liveness<'_>) -> Result<Vec<Graft>> {
    if paths.len() > MAX_GRAFTS {
        return Err(invalid_holder(format!(
            "A holder may name at most {MAX_GRAFTS} grafts"
        )));
    }
    let mut grafts: Vec<Graft> = Vec::new();
    for path in paths {
        let found = dunce::canonicalize(path)
            .ok()
            .and_then(|canonical| Some((registry::discover(&canonical).ok()??, canonical)));
        let Some((location, canonical)) = found else {
            return Err(invalid_holder(format!(
                "{} is not a graft. Name the directory of an active graft",
                path.display()
            )));
        };
        if !liveness.active(&location.project, &location.id) {
            return Err(invalid_holder(format!(
                "Graft {} at {} is not active",
                location.name,
                path.display()
            )));
        }
        if !grafts
            .iter()
            .any(|known| known.project == location.project && known.id == location.id)
        {
            grafts.push(Graft {
                project: location.project,
                id: location.id,
                name: location.name,
                path: canonical,
            });
        }
    }
    Ok(grafts)
}

/// `turn take`, asked once, for the primary at `primary` whose own project
/// is at `own`. A granted take points the links the configuration of the
/// project that keeps the turn lists for the resource at the holder's
/// grafts, once trusted, and puts them back if the take or its hooks fail.
/// When it grants a turn the holder did not hold, or changes the
/// grafts of one it holds, that project's `post-take` hooks run, with no
/// lock held; if one fails, the turn is given up again and the take refused.
/// `hooks` says whether the caller may run them. Only a take that would run
/// them requires the hooks to be trusted.
pub(super) fn take(
    primary: &Path,
    own: Paths,
    args: &api::TurnTake,
    hooks: Policy,
) -> Result<Value> {
    validate_resource(&args.resource)?;
    args.check()?;
    validate_holder(&args.holder, &args.lease)?;
    let process = match args.pid {
        None => None,
        Some(pid) => Some(Process {
            pid,
            start: crate::process::start_time(pid)
                .ok_or_else(|| invalid_holder(format!("No running process has the ID {pid}")))?,
        }),
    };
    let place = place_of(primary, own, &args.resource)?;
    let (p, lock) = Project::open_at(place.clone(), false)?;
    // The project that keeps the turn defines what runs when it is taken.
    let config = Config::read(&p.repo)?;
    let post_take = config.hooks_for(Event::PostTake);
    // Its links follow the turn: those of this resource.
    let linked: Vec<_> = config
        .links
        .iter()
        .filter(|link| link.resource == args.resource)
        .collect();
    let liveness = Liveness::new(&p);
    let who = Party {
        name: args.holder.clone(),
        lease: digest(&args.lease),
        process,
        grafts: grafts_at(&args.grafts, &liveness)?,
    };
    let stored = read(&p, &args.resource)?;
    let mut turn = stored.clone();
    let now = now();
    turn.settle(now, &|party| liveness.gone(party));
    let before = turn
        .holder
        .as_ref()
        .filter(|holder| holder.who.name == who.name)
        .map(|holder| holder.who.grafts.clone());
    let granted = turn.take(now, &who, args.ttl, &|party| liveness.gone(party))?;
    let changed = granted
        && before.is_none_or(|before| {
            turn.holder
                .as_ref()
                .is_some_and(|holder| holder.who.grafts != before)
        });
    // Only a take that would run a hook needs it trusted: a renewal or a
    // queued ask runs none, so an edited hook does not cost its holder the
    // turn or a waiter its place. A refused take changes nothing.
    if changed {
        hook::trust::require(&p, &config, Event::PostTake, hooks)?;
    }
    // A granted take, a renewal too, points the links at the holder's
    // grafts before the turn is recorded and its hooks run, so the hooks
    // find them in place. A queued ask leaves them alone. Untrusted links
    // stay as they are, and the take goes on without them.
    let mut undo = links::Undo::default();
    let mut pointed = None;
    if granted
        && !linked.is_empty()
        && let Some(holder) = &turn.holder
    {
        if hook::trust::links_untrusted(&p, &config, &args.resource)? {
            pointed = Some(links::skipped(&linked));
        } else {
            let grafts = holder.who.grafts.iter();
            let (entries, done) = links::point_all(
                &linked,
                grafts.map(|graft| (graft.project.as_path(), graft.path.as_path())),
            )?;
            pointed = Some(entries);
            undo = done;
        }
    }
    if turn != stored
        && let Err(error) = write(&p, &args.resource, &turn)
    {
        undo.undo();
        return Err(error);
    }
    let mut reply = turn.describe(&args.resource);
    reply["granted"] = json!(granted);
    if !granted {
        reply["position"] = json!(turn.position(&args.holder));
    }
    if let Some(entries) = pointed {
        reply["links"] = json!(entries);
    }
    let grafts: Vec<PathBuf> = turn
        .holder
        .as_ref()
        .map(|holder| {
            holder
                .who
                .grafts
                .iter()
                .map(|graft| graft.path.clone())
                .collect()
        })
        .unwrap_or_default();
    if !changed || post_take.is_empty() {
        return Ok(reply);
    }
    drop(liveness);
    drop(lock);
    let taken = hook::Taken {
        primary: &p.repo,
        resource: &args.resource,
        holder: &args.holder,
        grafts: &grafts,
    };
    let entries = hook::after_take(hooks, &post_take, &taken);
    if entries.iter().any(|entry| entry["status"] == "failed") {
        let (p, _lock) = Project::open_at(place, false)?;
        // The links lead where they did before the take.
        undo.undo();
        let liveness = Liveness::new(&p);
        let mut turn = read(&p, &args.resource)?;
        if turn.release(crate::registry::now(), &args.holder, &who.lease, &|party| {
            liveness.gone(party)
        })? {
            write(&p, &args.resource, &turn)?;
        }
        return Err(hook::take_failed(&args.resource, &args.holder, entries));
    }
    reply["hooks"] = json!(entries);
    Ok(reply)
}

/// `turn release`, for the primary at `primary` whose own project is at `own`.
pub(super) fn release(primary: &Path, own: Paths, args: &api::TurnRelease) -> Result<Value> {
    validate_resource(&args.resource)?;
    validate_holder(&args.holder, &args.lease)?;
    let (p, _lock) = Project::open_at(place_of(primary, own, &args.resource)?, false)?;
    let liveness = Liveness::new(&p);
    let stored = read(&p, &args.resource)?;
    let mut turn = stored.clone();
    let released = turn.release(now(), &args.holder, &digest(&args.lease), &|party| {
        liveness.gone(party)
    })?;
    if turn != stored {
        write(&p, &args.resource, &turn)?;
    }
    Ok(json!({
        "resource": args.resource,
        "released": released,
        "next": turn.queue.first().map(|waiter| &waiter.who.name),
    }))
}

/// The resources with stored turns in project `p`.
fn resources(p: &Project) -> Result<Vec<String>> {
    let directory = p.state.join(DIRECTORY);
    let mut resources = Vec::new();
    if exists(&directory)? {
        owned_directory(&directory)?;
        for entry in fs::read_dir(&directory)? {
            // Anything else here, such as a temporary file an interrupted
            // write left behind, is not turn state.
            if let Some(resource) = entry?
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX))
                && validate_resource(resource).is_ok()
            {
                resources.push(resource.to_owned());
            }
        }
    }
    resources.sort();
    Ok(resources)
}

/// The turns of project `p` that someone holds or waits for, as the next
/// `take` would find them, among the resources `keep` accepts.
fn listed(p: &Project, keep: &dyn Fn(&str) -> bool) -> Result<Vec<Value>> {
    let liveness = Liveness::new(p);
    let now = now();
    let mut turns = Vec::new();
    for resource in resources(p)?.into_iter().filter(|r| keep(r)) {
        let mut turn = read(p, &resource)?;
        turn.settle(now, &|party| liveness.gone(party));
        if !turn.idle() {
            turns.push(turn.describe(&resource));
        }
    }
    Ok(turns)
}

/// `turn list`: every resource someone holds or waits for, as the next
/// `take` would find it, from the primary's own project and from the project
/// that keeps the resources the configuration shares. Reads only, and
/// creates no state; `take` and `release` store what they forget.
pub(super) fn list(primary: &Path, own: Paths) -> Result<Value> {
    let (own, shared) = places(primary, own)?;
    let names: Vec<String> = shared
        .as_ref()
        .map(|(_, names)| names.clone())
        .unwrap_or_default();
    let mut turns = Vec::new();
    if let Some((p, _lock)) = Project::open_existing_at(own)? {
        turns.extend(listed(&p, &|resource| {
            !names.iter().any(|name| name == resource)
        })?);
    }
    if let Some((paths, _)) = shared
        && let Some((p, _lock)) = Project::open_existing_at(paths)?
    {
        turns.extend(listed(&p, &|resource| {
            names.iter().any(|name| name == resource)
        })?);
    }
    turns.sort_by(|a, b| a["resource"].as_str().cmp(&b["resource"].as_str()));
    Ok(json!({ "turns": turns }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn party(name: &str) -> Party {
        Party {
            name: name.into(),
            lease: digest(&format!("lease of {name}")),
            process: None,
            grafts: Vec::new(),
        }
    }

    fn a() -> Party {
        party("task-a")
    }
    fn b() -> Party {
        party("task-b")
    }
    fn c() -> Party {
        party("task-c")
    }

    fn stay(_: &Party) -> bool {
        false
    }

    fn queue(turn: &Turn) -> Vec<&str> {
        turn.queue
            .iter()
            .map(|waiter| waiter.who.name.as_str())
            .collect()
    }

    fn holder(turn: &Turn) -> Option<&str> {
        turn.holder.as_ref().map(|holder| holder.who.name.as_str())
    }

    fn take(turn: &mut Turn, now: u64, who: &Party, ttl: Option<u64>) -> bool {
        turn.take(now, who, ttl, &stay).unwrap()
    }

    fn release(turn: &mut Turn, now: u64, who: &Party) -> bool {
        turn.release(now, &who.name, &who.lease, &stay).unwrap()
    }

    #[test]
    fn the_turn_passes_in_the_order_holders_first_asked() {
        let mut turn = Turn::default();
        assert!(take(&mut turn, 100, &a(), None));
        assert!(!take(&mut turn, 101, &b(), None));
        assert!(!take(&mut turn, 102, &c(), None));
        assert_eq!(
            (holder(&turn), queue(&turn)),
            (Some("task-a"), vec!["task-b", "task-c"])
        );
        assert_eq!(
            (turn.position("task-b"), turn.position("task-c")),
            (Some(1), Some(2))
        );
        assert_eq!(turn.position("task-a"), None);

        assert!(release(&mut turn, 103, &a()));
        // The turn is free, but task-b asked first.
        assert!(!take(&mut turn, 104, &c(), None));
        assert_eq!((holder(&turn), turn.position("task-c")), (None, Some(2)));
        assert!(take(&mut turn, 105, &b(), None));
        assert_eq!(
            (holder(&turn), queue(&turn)),
            (Some("task-b"), vec!["task-c"])
        );
        assert!(release(&mut turn, 106, &b()));
        assert!(take(&mut turn, 107, &c(), None));
        assert!(release(&mut turn, 108, &c()));
        assert!(turn.idle());
    }

    #[test]
    fn asking_again_keeps_a_place_in_the_queue() {
        let mut turn = Turn::default();
        assert!(take(&mut turn, 100, &a(), None));
        assert!(!take(&mut turn, 101, &b(), None));
        assert!(!take(&mut turn, 102, &c(), None));
        assert!(!take(&mut turn, 350, &c(), None));
        assert!(!take(&mut turn, 400, &b(), None));
        assert!(!take(&mut turn, 600, &c(), None));
        assert_eq!(queue(&turn), ["task-b", "task-c"]);
        let first = &turn.queue[0];
        assert_eq!((first.joined, first.asked), (101, 400));
    }

    #[test]
    fn a_turn_that_is_not_renewed_runs_out_after_its_ttl() {
        let mut turn = Turn::default();
        assert!(take(&mut turn, 100, &a(), Some(10)));
        assert!(!take(&mut turn, 109, &b(), None));
        assert_eq!(holder(&turn), Some("task-a"));
        assert!(take(&mut turn, 110, &b(), None));
        let held = turn.holder.clone().unwrap();
        assert_eq!((held.since, held.ttl), (110, api::TURN_TTL_DEFAULT));
        // The earlier holder lost the turn and queues like anyone else, and
        // behind those already waiting.
        assert!(!take(&mut turn, 111, &a(), None));
        assert_eq!(turn.position("task-a"), Some(1));
        let mut alone = Turn::default();
        assert!(take(&mut alone, 100, &a(), Some(10)));
        assert!(take(&mut alone, 120, &a(), None));
        assert_eq!(alone.holder.unwrap().since, 120);
    }

    #[test]
    fn a_waiter_that_stopped_asking_loses_its_place() {
        let mut turn = Turn::default();
        assert!(take(&mut turn, 100, &a(), None));
        assert!(!take(&mut turn, 101, &b(), None));
        assert!(!take(&mut turn, 200, &c(), None));
        assert!(release(&mut turn, 300, &a()));
        // task-b is first and last asked at 101, so it has until 401.
        assert!(!take(&mut turn, 401, &c(), None));
        assert_eq!(queue(&turn), ["task-b", "task-c"]);
        assert!(take(&mut turn, 402, &c(), None));
        assert!(turn.queue.is_empty());
        assert!(!take(&mut turn, 403, &b(), None));
        assert_eq!(turn.queue[0].joined, 403);
    }

    #[test]
    fn holders_that_have_gone_are_forgotten() {
        let mut turn = Turn::default();
        assert!(take(&mut turn, 100, &a(), None));
        assert!(!take(&mut turn, 101, &b(), None));
        assert!(!take(&mut turn, 102, &c(), None));
        // task-a's process ended and task-b's grafts were archived.
        let gone = |who: &Party| who.name != "task-c";
        assert!(turn.take(103, &c(), None, &gone).unwrap());
        assert_eq!((holder(&turn), queue(&turn)), (Some("task-c"), vec![]));
        // Once forgotten, its name is free for another lease.
        let mut again = a();
        again.lease = digest("another lease");
        assert!(!turn.take(104, &again, None, &stay).unwrap());
        assert_eq!(turn.queue[0].who.lease, again.lease);
    }

    #[test]
    fn a_name_held_or_queued_under_another_lease_is_refused() {
        let mut turn = Turn::default();
        assert!(take(&mut turn, 100, &a(), None));
        assert!(!take(&mut turn, 101, &b(), None));
        let before = turn.clone();
        for name in ["task-a", "task-b"] {
            let mut impostor = party(name);
            impostor.lease = digest("someone else's lease");
            let error = turn.take(102, &impostor, None, &stay).unwrap_err();
            assert_eq!(api::error_json(&error)["code"], "lease_mismatch");
            assert_eq!(api::error_json(&error)["details"], json!({"holder": name}));
            let error = turn.release(102, name, &impostor.lease, &stay).unwrap_err();
            assert_eq!(api::error_json(&error)["code"], "lease_mismatch");
        }
        assert_eq!(turn, before);
    }

    #[test]
    fn taking_again_renews_the_turn_and_updates_what_bounds_the_holder() {
        let mut turn = Turn::default();
        assert!(take(&mut turn, 100, &a(), None));
        let expires = |turn: &Turn| turn.holder.as_ref().unwrap().expires_at();
        assert_eq!(expires(&turn), 100 + api::TURN_TTL_DEFAULT);
        assert!(take(&mut turn, 600, &a(), Some(60)));
        assert_eq!(expires(&turn), 660);
        // A renewal without a ttl keeps the one the turn has.
        assert!(take(&mut turn, 650, &a(), None));
        assert_eq!(expires(&turn), 710);
        assert_eq!(turn.holder.as_ref().unwrap().since, 100);

        let mut bounded = a();
        bounded.process = Some(Process {
            pid: 7,
            start: "1".into(),
        });
        bounded.grafts = vec![Graft {
            project: "/app".into(),
            id: "id".into(),
            name: "fix".into(),
            path: "/app.graft/fix".into(),
        }];
        assert!(take(&mut turn, 700, &bounded, None));
        // An ask that names neither keeps what the holder named before.
        assert!(take(&mut turn, 710, &a(), None));
        let who = &turn.holder.as_ref().unwrap().who;
        assert_eq!(
            (&who.process, &who.grafts),
            (&bounded.process, &bounded.grafts)
        );
    }

    #[test]
    fn release_clears_a_holder_or_a_waiter_and_nothing_else() {
        let mut turn = Turn::default();
        assert!(!release(&mut turn, 99, &a()));
        assert!(take(&mut turn, 100, &a(), None));
        assert!(!take(&mut turn, 101, &b(), None));
        assert!(!take(&mut turn, 102, &c(), None));
        assert!(release(&mut turn, 103, &b()));
        assert_eq!(
            (holder(&turn), queue(&turn)),
            (Some("task-a"), vec!["task-c"])
        );
        assert!(!release(&mut turn, 104, &b()));
        assert!(release(&mut turn, 105, &a()));
        assert_eq!((holder(&turn), queue(&turn)), (None, vec!["task-c"]));

        // A turn that has already run out still counts as given up.
        let mut late = Turn::default();
        assert!(take(&mut late, 100, &a(), Some(1)));
        assert!(release(&mut late, 200, &a()));
        assert!(late.idle());
    }

    #[test]
    fn describes_names_times_and_graft_paths_only() {
        let mut turn = Turn::default();
        assert_eq!(
            turn.describe("web"),
            json!({"resource":"web","holder":null,"since":null,"expires_at":null,"grafts":[],"queue":[]})
        );
        let mut holder = a();
        holder.grafts = vec![Graft {
            project: "/repo".into(),
            id: "secret-id".into(),
            name: "fix".into(),
            path: "/repo.graft/fix".into(),
        }];
        assert!(take(&mut turn, 100, &holder, Some(30)));
        assert!(!take(&mut turn, 101, &b(), None));
        let described = turn.describe("web");
        assert_eq!(
            described,
            json!({"resource":"web","holder":"task-a","since":100,"expires_at":130,
                   "grafts":["/repo.graft/fix"],"queue":["task-b"]})
        );
        assert!(!described.to_string().contains(&holder.lease));
    }

    #[test]
    fn holder_names_and_leases_are_checked() {
        let lease = Uuid::new_v4().to_string();
        for valid in ["agent", "869abc-fix", "session.2", "a_b", &"a".repeat(64)] {
            validate_holder(valid, &lease).unwrap();
        }
        for invalid in ["", "-agent", "two words", "a/b", "wé", &"a".repeat(65)] {
            let error = validate_holder(invalid, &lease).unwrap_err();
            assert_eq!(
                api::error_json(&error)["code"],
                "invalid_holder",
                "{invalid}"
            );
        }
        for lease in ["", "lease", "00000000-0000-0000-0000-000000000000"] {
            let error = validate_holder("agent", lease).unwrap_err();
            assert_eq!(api::error_json(&error)["code"], "invalid_holder", "{lease}");
        }
        // The stored digest never equals the lease and is stable.
        assert_eq!(digest(&lease), digest(&lease));
        assert_ne!(digest(&lease), lease);
    }

    #[test]
    fn resource_names_are_portable() {
        for valid in [
            "web",
            "ios-simulator-2",
            "3d-printer",
            "com3",
            &"a".repeat(64),
        ] {
            validate_resource(valid).unwrap();
        }
        for invalid in [
            "",
            "Web",
            "web server",
            "web_server",
            "-web",
            "web/api",
            "wéb",
            &"a".repeat(65),
        ] {
            let error = validate_resource(invalid).unwrap_err();
            assert_eq!(
                api::error_json(&error)["code"],
                "invalid_resource",
                "{invalid}"
            );
        }
    }

    /// A directory that is a repository and holds two more, `app` and
    /// `api`, as several repositories used together are kept.
    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        app: PathBuf,
        api: PathBuf,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let temp = tempfile::tempdir()?;
            let root = dunce::canonicalize(temp.path())?.join("root");
            let (app, api) = (root.join("app"), root.join("api"));
            for repo in [&root, &app, &api] {
                fs::create_dir_all(repo)?;
                crate::git::bytes(repo, &["init", "-q", "--template=", "-b", "main"])?;
            }
            Ok(Self {
                _temp: temp,
                root,
                app,
                api,
            })
        }

        fn ask(&self, repo: &Path, resource: &str, who: &str, pid: Option<u32>) -> Result<Value> {
            super::take(
                repo,
                Project::paths(repo)?,
                &api::TurnTake {
                    resource: resource.into(),
                    holder: who.into(),
                    lease: lease(who),
                    pid,
                    grafts: Vec::new(),
                    wait: 0,
                    ttl: None,
                },
                Policy::Run,
            )
        }

        fn give_up(&self, repo: &Path, resource: &str, who: &str) -> Result<Value> {
            super::release(
                repo,
                Project::paths(repo)?,
                &api::TurnRelease {
                    resource: resource.into(),
                    holder: who.into(),
                    lease: lease(who),
                },
            )
        }

        fn list(&self, repo: &Path) -> Result<Vec<(String, Value, Value)>> {
            let turns = list(repo, Project::paths(repo)?)?["turns"].take();
            Ok(turns
                .as_array()
                .unwrap()
                .iter()
                .map(|turn| {
                    let resource = turn["resource"].as_str().unwrap().to_owned();
                    (resource, turn["holder"].clone(), turn["queue"].clone())
                })
                .collect())
        }
    }

    /// One fixed lease per holder name, for the tests.
    fn lease(who: &str) -> String {
        thread_local! {
            static LEASES: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
        }
        LEASES.with(|leases| {
            leases
                .borrow_mut()
                .entry(who.to_owned())
                .or_insert_with(|| Uuid::new_v4().to_string())
                .clone()
        })
    }

    #[test]
    fn a_shared_resource_is_one_queue_for_every_repository_the_directory_configures() -> Result<()>
    {
        let f = Fixture::new()?;
        fs::write(f.root.join(crate::config::LOCAL), r#"{"turns": ["stack"]}"#)?;
        assert_eq!(f.list(&f.app)?, []);
        assert_eq!(f.ask(&f.app, "stack", "agent-1", None)?["granted"], true);
        let queued = f.ask(&f.api, "stack", "agent-2", None)?;
        assert_eq!(
            (&queued["granted"], &queued["holder"], &queued["position"]),
            (&json!(false), &json!("agent-1"), &json!(1))
        );
        // The same holder asking from another repository holds the same turn.
        assert_eq!(f.ask(&f.api, "stack", "agent-1", None)?["granted"], true);
        // The state is the root's; a resource it does not share is each repository's own.
        assert!(f.root.join(".git/graft/turns/turn-stack.json").is_file());
        assert!(!f.app.join(".git/graft/turns").exists());
        assert_eq!(f.ask(&f.app, "web", "agent-2", None)?["granted"], true);
        assert_eq!(f.ask(&f.api, "web", "agent-1", None)?["granted"], true);
        assert_eq!(
            f.list(&f.app)?,
            [
                ("stack".into(), json!("agent-1"), json!(["agent-2"])),
                ("web".into(), json!("agent-2"), json!([])),
            ]
        );
        assert_eq!(
            f.give_up(&f.app, "stack", "agent-1")?,
            json!({"resource": "stack", "released": true, "next": "agent-2"})
        );
        assert_eq!(f.ask(&f.api, "stack", "agent-2", None)?["granted"], true);
        Ok(())
    }

    #[test]
    fn a_repository_with_its_own_configuration_stays_in_the_shared_queue() -> Result<()> {
        let f = Fixture::new()?;
        fs::write(
            f.root.join(crate::config::SHARED),
            r#"{"turns": ["stack"], "repositories": {"app": {}, "api": {}}}"#,
        )?;
        fs::write(
            f.app.join(crate::config::SHARED),
            r#"{"include": [".env"]}"#,
        )?;
        assert_eq!(f.ask(&f.app, "stack", "agent-1", None)?["granted"], true);
        let queued = f.ask(&f.api, "stack", "agent-2", None)?;
        assert_eq!(
            (&queued["granted"], &queued["holder"]),
            (&json!(false), &json!("agent-1"))
        );
        assert!(f.root.join(".git/graft/turns/turn-stack.json").is_file());
        assert!(!f.app.join(".git/graft/turns").exists());
        assert_eq!(
            f.list(&f.app)?,
            [("stack".into(), json!("agent-1"), json!(["agent-2"]))]
        );
        // Its own `turns`, if it lists any, must name the same resources.
        fs::write(
            f.app.join(crate::config::SHARED),
            r#"{"turns": ["database"]}"#,
        )?;
        let error = f.ask(&f.app, "stack", "agent-1", None).unwrap_err();
        assert_eq!(api::error_json(&error)["code"], "invalid_config");
        Ok(())
    }

    #[test]
    fn turns_shared_from_a_directory_that_is_not_a_checkout_are_refused() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = dunce::canonicalize(temp.path())?;
        let app = root.join("app");
        fs::create_dir(&app)?;
        crate::git::bytes(&app, &["init", "-q", "--template=", "-b", "main"])?;
        fs::write(root.join(crate::config::SHARED), r#"{"turns": ["stack"]}"#)?;
        let error = super::take(
            &app,
            Project::paths(&app)?,
            &api::TurnTake {
                resource: "stack".into(),
                holder: "agent".into(),
                lease: Uuid::new_v4().to_string(),
                pid: None,
                grafts: Vec::new(),
                wait: 0,
                ttl: None,
            },
            Policy::Run,
        )
        .unwrap_err();
        assert_eq!(api::error_json(&error)["code"], "invalid_config");
        fs::write(root.join(crate::config::SHARED), r#"{"turns": ["Stack"]}"#)?;
        let error = list(&app, Project::paths(&app)?).unwrap_err();
        assert_eq!(api::error_json(&error)["code"], "invalid_config");
        Ok(())
    }

    #[test]
    fn a_holder_whose_process_ended_loses_the_turn() -> Result<()> {
        let f = Fixture::new()?;
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
            .args(if cfg!(windows) {
                &["/C", "ping", "-n", "30", "127.0.0.1"][..]
            } else {
                &["30"][..]
            })
            .spawn()?;
        let reply = f.ask(&f.app, "web", "short-lived", Some(child.id()))?;
        assert_eq!(reply["granted"], true);
        assert_eq!(f.ask(&f.app, "web", "patient", None)?["position"], 1);
        child.kill()?;
        child.wait()?;
        assert_eq!(
            f.list(&f.app)?,
            [("web".into(), Value::Null, json!(["patient"]))]
        );
        assert_eq!(f.ask(&f.app, "web", "patient", None)?["granted"], true);
        // A process that is not running cannot bound a holder.
        let error = f.ask(&f.app, "web", "nobody", Some(u32::MAX)).unwrap_err();
        assert_eq!(api::error_json(&error)["code"], "invalid_holder");
        Ok(())
    }

    #[test]
    fn only_an_active_graft_can_bound_a_holder() -> Result<()> {
        let f = Fixture::new()?;
        let error = super::take(
            &f.app,
            Project::paths(&f.app)?,
            &api::TurnTake {
                resource: "web".into(),
                holder: "agent".into(),
                lease: lease("agent"),
                pid: None,
                grafts: vec![f.api.clone()],
                wait: 0,
                ttl: None,
            },
            Policy::Run,
        )
        .unwrap_err();
        assert_eq!(api::error_json(&error)["code"], "invalid_holder");
        assert!(format!("{error:#}").contains("is not a graft"));
        Ok(())
    }

    #[test]
    fn turn_state_that_cannot_be_used_is_reported_and_left_unchanged() -> Result<()> {
        let f = Fixture::new()?;
        assert_eq!(f.ask(&f.app, "web", "agent", None)?["granted"], true);
        let path = f.app.join(".git/graft/turns/turn-web.json");
        let good = fs::read(&path)?;
        let older = String::from_utf8(good.clone())?.replace("\"version\":2", "\"version\":1");
        for unusable in [b"{\"version\":2".to_vec(), older.into_bytes()] {
            atomic(&path, &unusable)?;
            for error in [
                f.ask(&f.app, "web", "agent", None).unwrap_err(),
                f.give_up(&f.app, "web", "agent").unwrap_err(),
                list(&f.app, Project::paths(&f.app)?).unwrap_err(),
            ] {
                let message = format!("{error:#}");
                assert!(
                    message.contains("turn-web.json") && message.contains("left it unchanged"),
                    "{message}"
                );
            }
            assert_eq!(fs::read(&path)?, unusable);
            assert_eq!(f.ask(&f.app, "database", "agent", None)?["granted"], true);
        }
        Ok(())
    }

    #[test]
    fn wait_and_ttl_have_limits() {
        let take = |wait, ttl| api::TurnTake {
            resource: "web".into(),
            holder: "agent".into(),
            lease: String::new(),
            pid: None,
            grafts: Vec::new(),
            wait,
            ttl,
        };
        for (wait, ttl) in [
            (0, None),
            (api::TURN_WAIT_MAX, Some(1)),
            (0, Some(api::TURN_TTL_MAX)),
        ] {
            take(wait, ttl).check().unwrap();
        }
        for (wait, ttl) in [
            (api::TURN_WAIT_MAX + 1, None),
            (0, Some(0)),
            (0, Some(api::TURN_TTL_MAX + 1)),
        ] {
            take(wait, ttl).check().unwrap_err();
        }
    }

    #[cfg(unix)]
    #[test]
    fn post_take_hooks_of_the_keeping_project_run_when_a_turn_is_newly_granted() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new()?;
        let log = f.root.join("hook.log");
        let script = f.root.join("switch");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s|%s|%s|%s|%s\\n' \"$GIT_GRAFT_HOOK\" \"$GIT_GRAFT_TURN_RESOURCE\" \"$GIT_GRAFT_TURN_HOLDER\" \"$GIT_GRAFT_TURN_GRAFTS\" \"$(pwd)\" >> '{}'\n[ \"$GIT_GRAFT_TURN_HOLDER\" != broken ]\n",
                log.display()
            ),
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        fs::write(
            f.root.join(crate::config::LOCAL),
            format!(
                r#"{{"turns": ["stack"], "hooks": {{"switch": {{"event": "post-take", "argv": [{}]}}}}}}"#,
                json!(script)
            ),
        )?;
        let lines = || -> Vec<String> {
            fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        };
        let granted = f.ask(&f.app, "stack", "agent-1", None)?;
        assert_eq!(granted["hooks"][0]["status"], "ok", "{granted}");
        assert_eq!(granted["hooks"][0]["event"], "post-take");
        assert_eq!(
            lines(),
            [format!("post-take|stack|agent-1||{}", f.root.display())]
        );
        // A renewal, a queued ask and a resource the root does not keep run nothing.
        assert!(
            f.ask(&f.api, "stack", "agent-1", None)?
                .get("hooks")
                .is_none()
        );
        assert!(
            f.ask(&f.api, "stack", "agent-2", None)?
                .get("hooks")
                .is_none()
        );
        assert!(
            f.ask(&f.app, "web", "agent-1", None)?
                .get("hooks")
                .is_none()
        );
        assert_eq!(lines().len(), 1);
        f.give_up(&f.app, "stack", "agent-1")?;
        f.give_up(&f.app, "stack", "agent-2")?;

        // A failing hook gives the turn up again and refuses the take.
        let error = f.ask(&f.app, "stack", "broken", None).unwrap_err();
        let json = api::error_json(&error);
        assert_eq!(json["code"], "hook_failed");
        assert_eq!(json["details"]["hooks"][0]["exit_code"], 1);
        assert!(
            f.list(&f.app)?
                .iter()
                .all(|(resource, _, _)| resource != "stack")
        );
        assert_eq!(
            f.ask(&f.app, "stack", "agent-2", None)?["hooks"][0]["status"],
            "ok"
        );

        // A caller that may not run hooks takes the turn and reports them skipped.
        f.give_up(&f.app, "stack", "agent-2")?;
        let quiet = super::take(
            &f.app,
            Project::paths(&f.app)?,
            &api::TurnTake {
                resource: "stack".into(),
                holder: "quiet".into(),
                lease: lease("quiet"),
                pid: None,
                grafts: Vec::new(),
                wait: 0,
                ttl: None,
            },
            Policy::Skip,
        )?;
        assert_eq!(
            (&quiet["granted"], &quiet["hooks"][0]["status"]),
            (&json!(true), &json!("skipped"))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn an_edited_hook_needs_trust_only_for_a_take_that_would_run_it() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new()?;
        let log = f.root.join("hook.log");
        let script = f.root.join("switch");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$GIT_GRAFT_TURN_HOLDER\" >> '{}'\n",
                log.display()
            ),
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        let configure = |argv: Value| {
            fs::write(
                f.root.join(crate::config::SHARED),
                json!({"turns": ["stack"], "hooks": {"switch": {"event": "post-take", "argv": argv}}})
                    .to_string(),
            )
        };
        let trust = || -> Result<Value> {
            let (p, _lock) = Project::open_at(Project::paths(&f.root)?, false)?;
            hook::trust::trust(&p)
        };
        let ran = || fs::read_to_string(&log).unwrap_or_default();
        configure(json!([script]))?;
        let untrusted = f.ask(&f.app, "stack", "agent-1", None).unwrap_err();
        assert_eq!(api::error_json(&untrusted)["code"], "hooks_untrusted");
        assert_eq!(f.list(&f.app)?, []);
        trust()?;
        assert_eq!(f.ask(&f.app, "stack", "agent-1", None)?["granted"], true);
        assert_eq!(f.ask(&f.api, "stack", "agent-2", None)?["position"], 1);
        assert_eq!(ran(), "agent-1\n");

        // Once the trusted hook is edited, a renewal and a queued ask run
        // nothing and so are not refused.
        configure(json!([script, "--edited"]))?;
        let renewed = f.ask(&f.app, "stack", "agent-1", None)?;
        assert_eq!(renewed["granted"], true);
        assert!(renewed.get("hooks").is_none());
        let queued = f.ask(&f.api, "stack", "agent-2", None)?;
        assert_eq!(queued["position"], 1);
        assert!(queued.get("hooks").is_none());

        // A take that would run it is refused and changes nothing.
        f.give_up(&f.app, "stack", "agent-1")?;
        let error = f.ask(&f.api, "stack", "agent-2", None).unwrap_err();
        assert_eq!(api::error_json(&error)["code"], "hooks_untrusted");
        assert_eq!(
            f.list(&f.app)?,
            [("stack".into(), Value::Null, json!(["agent-2"]))]
        );
        // Trusted again, it runs for that take.
        trust()?;
        assert_eq!(f.ask(&f.api, "stack", "agent-2", None)?["granted"], true);
        assert_eq!(ran(), "agent-1\nagent-2\n");
        Ok(())
    }

    /// Give `repo` a first commit, so grafts can be added from it.
    #[cfg(unix)]
    fn commit(repo: &Path) -> Result<()> {
        for (key, value) in [
            ("user.name", "Turn Fixture"),
            ("user.email", "turns@example.invalid"),
            ("commit.gpgsign", "false"),
        ] {
            crate::git::bytes(repo, &["config", key, value])?;
        }
        fs::write(repo.join("README.md"), "readme\n")?;
        crate::git::bytes(repo, &["add", "-A"])?;
        crate::git::bytes(repo, &["commit", "-qm", "base"])?;
        Ok(())
    }

    /// Add the graft `name` of `repo`, and return its path and lease.
    #[cfg(unix)]
    fn add(repo: &Path, name: &str) -> Result<(PathBuf, String)> {
        let added = crate::workspace::run(
            repo,
            crate::workspace::Command::Spawn(serde_json::from_value(json!({"name": name}))?),
        )?;
        let path = dunce::canonicalize(added["path"].as_str().context("path")?)?;
        Ok((path, added["lease"].as_str().context("lease")?.to_owned()))
    }

    /// Ask for the turn on `stack` from `repo`, naming `grafts`.
    #[cfg(unix)]
    fn ask_with(repo: &Path, who: &str, grafts: &[&Path]) -> Result<Value> {
        super::take(
            repo,
            Project::paths(repo)?,
            &api::TurnTake {
                resource: "stack".into(),
                holder: who.into(),
                lease: lease(who),
                pid: None,
                grafts: grafts.iter().map(|path| path.to_path_buf()).collect(),
                wait: 0,
                ttl: None,
            },
            Policy::Run,
        )
    }

    /// Where Graft keeps the link of repository `key` for `stack` in `root`.
    #[cfg(unix)]
    fn link_of(root: &Path, key: &str) -> PathBuf {
        key.split('/')
            .fold(root.join(".graft/links/stack"), |at, part| at.join(part))
    }

    /// Write the configuration `body` with `stack` shared and linked for
    /// `repositories`, as the root's untracked local file, which needs no trust.
    #[cfg(unix)]
    fn configure_links(root: &Path, repositories: &[&str], extra: Value) -> Result<()> {
        let mut body = json!({
            "turns": ["stack"],
            "links": ["stack"],
            "repositories": repositories.iter().map(|key| (key.to_string(), json!({}))).collect::<serde_json::Map<_, _>>(),
        });
        for (key, value) in extra.as_object().into_iter().flatten() {
            body[key] = value.clone();
        }
        fs::write(root.join(crate::config::LOCAL), body.to_string())?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn links_lead_to_the_holder_s_graft_of_each_repository_or_to_the_repository() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new()?;
        commit(&f.app)?;
        commit(&f.api)?;
        let (app_link, api_link) = (link_of(&f.root, "app"), link_of(&f.root, "api"));
        // The hook records where both links lead when it runs.
        let log = f.root.join("hook.log");
        let script = f.root.join("switch");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s %s\\n' \"$(readlink '{}')\" \"$(readlink '{}')\" >> '{}'\n",
                app_link.display(),
                api_link.display(),
                log.display()
            ),
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        configure_links(
            &f.root,
            &["app", "api"],
            json!({"hooks": {"switch": {"event": "post-take", "argv": [script]}}}),
        )?;
        let (app_graft, _) = add(&f.app, "fix")?;
        let (api_graft, _) = add(&f.api, "fix")?;
        let leads = |link: &Path| fs::read_link(link).ok();
        let ran = || fs::read_to_string(&log).unwrap_or_default();

        // A holder with a graft of app only: app's link leads to it, api's to api.
        let granted = ask_with(&f.app, "agent-1", &[&app_graft])?;
        assert_eq!(granted["granted"], true);
        assert_eq!(
            granted["links"],
            // In the order the configuration names them; JSON objects here
            // are written in key order.
            json!([
                {"repository": "api", "path": api_link, "status": "pointed", "target": f.api},
                {"repository": "app", "path": app_link, "status": "pointed", "target": app_graft},
            ])
        );
        assert_eq!(leads(&app_link), Some(app_graft.clone()));
        assert_eq!(leads(&api_link), Some(f.api.clone()));
        // The hook found them in place.
        assert_eq!(
            ran(),
            format!("{} {}\n", app_graft.display(), f.api.display())
        );

        // A queued ask points no link and looks at none, even with
        // something else where one goes.
        fs::remove_file(&api_link)?;
        fs::create_dir(&api_link)?;
        let queued = ask_with(&f.api, "agent-2", &[&api_graft])?;
        assert_eq!(queued["granted"], false);
        assert!(queued.get("links").is_none());
        assert_eq!(leads(&app_link), Some(app_graft.clone()));
        fs::remove_dir(&api_link)?;

        // Naming other grafts points the links again, before the hook runs.
        ask_with(&f.api, "agent-1", &[&api_graft])?;
        assert_eq!(leads(&app_link), Some(f.app.clone()));
        assert_eq!(leads(&api_link), Some(api_graft.clone()));
        assert!(ran().ends_with(&format!("{} {}\n", f.app.display(), api_graft.display())));

        // A renewal puts back a link that is missing and keeps the others.
        fs::remove_file(&app_link)?;
        let renewed = ask_with(&f.api, "agent-1", &[])?;
        assert!(renewed.get("hooks").is_none());
        assert_eq!(renewed["links"][0]["status"], "kept");
        assert_eq!(renewed["links"][1]["status"], "pointed");
        assert_eq!(leads(&app_link), Some(f.app.clone()));

        // Release leaves the links; the next holder gets them.
        f.give_up(&f.app, "stack", "agent-1")?;
        assert_eq!(leads(&api_link), Some(api_graft.clone()));
        assert_eq!(ask_with(&f.app, "agent-2", &[&app_graft])?["granted"], true);
        assert_eq!(leads(&app_link), Some(app_graft.clone()));
        assert_eq!(leads(&api_link), Some(f.api.clone()));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn links_the_repository_carries_are_written_only_once_trusted() -> Result<()> {
        let f = Fixture::new()?;
        fs::write(
            f.root.join(crate::config::SHARED),
            r#"{"turns": ["stack"], "links": ["stack"], "repositories": {"app": {}}}"#,
        )?;
        let link = link_of(&f.root, "app");
        let granted = ask_with(&f.app, "agent-1", &[])?;
        assert_eq!(granted["granted"], true);
        assert_eq!(granted["links"][0]["status"], "skipped");
        assert_eq!(granted["links"][0]["reason"], "untrusted");
        assert!(
            granted["links"][0]["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("git graft hook trust"))
        );
        assert!(link.symlink_metadata().is_err());
        let listed = hook::list(&f.root, &f.root.join(".git/graft"))?;
        assert_eq!(listed["links"][0]["trusted"], false, "{listed}");

        let trusted = {
            let (p, _lock) = Project::open_at(Project::paths(&f.root)?, false)?;
            hook::trust::trust(&p)?
        };
        assert_eq!(trusted["trusted_links"], json!(["stack"]));
        assert_eq!(
            hook::list(&f.root, &f.root.join(".git/graft"))?["links"][0]["trusted"],
            true
        );
        assert_eq!(
            ask_with(&f.app, "agent-1", &[])?["links"][0]["status"],
            "pointed"
        );
        assert_eq!(fs::read_link(&link)?, f.app);

        // A change to what is linked needs trust again.
        fs::write(
            f.root.join(crate::config::SHARED),
            r#"{"turns": ["stack", "web"], "links": ["stack", "web"], "repositories": {"app": {}}}"#,
        )?;
        assert_eq!(
            ask_with(&f.app, "agent-1", &[])?["links"][0]["status"],
            "skipped"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn something_other_than_a_link_on_the_way_refuses_a_granted_take() -> Result<()> {
        use std::os::unix::fs::symlink;
        let f = Fixture::new()?;
        configure_links(&f.root, &["app"], json!({}))?;
        let link = link_of(&f.root, "app");
        fs::create_dir_all(&link)?;
        fs::write(link.join("kept"), "kept\n")?;
        let refused = |path: &Path| -> Result<()> {
            let error = ask_with(&f.app, "agent-1", &[]).unwrap_err();
            let json = api::error_json(&error);
            assert_eq!(json["code"], "invalid_config");
            assert_eq!(json["details"]["path"], json!(path));
            assert!(
                json["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("move it aside")),
                "{json}"
            );
            // Nothing changed: no turn.
            assert_eq!(f.list(&f.app)?, []);
            assert!(!f.root.join(".git/graft/turns/turn-stack.json").exists());
            Ok(())
        };
        refused(&link)?;
        assert_eq!(fs::read_to_string(link.join("kept"))?, "kept\n");
        // A link on the way is not followed.
        fs::rename(f.root.join(".graft"), f.root.join("aside"))?;
        symlink(f.root.join("aside"), f.root.join(".graft"))?;
        refused(&f.root.join(".graft"))?;
        fs::remove_file(f.root.join(".graft"))?;
        // With the way clear, the take makes the link and its directories.
        assert_eq!(ask_with(&f.app, "agent-1", &[])?["granted"], true);
        assert_eq!(fs::read_link(&link)?, f.app);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn a_take_that_fails_puts_every_link_back() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new()?;
        let nested = f.root.join("services").join("api");
        fs::create_dir_all(&nested)?;
        crate::git::bytes(&nested, &["init", "-q", "--template=", "-b", "main"])?;
        commit(&f.app)?;
        commit(&nested)?;
        // The hook fails for the holder named broken.
        let script = f.root.join("switch");
        fs::write(
            &script,
            "#!/bin/sh\n[ \"$GIT_GRAFT_TURN_HOLDER\" != broken ]\n",
        )?;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))?;
        configure_links(
            &f.root,
            &["app", "services/api"],
            json!({"hooks": {"switch": {"event": "post-take", "argv": [script]}}}),
        )?;
        let (app_link, api_link) = (link_of(&f.root, "app"), link_of(&f.root, "services/api"));
        let (first, _) = add(&f.app, "first")?;
        let (second, _) = add(&f.app, "second")?;
        let (api_graft, _) = add(&nested, "fix")?;
        let leads = |link: &Path| fs::read_link(link).ok();
        assert_eq!(ask_with(&f.app, "agent-1", &[&first])?["granted"], true);
        f.give_up(&f.app, "stack", "agent-1")?;
        let before = (leads(&app_link), leads(&api_link));
        assert_eq!(before, (Some(first.clone()), Some(nested.clone())));

        // A failing hook gives the turn up and puts the links back.
        let error = ask_with(&f.app, "broken", &[&second, &api_graft]).unwrap_err();
        assert_eq!(api::error_json(&error)["code"], "hook_failed");
        assert_eq!((leads(&app_link), leads(&api_link)), before);
        assert_eq!(f.list(&f.app)?, []);

        // So does a link that cannot be written: app's is pointed first,
        // then services/api's directory refuses it.
        let unwritable = api_link.parent().context("parent")?.to_owned();
        fs::set_permissions(&unwritable, fs::Permissions::from_mode(0o555))?;
        // The superuser writes anyway, so there is nothing to see.
        if fs::write(unwritable.join("probe"), "").is_err() {
            let error = ask_with(&f.app, "agent-2", &[&second, &api_graft]).unwrap_err();
            assert!(format!("{error:#}").contains("point the link"), "{error:#}");
            assert_eq!((leads(&app_link), leads(&api_link)), before);
            assert_eq!(f.list(&f.app)?, []);
            // A link that did not exist before is removed again.
            fs::set_permissions(&unwritable, fs::Permissions::from_mode(0o755))?;
            fs::remove_file(&app_link)?;
            fs::set_permissions(&unwritable, fs::Permissions::from_mode(0o555))?;
            ask_with(&f.app, "agent-2", &[&second, &api_graft]).unwrap_err();
            assert!(app_link.symlink_metadata().is_err());
        }
        fs::set_permissions(&unwritable, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn archiving_the_graft_a_link_leads_to_points_it_at_the_repository() -> Result<()> {
        let f = Fixture::new()?;
        commit(&f.app)?;
        configure_links(&f.root, &["app"], json!({}))?;
        let link = link_of(&f.root, "app");
        let (graft, lease) = add(&f.app, "fix")?;
        let (other, other_lease) = add(&f.app, "other")?;
        assert_eq!(
            ask_with(&f.app, "agent-1", &[&graft, &other])?["granted"],
            true
        );
        assert_eq!(fs::read_link(&link)?, graft);
        // Archiving a graft the link does not lead to leaves it.
        crate::workspace::run(
            &f.app,
            crate::workspace::Command::Drop(api::DropWorkspace {
                name: "other".into(),
                lease: other_lease,
                discard: false,
            }),
        )?;
        assert_eq!(fs::read_link(&link)?, graft);
        let removed = crate::workspace::run(
            &f.app,
            crate::workspace::Command::Drop(api::DropWorkspace {
                name: "fix".into(),
                lease,
                discard: false,
            }),
        )?;
        assert_eq!(removed["removed"], true);
        assert!(!graft.exists());
        assert_eq!(fs::read_link(&link)?, f.app);

        // So does prune, when it archives an expired graft.
        let (expired, _) = add(&f.app, "expired")?;
        assert_eq!(ask_with(&f.app, "agent-2", &[&expired])?["granted"], true);
        assert_eq!(fs::read_link(&link)?, expired);
        {
            let (p, _lock) = Project::open_at(Project::paths(&f.app)?, false)?;
            registry::expire(&p, "expired")?;
        }
        let pruned = crate::workspace::run(
            &f.app,
            crate::workspace::Command::Gc(api::Gc {
                apply: true,
                purge_cache: false,
                purge_archives: None,
            }),
        )?;
        assert_eq!(pruned["retained"][0]["reason"], "archived", "{pruned}");
        assert_eq!(fs::read_link(&link)?, f.app);
        Ok(())
    }
}
