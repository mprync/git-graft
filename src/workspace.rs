//! Graft creation, inspection, integration and recoverable retirement.
pub(crate) mod hook;
mod land;
mod objects;
mod push;
mod registry;
mod remotes;
mod turns;

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::{
    api, config::Config, engine, git, include, layer, registry::Project, runtime::Runtime, storage,
};
pub(crate) use objects::{
    HISTORY_CACHE_DIR, HISTORY_CACHE_MARKER, history_cache_root, verify_history_cache_generation,
};
pub(crate) use registry::has_live;
use registry::{Record, Status};
pub use remotes::Pushing;
pub(crate) use turns::validate_resource;

pub(crate) use api::Spawn;

/// One operation on a project's grafts. Built from the command line (see
/// `cli`) or from an MCP tool call, then run by [`run`].
#[derive(Debug)]
pub enum Command {
    Spawn(Spawn),
    List,
    /// List the invoking checkout's untracked and ignored files.
    LocalFiles,
    Status(api::Name),
    Doctor,
    Heartbeat(api::Owned),
    Env(api::Name),
    Exec(api::Exec),
    RegisterManager(api::RegisterManager),
    ListManagers,
    UnregisterManager(api::ManagerName),
    ListHooks,
    /// Record the hooks the repository carries as trusted.
    TrustHooks,
    AddDep(api::AddDep),
    SeedLayer(api::SeedLayer),
    /// Save, restore or forget; the string is the action.
    Layer(&'static str, api::Layer),
    Land(api::Land),
    Push(api::Push),
    Drop(api::DropWorkspace),
    Restore(api::Owned),
    UndoLand(api::Owned),
    Lock {
        workspace: api::Owned,
        reason: Option<String>,
    },
    Unlock(api::Owned),
    Move {
        workspace: api::Owned,
        path: PathBuf,
    },
    Repair {
        workspace: api::Owned,
        path: PathBuf,
    },
    Gc(api::Gc),
    TurnTake(api::TurnTake),
    TurnRelease(api::TurnRelease),
    TurnList,
}

impl Command {
    /// `merge`, or its reversal when `undo` was requested.
    pub(crate) fn land(land: api::Land) -> Self {
        if land.undo {
            Self::UndoLand(api::Owned {
                name: land.name,
                lease: land.lease,
            })
        } else {
            Self::Land(land)
        }
    }
}

fn summary(r: &Record) -> Value {
    json!({"id":r.id, "name":r.name, "path":r.path,
        "branch":r.branch, "base":r.base, "port":r.port, "status":r.status,
        "phase":match r.status {Status::Pending=>"creating", Status::Active if r.plan.is_some()=>"landing",
        Status::Active=>"ready", Status::Dropped=>"dropped", Status::Landed=>"landed"},
        "locked":r.locked, "storage":r.storage, "last_heartbeat":r.heartbeat})
}

fn with_lease(r: &Record) -> Value {
    let mut value = summary(r);
    value["lease"] = json!(r.lease);
    value
}

fn project_history(project: &Project) -> Result<api::History> {
    // Read the primary repository's own setting, including for nested spawns.
    let out = git::raw(
        &project.repo,
        &[
            "config",
            "--local",
            "--no-includes",
            "--null",
            "--get",
            "graft.history",
        ],
    )?;
    match (out.status.code(), out.stdout.as_slice()) {
        (Some(1), b"") | (Some(0), b"local\0") => Ok(api::History::Local),
        (Some(0), b"isolated\0") => Ok(api::History::Isolated),
        (Some(0), b"tip\0") => Ok(api::History::Tip),
        (Some(0), _) => anyhow::bail!("graft.history must be isolated, tip or local"),
        _ => anyhow::bail!("could not read the project's graft.history setting"),
    }
}

/// Join a scoped worker, re-raising its panic on the caller's thread.
pub(crate) fn joined<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    handle
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Add the graft `args` describes. The caller holds the project lock, `lock`,
/// and the shared build lock, `builds`.
fn spawn(
    p: &Project,
    origin: &Path,
    mut args: Spawn,
    lock: &std::fs::File,
    mut builds: crate::registry::Held,
    hooks: hook::Policy,
    pushing: Pushing,
) -> Result<Value> {
    if let Some(id) = &mut args.request_id {
        *id = uuid::Uuid::parse_str(id)
            .context("request ID must be a UUID")?
            .to_string();
    }
    crate::registry::validate_name(&args.name)?;
    let populated = args.orphan.is_none() && !args.no_checkout;
    if let Some(patterns) = &args.include {
        include::validate(patterns)?;
        ensure!(
            patterns.is_empty() || populated,
            "include requires a populated checkout"
        );
    }
    // The project's configuration is read before anything changes, so a
    // file Graft cannot read refuses the add instead of leaving a graft whose
    // hooks never ran or that copied the wrong files.
    let config = Config::read(&p.repo)?;
    hook::trust::require(p, &config, hook::Event::PostAdd, hooks)?;
    // Remotes that need trust they lack are left out, not refused.
    let untrusted = hook::trust::untrusted_remotes(p, &config)?;
    let include = if populated {
        include::select(args.include.as_deref(), &config)?
    } else {
        None
    };
    ensure!(
        include.is_none() || origin.join(".git").symlink_metadata().is_ok(),
        "include needs a checkout with working files; this repository is bare"
    );
    let mut request = serde_json::to_value(&args)?;
    let previous = args
        .request_id
        .as_ref()
        .and_then(|id| registry::for_request(p, &args.name, id))
        .filter(|record| !record.request.is_null());
    // Each Git query below costs a process launch. Run the read-only ones
    // concurrently, then report their results in the original order.
    let custom_branch = args.orphan.as_ref().or(args.branch.as_ref());
    let resolve_base =
        args.orphan.is_none() && previous.as_ref().is_none_or(|record| record.base.is_none());
    let (configured, branch_format, resolved, source) = std::thread::scope(|scope| {
        let configured = (args.history.is_none() && previous.is_none())
            .then(|| scope.spawn(|| project_history(p)));
        let branch_format = custom_branch.map(|name| {
            scope.spawn(move || {
                git::bytes(
                    &p.repo,
                    &["check-ref-format", &format!("refs/heads/{name}")],
                )
            })
        });
        let resolved = resolve_base.then(|| scope.spawn(|| git::oid(origin, &args.base)));
        let source = objects::preflight(origin);
        (
            configured.map(joined),
            branch_format.map(joined),
            resolved.map(joined),
            source,
        )
    });
    let history = match args.history {
        Some(mode) => mode,
        None => {
            let history = match &previous {
                Some(record) => recorded_history(&record.request),
                None => configured.context("project history was not read")??,
            };
            match history {
                api::History::Local => request["project_local_history"] = Value::Bool(true),
                api::History::Tip => request["project_history"] = json!(history),
                api::History::Isolated => (),
            }
            history
        }
    };
    let mut reserved_base = None;
    if let Some(current) = previous {
        ensure!(
            current.request == request,
            "request ID was used with different add options"
        );
        if current.status == Status::Active {
            registry::verify(p, &current)?;
            return with_leftovers(p, &current);
        }
        reserved_base = current.base;
    }
    let branch = if args.detach {
        None
    } else {
        Some(
            args.orphan
                .clone()
                .or(args.branch.clone())
                .unwrap_or_else(|| format!("graft/{}", args.name)),
        )
    };
    // The generated graft/<name> uses the portable name validated above.
    // Reject invalid caller-supplied branches before reserving a workspace.
    if let Some(name) = custom_branch {
        ensure!(
            !name.starts_with('-') && !name.starts_with("refs/") && name != "HEAD",
            "invalid branch name"
        );
        branch_format.context("branch name was not checked")??;
    }
    let base = if args.orphan.is_some() {
        None
    } else {
        let oid = match reserved_base {
            Some(base) => base,
            None => resolved.context("base was not resolved")??,
        };
        storage::validate_sparse(origin, &oid, &args.sparse)?;
        Some(oid)
    };
    let source = source?;
    crate::trace::mark("preflight");
    let local = history == api::History::Local && cfg!(target_os = "macos");
    let proof = base
        .as_deref()
        .filter(|oid| {
            !args.no_checkout
                && args.sparse.is_empty()
                && (local
                    || history == api::History::Tip && objects::needs_tip_proof(&p.state, oid))
        })
        .and_then(|oid| objects::CheckoutDiscovery::start(origin, oid, local));
    crate::trace::mark("start discovery");
    let post_add = config.hooks_for(hook::Event::PostAdd);
    let (mut r, _graft_lock) = loop {
        match registry::reserve_locked(
            p,
            &args.name,
            args.request_id.as_deref(),
            base.as_deref(),
            branch.as_deref(),
            request.clone(),
        ) {
            // A concurrent retry waits for the first attempt outside every
            // lock, since that attempt needs the project lock to finish,
            // then reserves again and replies with its graft.
            Err(error) if error.is::<registry::Building>() => {
                drop(builds);
                lock.unlock().context("release the project lock")?;
                registry::wait_for_graft(p, &args.name)?;
                lock.lock().context("lock graft registry")?;
                builds = p.lock_builds(true)?;
            }
            reserved => break reserved?,
        }
    };
    if r.status == Status::Active {
        registry::verify(p, &r)?;
        return with_leftovers(p, &r);
    }
    crate::trace::mark("reserve");
    let creation = Creation {
        history,
        base: base.as_deref(),
        branch: branch.as_deref(),
        sparse: &args.sparse,
        no_checkout: args.no_checkout,
        no_deps: args.no_deps,
        include: include.as_ref(),
        locked: args.lock.then(|| args.reason.unwrap_or_default()),
    };
    // The graft lock keeps every other command off this graft and its
    // record, and the build lock keeps the shared inputs in place, so other
    // grafts can be added meanwhile.
    let built = (lock.unlock().context("release the project lock"))
        .and_then(|()| create(p, origin, &source, &mut r, &creation, proof))
        .and_then(|()| {
            drop(builds);
            lock.lock().context("lock graft registry")?;
            registry::finish(p, &mut r)
        });
    crate::trace::mark("finish");
    if let Err(error) = built {
        // A failure after the checkout published its index, such as while
        // attaching dependencies, leaves a complete checkout without the lock.
        let guard = match storage::index_reserved(&r.path.join(".git")) {
            Ok(true) => {
                "Until then the graft's index lock keeps Git from committing the files it lacks as deletions."
            }
            _ => {
                "Its checkout was published before it stopped, but the graft is not ready; do not work in it until the retry."
            }
        };
        // Best effort: the reply is still accurate if the flush fails.
        let _ = registry::flush_reservation(p, &r);
        return Err(api::fault(
            "spawn_incomplete",
            format!("Creating the graft stopped: {error:#}"),
            json!({"name":r.name,"lease":r.lease,"path":r.path,"recovery":format!("Retry the same request: Graft archives this attempt whole and builds the graft again. {guard}")}),
        ));
    }
    let mut reply = with_leftovers(p, &r)?;
    with_remotes(p, &r, &config, &untrusted, history, pushing, &mut reply);
    if post_add.is_empty() {
        return Ok(reply);
    }
    // The graft is recorded and usable whatever its hooks do. They run
    // under the graft lock taken at reservation; the build lock is released.
    let mut hooks = hook::Runner::new(hooks, lock, None);
    hooks.after(p, &post_add, &r)?;
    Ok(hooks.report(reply))
}

/// Give the graft `r` the remotes `config` names, and report them in `reply`
/// as `remotes`. The graft is usable either way, so a remote that fails is
/// reported by name, not returned. Remote-tracking refs are copied only with
/// local history, which already holds their objects. The remotes in
/// `untrusted` need trust they lack and are left out. `pushing` says whether
/// the caller may push through them.
fn with_remotes(
    p: &Project,
    r: &Record,
    config: &Config,
    untrusted: &[String],
    history: api::History,
    pushing: Pushing,
    reply: &mut Value,
) {
    if config.remotes.is_empty() {
        return;
    }
    let copy = history == api::History::Local;
    reply["remotes"] = json!(remotes::configure(
        &p.repo,
        &r.path,
        &config.remotes,
        untrusted,
        copy,
        pushing
    ));
}

/// The history a recorded request built its graft with.
fn history_of(request: &Value) -> api::History {
    serde_json::from_value::<Option<api::History>>(request["history"].clone())
        .ok()
        .flatten()
        .unwrap_or_else(|| recorded_history(request))
}

/// The reply for a finished graft, with its lease.
fn with_leftovers(p: &Project, r: &Record) -> Result<Value> {
    with_attempts(p, r, with_lease(r))
}

/// Add `archived_attempts` to a reply about the graft `r`: the unfinished
/// earlier attempts of its request that a retry archived whole, each with its
/// archive path and lease. Every reply about the graft repeats them, so they
/// are not lost with the add reply.
fn with_attempts(p: &Project, r: &Record, mut value: Value) -> Result<Value> {
    let archived = registry::archived_attempts(p, r)?;
    if !archived.is_empty() {
        value["archived_attempts"] = json!(archived);
    }
    Ok(value)
}

/// The history of a recorded request that relied on the project's policy.
/// The policy is recorded at reservation, so a retry uses it even after
/// reconfiguration. Older requests without either marker used isolated history.
fn recorded_history(request: &Value) -> api::History {
    if request["project_local_history"] == true {
        api::History::Local
    } else if request["project_history"] == "tip" {
        api::History::Tip
    } else {
        api::History::Isolated
    }
}

/// What a reserved graft is built from: the options of the first attempt or
/// of a retry of the same request.
struct Creation<'a> {
    history: api::History,
    base: Option<&'a str>,
    branch: Option<&'a str>,
    sparse: &'a [String],
    no_checkout: bool,
    no_deps: bool,
    /// What to copy from the invoking checkout besides the commit's files.
    include: Option<&'a include::Patterns>,
    /// The lock reason, when the graft is created locked.
    locked: Option<String>,
}

/// Build the freshly reserved graft `r`, ready for [`registry::finish`]. The
/// caller holds the graft's workspace lock and the shared build lock, but
/// not the project lock. The reservation's index lock stays in place if
/// building stops, so Git cannot use the graft while it is incomplete.
fn create(
    p: &Project,
    origin: &Path,
    source: &objects::Primary<'_>,
    r: &mut Record,
    c: &Creation<'_>,
    proof: Option<objects::CheckoutDiscovery>,
) -> Result<()> {
    registry::verify(p, r)?;
    crate::trace::mark("verify reservation");
    let (objects, prepared_checkout) = objects::initialize(
        source, &r.path, &p.state, c.base, c.branch, c.history, proof,
    )?;
    crate::trace::mark("initialize objects");
    let mut checkout = match (prepared_checkout, c.base) {
        (Some(checkout), _) => checkout,
        (None, Some(oid)) if !c.no_checkout => {
            let materialize = if c.history != api::History::Isolated {
                storage::materialize_local
            } else {
                storage::materialize
            };
            materialize(&r.path, &p.state, oid, c.sparse, origin)?
        }
        _ => {
            // An unborn or --no-checkout graft has no index to publish, so it
            // releases the reservation here. A checkout releases it only by
            // publishing its index.
            storage::release_index(&r.path.join(".git"))?;
            json!({"backend":"empty","files":0,"logical_bytes":0})
        }
    };
    checkout["objects"] = objects;
    crate::trace::mark("checkout");
    if c.base.is_some() && !c.no_checkout && c.sparse.is_empty() && !c.no_deps {
        if let Some(deps) = layer::inherit_shared(&r.path, &p.state, &p.repo)? {
            checkout["dependencies"] = deps;
        }
        let registered = crate::package_manager::attach(p, &r.path);
        if !registered.is_empty() {
            checkout["registered_dependencies"] = json!(registered);
        }
    }
    // Copy chosen untracked and ignored files last, so they never replace
    // the checkout or an attached install.
    if c.base.is_some()
        && !c.no_checkout
        && let Some(patterns) = c.include
    {
        let attached = crate::package_manager::attached_directories(&checkout)?;
        checkout["included"] =
            include::copy(origin, &r.path, patterns, !c.sparse.is_empty(), &attached)?;
        crate::trace::mark("include");
    }
    r.storage = checkout;
    r.locked.clone_from(&c.locked);
    Ok(())
}

/// Drop the ignored `node_modules` that Graft itself attached from the shared
/// layer, while it still hashes to the `contents` its attach receipt recorded
/// in `storage`. Once the graft changes it, it counts as local work again.
/// Every other change, including other ignored files, is kept.
fn without_attached_install(
    path: &Path,
    storage: &Value,
    found: Vec<git::Change>,
) -> Vec<git::Change> {
    let attached = |change: &git::Change| {
        change.status == "!!"
            && (change.path == "node_modules" || change.path.starts_with("node_modules/"))
    };
    if !found.iter().any(attached) {
        return found;
    }
    let dependencies = &storage["dependencies"];
    let unchanged = dependencies["status"] == "attached"
        && dependencies["contents"]
            .as_str()
            .is_some_and(|contents| layer::attached_unchanged(path, contents));
    if unchanged {
        return found
            .into_iter()
            .filter(|change| !attached(change))
            .collect();
    }
    found
}

/// Drop from `found` the files Graft itself put in the graft at `add` and
/// the graft has not changed: the attached install and the included copies.
fn without_graft_files(path: &Path, storage: &Value, found: Vec<git::Change>) -> Vec<git::Change> {
    let found = without_attached_install(path, storage, found);
    include::without_unchanged_copies(path, storage, found)
}

/// Which command checks that a graft holds no unrecorded work.
#[derive(Clone, Copy)]
enum Retiring {
    Merge,
    Remove,
}

/// Most changed paths a `local_changes` refusal names.
const LISTED_CHANGES: usize = 200;

/// Refuse to archive a graft whose files hold work Git has not recorded:
/// staged, unstaged and untracked changes and, unless `allow_ignored`,
/// ignored files. The refusal is `local_changes`, naming each path with its
/// status, and touches nothing. For `remove`, `discard_archives` is always
/// true: `remove --discard` would archive every one of these files with the
/// rest of the graft, never delete them. `merge` has no such option.
///
/// The `node_modules` that Graft itself attached is not local work; see
/// `without_attached_install`. Nor are the untracked and ignored files it
/// copied in at `add` while unchanged; see `include::without_unchanged_copies`.
fn clean(path: &Path, allow_ignored: bool, retiring: Retiring, storage: &Value) -> Result<()> {
    git::check_idle(path)?;
    let mut found: Vec<_> = git::changes(path)?
        .into_iter()
        .filter(|change| !(allow_ignored && change.status == "!!"))
        .collect();
    found = without_graft_files(path, storage, found);
    if found.is_empty() {
        return Ok(());
    }
    let paths: Vec<_> = found
        .iter()
        .take(LISTED_CHANGES)
        .map(|change| {
            let status = match change.status.as_bytes() {
                b"??" => "untracked",
                b"!!" => "ignored",
                // The staged side names a staged addition or deletion.
                [b'A', _] | [b' ', b'A'] => "added",
                [b'D', _] | [b' ', b'D'] => "deleted",
                _ => "modified",
            };
            json!({"path": change.path, "status": status})
        })
        .collect();
    let remedy = match retiring {
        Retiring::Merge => {
            "Commit them in the graft and retry the merge. Its files and index have been retained"
        }
        Retiring::Remove => {
            "Commit or merge them, or archive the graft with them using remove --discard and keep the recovery lease. Its files and index have been retained"
        }
    };
    let mut details = json!({"paths": paths, "count": found.len()});
    if matches!(retiring, Retiring::Remove) {
        details["discard_archives"] = json!(true);
    }
    Err(api::fault(
        "local_changes",
        format!(
            "The graft has {} local changes or ignored files. {remedy}",
            found.len()
        ),
        details,
    ))
}

/// Refuse a graft with a Git operation in progress or a change to a tracked
/// file, naming every local change as `clean` does. Neither is a
/// `pre-archive` hook's to undo, so `remove` checks them before the hook
/// runs, and the hook's cleanup is not spent on a graft that would be refused
/// anyway. A graft with only untracked or ignored files is left to `clean`.
fn clean_tracked(path: &Path, retiring: Retiring, storage: &Value) -> Result<()> {
    git::check_idle(path)?;
    let tracked = git::changes(path)?
        .iter()
        .any(|change| change.status != "??" && change.status != "!!");
    if tracked {
        clean(path, false, retiring, storage)
    } else {
        Ok(())
    }
}

/// What an archive did. A merge receipt says every commit of the graft was
/// merged. When `remove --discard` archived commits made after an earlier
/// merge had landed, the action is `remove`: the receipt still names the
/// merge that landed, and `head` is the archived graft HEAD whose later
/// commits were never merged.
fn receipt(p: &Project, r: &Record) -> Result<Value> {
    let value = match (&r.plan, &r.unmerged_head) {
        (Some(plan), None) => {
            json!({"action":"merge","name":r.name,"target":plan.target,"previous":plan.old,
            "commit":plan.new,"graft_tip":plan.tip,"removed":true,"recovery_lease":r.lease})
        }
        (Some(plan), Some(head)) => {
            json!({"action":"remove","name":r.name,"target":plan.target,"previous":plan.old,
            "commit":plan.new,"graft_tip":plan.tip,"head":head,"unmerged_commits":true,
            "removed":true,"recovery_lease":r.lease})
        }
        (None, _) => {
            json!({"action":"remove","name":r.name,"removed":true,"recovery_lease":r.lease})
        }
    };
    with_attempts(p, r, value)
}

fn retire(p: &Project, name: &str, lease: &str, status: Status) -> Result<Value> {
    receipt(p, &registry::retire(p, name, lease, status, None)?)
}

fn destination_contains(p: &Project, plan: &crate::landing::Plan) -> Result<bool> {
    let dest = format!("refs/heads/{}", plan.target);
    let oid = git::checked_oid(&plan.new)?;
    let revision = format!("{oid}^{{commit}}");
    let present = git::raw(
        &p.repo,
        &[
            "--no-replace-objects",
            "rev-parse",
            "--quiet",
            "--verify",
            "--end-of-options",
            &revision,
        ],
    )?;
    if present.status.code() == Some(1) {
        // A rejected plan's result can still live exclusively in the private plan store.
        return Ok(false);
    }
    ensure!(
        present.status.success(),
        "could not inspect the merge result: {}",
        String::from_utf8_lossy(&present.stderr)
    );
    let output = git::raw(
        &p.repo,
        &[
            "--no-replace-objects",
            "merge-base",
            "--is-ancestor",
            &plan.new,
            &dest,
        ],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!(
            "could not verify the recorded merge on {}: {}",
            plan.target,
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn land_workspace(p: &Project, opts: &api::Land, hooks: &mut hook::Runner<'_>) -> Result<Value> {
    let _lock = registry::workspace_lock(p, &opts.name)?;
    if let Ok(r) = registry::load_retired(p, &opts.name, &opts.lease)
        && r.plan.as_ref().is_some_and(|plan| plan.target == opts.into)
    {
        return receipt(p, &r);
    }
    let mut r = registry::authenticate(p, &opts.name, &opts.lease)?;
    if r.retiring_to().is_some() {
        ensure!(
            r.plan.as_ref().is_some_and(|plan| plan.target == opts.into),
            "retry the recorded merge"
        );
        return retire(p, &r.name, &r.lease, Status::Landed);
    }
    ensure!(r.status == Status::Active, "graft creation is incomplete");
    // Verify the graft is where its record says before touching its files.
    registry::preflight_retire(p, &r)?;
    hooks.pre_archive(p, &mut r)?;
    // We hold the workspace lock, which a killed merge --commit has released.
    crate::landing::reclaim_staging_lock(&r.path)?;
    if let Some(plan) = &r.plan {
        ensure!(plan.target == opts.into, "retry merge into {}", plan.target);
        if destination_contains(p, plan)? {
            crate::landing::finish_applied_checkout(&p.repo, plan)?;
            if git::oid(&r.path, "HEAD")? != plan.tip {
                return keep_advanced(p, r);
            }
        }
    }
    if !opts.discard_ignored {
        ensure!(
            !without_graft_files(&r.path, &r.storage, git::changes(&r.path)?)
                .iter()
                .any(|c| c.status == "!!"),
            "graft has ignored files, such as build output; rerun with --discard-ignored to keep them in its archive"
        );
    }
    let plan = if let Some(plan) = &r.plan {
        plan.clone()
    } else {
        if opts.reconcile {
            crate::cache::prepare_package_roots(p)?;
        }
        let installations = crate::package_manager::attached_directories(&r.storage)?;
        let plan = land::prepare(p, &r.path, opts, &installations)?;
        r.plan = Some(plan.clone());
        registry::save(p, &r)?;
        plan
    };
    let apply = || -> Result<()> {
        clean(&r.path, opts.discard_ignored, Retiring::Merge, &r.storage)?;
        ensure!(
            git::oid(&r.path, "HEAD")? == plan.tip,
            "graft HEAD changed during merge"
        );
        land::apply(p, &plan)
    };
    if let Err(error) = apply() {
        let recovery_required = error
            .downcast_ref::<api::Fault>()
            .is_some_and(|fault| fault.code == "checkout_recovery_required");
        if !recovery_required && !destination_contains(p, &plan)? {
            crate::landing::require_no_pending_checkout(&p.repo, &plan)?;
            r.plan = None;
            registry::save(p, &r)?;
        }
        return Err(error);
    }
    if git::oid(&r.path, "HEAD")? != plan.tip {
        return keep_advanced(p, r);
    }
    clean(&r.path, opts.discard_ignored, Retiring::Merge, &r.storage)?;
    retire(p, &r.name, &r.lease, Status::Landed)
}

/// Finish a merge whose graft gained commits after the merged tip. Archiving
/// would present those commits as merged, so the graft stays active with its
/// lease and every file, and the receipt names both tips.
fn keep_advanced(p: &Project, mut r: Record) -> Result<Value> {
    let plan = r.plan.take().context("no merge is pending")?;
    let head = git::oid(&r.path, "HEAD")?;
    registry::save(p, &r)?;
    let value = json!({"action":"merge","name":r.name,"target":plan.target,"previous":plan.old,
        "commit":plan.new,"graft_tip":plan.tip,"graft_head":head,"removed":false,
        "message":format!("Merged the graft up to {}. The graft has later work at {head}, so it stays active; merge it again to land that work.", plan.tip)});
    with_attempts(p, &r, value)
}

const PENDING_MERGE: &str =
    "a merge is pending; retry merge or explicitly archive the complete repository with --discard";

fn drop_workspace(
    p: &Project,
    opts: &api::DropWorkspace,
    hooks: &mut hook::Runner<'_>,
) -> Result<Value> {
    let _lock = registry::workspace_lock(p, &opts.name)?;
    // Replay the receipt of an earlier remove. Anything else archived with
    // this lease gets authenticate's graft_archived error and restore hint.
    if let Ok(r) = registry::load_retired(p, &opts.name, &opts.lease)
        && (r.status == Status::Dropped || opts.discard)
    {
        return receipt(p, &r);
    }
    let mut r = registry::authenticate(p, &opts.name, &opts.lease)?;
    if r.retiring_to() == Some(Status::Landed) && opts.discard {
        return retire(p, &r.name, &r.lease, Status::Landed);
    }
    if r.retiring_to().is_none() {
        if opts.discard && r.placing() {
            return retire(p, &r.name, &r.lease, Status::Dropped);
        }
        registry::preflight_retire(p, &r)?;
        // A graft whose creation never finished has no hook to undo.
        if r.status == Status::Active && !r.placing() {
            if !opts.discard {
                // Refuse what the hook cannot clear before it runs, so a
                // plain remove of a graft that holds work keeps its setup.
                ensure!(r.plan.is_none(), PENDING_MERGE);
                clean_tracked(&r.path, Retiring::Remove, &r.storage)?;
                registry::require_merged(&r, &p.repo)?;
            }
            hooks.pre_archive(p, &mut r)?;
        }
        if let Some(plan) = &r.plan {
            ensure!(opts.discard, PENDING_MERGE);
            // A disconnected land can leave a plan behind while someone
            // continues editing the private checkout. Preserve those edits
            // and the undo receipt if the destination already contains it.
            if destination_contains(p, plan)? {
                crate::landing::finish_applied_checkout(&p.repo, plan)?;
                // Commits made after the merged tip were never merged.
                let head = (!unborn(&r.path)?)
                    .then(|| git::oid(&r.path, "HEAD"))
                    .transpose()?
                    .filter(|head| *head != plan.tip);
                return receipt(
                    p,
                    &registry::retire(p, &r.name, &r.lease, Status::Landed, head.as_deref())?,
                );
            }
            crate::landing::require_no_pending_checkout(&p.repo, plan)?;
            r.plan = None;
            registry::save(p, &r)?;
        }
        if !opts.discard {
            ensure!(
                r.status == Status::Active,
                "partial graft requires --discard to archive"
            );
            clean(&r.path, false, Retiring::Remove, &r.storage)?;
            registry::require_merged(&r, &p.repo)?;
        }
    }
    retire(p, &r.name, &r.lease, Status::Dropped)
}

/// Add `damaged_records` when a record file cannot be used, so the person
/// sees it in `list` and `prune`. A healthy project's output is unchanged.
fn with_damaged(p: &Project, value: &mut Value) -> Result<()> {
    let damaged = registry::damaged(p)?;
    if !damaged.is_empty() {
        value["damaged_records"] = json!(damaged);
    }
    Ok(())
}

fn unborn(path: &Path) -> Result<bool> {
    let Some(head) = git::symbolic_ref(path, "HEAD")? else {
        return Ok(false);
    };
    let out = git::raw(path, &["show-ref", "--verify", "--quiet", &head])?;
    match out.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => anyhow::bail!("could not verify unborn HEAD"),
    }
}

fn report_status(p: &Project, name: &str) -> Result<Value> {
    let r = registry::load(p, name)?;
    let mut result = summary(&r);
    registry::verify(p, &r)?;
    if r.status == Status::Active {
        result["head"] = if unborn(&r.path)? {
            Value::Null
        } else {
            json!(git::oid(&r.path, "HEAD")?)
        };
        result["branch"] = json!(
            git::symbolic_ref(&r.path, "HEAD")?
                .and_then(|s| s.strip_prefix("refs/heads/").map(str::to_owned))
        );
        // Copies made at add are not changes until the graft changes them.
        let changes =
            include::without_unchanged_copies(&r.path, &r.storage, git::changes(&r.path)?);
        result["change_count"] = json!(changes.len());
        result["changes"] = json!(changes.iter().take(200).collect::<Vec<_>>());
    }
    with_attempts(p, &r, result)
}

/// The checkout a command runs from: the primary, or the active graft it
/// was invoked inside.
fn invoking(p: &Project, location: Option<registry::Location>, repo: &Path) -> Result<PathBuf> {
    let Some(location) = location else {
        return Ok(p.repo.clone());
    };
    // A child can start from a commit that only its parent workspace owns.
    let parent = registry::load(p, &location.name)?;
    ensure!(
        parent.id == location.id
            && parent.status == Status::Active
            && dunce::canonicalize(repo)?.starts_with(&parent.path),
        "the invoking graft moved or is no longer active"
    );
    registry::verify(p, &parent)?;
    Ok(parent.path)
}

/// How long a waiting `turn take` pauses before it asks again.
const TURN_POLL: Duration = Duration::from_secs(1);

/// After a `turn take` replied `reply`, how long to pause before asking
/// again, or `None` when the turn was granted or the wait ended at `deadline`.
pub(crate) fn turn_pause(reply: &Value, deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    (reply["granted"] == false && !left.is_zero()).then(|| left.min(TURN_POLL))
}

/// `turn take --wait`: ask until the turn is granted or the wait runs out.
/// The Git version and the project are found once. Each ask then opens the
/// project as a whole command does: between asks this process holds no
/// project, graft or build lock, and the next ask finds a graft archived
/// meanwhile, or a replaced lease, as any other command would.
fn wait_for_turn(repo: &Path, mut args: api::TurnTake, hooks: hook::Policy) -> Result<Value> {
    args.check()?;
    let deadline = Instant::now() + Duration::from_secs(args.wait);
    args.wait = 0;
    let found = find(repo, &Command::TurnTake(args.clone()))?;
    loop {
        let reply = run_found(
            repo,
            found.clone(),
            Command::TurnTake(args.clone()),
            hooks,
            Pushing::Disabled,
        )?;
        match turn_pause(&reply, deadline) {
            Some(pause) => std::thread::sleep(pause),
            None => return Ok(reply),
        }
    }
}

pub fn run(repo: &Path, command: Command) -> Result<Value> {
    // The command line is the person's own, so it always runs their hooks
    // and may push.
    run_with(repo, command, hook::Policy::Run, Pushing::Allowed)
}

/// [`run`] for a caller that says whether it may run the project's hooks,
/// and whether it may push through the remotes a graft gets.
pub fn run_with(
    repo: &Path,
    command: Command,
    hooks: hook::Policy,
    pushing: Pushing,
) -> Result<Value> {
    match command {
        Command::TurnTake(args) if args.wait > 0 => wait_for_turn(repo, args, hooks),
        command => run_once(repo, command, hooks, pushing),
    }
}

/// What every command finds first: the Git version, and where the project
/// and the invoking graft are.
#[derive(Clone)]
pub(crate) struct Found {
    version: String,
    supported: bool,
    location: Option<registry::Location>,
    paths: crate::registry::Paths,
}

/// Find what `command` needs before it opens the project. Refuses a Git older
/// than Graft supports, except to `doctor`, which reports it. A version
/// failure is reported before a failure to find the project.
pub(crate) fn find(repo: &Path, command: &Command) -> Result<Found> {
    // Resolving the project is read-only, so it runs beside the version check.
    let (version, located) = std::thread::scope(|scope| {
        let version = scope.spawn(|| git::text(repo, &["--version"]));
        let located = registry::discover(repo).and_then(|location| {
            let primary = location
                .as_ref()
                .map_or(repo, |where_| where_.project.as_path())
                .to_path_buf();
            Ok((location, Project::paths(&primary)?))
        });
        (joined(version), located)
    });
    let version = version?;
    crate::trace::mark("git version and project paths");
    let supported = git::supported_version(&version);
    if !supported && !matches!(command, Command::Doctor) {
        return Err(api::fault(
            "unsupported_git",
            format!(
                "Graft requires Git {} or newer. Found {version}",
                git::MINIMUM_VERSION
            ),
            Value::Null,
        ));
    }
    let (location, paths) = located?;
    Ok(Found {
        version,
        supported,
        location,
        paths,
    })
}

fn run_once(repo: &Path, command: Command, hooks: hook::Policy, pushing: Pushing) -> Result<Value> {
    let found = find(repo, &command)?;
    run_found(repo, found, command, hooks, pushing)
}

/// Run `command` with what [`find`] found for it.
/// `hooks` says whether the caller may run the project's hooks, and
/// `pushing` whether it may push through the remotes a graft gets.
pub(crate) fn run_found(
    repo: &Path,
    found: Found,
    command: Command,
    hooks: hook::Policy,
    pushing: Pushing,
) -> Result<Value> {
    let Found {
        version,
        supported,
        location,
        paths,
    } = found;
    if matches!(command, Command::ListManagers) {
        return match Project::open_existing_at(paths)? {
            Some((project, _lock)) => crate::package_manager::list(&project),
            None => Ok(json!({"managers": []})),
        };
    }
    // Turns belong to holders, not to grafts, and may be kept in another
    // project, so they authenticate no graft and open their own project.
    // Listing creates no state in a project that has none.
    match command {
        Command::TurnList => return turns::list(&paths.0.clone(), paths),
        Command::TurnTake(args) => return turns::take(&paths.0.clone(), paths, &args, hooks),
        Command::TurnRelease(args) => return turns::release(&paths.0.clone(), paths, &args),
        _ => {}
    }
    // Hooks live in the primary checkout's configuration files, so listing
    // them needs no Graft state.
    if matches!(command, Command::ListHooks) {
        let (primary, _, state, _) = paths;
        return hook::list(&primary, &state);
    }
    let (p, lock) = Project::open_at(paths, !matches!(&command, Command::Spawn(_)))?;
    let p = &p;
    // Commands that only read, or change one graft's record or a turn, run
    // beside builds. Adds share the build lock; every other command excludes
    // builds.
    let builds = match &command {
        Command::Doctor
        | Command::List
        | Command::LocalFiles
        | Command::ListManagers
        | Command::Status(_)
        | Command::Env(_)
        | Command::Heartbeat(_)
        | Command::Lock { .. }
        | Command::Unlock(_)
        | Command::TrustHooks => None,
        Command::Gc(args) if !args.apply => None,
        Command::Spawn(_) => Some(p.lock_builds(true)?),
        _ => Some(p.lock_builds(false)?),
    };
    crate::trace::mark("open project");
    match &command {
        Command::Doctor
        | Command::List
        | Command::LocalFiles
        | Command::ListManagers
        | Command::Status(_)
        | Command::Env(_) => {}
        Command::Gc(args) if !args.apply => {}
        _ => {
            p.require_private_state()?;
            p.require_private_workspaces()?;
        }
    }
    let (name, lease) = match &command {
        Command::Doctor => {
            let source = storage::doctor(&p.state)?;
            let native = source["native_clone"].as_bool().unwrap_or(false);
            return Ok(json!({"common_dir":p.common,
            "source_storage":source,"file_sharing":storage::file_sharing(native),
            "history_sharing":storage::history_sharing(&p.state, native),
            "git":version,"git_minimum":git::MINIMUM_VERSION,"git_supported":supported}));
        }
        Command::List => {
            let mut listed =
                json!({"workspaces":registry::list(p)?,"recoverable":registry::recoverable(p)?});
            with_damaged(p, &mut listed)?;
            return Ok(listed);
        }
        Command::ListManagers => return crate::package_manager::list(p),
        Command::TurnList | Command::TurnTake(_) | Command::TurnRelease(_) => {
            unreachable!("turns return before the project opens")
        }
        Command::UnregisterManager(a) => return crate::package_manager::unregister(p, &a.manager),
        Command::ListHooks => unreachable!("handled above"),
        Command::TrustHooks => return hook::trust::trust(p),
        Command::RegisterManager(a) => match (&a.workspace, &a.lease) {
            (Some(name), Some(lease)) => {
                let source = registry::authenticate(p, name, lease)?;
                ensure!(
                    source.status == Status::Active && source.plan.is_none(),
                    "graft has a pending operation"
                );
                let _workspace_lock = registry::workspace_lock(p, name)?;
                registry::verify(p, &source)?;
                return crate::package_manager::register(p, &source.path, a);
            }
            (None, None) => return crate::package_manager::register(p, &p.repo, a),
            _ => anyhow::bail!("--graft and --lease must be given together"),
        },
        Command::Status(a) => return report_status(p, &a.name),
        Command::LocalFiles => {
            let config = Config::read(&p.repo)?;
            return include::overview(&invoking(p, location, repo)?, &config);
        }
        Command::Spawn(_) => {
            let Command::Spawn(args) = command else {
                unreachable!()
            };
            let origin = invoking(p, location, repo)?;
            let builds = builds.context("adding a graft requires the build lock")?;
            return spawn(p, &origin, args, &lock, builds, hooks, pushing);
        }
        Command::SeedLayer(a) => return engine::seed_layer(p, a),
        Command::Drop(a) => {
            let mut hooks = hook::Runner::new(hooks, &lock, builds);
            let dropped = drop_workspace(p, a, &mut hooks)?;
            return Ok(hooks.report(dropped));
        }
        Command::Land(a) => {
            let mut hooks = hook::Runner::new(hooks, &lock, builds);
            let landed = land_workspace(p, a, &mut hooks)?;
            return Ok(hooks.report(landed));
        }
        Command::Restore(a) => {
            // Read first, so a configuration Graft cannot read refuses the
            // restore instead of reopening a graft whose hooks never ran.
            let mut hooks = hook::Runner::new(hooks, &lock, builds);
            let post_restore = hooks.hooks(p, hook::Event::PostRestore)?;
            let config = Config::read(&p.repo)?;
            let untrusted = hook::trust::untrusted_remotes(p, &config)?;
            let (r, reopened) = registry::restore_locked(p, &a.name, &a.lease)?;
            let mut reply = with_leftovers(p, &r)?;
            // An archive made before the configuration named remotes gets them now.
            if reopened.is_some() && r.status == Status::Active {
                let history = history_of(&r.request);
                with_remotes(p, &r, &config, &untrusted, history, pushing, &mut reply);
            }
            // A repeated restore returns the graft without running the hooks
            // again. One this call reopened keeps the graft's lock for them.
            let (false, Some(_graft_lock), Status::Active) =
                (post_restore.is_empty(), reopened, r.status)
            else {
                return Ok(reply);
            };
            hooks.after(p, &post_restore, &r)?;
            return Ok(hooks.report(reply));
        }
        Command::UndoLand(a) => {
            let r = registry::load_retired(p, &a.name, &a.lease)?;
            let plan = r.plan.as_ref().context("graft was not archived by merge")?;
            let mut value = land::undo(p, plan, &a.lease)?;
            value["name"] = json!(r.name);
            return Ok(value);
        }
        Command::Lock {
            workspace: a,
            reason,
        } => {
            return Ok(summary(&registry::lock(
                p,
                &a.name,
                &a.lease,
                reason.as_deref(),
            )?));
        }
        Command::Unlock(a) => return Ok(summary(&registry::unlock(p, &a.name, &a.lease)?)),
        Command::Move { workspace: a, path } => {
            return Ok(summary(&registry::move_workspace(
                p, &a.name, &a.lease, path,
            )?));
        }
        Command::Repair { workspace: a, path } => {
            return Ok(summary(&registry::repair(p, &a.name, &a.lease, path)?));
        }
        Command::Gc(a) => {
            let mut hooks = hook::Runner::new(hooks, &lock, builds);
            // Refuse before archiving anything when the pre-archive hooks
            // need trust they do not have.
            if a.apply {
                hooks.hooks(p, hook::Event::PreArchive)?;
            }
            // The archives to purge are found before prune archives
            // anything, so it never purges a graft it has just archived.
            let purge = a
                .purge_archives
                .map(|days| registry::archives_to_purge(p, days))
                .transpose()?;
            let retained = registry::prune_with(p, a.apply, &mut |r| hooks.before_prune(p, r))?;
            let cache = if !a.purge_cache {
                json!({"requested":false,"purged":false})
            } else {
                engine::gc::purge_caches(p, a.apply)?
            };
            let mut failed = None;
            let archives = match purge {
                None => json!({"requested": false, "purged": []}),
                Some(purge) => {
                    let mut archives = json!({
                        "requested": true,
                        "older_than_days": purge.days(),
                        "purged": purge.report(),
                    });
                    if a.apply {
                        let mut purged = Vec::new();
                        failed = registry::purge_archives(p, &purge, &mut purged).err();
                        archives["purged"] = json!(purged);
                    }
                    archives
                }
            };
            let mut pruned =
                json!({"applied":a.apply,"retained":retained,"cache":cache,"archives":archives});
            with_damaged(p, &mut pruned)?;
            // Prune has archived grafts by now, so a purge that fails carries
            // the whole reply, with the archives it did delete.
            if let Some(error) = failed {
                return Err(api::fault(
                    "purge_failed",
                    format!(
                        "The purge of archives did not complete: {error:#}. details has the prune reply, and details.archives.purged lists the archives deleted before the failure"
                    ),
                    pruned,
                ));
            }
            return Ok(pruned);
        }
        Command::Env(a) => {
            let r = registry::load(p, &a.name)?;
            registry::verify(p, &r)?;
            return Ok(Runtime::new(&r.path, &p.state, r.port, &r.id)?.environment());
        }
        Command::Exec(a) => (&a.name, &a.lease),
        Command::AddDep(a) => (&a.name, &a.lease),
        Command::Heartbeat(a) => (&a.name, &a.lease),
        Command::Layer(_, a) => (&a.name, &a.lease),
        Command::Push(a) => (&a.name, &a.lease),
    };
    let mut r = registry::authenticate(p, name, lease)?;
    ensure!(
        r.status == Status::Active && r.plan.is_none(),
        "graft has a pending operation"
    );
    // Exec shares the graft lock with the process it starts, so a process
    // left running in the background still blocks merge, remove and prune.
    // Heartbeats share it too, so an agent can renew its lease meanwhile.
    let (_workspace_lock, exec_lock) = match command {
        Command::Exec(_) | Command::Heartbeat(_) => (None, Some(registry::exec_lock(p, name)?)),
        _ => (Some(registry::workspace_lock(p, name)?), None),
    };
    registry::verify(p, &r)?;
    let now = crate::registry::now();
    if r.heartbeat != now {
        r.heartbeat = now;
        registry::save(p, &r)?;
    }
    match command {
        Command::Heartbeat(_) => Ok(summary(&r)),
        Command::Layer(action, a) => layer::run(&r.path, &p.state, action, &a.key, a.copy),
        Command::Exec(a) => {
            ensure!(!a.argv.is_empty(), "program and arguments are required");
            let profile = crate::package_manager::execution(p, a.manager.as_deref(), &a.argv[0])?;
            crate::cache::prepare_package_roots(p)?;
            let runtime = Runtime::new(&r.path, &p.state, r.port, &r.id)?;
            runtime.prepare_private_directory()?;
            drop((lock, builds));
            let hold = exec_lock
                .as_ref()
                .context("exec requires the shared graft lock")?;
            runtime.execute_registered(&a.argv, &profile, hold)
        }
        Command::AddDep(a) => {
            let attached = engine::check_dependency(&r.path, &r.storage["dependencies"], &a)?;
            crate::cache::prepare_package_roots(p)?;
            let runtime = Runtime::new(&r.path, &p.state, r.port, &r.id)?;
            runtime.prepare_private_directory()?;
            drop((lock, builds));
            runtime.install(&a.manager, &a.packages, a.offline, attached)
        }
        Command::Push(a) => push::run(p, &r, &a, (lock, builds)),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_changes_name_each_path_with_its_status() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let repo = temp.path();
        git::bytes(repo, &["init", "-q", "--template=", "-b", "main"])?;
        for (key, value) in [
            ("user.name", "Clean Fixture"),
            ("user.email", "clean@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "disabled-hooks"),
        ] {
            git::bytes(repo, &["config", key, value])?;
        }
        std::fs::write(repo.join(".gitignore"), "build/\n")?;
        std::fs::write(repo.join("tracked.txt"), "tracked\n")?;
        std::fs::write(repo.join("old.txt"), "old\n")?;
        git::bytes(repo, &["add", "-A"])?;
        git::bytes(repo, &["commit", "-qm", "base"])?;
        clean(repo, false, Retiring::Remove, &Value::Null)?;

        std::fs::write(repo.join("new.txt"), "new\n")?;
        git::bytes(repo, &["add", "new.txt"])?;
        git::bytes(repo, &["rm", "-q", "old.txt"])?;
        let error =
            api::error_json(&clean(repo, false, Retiring::Merge, &Value::Null).unwrap_err());
        assert_eq!(
            error["details"],
            json!({"paths": [
                {"path": "new.txt", "status": "added"},
                {"path": "old.txt", "status": "deleted"},
            ], "count": 2}),
            "a merge has no discard option to describe"
        );
        git::bytes(repo, &["reset", "-q", "--", "new.txt", "old.txt"])?;
        std::fs::remove_file(repo.join("new.txt"))?;
        git::bytes(repo, &["checkout", "-q", "--", "old.txt"])?;

        std::fs::write(repo.join("tracked.txt"), "edited\n")?;
        std::fs::write(repo.join("notes.txt"), "untracked\n")?;
        std::fs::create_dir(repo.join("build"))?;
        std::fs::write(repo.join("build").join("out"), "ignored\n")?;
        let error =
            api::error_json(&clean(repo, false, Retiring::Remove, &Value::Null).unwrap_err());
        assert_eq!(error["code"], "local_changes", "{error}");
        assert_eq!(
            error["details"],
            json!({"paths": [
                {"path": "tracked.txt", "status": "modified"},
                {"path": "notes.txt", "status": "untracked"},
                {"path": "build/", "status": "ignored"},
            ], "count": 3, "discard_archives": true})
        );
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|message| message.contains("remove --discard")),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("tracked.txt"))?,
            "edited\n"
        );

        // A merge allowed to archive ignored files still names the others.
        let error = api::error_json(&clean(repo, true, Retiring::Merge, &Value::Null).unwrap_err());
        assert_eq!(error["details"]["count"], 2, "{error}");
        std::fs::write(repo.join("tracked.txt"), "tracked\n")?;
        std::fs::remove_file(repo.join("notes.txt"))?;
        clean(repo, true, Retiring::Merge, &Value::Null)?;
        Ok(())
    }
}
