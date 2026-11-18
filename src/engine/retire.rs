//! Retire only the recorded Git worktree and the exact branch tip we inspected.
use crate::landing::Plan;
use crate::{
    api::fault,
    git, recovery,
    registry::{Phase, Project, Record, exists},
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{fs, path::Path};

pub(super) fn branch_oid(p: &Project, branch: &str) -> Result<Option<String>> {
    recovery::direct_ref(&p.repo, &format!("refs/heads/{branch}"))
}
pub(super) fn require_clean(path: &Path, allow_ignored: bool) -> Result<()> {
    git::check_idle(path)?;
    let changes = git::changes(path)?;
    let changes: Vec<_> = changes
        .iter()
        .filter(|c| !allow_ignored || c.status != "!!")
        .collect();
    if !changes.is_empty() {
        return Err(fault(
            "workspace_dirty",
            "Workspace contains local files or changes",
            json!({"count":changes.len(),"changes":changes.iter().take(30).collect::<Vec<_>>()}),
        ));
    }
    Ok(())
}
pub(super) fn check_lock(p: &Project, r: &Record) -> Result<()> {
    let path = p.workspace(&r.name);
    if git::worktrees(&p.repo)?
        .iter()
        .any(|w| w.locked && w.path == path)
    {
        return Err(fault(
            "git_locked",
            "The Git worktree has been explicitly locked",
            json!({"path":path}),
        ));
    }
    Ok(())
}
pub(super) fn eligible(p: &Project, r: &Record) -> Result<String> {
    r.ready()?;
    let path = p.verify(r)?;
    check_lock(p, r)?;
    require_clean(&path, false)?;
    let tip = git::oid(&path, "HEAD")?;
    if tip != r.base {
        let refs = git::text(
            &path,
            &[
                "--no-replace-objects",
                "for-each-ref",
                "--format=%(refname)",
                "--contains",
                &tip,
                "refs/heads/",
            ],
        )?;
        if !refs
            .lines()
            .any(|reference| !reference.starts_with("refs/heads/graft/"))
        {
            return Err(fault(
                "unmerged_commits",
                "Land the workspace commits or explicitly discard them",
                json!({"branch":r.branch(),"tip":tip}),
            ));
        }
    }
    Ok(tip)
}
pub(super) fn drop_workspace(p: &Project, r: &mut Record, discard: bool) -> Result<()> {
    if let Phase::Removing {
        tip,
        discard: original,
        discard_ignored,
        landed,
    } = &r.state
    {
        ensure!(
            landed.is_none(),
            "A landing is pending. Retry land with the recorded target."
        );
        let tip = tip.clone();
        // A retry of a destructive drop needs the same explicit consent.
        ensure!(
            !*original || discard,
            "retry drop with --discard to finish the requested removal"
        );
        return finish(p, r, &tip, discard, *discard_ignored, &receipt(r, None));
    }
    if matches!(r.state, Phase::Creating) && r.admin.is_none() {
        p.root(false)?;
        let path = p.workspace(&r.name);
        if !exists(&path)? && !git::worktrees(&p.repo)?.iter().any(|w| w.path == path) {
            // No verified registration. An orphan branch may be the user's.
            // Preserve it and report it rather than treating it as disposable.
            ensure!(
                branch_oid(p, &r.branch())?.is_none(),
                "unverified branch {} exists; inspect it with Git before releasing the reservation",
                r.branch()
            );
            return p.complete(r, &receipt(r, None));
        }
        let admin = p.identify(r)?;
        ensure!(
            fs::read(admin.join("graft-owner")).ok().as_deref() == Some(r.lease.as_bytes()),
            "interrupted before ownership was recorded; inspect the worktree and branch with Git"
        );
        r.admin = Some(admin);
        p.save(r)?;
    }
    let tip = if discard {
        let path = p.verify(r)?;
        check_lock(p, r)?;
        git::oid(&path, "HEAD")?
    } else {
        eligible(p, r)?
    };
    remove(p, r, &tip, discard, discard, None)
}

pub(super) fn receipt(r: &Record, landed: Option<&Plan>) -> Value {
    let mut result = match landed {
        Some(plan) => {
            json!({"action":"land","name":r.name,"target":plan.target,"previous":plan.old,
            "commit":plan.new,"workspace_tip":plan.tip,"removed":true})
        }
        None => json!({"action":"drop","name":r.name,"removed":true}),
    };
    if r.admin.is_some() {
        result["recovery_lease"] = json!(r.lease);
    }
    result
}
pub(super) fn remove(
    p: &Project,
    r: &mut Record,
    tip: &str,
    discard: bool,
    discard_ignored: bool,
    landed: Option<Plan>,
) -> Result<()> {
    recovery::ensure_supported(p)?;
    p.verify(r)?;
    check_lock(p, r)?;
    let result = receipt(r, landed.as_ref());
    r.state = Phase::Removing {
        tip: tip.into(),
        discard,
        discard_ignored,
        landed,
    };
    p.save(r)?;
    finish_supported(p, r, tip, discard, discard_ignored, &result)
}

pub(super) fn finish(
    p: &Project,
    r: &Record,
    tip: &str,
    discard: bool,
    discard_ignored: bool,
    result: &Value,
) -> Result<()> {
    recovery::ensure_supported(p)?;
    finish_supported(p, r, tip, discard, discard_ignored, result)
}

fn finish_supported(
    p: &Project,
    r: &Record,
    tip: &str,
    discard: bool,
    discard_ignored: bool,
    result: &Value,
) -> Result<()> {
    if let Phase::Removing {
        landed: Some(plan), ..
    } = &r.state
    {
        let target = branch_oid(p, &plan.target)?.ok_or_else(|| {
            anyhow::anyhow!("the destination branch disappeared; the workspace has been kept")
        })?;
        let output = git::raw(
            &p.repo,
            &[
                "--no-replace-objects",
                "merge-base",
                "--is-ancestor",
                &plan.new,
                &target,
            ],
        )?;
        ensure!(
            output.status.success(),
            "the destination no longer contains the integration commit; the workspace has been kept"
        );
    }
    if !recovery::lease_used(p, &r.lease)? && exists(&p.workspace(&r.name))? {
        let path = p.verify(r)?;
        check_lock(p, r)?;
        ensure!(
            git::oid(&path, "HEAD")? == tip && branch_oid(p, &r.branch())?.as_deref() == Some(tip),
            "workspace branch changed during retirement; keeping worktree"
        );
        if !discard {
            require_clean(&path, discard_ignored)?;
        }
    }
    recovery::retire(p, r, tip, discard)?;
    p.complete(r, result)
}
