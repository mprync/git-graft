//! Reverse the net tree change of one completed landing while preserving Git history.
use std::{path::Path, process::Command};

use anyhow::{Result, ensure};
use serde_json::{Value, json};

use crate::{
    api::fault,
    git,
    landing::{self, Plan},
    recovery,
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
        "the pinned undo commit does not match the completed merge"
    );
    Ok(())
}

fn create_candidate(
    mut command: Command,
    pins: &Path,
    reference: &str,
    tree: &str,
    landed: &str,
    message: &str,
) -> Result<String> {
    let output = command
        .args([
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
            message,
        ])
        .output()?;
    ensure!(
        output.status.success(),
        "create reversal commit: {}",
        String::from_utf8_lossy(&output.stderr).trim_end()
    );
    let commit = String::from_utf8(output.stdout)?.trim_end().to_owned();
    git::checked_oid(&commit)?;
    git::create_private_ref(pins, reference, &commit)?;
    Ok(commit)
}

fn target_advanced(target: &str, expected: &str, actual: Option<&str>) -> anyhow::Error {
    fault(
        "target_advanced",
        format!("Destination {target} is no longer at the expected commit"),
        json!({"target": target, "expected": expected, "actual": actual}),
    )
}

pub(crate) fn reverse(
    repo: &Path,
    objects: &Path,
    plan: &Plan,
    undo_ref: &str,
    apply: impl FnOnce(&Plan) -> Result<()>,
) -> Result<Value> {
    git::checked_oid(&plan.old)?;
    git::checked_oid(&plan.new)?;
    if plan.old == plan.new {
        return Err(fault(
            "nothing_to_undo",
            "The merge did not advance its destination",
            json!({"target": plan.target}),
        ));
    }
    let target_ref = format!("refs/heads/{}", plan.target);
    git::bytes(repo, &["check-ref-format", &target_ref])?;
    let target = recovery::direct_ref(repo, &target_ref)?;
    let candidate = recovery::direct_ref(objects, undo_ref)?;
    let landed = Some(plan.new.as_str());
    let replay = candidate.is_some() && target == candidate;
    if !replay && target.as_deref() != landed {
        return Err(target_advanced(&plan.target, &plan.new, target.as_deref()));
    }

    let old_tree = tree(objects, &plan.old)?;
    let commit = match candidate {
        Some(commit) => {
            verify_candidate(objects, &commit, &old_tree, &plan.new)?;
            commit
        }
        None => {
            let message = format!("Undo graft merge into {}", plan.target);
            let mut command = git::command(repo);
            git::use_private_objects(&mut command, objects);
            create_candidate(command, objects, undo_ref, &old_tree, &plan.new, &message)?
        }
    };
    let undo = Plan {
        target: plan.target.clone(),
        old: plan.new.clone(),
        new: commit.clone(),
        tip: plan.new.clone(),
    };
    if replay {
        landing::finish_applied_checkout(repo, &undo)?;
    } else if let Err(error) = apply(&undo) {
        if let Ok(current) = recovery::direct_ref(repo, &target_ref)
            && current.as_deref() != landed
            && current.as_deref() != Some(commit.as_str())
        {
            return Err(target_advanced(&plan.target, &plan.new, current.as_deref()));
        }
        return Err(error);
    }
    let current = recovery::direct_ref(repo, &target_ref)?;
    if current.as_deref() != Some(commit.as_str()) {
        return Err(target_advanced(&plan.target, &commit, current.as_deref()));
    }
    Ok(json!({"action": "undo", "target": plan.target,
        "previous": plan.new, "commit": commit}))
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
        let candidate = create_candidate(
            git::command(repo),
            repo,
            "refs/graft/test/undo",
            &old_tree,
            &landed,
            "test",
        )?;
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
