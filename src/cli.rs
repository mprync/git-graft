//! The command-line surface: clap definitions for every `git graft` command
//! except `mcp`, and their conversion into [`workspace::Command`].
use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::{api, workspace};

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Create a graft: a lightweight, independent copy of the repository.
    #[command(after_help = "\
Examples:
  git graft add fix-parser                     From the current HEAD, on branch graft/fix-parser
  git graft add hotfix --base v1.2.0           From another commit or tag
  git graft add docs --sparse docs             Check out only the docs directory
  git graft add api --include .env             Copy the untracked .env file too
  git graft add api --include '*'              Copy every untracked and ignored file

Untracked and ignored files stay behind unless --include names them, or the
project's .graftconfig or .graftconfig.local lists .gitignore-style patterns
under \"include\". Those files are read from the root of the primary checkout,
or else from the nearest directory above it that holds either, up to home when
the checkout is inside it, under \"repositories\" and the checkout's path
relative to that directory.")]
    Add(api::Spawn),
    /// List active and recoverable grafts.
    List,
    /// List the checkout's untracked and ignored files, which add --include can copy.
    LocalFiles,
    /// Show a graft's HEAD and changed files, or its environment with --env.
    Status(api::StatusArgs),
    /// Check Git support and whether grafts can share files.
    Doctor,
    /// Run a program inside a graft.
    #[command(after_help = "\
Examples:
  git graft exec fix-parser --lease <lease> -- npm test
  git graft exec fix-parser --lease <lease> -- cargo build --release")]
    Exec(api::Exec),
    /// Install and reuse dependencies.
    Deps(Deps),
    /// Show the hooks and remotes .graftconfig and .graftconfig.local define, and trust them.
    #[command(after_help = "\
Examples:
  git graft hook list    Show every hook, in the order it runs, its file, and
                         whether it is trusted, and the remotes a new graft gets
  git graft hook trust   Let the hooks the repository carries run, and the
                         remotes it lists be set up, as defined now

Hooks are defined by name in .graftconfig, which a project commits, and
.graftconfig.local, the person's own, at the root of the primary checkout:

  {\"hooks\": {\"database\": {\"event\": \"post-add\", \"argv\": [\"/usr/local/bin/create-db\"]}}}

Where the checkout has neither file, the nearest directory above it that holds
either, up to home when the checkout is inside it, configures it under
\"repositories\", by its path relative to that directory. A hook there may name
its program relative to that directory:

  {\"repositories\": {\"app\": {\"hooks\": {\"setup\": {\"event\": \"post-add\", \"argv\": [\"./scripts/setup\"]}}}}}

The event is post-add, pre-archive or post-restore. A local hook replaces the
shared hook of the same name, null turns it off, and a new name adds one.
A hook is started without a shell, in the graft, and is told about the graft
through GIT_GRAFT_HOOK, GIT_GRAFT_HOOK_NAME, GIT_GRAFT_NAME, GIT_GRAFT_PATH,
GIT_GRAFT_PRIMARY, GIT_GRAFT_CONFIG_DIR, GIT_GRAFT_BRANCH, GIT_GRAFT_BASE and
GIT_GRAFT_PORT. Its program is an absolute path, a bare name looked up in PATH,
or, in a directory above the checkout, a path inside that directory. Graft waits until
the hook's output closes, so a job it starts in the background must redirect
stdout and stderr. A pre-archive hook that fails stops remove, merge and prune
from archiving the graft, and can run more than once for the same graft.

A hook in .graftconfig, or in .graftconfig.local once Git tracks that file,
comes with the repository, so it runs only after git graft hook trust. So
does every hook from a directory above the checkout on Windows, which does not
check who owns that directory's files. Trust covers a program named relative
to a directory above the checkout by its contents too. Until then, and again
after any change to those hooks, add, remove, merge, prune --apply and restore
refuse with hooks_untrusted before changing anything.

The remotes those files list under \"remotes\", which every new graft gets,
need trust by the same rule, recorded apart from the hooks: until then, and
after any change to them, add and restore leave them out of the graft and
report them as skipped with the reason untrusted. There is no MCP tool that
trusts hooks or remotes.")]
    Hook(HookArgs),
    /// Merge a graft's commits into a local branch, then archive the graft.
    #[command(after_help = "\
Examples:
  git graft merge fix-parser --lease <lease>                   Into main
  git graft merge fix-parser --lease <lease> --into release    Into another local branch
  git graft merge fix-parser --lease <lease> --commit \"Fix\"    Commit pending edits first
  git graft merge fix-parser --lease <recovery_lease> --undo   Reverse the merge

A conflict changes nothing and keeps the graft. To resolve it, run in the graft:
  git fetch --no-tags <primary checkout> refs/heads/main && git merge FETCH_HEAD
then fix, commit and retry the merge with the same lease.

Uncommitted edits in the primary checkout stay as they are. Only edits to
paths the merge changes stop it, as destination_has_local_changes.")]
    Merge(api::Land),
    /// Push a graft's commits to a branch on a remote of the primary repository.
    #[command(after_help = "\
Examples:
  git graft push fix-parser --lease <lease>                   To origin, as the graft's branch
  git graft push fix-parser --lease <lease> --to fix/parser   To another branch name
  git graft push fix-parser --lease <lease> --remote fork     To another remote of the primary

The push runs in the primary repository, so its remote URL, credentials and
pre-push hook apply. Nothing is forced, and uncommitted work is not pushed.
The graft stays active. To finish, archive it with `remove --discard` and
keep the recovery lease: Graft does not treat pushed commits as merged.")]
    Push(api::Push),
    /// Archive a graft and its files. Refuses if it has unmerged work unless --discard is given.
    #[command(after_help = "\
Examples:
  git graft remove fix-parser --lease <lease>             A graft whose work is merged
  git graft remove fix-parser --lease <lease> --discard   Archive unmerged work too

Save the returned recovery_lease: `git graft restore` needs it.")]
    Remove(api::DropWorkspace),
    /// Archive every expired graft. Previews unless --apply is given.
    #[command(after_help = "\
Examples:
  git graft prune           List what would be archived, and why others are kept
  git graft prune --apply   Archive them")]
    Prune(api::Gc),
    /// Reopen an archived graft.
    #[command(after_help = "\
Example:
  git graft restore fix-parser --lease <recovery_lease>

Use `git graft list` to see recoverable grafts. Restore returns a new lease.")]
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
    /// Take turns on something the grafts share, such as a development server or database.
    Turn(Turn),
    /// Renew a graft's lease.
    #[command(hide = true)]
    Heartbeat(api::Owned),
}

#[derive(Debug, Args)]
pub struct Turn {
    #[command(subcommand)]
    command: TurnCommand,
}

#[derive(Debug, Subcommand)]
enum TurnCommand {
    /// Take the turn on a shared resource, join its queue, or renew a turn already held.
    #[command(after_help = "\
Examples:
  git graft turn take web --holder fix-parser --lease <uuid>             Ask once
  git graft turn take web --holder fix-parser --lease <uuid> --wait 600  Keep asking for ten minutes
  git graft turn take web --holder fix-parser --lease <uuid> --pid $PPID Lose the turn when the shell ends
  git graft turn take web --holder fix-parser --lease <uuid> \\
      --graft ../app.graft/fix-parser --graft ../api.graft/fix-parser    One turn for grafts of two repositories

The holder is the work that needs the resource, such as an agent or a task,
under a name it chooses. Its lease is a random UUIDv4 it chooses for its first
take and gives to every later take and release under that name. It loses the
turn and its place in the queue when the turn runs out, when the --pid process
ends, or when none of the --graft grafts is active any more.

Prints \"granted\": true when the holder holds the turn. Otherwise it is queued
and \"position\" is its place in line; ask again within 300 seconds to keep it.
Being queued is not a failure: the exit status is 0 either way. A resource the
configuration lists under \"turns\" in a directory above the primary is shared
by every repository that directory configures. A post-take hook in the
configuration runs when a turn is newly granted or its grafts change; if it
fails, the turn is given up and the take refused. Graft does nothing else to
the resource.")]
    Take(api::TurnTake),
    /// Give up the turn on a shared resource, or leave its queue.
    Release(api::TurnRelease),
    /// List shared resources that have a holder or a queue.
    List,
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
    #[command(after_help = "\
Example:
  npm ci
  git graft deps share --key node24   Grafts created afterwards get node_modules")]
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

#[derive(Debug, Args)]
pub struct HookArgs {
    #[command(subcommand)]
    command: HookCommand,
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// List the configured hooks, in the order they run, and the remotes a new graft gets.
    List,
    /// Let the hooks the repository carries run, and the remotes it lists be set up, as they are defined now.
    Trust,
}

impl From<Cmd> for workspace::Command {
    fn from(cmd: Cmd) -> Self {
        use workspace::Command as Op;
        match cmd {
            Cmd::Add(args) => Op::Spawn(args),
            Cmd::List => Op::List,
            Cmd::LocalFiles => Op::LocalFiles,
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
                    LayerAction::Remove(target) => Op::Layer("remove", target),
                },
                DepsCommand::Manager(ManagerArgs { command }) => match command {
                    ManagerCommand::Add(args) => Op::RegisterManager(args),
                    ManagerCommand::List => Op::ListManagers,
                    ManagerCommand::Remove(args) => Op::UnregisterManager(args),
                },
            },
            Cmd::Hook(HookArgs { command }) => match command {
                HookCommand::List => Op::ListHooks,
                HookCommand::Trust => Op::TrustHooks,
            },
            Cmd::Merge(args) => Op::land(args),
            Cmd::Push(args) => Op::Push(args),
            Cmd::Remove(args) => Op::Drop(args),
            Cmd::Prune(args) => Op::Gc(args),
            Cmd::Restore(args) => Op::Restore(args),
            Cmd::Lock { workspace, reason } => Op::Lock { workspace, reason },
            Cmd::Unlock(args) => Op::Unlock(args),
            Cmd::Move { workspace, path } => Op::Move { workspace, path },
            Cmd::Repair { workspace, path } => Op::Repair { workspace, path },
            Cmd::Turn(Turn { command }) => match command {
                TurnCommand::Take(args) => Op::TurnTake(args),
                TurnCommand::Release(args) => Op::TurnRelease(args),
                TurnCommand::List => Op::TurnList,
            },
            Cmd::Heartbeat(args) => Op::Heartbeat(args),
        }
    }
}
