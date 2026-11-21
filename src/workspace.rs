//! Graft creation, inspection, integration and recoverable retirement.
mod land;
mod objects;
mod registry;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::{api, engine, git, layer, registry::Project, runtime::Runtime, storage};
pub(crate) use objects::{
    HISTORY_CACHE_DIR, HISTORY_CACHE_MARKER, history_cache_root, verify_history_cache_generation,
};
pub(crate) use registry::has_live;
use registry::{Record, Status};

pub(crate) use api::Spawn;

/// One operation on a project's grafts. Built from the command line (see
/// `cli`) or from an MCP tool call, then run by [`run`].
#[derive(Debug)]
pub enum Command {
    Spawn(Spawn),
    List,
    Status(api::Name),
    Doctor,
    Heartbeat(api::Owned),
    Env(api::Name),
    Exec(api::Exec),
    RegisterManager(api::RegisterManager),
    ListManagers,
    UnregisterManager(api::ManagerName),
    AddDep(api::AddDep),
    SeedLayer(api::SeedLayer),
    /// Save, restore or forget; the string is the action.
    Layer(&'static str, api::Layer),
    Land(api::Land),
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

fn spawn(p: &Project, origin: &Path, mut args: Spawn) -> Result<Value> {
    if let Some(id) = &mut args.request_id {
        *id = uuid::Uuid::parse_str(id)
            .context("request ID must be a UUID")?
            .to_string();
    }
    crate::registry::validate_name(&args.name)?;
    let mut request = serde_json::to_value(&args)?;
    let previous = args
        .request_id
        .as_ref()
        .and_then(|id| registry::authenticate(p, &args.name, id).ok())
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
            // A retry uses the policy at reservation, even after reconfiguration.
            // Older requests without the marker used isolated history.
            let history = match &previous {
                Some(record) if record.request["project_local_history"] == true => {
                    api::History::Local
                }
                Some(record) if record.request["project_history"] == "tip" => api::History::Tip,
                Some(_) => api::History::Isolated,
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
            return Ok(with_lease(&current));
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
    let mut r = registry::reserve(
        p,
        &args.name,
        args.request_id.as_deref(),
        base.as_deref(),
        branch.as_deref(),
        request,
    )?;
    if r.status == Status::Active {
        registry::verify(p, &r)?;
        return Ok(with_lease(&r));
    }
    crate::trace::mark("reserve");
    let build = (|| -> Result<()> {
        registry::verify(p, &r)?;
        crate::trace::mark("verify reservation");
        let (objects, prepared_checkout) = objects::initialize(
            &source,
            &r.path,
            &p.state,
            base.as_deref(),
            branch.as_deref(),
            history,
            proof,
        )?;
        crate::trace::mark("initialize objects");
        let mut checkout = match (prepared_checkout, &base) {
            (Some(checkout), _) => checkout,
            (None, Some(oid)) if !args.no_checkout => {
                let materialize = if history != api::History::Isolated {
                    storage::materialize_local
                } else {
                    storage::materialize
                };
                materialize(&r.path, &p.state, oid, &args.sparse, origin)?
            }
            _ => json!({"backend":"empty","files":0,"logical_bytes":0}),
        };
        checkout["objects"] = objects;
        crate::trace::mark("checkout");
        if base.is_some() && !args.no_checkout && args.sparse.is_empty() && !args.no_deps {
            if let Some(deps) = layer::inherit_shared(&r.path, &p.state, &p.repo)? {
                checkout["dependencies"] = deps;
            }
            let registered = crate::package_manager::attach(p, &r.path);
            if !registered.is_empty() {
                checkout["registered_dependencies"] = json!(registered);
            }
        }
        r.storage = checkout;
        if args.lock {
            r.locked = Some(args.reason.unwrap_or_default());
        }
        let finished = registry::finish(p, &mut r);
        crate::trace::mark("finish");
        finished
    })();
    if let Err(error) = build {
        return Err(api::fault(
            "spawn_incomplete",
            format!("Creating the graft stopped: {error:#}"),
            json!({"name":r.name,"lease":r.lease,"path":r.path,"recovery":"Inspect the reserved path. Run remove with --discard to archive the partial graft."}),
        ));
    }
    Ok(with_lease(&r))
}

fn clean(path: &Path, allow_ignored: bool) -> Result<()> {
    git::check_idle(path)?;
    let changes = git::changes(path)?;
    ensure!(
        changes.iter().all(|c| allow_ignored && c.status == "!!"),
        "graft has local changes or ignored files; its files and index have been retained"
    );
    Ok(())
}

fn receipt(r: &Record) -> Value {
    match &r.plan {
        Some(plan) => {
            json!({"action":"merge","name":r.name,"target":plan.target,"previous":plan.old,
            "commit":plan.new,"graft_tip":plan.tip,"removed":true,"recovery_lease":r.lease})
        }
        None => json!({"action":"remove","name":r.name,"removed":true,"recovery_lease":r.lease}),
    }
}

fn retire(p: &Project, name: &str, lease: &str, status: Status) -> Result<Value> {
    Ok(receipt(&registry::retire(p, name, lease, status)?))
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

fn land_workspace(p: &Project, opts: &api::Land) -> Result<Value> {
    let _lock = registry::workspace_lock(p, &opts.name)?;
    if let Ok(r) = registry::load_retired(p, &opts.name, &opts.lease) {
        ensure!(
            r.plan.as_ref().is_some_and(|plan| plan.target == opts.into),
            "retirement does not match requested merge"
        );
        return Ok(receipt(&r));
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
    registry::preflight_retire(p, &r)?;
    if let Some(plan) = &r.plan {
        ensure!(plan.target == opts.into, "retry merge into {}", plan.target);
        if destination_contains(p, plan)? {
            crate::landing::finish_applied_checkout(&p.repo, plan)?;
        }
    }
    if !opts.discard_ignored {
        ensure!(
            !git::changes(&r.path)?.iter().any(|c| c.status == "!!"),
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
        clean(&r.path, opts.discard_ignored)?;
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
    clean(&r.path, opts.discard_ignored)?;
    ensure!(
        git::oid(&r.path, "HEAD")? == plan.tip,
        "graft advanced; archiving remains pending"
    );
    retire(p, &r.name, &r.lease, Status::Landed)
}

fn drop_workspace(p: &Project, opts: &api::DropWorkspace) -> Result<Value> {
    let _lock = registry::workspace_lock(p, &opts.name)?;
    if let Ok(r) = registry::load_retired(p, &opts.name, &opts.lease) {
        ensure!(
            r.status == Status::Dropped || opts.discard,
            "graft was archived by merge"
        );
        return Ok(receipt(&r));
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
        if let Some(plan) = &r.plan {
            ensure!(
                opts.discard,
                "a merge is pending; retry merge or explicitly archive the complete repository with --discard"
            );
            // A disconnected land can leave a plan behind while someone
            // continues editing the private checkout. Preserve those edits
            // and the undo receipt if the destination already contains it.
            if destination_contains(p, plan)? {
                crate::landing::finish_applied_checkout(&p.repo, plan)?;
                return retire(p, &r.name, &r.lease, Status::Landed);
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
            clean(&r.path, false)?;
            if let Some(base) = &r.base {
                ensure!(
                    git::oid(&r.path, "HEAD")? == *base,
                    "merge the graft's commits or archive it explicitly with --discard"
                );
            } else {
                ensure!(
                    unborn(&r.path)?,
                    "merge the graft's commits or archive it explicitly with --discard"
                );
            }
        }
    }
    retire(p, &r.name, &r.lease, Status::Dropped)
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
        let changes = git::changes(&r.path)?;
        result["change_count"] = json!(changes.len());
        result["changes"] = json!(changes.iter().take(200).collect::<Vec<_>>());
    }
    Ok(result)
}

pub fn run(repo: &Path, command: Command) -> Result<Value> {
    // Resolving the project is read-only, so it runs beside the version check;
    // a version failure is still reported first.
    let (version, located) = std::thread::scope(|scope| {
        let version = scope.spawn(|| git::text(repo, &["--version"]));
        let located = registry::discover(repo).map(|location| {
            let primary = location
                .as_ref()
                .map_or(repo, |where_| where_.project.as_path())
                .to_path_buf();
            let paths = Project::paths(&primary);
            (location, primary, paths)
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
    let (location, _, paths) = located?;
    if matches!(command, Command::ListManagers) {
        return match Project::open_existing_at(paths?)? {
            Some((project, _lock)) => crate::package_manager::list(&project),
            None => Ok(json!({"managers": []})),
        };
    }
    let (p, lock) = Project::open_at(paths?, !matches!(&command, Command::Spawn(_)))?;
    let p = &p;
    crate::trace::mark("open project");
    match &command {
        Command::Doctor
        | Command::List
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
            "source_storage":source,"file_sharing":storage::file_sharing(&p.repo, native),
            "git":version,"git_minimum":git::MINIMUM_VERSION,"git_supported":supported}));
        }
        Command::List => {
            return Ok(
                json!({"workspaces":registry::list(p)?,"recoverable":registry::recoverable(p)?}),
            );
        }
        Command::ListManagers => return crate::package_manager::list(p),
        Command::UnregisterManager(a) => return crate::package_manager::unregister(p, &a.manager),
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
        Command::Spawn(_) => {
            let Command::Spawn(args) = command else {
                unreachable!()
            };
            let origin = if let Some(location) = location {
                // A child can start from a commit that only its parent workspace owns.
                let parent = registry::load(p, &location.name)?;
                ensure!(
                    parent.id == location.id
                        && parent.status == Status::Active
                        && dunce::canonicalize(repo)?.starts_with(&parent.path),
                    "the invoking graft moved or is no longer active"
                );
                registry::verify(p, &parent)?;
                parent.path
            } else {
                p.repo.clone()
            };
            return spawn(p, &origin, args);
        }
        Command::SeedLayer(a) => return engine::seed_layer(p, a),
        Command::Drop(a) => return drop_workspace(p, a),
        Command::Land(a) => return land_workspace(p, a),
        Command::Restore(a) => return Ok(with_lease(&registry::restore(p, &a.name, &a.lease)?)),
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
            let retained = registry::prune(p, a.apply)?;
            let cache = if !a.purge_cache {
                json!({"requested":false,"purged":false})
            } else {
                engine::gc::purge_caches(p, a.apply)?
            };
            return Ok(json!({"applied":a.apply,"retained":retained,"cache":cache}));
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
            drop(lock);
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
            drop(lock);
            runtime.install(&a.manager, &a.packages, a.offline, attached)
        }
        _ => unreachable!(),
    }
}
