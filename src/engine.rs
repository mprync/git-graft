//! The command dispatcher. All cooperating clients share the same registry,
//! project lock, per-workspace lock and lease checks.
use crate::{
    api::{self, Operation, Spawn, fault},
    git, landing, layer, recovery,
    registry::{self, Phase, Project, Record, exists, now},
    runtime, storage,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

mod gc;
mod retire;

/// Free space is a volume-wide observation. Never use it to attribute private bytes.
fn available(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stat = MaybeUninit::<libc::statvfs>::uninit();
        // Both pointers are valid for the duration of the call. The struct is
        // initialized by libc only on a successful return.
        if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
            return None;
        }
        let stat = unsafe { stat.assume_init() };
        Some(
            (u128::from(stat.f_bavail) * u128::from(stat.f_frsize)).min(u128::from(u64::MAX))
                as u64,
        )
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free = 0;
        // The terminated path and output value outlive the synchronous call.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut free,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(free)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

fn spawn(p: &Project, args: Spawn) -> Result<Value> {
    registry::validate_name(&args.name)?;
    let records = p.records()?;
    let request_id = args
        .request_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .context("request_id must be a UUID")?
        .map(|id| id.to_string());
    if let Some(r) = records.iter().find(|r| r.name == args.name) {
        if request_id.as_deref() == Some(&r.lease)
            && r.requested_base == args.base
            && r.sparse == args.sparse
        {
            r.ready()?;
            p.verify(r)?;
            let mut result = p.summary(r);
            result["lease"] = json!(r.lease);
            return Ok(result);
        }
        return Err(fault(
            "already_exists",
            "Workspace name is already in use",
            json!({"name":args.name}),
        ));
    }
    if let Some(id) = &request_id {
        ensure!(
            !records.iter().any(|r| r.lease == *id)
                && p.receipt(id)?.is_none()
                && !recovery::lease_used(p, id)?,
            "request_id has already been used; generate a fresh UUID for a new workspace"
        );
    }
    let oid = git::oid(&p.invocation, &args.base)
        .with_context(|| format!("resolve committed base {}", args.base))?;
    storage::validate_sparse(&p.repo, &oid, &args.sparse)?;
    p.root(true)?;
    let path = p.workspace(&args.name);
    let branch = format!("graft/{}", args.name);
    if exists(&path)? || p.registered.iter().any(|w| w.path == path) || branch_occupied(p, &branch)?
    {
        return Err(fault(
            "already_exists",
            "The destination path or Git branch already exists",
            json!({"branch":branch,"path":path}),
        ));
    }
    ensure!(
        !exists(&p.state.join("runtime").join(&args.name))?,
        "private runtime directory is already occupied"
    );
    let start = now();
    let before = available(&p.root);
    let mut r = Record {
        name: args.name.clone(),
        lease: request_id.unwrap_or_else(|| Uuid::new_v4().to_string()),
        base: oid.clone(),
        requested_base: args.base.clone(),
        sparse: args.sparse.clone(),
        port: registry::port_for(&records, &args.name)?,
        created: start,
        heartbeat: start,
        admin: None,
        state: Phase::Creating,
        storage: Value::Null,
    };
    p.save(&r)?;
    let create = || -> Result<()> {
        git::bytes(
            &p.repo,
            &[
                "worktree",
                "add",
                "--no-checkout",
                "--no-track",
                "-b",
                &branch,
                "--",
                git::path_str(&path)?,
                &oid,
            ],
        )?;
        Ok(())
    };
    if let Err(e) = create() {
        // Native Git may have created the branch before an I/O or hook failure.
        if !exists(&path)? && retire::branch_oid(p, &branch)?.is_none() {
            p.forget(&r)?;
            return Err(e);
        }
        return Err(incomplete(&r, e));
    }
    let result = (|| -> Result<()> {
        p.mark(&mut r)?;
        r.storage = storage::materialize_from(&path, &p.state, &oid, &args.sparse, &p.repo)?;
        r.storage["volume_available_before"] = json!(before);
        r.storage["volume_available_after"] = json!(available(&p.root));
        r.heartbeat = now();
        r.state = Phase::Ready;
        p.save(&r)
    })();
    if let Err(e) = result {
        return Err(incomplete(&r, e));
    }
    let mut response = p.summary(&r);
    response["lease"] = json!(r.lease);
    Ok(response)
}
fn incomplete(r: &Record, e: anyhow::Error) -> anyhow::Error {
    fault(
        "spawn_incomplete",
        format!("Workspace creation stopped: {e:#}"),
        json!({"name":r.name,"lease":r.lease,"phase":"creating","recovery":"Inspect status, then drop --discard to retire the partial checkout."}),
    )
}

/// Recent Git versions can check exact ref existence without peeling a symbolic
/// or broken ref. Keep the full check for Git versions predating --exists.
fn branch_occupied(p: &Project, branch: &str) -> Result<bool> {
    let reference = format!("refs/heads/{branch}");
    let out = git::raw(&p.repo, &["show-ref", "--exists", &reference])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(2) => Ok(false),
        Some(129) => Ok(retire::branch_oid(p, branch)?.is_some()),
        _ => anyhow::bail!(
            "cannot check {reference}: {}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        ),
    }
}

fn status(p: &Project, r: &Record) -> Result<Value> {
    let mut response = p.summary(r);
    match p.verify(r) {
        Ok(path) => {
            response["head"] = json!(git::oid(&path, "HEAD")?);
            let paths = git::bytes(
                &path,
                &[
                    "diff",
                    "--name-only",
                    "--no-renames",
                    "-z",
                    &r.base,
                    "HEAD",
                    "--",
                ],
            )?;
            let paths: Vec<_> = paths.split(|&b| b == 0).filter(|s| !s.is_empty()).collect();
            response["committed_path_count"] = json!(paths.len());
            response["committed_paths"] = json!(
                paths
                    .iter()
                    .take(200)
                    .map(|s| String::from_utf8_lossy(s))
                    .collect::<Vec<_>>()
            );
            let changes = git::changes(&path)?;
            response["change_count"] = json!(changes.len());
            response["changes"] = json!(changes.iter().take(200).collect::<Vec<_>>());
            response["changes_truncated"] = json!(changes.len() > 200);
            response["registered"] = json!(true);
        }
        Err(e) => {
            response["registered"] = json!(false);
            response["registration_error"] = json!(format!("{e:#}"));
        }
    }
    Ok(response)
}

fn doctor(p: &Project) -> Result<Value> {
    let known = [
        "package-lock.json",
        "npm-shrinkwrap.json",
        "pnpm-lock.yaml",
        "bun.lock",
        "bun.lockb",
        "yarn.lock",
        "Cargo.lock",
        "uv.lock",
    ];
    let head = git::raw(&p.invocation, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    let locks = if head.status.success() {
        let paths = git::bytes(&p.invocation, &["ls-tree", "-z", "--name-only", "HEAD"])?;
        known
            .into_iter()
            .filter(|name| paths.split(|&b| b == 0).any(|path| path == name.as_bytes()))
            .collect()
    } else {
        Vec::new()
    };
    Ok(
        json!({"version":env!("CARGO_PKG_VERSION"),"git":git::text(&p.repo,&["--version"])?,
        "common_dir":p.common,"workspace_root":p.root,"source_storage":storage::doctor(&p.state)?,
        "volume_available_bytes":available(&p.state),"lease_seconds":registry::LEASE_SECS,
        "root_lockfiles":locks,"git_worktrees":p.registered,"recovery_supported":recovery::supported(p)?}),
    )
}

fn land(p: &Project, r: &mut Record, opt: &api::Land) -> Result<Value> {
    recovery::ensure_supported(p)?;
    if let Phase::Removing {
        tip,
        discard,
        discard_ignored,
        landed,
    } = &r.state
    {
        let plan = landed
            .as_ref()
            .context("A drop is pending. Retry drop with the original lease.")?;
        ensure!(
            plan.target == opt.into,
            "retry landing to the recorded destination {}",
            plan.target
        );
        let result = retire::receipt(r, Some(plan));
        retire::finish(
            p,
            r,
            tip,
            *discard,
            *discard_ignored || opt.discard_ignored,
            &result,
        )?;
        return Ok(result);
    }
    let path = p.verify(r)?;
    git::check_idle(&path)?;
    let (plan, allow_ignored) = if let Phase::Landing {
        plan,
        discard_ignored,
    } = &r.state
    {
        ensure!(
            plan.target == opt.into,
            "retry landing to the recorded destination {}",
            plan.target
        );
        (plan.clone(), *discard_ignored || opt.discard_ignored)
    } else {
        r.ready()?;
        retire::check_lock(p, r)?;
        if !opt.discard_ignored && git::changes(&path)?.iter().any(|c| c.status == "!!") {
            return Err(fault(
                "ignored_files",
                "Workspace contains ignored files. Move them or pass --discard-ignored.",
                json!({"worktree_path":path}),
            ));
        }
        let plan = landing::prepare(&p.repo, &path, &p.state, opt)?;
        r.state = Phase::Landing {
            plan: plan.clone(),
            discard_ignored: opt.discard_ignored,
        };
        p.save(r)?;
        (plan, opt.discard_ignored)
    };
    if git::oid(&path, "HEAD")? != plan.tip {
        r.state = Phase::Ready;
        p.save(r)?;
        return Err(fault(
            "tip_changed",
            "Workspace advanced while landing. Review its changes and retry.",
            json!({"name":r.name}),
        ));
    }
    // A dirty destination, hooks or a ref race may reject a plan. On error the
    // workspace is kept. Replan only when we can prove the commit did not land.
    if let Err(error) = landing::apply(&p.repo, &plan) {
        let dest = format!("refs/heads/{}", plan.target);
        let contains = git::raw(
            &p.repo,
            &[
                "--no-replace-objects",
                "merge-base",
                "--is-ancestor",
                &plan.new,
                &dest,
            ],
        )?;
        if contains.status.code() == Some(1) {
            r.state = Phase::Ready;
            p.save(r)?;
        }
        return Err(error);
    }
    retire::require_clean(&path, allow_ignored).with_context(|| {
        format!(
            "{} contains {}; workspace cleanup is pending",
            plan.target, plan.new
        )
    })?;
    let result = retire::receipt(r, Some(&plan));
    retire::remove(p, r, &plan.tip, false, allow_ignored, Some(plan.clone()))?;
    Ok(result)
}

pub fn run(repo: &Path, op: Operation) -> Result<Value> {
    let (project, project_lock) = Project::open(repo)?;
    let p = &project;
    match &op {
        Operation::Doctor | Operation::List | Operation::Status(_) | Operation::Env(_) => {}
        Operation::Gc(args) if !args.apply => {}
        Operation::SeedLayer(_) => p.require_private_state()?,
        _ => {
            p.require_private_state()?;
            p.require_private_workspaces()?;
        }
    }
    let (name, lease) = match &op {
        Operation::Doctor => return doctor(p),
        Operation::List => {
            return Ok(
                json!({"workspaces":p.records()?.iter().map(|r|p.summary(r)).collect::<Vec<_>>(),
                    "recoverable":recovery::list(p)?}),
            );
        }
        Operation::Spawn(_) => {
            let Operation::Spawn(args) = op else {
                unreachable!()
            };
            return spawn(p, args);
        }
        Operation::Gc(args) => return gc::run(p, args.apply, args.purge_cache),
        Operation::Restore(a) | Operation::UndoLand(a) => {
            registry::validate_name(&a.name)?;
            Uuid::parse_str(&a.lease).context("invalid recovery lease")?;
            let _workspace_lock = p.work_lock(&a.name)?;
            return if matches!(op, Operation::UndoLand(_)) {
                crate::undo::run(p, &a.name, &a.lease)
            } else {
                recovery::restore(p, &a.name, &a.lease)
            };
        }
        Operation::SeedLayer(a) => {
            let root = git::text(&p.repo, &["rev-parse", "--show-toplevel"])
                .context("seeding requires a primary checkout with an installed node_modules")?;
            ensure!(
                dunce::canonicalize(root)? == p.repo,
                "the primary checkout moved"
            );
            let common = git::text(
                &p.repo,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )?;
            ensure!(
                dunce::canonicalize(common)? == p.common,
                "the primary checkout changed repositories"
            );
            return layer::run(&p.repo, &p.state, "save", &a.key, a.copy);
        }
        Operation::Status(a) => return status(p, &p.record(&a.name)?),
        Operation::Env(a) => {
            let r = p.record(&a.name)?;
            return runtime::env(&p.verify(&r)?, &p.state, r.port);
        }
        Operation::Heartbeat(a) => (&a.name, &a.lease),
        Operation::Land(a) => (&a.name, &a.lease),
        Operation::Drop(a) => (&a.name, &a.lease),
        Operation::Exec(a) => (&a.name, &a.lease),
        Operation::AddDep(a) => (&a.name, &a.lease),
        Operation::Layer(a) => (&a.name, &a.lease),
    };
    let mut r = match p.record(name) {
        Ok(record) => record,
        Err(error) => {
            if error
                .downcast_ref::<api::Fault>()
                .is_some_and(|f| f.code == "not_found")
                && matches!(op, Operation::Land(_) | Operation::Drop(_))
                && let Some(receipt) = p.receipt(lease)?
            {
                let result = &receipt["result"];
                let matches_request = match &op {
                    Operation::Land(a) => result["action"] == "land" && result["target"] == a.into,
                    Operation::Drop(_) => result["action"] == "drop",
                    _ => false,
                };
                if result["name"] == name.as_str() && matches_request {
                    return Ok(result.clone());
                }
            }
            return Err(error);
        }
    };
    r.authorize(lease)?;
    let workspace_lock = p.work_lock(name)?;
    match op {
        Operation::Drop(a) => retire::drop_workspace(p, &mut r, a.discard)?,
        Operation::Land(a) => {
            let result = land(p, &mut r, &a)?;
            drop(workspace_lock);
            gc::forget_lock(p, &r.name)?;
            return Ok(result);
        }
        op => {
            r.ready()?;
            let path = p.verify(&r)?;
            r.heartbeat = now();
            p.save(&r)?;
            if let Operation::Layer(a) = op {
                return layer::run(&path, &p.state, &a.action, &a.key, a.copy);
            }
            if matches!(op, Operation::Heartbeat(_)) {
                return Ok(p.summary(&r));
            }
            // Keep the workspace lock across the subprocess. Other workspaces
            // can run, renew their leases and land while this child is alive.
            drop(project_lock);
            let result = match op {
                Operation::Exec(a) => runtime::exec(&path, &p.state, r.port, &a.argv),
                Operation::AddDep(a) => {
                    runtime::dependency(&path, &p.state, r.port, &a.manager, &a.packages, a.offline)
                }
                _ => unreachable!(),
            };
            let (_p, _project_lock) = Project::open(&p.common)?;
            r.heartbeat = now();
            p.save(&r)?;
            return result;
        }
    }
    drop(workspace_lock);
    gc::forget_lock(p, &r.name)?;
    Ok(retire::receipt(&r, None))
}
