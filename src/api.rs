//! Typed arguments shared by clap, JSON Schema and the MCP dispatcher.
use clap::{Args, Subcommand};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

#[derive(Debug, Subcommand)]
pub enum Operation {
    /// Probe checkout capabilities on the repository's filesystem.
    Doctor,
    /// List all managed workspaces, heartbeats and transaction phases.
    List,
    /// Start an isolated worktree at a committed revision.
    Spawn(Spawn),
    /// Inspect Git changes and the original storage receipt.
    Status(Name),
    /// Renew a workspace lease.
    Heartbeat(Owned),
    /// Inspect the runtime paths and assigned port.
    Env(Name),
    /// Run argv in the workspace with project caches and private outputs.
    Exec(Exec),
    /// Add packages, or perform a locked install when no packages are given.
    AddDep(AddDep),
    /// Save, restore or forget a reusable installed node_modules snapshot.
    Layer(Layer),
    /// Seed a dependency layer from the primary checkout's installed node_modules.
    SeedLayer(SeedLayer),
    /// Integrate a workspace into a local branch and retire the workspace.
    Land(Land),
    /// Retire a managed workspace and return its recovery lease.
    Drop(DropWorkspace),
    /// Reopen a retired workspace using its recovery lease.
    Restore(Owned),
    /// Reverse a completed land by appending a commit to its unchanged destination.
    UndoLand(Owned),
    /// Preview stale workspaces and unused source snapshots eligible for cleanup.
    Gc(Gc),
}

fn head() -> String {
    "HEAD".into()
}
fn main_branch() -> String {
    "main".into()
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spawn {
    /// Unique workspace name. Use lowercase ASCII letters, numbers and hyphens.
    pub name: String,
    /// Optional client-generated UUID. Reuse it to retry the same spawn after a disconnect.
    #[arg(long)]
    pub request_id: Option<String>,
    /// Committed revision to check out. Defaults to the invoking checkout's HEAD.
    #[arg(long, default_value = "HEAD")]
    #[serde(default = "head")]
    pub base: String,
    /// Include this directory using Git cone-mode sparse checkout. Repeatable.
    #[arg(long, value_name = "DIR")]
    #[serde(default)]
    pub sparse: Vec<String>,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Name {
    pub name: String,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Owned {
    pub name: String,
    /// Lease returned by spawn.
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Exec {
    pub name: String,
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
    /// Program and arguments. Use -- before the program on the CLI.
    #[arg(last = true, num_args = 1.., required = true)]
    pub argv: Vec<String>,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddDep {
    pub name: String,
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
    /// npm, pnpm, bun, cargo or uv.
    #[arg(long)]
    pub manager: String,
    /// Ask the package manager to use only locally cached packages.
    #[arg(long)]
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub packages: Vec<String>,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub name: String,
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
    /// save, restore or forget.
    pub action: String,
    /// Compatibility key for toolchain, install flags and environment.
    #[arg(long)]
    pub key: String,
    /// Request independent byte copies for this transfer, even on a CoW volume.
    #[arg(long)]
    #[serde(default)]
    pub copy: bool,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SeedLayer {
    /// Attest that this relocatable install matches the lock, toolchain, flags and environment named by the key and contains no private data.
    #[arg(long)]
    pub key: String,
    /// Request independent byte copies even on a copy-on-write volume.
    #[arg(long)]
    #[serde(default)]
    pub copy: bool,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Land {
    pub name: String,
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
    /// Local destination branch.
    #[arg(long, default_value = "main")]
    #[serde(default = "main_branch")]
    pub into: String,
    /// Create a single-parent commit with the merged tree.
    #[arg(long)]
    #[serde(default)]
    pub squash: bool,
    /// Stage all nonignored changes and commit them on the workspace branch first.
    #[arg(long, value_name = "MESSAGE")]
    pub commit: Option<String>,
    /// Allow retirement with ignored files. Use the recovery lease to reopen them.
    #[arg(long)]
    #[serde(default)]
    pub discard_ignored: bool,
    /// Attempt offline resolution of supported additive dependency conflicts.
    #[arg(long)]
    #[serde(default)]
    pub reconcile: bool,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DropWorkspace {
    pub name: String,
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
    /// Allow retirement with local work and unlanded commits. Keep the recovery lease.
    #[arg(long)]
    #[serde(default)]
    pub discard: bool,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Gc {
    /// Apply the eligible removals. The default is a preview.
    #[arg(long)]
    #[serde(default)]
    pub apply: bool,
    /// Also reclaim package caches and installed layers when all workspaces are retired.
    #[arg(long)]
    #[serde(default)]
    pub purge_cache: bool,
}

/// A stable machine-readable error, optionally with recovery information.
#[derive(Debug)]
pub struct Fault {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}
impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}
impl std::error::Error for Fault {}

pub fn fault(code: &'static str, message: impl Into<String>, details: Value) -> anyhow::Error {
    Fault {
        code,
        message: message.into(),
        details,
    }
    .into()
}
pub fn error_json(e: &anyhow::Error) -> Value {
    let kind = e.downcast_ref::<Fault>();
    json!({"code": kind.map_or("operation_failed", |f| f.code), "message": format!("{e:#}"),
           "details": kind.map(|f| &f.details)})
}
