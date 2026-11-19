//! Workspace creation, inspection, integration and recoverable retirement.
mod land;
mod objects;
mod registry;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::{api, engine, git, layer, registry::Project, runtime::Runtime, storage};
pub(crate) use objects::{
    HISTORY_CACHE_DIR, HISTORY_CACHE_MARKER, history_cache_root, verify_history_cache_generation,
};
pub(crate) use registry::has_live;
use registry::{Record, Status};

pub(crate) use api::Spawn;

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create an independent repository from a local commit.
    #[command(visible_alias = "add")]
    Spawn(Spawn),
    /// Show active and recoverable independent repositories.
    List,
    /// Inspect the workspace, including its current HEAD and files.
    Status(api::Name),
    /// Check local Git and source-sharing capabilities.
    Doctor,
    /// Renew the workspace lease.
    Heartbeat(api::Owned),
    /// Show this workspace's runtime and cache paths.
    Env(api::Name),
    /// Execute a program in the workspace.
    Exec(api::Exec),
    /// Register a package manager for cache and dependency reuse.
    RegisterManager(api::RegisterManager),
    /// List registered package managers.
    ListManagers,
    /// Remove a registered package manager.
    UnregisterManager(api::ManagerName),
    /// Add dependencies or perform a locked install.
    AddDep(api::AddDep),
    /// Seed installed dependencies from the primary checkout.
    SeedLayer(api::SeedLayer),
    /// Save, restore or forget an installed dependency layer.
    Layer(api::Layer),
    /// Transfer committed work to a local branch and archive the repository.
    Land(api::Land),
    /// Archive the complete independent repository and its files.
    #[command(visible_alias = "remove")]
    Drop(api::DropWorkspace),
    /// Reopen an archived repository.
    Restore(api::Owned),
    /// Append a reversal commit to the exact landed destination.
    UndoLand(api::Owned),
    /// Protect against moves, retirement and cleanup.
    Lock {
        #[command(flatten)]
        workspace: api::Owned,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Release an explicit workspace lock.
    Unlock(api::Owned),
    /// Move a workspace to an available path on its volume.
    Move {
        #[command(flatten)]
        workspace: api::Owned,
        path: PathBuf,
    },
    /// Reconnect a workspace that was moved using the filesystem.
    Repair {
        #[command(flatten)]
        workspace: api::Owned,
        path: PathBuf,
    },
    /// Preview or archive eligible expired workspaces.
    #[command(visible_alias = "prune")]
    Gc(api::Gc),
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

fn project_uses_local_history(project: &Project) -> Result<bool> {
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
        (Some(1), b"") | (Some(0), b"isolated\0") => Ok(false),
        (Some(0), b"local\0") => Ok(true),
        (Some(0), _) => anyhow::bail!("graft.history must be local or isolated"),
        _ => anyhow::bail!("could not read the project's graft.history setting"),
    }
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
    let local = match args.history {
        Some(mode) => mode == api::History::Local,
        None => {
            // A retry uses the policy at reservation, even after reconfiguration.
            // Older requests without the marker used isolated history.
            let local = match &previous {
                Some(record) => {
                    record.request.get("project_local_history") == Some(&Value::Bool(true))
                }
                None => project_uses_local_history(p)?,
            };
            if local {
                request["project_local_history"] = Value::Bool(true);
            }
            local
        }
    };
    let mut reserved_base = None;
    if let Some(current) = previous {
        ensure!(
            current.request == request,
            "request ID was used with different spawn options"
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
    if let Some(name) = args.orphan.as_ref().or(args.branch.as_ref()) {
        ensure!(
            !name.starts_with('-') && !name.starts_with("refs/") && name != "HEAD",
            "invalid branch name"
        );
        git::bytes(
            &p.repo,
            &["check-ref-format", &format!("refs/heads/{name}")],
        )?;
    }
    let base = if args.orphan.is_some() {
        None
    } else {
        let oid = match reserved_base {
            Some(base) => base,
            None => git::oid(origin, &args.base)?,
        };
        storage::validate_sparse(origin, &oid, &args.sparse)?;
        Some(oid)
    };
    let source = objects::preflight(origin)?;
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
    let build = (|| -> Result<()> {
        registry::verify(p, &r)?;
        let initialize = if local {
            objects::initialize_local
        } else {
            objects::initialize
        };
        let objects = initialize(
            &source,
            &r.path,
            &p.state,
            base.as_deref(),
            branch.as_deref(),
        )?;
        let mut checkout = match &base {
            Some(oid) if !args.no_checkout => {
                let materialize = if local {
                    storage::materialize_local
                } else {
                    storage::materialize
                };
                materialize(&r.path, &p.state, oid, &args.sparse, origin)?
            }
            _ => json!({"backend":"empty","files":0,"logical_bytes":0}),
        };
        checkout["objects"] = objects;
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
        registry::finish(p, &mut r)
    })();
    if let Err(error) = build {
        return Err(api::fault(
            "spawn_incomplete",
            format!("Workspace creation stopped: {error:#}"),
            json!({"name":r.name,"lease":r.lease,"path":r.path,"recovery":"Inspect the reserved path. Drop with --discard to archive the partial repository."}),
        ));
    }
    Ok(with_lease(&r))
}

fn clean(path: &Path, allow_ignored: bool) -> Result<()> {
    git::check_idle(path)?;
    let changes = git::changes(path)?;
    ensure!(
        changes.iter().all(|c| allow_ignored && c.status == "!!"),
        "workspace has local changes or ignored files; its files and index have been retained"
    );
    Ok(())
}

fn receipt(r: &Record) -> Value {
    match &r.plan {
        Some(plan) => {
            json!({"action":"land","name":r.name,"target":plan.target,"previous":plan.old,
            "commit":plan.new,"workspace_tip":plan.tip,"removed":true,"recovery_lease":r.lease})
        }
        None => json!({"action":"drop","name":r.name,"removed":true,"recovery_lease":r.lease}),
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
        "could not inspect the landing result: {}",
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
            "could not verify the recorded landing on {}: {}",
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
            "retirement does not match requested land"
        );
        return Ok(receipt(&r));
    }
    let mut r = registry::authenticate(p, &opts.name, &opts.lease)?;
    if r.retiring_to().is_some() {
        ensure!(
            r.plan.as_ref().is_some_and(|plan| plan.target == opts.into),
            "retry the recorded landing"
        );
        return retire(p, &r.name, &r.lease, Status::Landed);
    }
    ensure!(
        r.status == Status::Active,
        "workspace creation is incomplete"
    );
    registry::preflight_retire(p, &r)?;
    if let Some(plan) = &r.plan {
        ensure!(plan.target == opts.into, "retry land into {}", plan.target);
        if destination_contains(p, plan)? {
            crate::landing::finish_applied_checkout(&p.repo, plan)?;
        }
    }
    if !opts.discard_ignored {
        ensure!(
            !git::changes(&r.path)?.iter().any(|c| c.status == "!!"),
            "workspace has ignored files; use --discard-ignored to retain them in its archive"
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
            "workspace HEAD changed during land"
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
        "workspace advanced; retirement remains pending"
    );
    retire(p, &r.name, &r.lease, Status::Landed)
}

fn drop_workspace(p: &Project, opts: &api::DropWorkspace) -> Result<Value> {
    let _lock = registry::workspace_lock(p, &opts.name)?;
    if let Ok(r) = registry::load_retired(p, &opts.name, &opts.lease) {
        ensure!(
            r.status == Status::Dropped || opts.discard,
            "workspace was retired by landing"
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
                "a landing is pending; retry land or explicitly archive the complete repository with --discard"
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
                "partial workspace requires --discard to archive"
            );
            clean(&r.path, false)?;
            if let Some(base) = &r.base {
                ensure!(
                    git::oid(&r.path, "HEAD")? == *base,
                    "land the workspace commits or explicitly archive with --discard"
                );
            } else {
                ensure!(
                    unborn(&r.path)?,
                    "land the workspace commits or explicitly archive with --discard"
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
    let version = git::text(repo, &["--version"])?;
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
    let location = registry::discover(repo)?;
    let primary = location
        .as_ref()
        .map_or(repo, |where_| where_.project.as_path());
    if matches!(command, Command::ListManagers) {
        return match Project::open_existing(primary)? {
            Some((project, _lock)) => crate::package_manager::list(&project),
            None => Ok(json!({"managers": []})),
        };
    }
    let (p, lock) = if matches!(&command, Command::Spawn(_)) {
        Project::open_for_spawn(primary)?
    } else {
        Project::open(primary)?
    };
    let p = &p;
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
            return Ok(json!({"common_dir":p.common,
            "source_storage":storage::doctor(&p.state)?,"git":version,
            "git_minimum":git::MINIMUM_VERSION,"git_supported":supported}));
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
                    "workspace has a pending operation"
                );
                let _workspace_lock = registry::workspace_lock(p, name)?;
                registry::verify(p, &source)?;
                return crate::package_manager::register(p, &source.path, a);
            }
            (None, None) => return crate::package_manager::register(p, &p.repo, a),
            _ => anyhow::bail!("workspace and lease must be supplied together"),
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
                    "the invoking workspace moved or is no longer active"
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
            let plan = r
                .plan
                .as_ref()
                .context("workspace was not retired by landing")?;
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
        Command::Layer(a) => (&a.name, &a.lease),
    };
    let mut r = registry::authenticate(p, name, lease)?;
    ensure!(
        r.status == Status::Active && r.plan.is_none(),
        "workspace has a pending operation"
    );
    let _workspace_lock = registry::workspace_lock(p, name)?;
    registry::verify(p, &r)?;
    let now = crate::registry::now();
    if r.heartbeat != now {
        r.heartbeat = now;
        registry::save(p, &r)?;
    }
    match command {
        Command::Heartbeat(_) => Ok(summary(&r)),
        Command::Layer(a) => layer::run(&r.path, &p.state, &a.action, &a.key, a.copy),
        Command::Exec(a) => {
            ensure!(!a.argv.is_empty(), "program and arguments are required");
            let profile = crate::package_manager::execution(p, a.manager.as_deref(), &a.argv[0])?;
            crate::cache::prepare_package_roots(p)?;
            let runtime = Runtime::new(&r.path, &p.state, r.port, &r.id)?;
            runtime.prepare_private_directory()?;
            drop(lock);
            runtime.execute_registered(&a.argv, &profile)
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
