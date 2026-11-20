//! Typed arguments shared by clap, JSON Schema and the MCP dispatcher.
use clap::Args;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

fn head() -> String {
    "HEAD".into()
}
fn main_branch() -> String {
    "main".into()
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spawn {
    /// Unique graft name. Use lowercase ASCII letters, numbers and hyphens.
    pub name: String,
    /// Secret random UUIDv4 used as the lease. Reuse it to retry the identical request.
    #[arg(long)]
    pub request_id: Option<String>,
    /// Committed revision to check out. Defaults to the invoking checkout's HEAD.
    #[arg(long, default_value = "HEAD")]
    #[serde(default = "head")]
    pub base: String,
    /// Omit for normal work in a trusted clone. Uses the project's graft.history setting, or local by default.
    /// Local reuses eligible Git object files, including objects from other branches and staged or dangling blobs.
    /// Choose isolated when only objects reachable from the selected commit should be readable.
    /// Choose tip when the checkout needs only the starting commit and its files.
    #[arg(long, value_enum)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<History>,
    /// Include this directory using Git cone-mode sparse checkout. Repeatable.
    #[arg(long, value_name = "DIR")]
    #[serde(default)]
    pub sparse: Vec<String>,
    /// Leave out the project's registered shared install.
    #[arg(long)]
    #[serde(default)]
    pub no_deps: bool,
    /// Name of the new private branch at the selected base.
    #[arg(short = 'b', long, conflicts_with_all = ["detach", "orphan"])]
    pub branch: Option<String>,
    /// Leave HEAD detached at the selected commit.
    #[arg(short = 'd', long, conflicts_with = "orphan")]
    #[serde(default)]
    pub detach: bool,
    /// Start with an empty, unborn branch.
    #[arg(long, conflicts_with_all = ["base", "sparse"])]
    pub orphan: Option<String>,
    /// Initialize the repository and HEAD without populating files or the index.
    #[arg(long, conflicts_with = "sparse")]
    #[serde(default)]
    pub no_checkout: bool,
    /// Lock the new graft against moves and pruning.
    #[arg(long)]
    #[serde(default)]
    pub lock: bool,
    /// Why the graft is locked.
    #[arg(long, requires = "lock")]
    pub reason: Option<String>,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, Deserialize, Serialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum History {
    Isolated,
    Local,
    Tip,
}

impl Spawn {
    pub(crate) fn from_mcp(
        args: serde_json::Map<String, Value>,
        no_deps: bool,
    ) -> anyhow::Result<Self> {
        let explicit_base = args.contains_key("base");
        let mut s: Self = serde_json::from_value(Value::Object(args))?;
        anyhow::ensure!(
            usize::from(s.detach)
                + usize::from(s.branch.is_some())
                + usize::from(s.orphan.is_some())
                <= 1,
            "select only one of branch, detach and orphan"
        );
        anyhow::ensure!(
            s.orphan.is_none() || !explicit_base,
            "orphan and base cannot be combined"
        );
        anyhow::ensure!(
            s.sparse.is_empty() || (s.orphan.is_none() && !s.no_checkout),
            "sparse requires a populated checkout"
        );
        anyhow::ensure!(s.reason.is_none() || s.lock, "a lock reason requires lock");
        s.no_deps |= no_deps;
        Ok(s)
    }
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Name {
    /// Name of the graft.
    pub name: String,
}

/// Arguments of `status`: inspect a graft, or show its environment.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Name of the graft.
    pub name: String,
    /// Show the graft's runtime and cache paths instead of its status.
    #[arg(long)]
    pub env: bool,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Owned {
    /// Name of the graft.
    pub name: String,
    /// Lease returned when the graft was created.
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Exec {
    /// Name of the graft.
    pub name: String,
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
    /// Registered manager ID; use for wrappers or when the program name differs.
    #[arg(long)]
    pub manager: Option<String>,
    /// Program and arguments. Use -- before the program on the CLI.
    #[arg(last = true, num_args = 1.., required = true)]
    pub argv: Vec<String>,
}

/// Register a package manager's cache and optional reusable install.
#[derive(Debug, Args, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterManager {
    /// Portable manager ID; used to select this registration for execution.
    pub manager: String,
    /// Environment variable to receive the project-scoped cache path.
    #[arg(long)]
    pub cache_env: Option<String>,
    /// Ordered arguments prepended to the command; repeat per argument. If provided, one must contain {cache}, replaced with the cache path.
    #[arg(long = "cache-arg", allow_hyphen_values = true)]
    #[serde(default)]
    pub cache_args: Vec<String>,
    /// Optional checkout-relative installed directory to seed for reuse; requires inputs and key.
    #[arg(long)]
    pub install_dir: Option<String>,
    /// Checkout-relative committed and tracked install input; repeat for each file.
    #[arg(long = "input")]
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Compatibility key covering ABI, toolchain and other install settings.
    #[arg(long)]
    pub key: Option<String>,
    /// Source graft for the install; omit to use the primary checkout. Requires --lease.
    #[arg(long = "graft", value_name = "NAME")]
    pub workspace: Option<String>,
    /// Lease for the source graft. Requires --graft.
    #[arg(long)]
    pub lease: Option<String>,
}

#[derive(Debug, Args, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerName {
    /// Registered manager ID.
    pub manager: String,
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct RegisterCli {
        #[command(flatten)]
        args: RegisterManager,
    }

    #[test]
    fn cli_accepts_repeated_cache_arguments_and_inputs() {
        let parsed = RegisterCli::try_parse_from([
            "test",
            "pip",
            "--cache-arg",
            "--cache-dir={cache}",
            "--cache-arg",
            "{cache}",
            "--input",
            "requirements.txt",
            "--input",
            "pyproject.toml",
        ])
        .unwrap()
        .args;
        assert_eq!(parsed.cache_args, ["--cache-dir={cache}", "{cache}"]);
        assert_eq!(parsed.inputs, ["requirements.txt", "pyproject.toml"]);
        let json = serde_json::to_value(parsed).unwrap();
        assert!(json.get("cache_args").is_some());
        assert!(json.get("inputs").is_some());
    }

    #[test]
    fn older_exec_json_and_minimal_registration_deserialize() {
        let exec: Exec =
            serde_json::from_value(json!({"name":"a","lease":"l","argv":["pwd"]})).unwrap();
        assert!(exec.manager.is_none());
        let args: RegisterManager = serde_json::from_value(json!({"manager":"pip"})).unwrap();
        assert!(args.cache_args.is_empty() && args.inputs.is_empty());
    }
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddDep {
    /// Name of the graft.
    pub name: String,
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
    /// Package manager: npm, pnpm, bun, cargo or uv.
    #[arg(long)]
    pub manager: String,
    /// Ask the package manager to use only locally cached packages.
    #[arg(long)]
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    /// Packages to add. Leave empty for a locked install from the lockfile.
    pub packages: Vec<String>,
}

/// Which graft's installed dependencies to save, restore or remove.
#[derive(Debug, Args)]
pub struct Layer {
    /// Name of the graft.
    pub name: String,
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
    /// Compatibility key for toolchain, install flags and environment.
    #[arg(long)]
    pub key: String,
    /// Use independent byte copies. The default requires copy-on-write.
    #[arg(long)]
    pub copy: bool,
}

#[derive(Debug, Args)]
pub struct SeedLayer {
    /// Attest that this relocatable install matches the lock, toolchain, flags and environment named by the key, is
    /// independent of other source files, contains no private data and keeps its installation inputs valid.
    #[arg(long)]
    pub key: String,
    /// Take a one-off manual snapshot of the install instead of sharing it with future grafts.
    /// Restore it into a graft with `deps layer restore`.
    #[arg(long)]
    pub snapshot: bool,
    /// Use independent byte copies. Only for --snapshot; sharing requires copy-on-write.
    #[arg(long)]
    pub copy: bool,
}

#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Land {
    /// Name of the graft.
    pub name: String,
    /// The graft's lease, or with undo its recovery lease.
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
    /// Stage all nonignored changes and commit them on the graft's branch first.
    #[arg(long, value_name = "MESSAGE")]
    pub commit: Option<String>,
    /// Archive the graft even if it has ignored files. Use the recovery lease to reopen them.
    #[arg(long)]
    #[serde(default)]
    pub discard_ignored: bool,
    /// Attempt offline resolution of supported additive dependency conflicts.
    #[arg(long)]
    #[serde(default)]
    pub reconcile: bool,
    /// Reverse an earlier merge instead: append a commit that undoes it if the branch is still at the merged commit.
    #[arg(
        long,
        conflicts_with_all = ["into", "squash", "commit", "discard_ignored", "reconcile"]
    )]
    #[serde(default)]
    pub undo: bool,
}

impl Land {
    /// Reject merge-only options combined with `undo`. The CLI enforces this
    /// through clap; JSON callers also need to say whether `into` was given,
    /// because it has a default.
    pub(crate) fn check_undo(&self, explicit_into: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.undo
                || !(explicit_into
                    || self.squash
                    || self.commit.is_some()
                    || self.discard_ignored
                    || self.reconcile),
            "undo cannot be combined with into, squash, commit, discard_ignored or reconcile"
        );
        Ok(())
    }
}

/// Arguments of `remove` and the MCP `graft_remove` tool: archive one graft.
#[derive(Debug, Args, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DropWorkspace {
    /// Name of the graft.
    pub name: String,
    /// The graft's lease, returned when it was added.
    #[arg(long, env = "GIT_GRAFT_LEASE", hide_env_values = true)]
    pub lease: String,
    /// Archive even with local work or unmerged commits. Keep the recovery lease.
    #[arg(long)]
    #[serde(default)]
    pub discard: bool,
}

/// Arguments of `prune`: preview or archive every expired graft.
#[derive(Debug, Args)]
pub struct Gc {
    /// Archive the expired grafts and clean caches. The default is a preview.
    #[arg(long)]
    pub apply: bool,
    /// Also reclaim package caches and installed layers when all grafts are archived.
    #[arg(long)]
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
