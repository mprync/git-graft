//! Shared dependency and cache operations for managed workspaces.
use crate::{
    api::{self, fault},
    git, layer,
    registry::Project,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;

pub(crate) mod gc;

pub(crate) fn check_dependency(
    path: &Path,
    dependencies: &Value,
    args: &api::AddDep,
) -> Result<bool> {
    let attached = dependencies["status"] == "attached"
        && matches!(args.manager.as_str(), "npm" | "pnpm" | "bun");
    if attached {
        if args.packages.is_empty() {
            return Err(fault(
                "dependencies_already_attached",
                "This workspace already has a shared install. Pass the packages to add.",
                json!({"manager": dependencies["manager"]}),
            ));
        }
        ensure!(
            dependencies["manager"] == args.manager.as_str(),
            "use the shared install's package manager to add dependencies"
        );
        layer::check_attached_for_add(path)?;
    }
    Ok(attached)
}

pub(crate) fn seed_layer(p: &Project, a: &api::SeedLayer) -> Result<Value> {
    if !a.snapshot && a.copy {
        return Err(fault(
            "invalid_dependency_mode",
            "Shared dependencies require copy-on-write; omit --copy or use --snapshot",
            Value::Null,
        ));
    }
    let root = git::text(&p.repo, &["rev-parse", "--show-toplevel"])
        .context("sharing requires a primary checkout with an installed node_modules")?;
    ensure!(
        dunce::canonicalize(root)? == p.repo,
        "the primary checkout moved"
    );
    ensure!(
        git::common_dir(&p.repo)? == p.common && git::absolute_git_dir(&p.repo)? == p.common,
        "the primary checkout changed repositories"
    );
    if !a.snapshot {
        layer::seed_shared(&p.repo, &p.state, &a.key)
    } else {
        layer::run(&p.repo, &p.state, "save", &a.key, a.copy)
    }
}
