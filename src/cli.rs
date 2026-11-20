//! The command-line surface: clap definitions for every `git graft` command
//! except `mcp`, and their conversion into [`workspace::Command`].
use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::{api, workspace};

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Create a graft: a lightweight, independent copy of the repository.
    Add(api::Spawn),
    /// List active and recoverable grafts.
    List,
    /// Show a graft's HEAD and changed files, or its environment with --env.
    Status(api::StatusArgs),
    /// Check Git support and whether grafts can share files.
    Doctor,
    /// Run a program inside a graft.
    Exec(api::Exec),
    /// Install and reuse dependencies.
    Deps(Deps),
    /// Merge a graft's commits into a local branch, then archive the graft.
    Merge(api::Land),
    /// Archive a graft and its files. Refuses if it has unmerged work unless --discard is given.
    Remove(api::DropWorkspace),
    /// Archive every expired graft. Previews unless --apply is given.
    Prune(api::Gc),
    /// Reopen an archived graft.
    Restore(api::Owned),
    /// Protect a graft from moves, pruning and cleanup.
    Lock {
        #[command(flatten)]
        workspace: api::Owned,
        /// Why the graft is locked.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Release a graft's lock.
    Unlock(api::Owned),
    /// Move a graft to another path on the same volume.
    Move {
        #[command(flatten)]
        workspace: api::Owned,
        /// New path for the graft.
        path: PathBuf,
    },
    /// Reconnect a graft that was moved outside git graft.
    Repair {
        #[command(flatten)]
        workspace: api::Owned,
        /// Where the graft is now.
        path: PathBuf,
    },
    /// Renew a graft's lease.
    #[command(hide = true)]
    Heartbeat(api::Owned),
}

#[derive(Debug, Args)]
pub struct Deps {
    #[command(subcommand)]
    command: DepsCommand,
}

#[derive(Debug, Subcommand)]
enum DepsCommand {
    /// Install packages in a graft, or run a locked install from its lockfile.
    Add(api::AddDep),
    /// Save, restore or remove a graft's installed dependencies.
    Layer(LayerArgs),
    /// Share the primary checkout's installed dependencies with future grafts.
    Share(api::SeedLayer),
    /// Register package managers so grafts reuse their caches and installs.
    Manager(ManagerArgs),
}

#[derive(Debug, Args)]
struct LayerArgs {
    #[command(subcommand)]
    action: LayerAction,
}

#[derive(Debug, Subcommand)]
enum LayerAction {
    /// Save the graft's installed dependencies for reuse.
    Save(api::Layer),
    /// Restore saved dependencies into the graft.
    Restore(api::Layer),
    /// Delete the saved dependencies for a key.
    Remove(api::Layer),
}

#[derive(Debug, Args)]
struct ManagerArgs {
    #[command(subcommand)]
    command: ManagerCommand,
}

#[derive(Debug, Subcommand)]
enum ManagerCommand {
    /// Register a package manager's cache, and optionally a reusable install.
    Add(api::RegisterManager),
    /// List registered package managers.
    List,
    /// Unregister a package manager. Its cache is kept.
    Remove(api::ManagerName),
}

impl From<Cmd> for workspace::Command {
    fn from(cmd: Cmd) -> Self {
        use workspace::Command as Op;
        match cmd {
            Cmd::Add(args) => Op::Spawn(args),
            Cmd::List => Op::List,
            Cmd::Status(api::StatusArgs { name, env: false }) => Op::Status(api::Name { name }),
            Cmd::Status(api::StatusArgs { name, env: true }) => Op::Env(api::Name { name }),
            Cmd::Doctor => Op::Doctor,
            Cmd::Exec(args) => Op::Exec(args),
            Cmd::Deps(Deps { command }) => match command {
                DepsCommand::Add(args) => Op::AddDep(args),
                DepsCommand::Share(args) => Op::SeedLayer(args),
                DepsCommand::Layer(LayerArgs { action }) => match action {
                    LayerAction::Save(target) => Op::Layer("save", target),
                    LayerAction::Restore(target) => Op::Layer("restore", target),
                    LayerAction::Remove(target) => Op::Layer("forget", target),
                },
                DepsCommand::Manager(ManagerArgs { command }) => match command {
                    ManagerCommand::Add(args) => Op::RegisterManager(args),
                    ManagerCommand::List => Op::ListManagers,
                    ManagerCommand::Remove(args) => Op::UnregisterManager(args),
                },
            },
            Cmd::Merge(args) => Op::land(args),
            Cmd::Remove(args) => Op::Drop(args),
            Cmd::Prune(args) => Op::Gc(args),
            Cmd::Restore(args) => Op::Restore(args),
            Cmd::Lock { workspace, reason } => Op::Lock { workspace, reason },
            Cmd::Unlock(args) => Op::Unlock(args),
            Cmd::Move { workspace, path } => Op::Move { workspace, path },
            Cmd::Repair { workspace, path } => Op::Repair { workspace, path },
            Cmd::Heartbeat(args) => Op::Heartbeat(args),
        }
    }
}
