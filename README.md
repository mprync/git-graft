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
| Dependencies | Package downloads can be shared across grafts. Supported filesystems can also share the disk blocks of a trusted installation while giving each graft its own writable files. |
| Recovery | Land and drop archive the complete workspace, including its local Git state and ignored files. `git graft gc` previews eligible cleanup. |

For a trusted local clone, `git graft spawn fix-parser --history local` can reuse the existing Git packs, their indexes and eligible source files through copy-on-write. Each graft still owns its files and can commit independently. The local snapshot includes other branches’ objects, staged blobs and dangling objects. Use `--history isolated` to limit readable objects to the selected history. The JSON field `storage.objects.source` reports `local-snapshot` when the filesystem and repository support it. [History and recovery](docs/safety.md#workspace-contents)

For just one commit and its files, use `git graft spawn fix-parser --base main --history tip`. A branch name or commit ID also works. The graft owns its index and can stage, commit and land changes. Its Git history starts at the selected commit.

Set `git config --local graft.history tip` or `git config --local graft.history local` once in the primary repository to select the policy for ordinary CLI and MCP spawns. Use `git config --local --unset graft.history` to restore the isolated default.

For an installed npm project with a committed, supported lockfile, register the installation once. Choose a compatibility key that identifies the Node and npm versions, install options and environment used to create it.

```sh
git graft seed-layer --shared --key node24-npm11-default
git graft spawn test-parser --base main
```

Inspect `storage.dependencies.status` in the spawn result to see whether Graft attached the npm installation. `git graft add-dep` supports npm, pnpm, Bun, Cargo and uv.

Agents can register any package manager that accepts a cache directory. For example, register NuGet once in a project and every graft running `dotnet` through Graft will use that project’s package cache.

```sh
git graft register-manager dotnet --cache-env NUGET_PACKAGES
git graft exec test-parser --lease "<workspace lease>" -- dotnet restore
```

Managers can also register a relocatable installed directory, its tracked inputs and a toolchain key. Compatible future grafts can use the installation immediately. [Register a package manager](docs/operations.md#register-a-package-manager) · [Workspace compatibility](docs/workspaces.md)

## Performance

On macOS/APFS, a first graft from a clean Homebrew clone configured for local history took **0.55 s** and added **0.33 MB** of unique file data. A native worktree took **0.34 s** and added **29.94 MB**. Two grafts together added **0.64 MB**, a **98.93%** saving. Source files and Git history added zero unique file blocks in this fixture. The measurement excludes filesystem metadata.

Including the first `git status`, the clean checkout took **0.97 s** for Graft and **0.46 s** for a native worktree. On the Homebrew fixture with staged, unstaged and untracked changes, the earlier isolated-history release took **6.06 s** for its first spawn. The local-history release took **0.55 s**. [Benchmarks and raw measurements](docs/benchmarks.md)

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
