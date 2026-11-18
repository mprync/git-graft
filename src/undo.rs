//! Reverse the net tree change of one completed landing while preserving Git history.
use std::path::Path;

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use crate::{
    api::fault,
    git,
    landing::{self, Plan},
    recovery,
    registry::Project,
};

fn tree(repo: &Path, commit: &str) -> Result<String> {
    let tree = git::text(
        repo,
        &[
            "--no-replace-objects",
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{commit}^{{tree}}"),
        ],
    )?;
    git::checked_oid(&tree)?;
    Ok(tree)
}

fn verify_candidate(repo: &Path, commit: &str, tree: &str, landed: &str) -> Result<()> {
    // Read the stored headers directly. History traversal may simplify parents
    // in a shallow clone or when the repository uses grafts or replacement refs.
    ensure!(
        git::text(repo, &["--no-replace-objects", "cat-file", "-t", commit])? == "commit",
        "the pinned undo object is not a commit"
    );
    let object = git::bytes(
        repo,
        &["--no-replace-objects", "cat-file", "commit", commit],
    )?;
    let mut headers = object
        .split(|&byte| byte == b'\n')
        .take_while(|line| !line.is_empty());
    let expected_tree = format!("tree {tree}");
    let expected_parent = format!("parent {landed}");
    ensure!(
        headers.next() == Some(expected_tree.as_bytes())
            && headers.next() == Some(expected_parent.as_bytes())
            && !headers.any(|line| line.starts_with(b"parent ")),
        "the pinned undo commit does not match the completed landing"
    );
    Ok(())
}

fn create_candidate(
    repo: &Path,
    reference: &str,
    tree: &str,
    landed: &str,
    name: &str,
) -> Result<String> {
    let commit = git::text(
        repo,
        &[
            "--no-replace-objects",
            "-c",
            "core.fsync=loose-object",
            "-c",
            "core.fsyncMethod=fsync",
            "commit-tree",
            tree,
            "-p",
            landed,
            "-m",
            &format!("Undo graft landing of {name}"),
        ],
    )?;
    git::checked_oid(&commit)?;
    let edits = format!("option no-deref\ncreate {reference} {commit}\n");
    recovery::transaction(repo, &edits, None, || {
        ensure!(
            recovery::direct_ref(repo, reference)?.is_none(),
            "undo ref appeared while preparing the undo"
        );
        Ok(())
    })?;
    Ok(commit)
}

fn target_advanced(target: &str, expected: &str, actual: Option<&str>) -> anyhow::Error {
    fault(
        "target_advanced",
        format!("Destination {target} is no longer at the expected commit"),
        json!({"target": target, "expected": expected, "actual": actual}),
    )
}

/// Undo under the project and name locks. Only the pinned commit is a replay.
pub fn run(p: &Project, name: &str, retired_lease: &str) -> Result<Value> {
    let plan = recovery::retired_landing(p, name, retired_lease)?;
    if plan.old == plan.new {
        return Err(fault(
            "nothing_to_undo",
            "The landing did not advance its destination",
            json!({"name": name, "target": plan.target}),
        ));
    }
    let repo = &p.repo;
    let target_ref = format!("refs/heads/{}", plan.target);
    git::bytes(repo, &["check-ref-format", &target_ref])?;
    let target = recovery::direct_ref(repo, &target_ref)?;
    let undo_ref = recovery::undo_ref(retired_lease)?;
    let candidate = recovery::direct_ref(repo, &undo_ref)?;
    let landed = Some(plan.new.as_str());
    let replay = candidate.is_some() && target == candidate;
    if !replay && target.as_deref() != landed {
        return Err(target_advanced(&plan.target, &plan.new, target.as_deref()));
    }

    let old_tree = tree(repo, &plan.old)?;
    let commit = match candidate {
        Some(commit) => {
            verify_candidate(repo, &commit, &old_tree, &plan.new)?;
            commit
        }
        None => create_candidate(repo, &undo_ref, &old_tree, &plan.new, name)?,
    };
    if !replay {
        // The candidate is durable before the target changes, allowing retry
        // after a lost response. For checked-out branches, the native Git merge
        // retains landing's documented race with other Git commands and edits.
        let undo = Plan {
            target: plan.target.clone(),
            old: plan.new.clone(),
            new: commit.clone(),
            tip: plan.new.clone(),
        };
        if let Err(error) = landing::apply(repo, &undo) {
            if let Ok(current) = recovery::direct_ref(repo, &target_ref)
                && current.as_deref() != landed
                && current.as_deref() != Some(commit.as_str())
            {
                return Err(target_advanced(&plan.target, &plan.new, current.as_deref()));
            }
            return Err(error);
        }
    }
    let current = recovery::direct_ref(repo, &target_ref)?;
    if current.as_deref() != Some(commit.as_str()) {
        return Err(target_advanced(&plan.target, &commit, current.as_deref()));
    }
    Ok(
        json!({"action": "undo", "name": name, "target": plan.target,
        "previous": plan.new, "commit": commit}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_saved_tree_and_sole_landed_parent_match() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo = dir.path();
        git::bytes(repo, &["init", "-q"])?;
        for (key, value) in [
            ("user.name", "Undo Fixture"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
            ("core.hooksPath", "test-hooks"),
        ] {
            git::bytes(repo, &["config", key, value])?;
        }
        git::bytes(repo, &["commit", "--allow-empty", "-m", "base"])?;
        let base = git::oid(repo, "HEAD")?;
        let old_tree = tree(repo, &base)?;
        std::fs::write(repo.join("landed"), "landed")?;
        git::bytes(repo, &["add", "landed"])?;
        git::bytes(repo, &["commit", "-m", "landed"])?;
        let landed = git::oid(repo, "HEAD")?;
        let candidate = create_candidate(repo, "refs/graft/test/undo", &old_tree, &landed, "test")?;
        verify_candidate(repo, &candidate, &old_tree, &landed)?;
        assert!(verify_candidate(repo, &candidate, &tree(repo, &landed)?, &landed).is_err());
        assert!(verify_candidate(repo, &candidate, &old_tree, &base).is_err());
        let merge = git::text(
            repo,
            &[
                "commit-tree",
                &old_tree,
                "-p",
                &landed,
                "-p",
                &base,
                "-m",
                "two parents",
            ],
        )?;
        assert!(verify_candidate(repo, &merge, &old_tree, &landed).is_err());
        git::bytes(
            repo,
            &[
                "tag",
                "--no-sign",
                "-am",
                "tagged candidate",
                "candidate",
                &candidate,
            ],
        )?;
        let tag = git::text(repo, &["rev-parse", "refs/tags/candidate"])?;
        assert!(verify_candidate(repo, &tag, &old_tree, &landed).is_err());
        Ok(())
    }
}
