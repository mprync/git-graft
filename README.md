# git-graft

Give each coding agent its own Git branch, checkout and build outputs, with a suggested port unique among active workspaces in the project. On filesystems with copy-on-write support, worktrees can share unchanged source data. Agents and people use the same Rust binary through `git graft` or a local MCP server.

## Try it

From a fresh clone of this project, install the Git subcommand with Rust 1.89 or newer.

```sh
cargo install --locked --path .
```

In a repository with at least one commit, run the following. The examples use a destination branch named `main`.

```sh
git graft doctor
git graft spawn fix-parser --base main
```

`spawn` returns the actual checkout path, branch, resolved commit, storage receipt and a lease for that instance. Edit the files in `worktree_path` with your usual tools. In a POSIX shell, export the lease once. In PowerShell, set `$env:GIT_GRAFT_LEASE` or pass `--lease` to each command.

```sh
export GIT_GRAFT_LEASE="<lease from spawn>"
git graft exec fix-parser -- git status --short
git graft status fix-parser
git graft land fix-parser --into main --commit "Fix parser handling of empty input"
```

The optional `--commit` stages all nonignored changes in the agent's workspace. Without it, land expects the agent's changes to be committed. A successful land returns the integrated commit and releases the checkout, private runtime and task branch. A conflict returns the affected paths and preserves the workspace.

## What is shared

| Resource | Behavior |
| --- | --- |
| Source files | A cold spawn builds a snapshot from a Git commit. Eligible macOS and Linux volumes clone files from it. A warm spawn at the same revision reuses the snapshot. A cached first parent can supply unchanged files for a newer revision. |
| Other filesystems and sparse checkouts | Git populates an ordinary private worktree. Use repeated `--sparse DIR` arguments to request cone-mode sparse checkout. The receipt records the selected backend and fallback reason. |
| Package downloads | npm, pnpm, Bun, Cargo and uv have project-scoped caches. Installs run on request. An empty package list requests a locked install, sync or fetch, depending on the manager. |
| Installed Node packages | Explicitly save and restore a relocatable `node_modules` layer between compatible workspaces. Regular files use independent CoW clones or byte copies. The compatibility key covers source, lockfile, platform and caller-specified toolchain or install settings. |
| Build and runtime state | Scoped commands receive a private Cargo target/build directory, a private uv environment and a project-unique suggested `PORT`. Cargo can use a shared sccache when available. |
| Local developer data | Source snapshots come from committed content. Untracked and ignored files from the developer's checkout stay there. Committed secrets and symlinks retain normal Git behavior. |

On APFS, five workspaces at the same 64 MiB commit used **64.33 MiB with Graft versus 320.91 MiB with Git** in the median of three runs. Five workspaces with an identical 32 MiB npm dependency saved **129.03 MiB**. Native Git finished both workloads faster. A full checkout on NTFS or Linux without reflinks can use as much source storage as plain Git. [Platform status, timings and disk measurements](docs/verification.md)

## Connect an agent

Use a stdio client configuration pointing at the installed binary and the absolute path to the repository.

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

Append `--allow-exec` to enable command execution, dependency installation, installed layers and opt-in offline lockfile reconciliation. Use trusted repositories and clients. Git hooks, filters and configured merge drivers can execute local code during Git operations. Worktrees share the repository and run with the operating system permissions of the server process.

Agents can generate a UUID and supply it as `request_id` to retry a spawn after a disconnect. Discovery exposes typed parameters, tool permissions and structured results. [Agent workflow skill](skills/git-graft/SKILL.md)

## Inspect and reclaim space

```sh
git graft list
git graft status fix-parser
git graft gc
git graft gc --apply
```

Garbage collection previews stale, clean workspaces and unused source snapshots. A dirty, locked, busy or unmerged workspace is retained. To intentionally discard an owned workspace, use `drop NAME --lease TOKEN --discard`. Shared package caches and installed layers can be reclaimed with `gc --purge-cache --apply` once all workspaces have retired.

## Develop and verify

```sh
cargo test --locked
npm test
npm run bench
npm run coverage
```

The TypeScript end-to-end suite builds the release binary, drives real Git repositories and MCP clients, and writes a JSON report under `target/e2e/`. It requires the package managers it exercises. The ordinary CLI and MCP server require only the binary, Git and whichever tools a workspace uses. [Test prerequisites and coverage definitions](docs/verification.md)

[Operations and recovery](docs/operations.md) · [Review of the proposed architecture](docs/research.md) · [Contributing](CONTRIBUTING.md) · [MIT license](LICENSE)
