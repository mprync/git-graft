# git-graft

Graft gives each task a standalone local Git repository. You can fix a bug in your main checkout while an agent works on the same branch name in a graft. Each checkout has its own files, index, branches, stash and object database. When a task is ready, `git graft land` brings its commits back to a branch you choose.

Graft uses ordinary Git repositories and runs locally. You can open a graft in your editor, commit with Git and inspect its history even after the original checkout moves. Graft manages creation, integration and recovery through a command-line interface and a stdio MCP server.

## Install

Build from a clone of this repository with Rust 1.89 or newer. Git 2.46 or newer must be on your `PATH`.

```sh
cargo install --locked --path .
git graft --version
```

## Create a workspace and land a change

From a repository with a committed `main` branch, check the filesystem capabilities and create a workspace.

```sh
git graft doctor
git graft spawn fix-parser --base main
```

The JSON result includes an absolute `path` and a secret `lease`. Open the path in your editor and make your changes. Git commands inside that directory operate on the task's repository.

```sh
git status
git add -p
git commit -m "Fix parser"
```

From the original checkout, use the lease returned by `spawn` to land the commit. Substitute your destination branch if it has a different name.

```sh
git graft status fix-parser
git graft land fix-parser --into main --lease "<workspace lease>"
```

A successful land updates the destination and archives the workspace. Keep the returned `recovery_lease`. You can reopen the archive with `git graft restore fix-parser --lease "<recovery lease>"`. If the destination still points at the landed commit, `git graft undo-land` can append a reversal. Conflicts and local edits that need attention are reported before retirement. [Operations and recovery](docs/operations.md)

## What a workspace contains

| Resource | Behavior |
| --- | --- |
| Git history | The selected commit and its locally available ancestry. Private branches and stashes created in the graft stay there. |
| Source files | Files from the selected commit. Edits, staged changes and untracked files in the original checkout stay in place. |
| Storage | Graft reuses verified Git packs and eligible files through filesystem copy-on-write. It also supports native Git checkout and private copies. |
| Dependencies | Each task has private build outputs. Compatible tasks can start with a writable copy-on-write view of a trusted npm installation. |
| Recovery | Land and drop archive the complete workspace, including its local Git state and ignored files. `git graft gc` previews eligible cleanup. |

For an installed npm project with a committed, supported lockfile, register the installation once. Choose a compatibility key that identifies the Node and npm versions, install options and environment used to create it.

```sh
git graft seed-layer --shared --key node24-npm11-default
git graft spawn test-parser --base main
```

Inspect `storage.dependencies.status` in the spawn result to see whether Graft attached the installation. `git graft add-dep` supports npm, pnpm, Bun, Cargo and uv. [Dependency setup](docs/operations.md#reuse-an-npm-installation) · [Workspace options and compatibility](docs/workspaces.md)

## Performance

On macOS/APFS, the release created a standalone checkout of Homebrew’s 53,716-commit history in 3.86 seconds and repeated it in 0.57 seconds. Native `git worktree add` took 0.36 seconds for each. With a reusable npm installation, two active tasks added 3.53 MB of physical file data with Graft and 70.64 MB with native checkouts. Homebrew’s full private history used more disk than native Git. [Benchmarks, methodology and raw results](docs/benchmarks.md)

## Use with an MCP client

Configure a local stdio server with the installed binary and an absolute path to the original repository.

```json
{
  "mcpServers": {
    "git-graft": {
      "command": "git-graft",
      "args": ["--repo", "/absolute/path/to/project", "mcp"]
    }
  }
}
```

The host can grant command execution, dependency tools and npm lockfile reconciliation with `--allow-exec`. For retryable spawns, supply a cryptographically random UUIDv4 as `request_id`. It also acts as the workspace lease. Keep leases out of logs and issue reports. [Agent workflow](skills/git-graft/SKILL.md)

## Trust and development

Run Graft with trusted local repositories and clients. Git hooks, checkout filters and package scripts can run with your operating-system permissions. Archives may contain credentials and other private files. Protect the storage directory and keep independent backups of important work. [Safety](docs/safety.md) · [Security reporting](SECURITY.md)

Run `cargo test --locked` for the Rust suite. With Node.js 24.5 or newer and the documented package managers installed, `npm test` builds the release binary and exercises the CLI and MCP server in disposable repositories. [Verification](docs/verification.md) · [Contributing](CONTRIBUTING.md) · [MIT license](LICENSE)
