# git-graft

Give each coding agent its own Git branch, checkout and build outputs, with a suggested port unique among active workspaces in the project. On filesystems with copy-on-write support, worktrees can share unchanged source data. Agents and people use the same Rust binary through `git graft` or a local MCP server.

## Try it

From a fresh clone of this project, install the Git subcommand with Rust 1.89 or newer. Use Git 2.46 or newer for retirement and recovery. `git graft doctor` checks the installed Git's transaction support.

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

The optional `--commit` stages all nonignored changes in the agent's workspace. Without it, land expects the agent's changes to be committed. A successful land returns the integrated commit and a recovery lease, then releases the workspace name. A conflict returns the affected paths and preserves the workspace.

## What is shared

| Resource | Behavior |
| --- | --- |
| Source files | On eligible volumes, even the first workspace can share the primary checkout's existing data blocks. Graft checks candidate bytes against the requested Git revision and checkout settings. A verified snapshot can supply later workspaces at the same commit or its child. |
| Other filesystems and sparse checkouts | Git populates an ordinary private worktree. Use repeated `--sparse DIR` arguments to request cone-mode sparse checkout. The receipt records the selected backend and fallback reason. |
| Package downloads | npm, pnpm, Bun, Cargo and uv have project-scoped caches. Installs run on request. An empty package list requests a locked install, sync or fetch, depending on the manager. |
| Installed Node packages | Seed a relocatable install from the primary checkout with `git graft seed-layer --key KEY`, or save one from a managed workspace. Restore it with `git graft layer NAME restore --key KEY`. Each workspace can edit its own files. |
| Build and runtime state | Scoped commands receive a private Cargo target/build directory, a private uv environment and a project-unique suggested `PORT`. Cargo can use a shared sccache when available. |
| Local developer data | Source snapshots come from committed content. Untracked and ignored files from the developer's checkout stay there. Committed secrets and symlinks retain normal Git behavior. |

On macOS APFS, two agents starting from one populated checkout and reusing its npm install needed **0.69 MiB of additional disk**, compared with **67.77 MiB** for native Git worktrees and npm. Median paired savings were **98.99%** over three runs. The second spawn took 0.279 s with Graft and 0.067 s with native Git. On NTFS and Linux volumes without reflinks, a full source checkout can cost as much storage as plain Git. [Workload, timings and platform verification](docs/verification.md)

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

Garbage collection previews stale, clean workspaces and verified unused source snapshots. A dirty, locked, busy or unmerged workspace is retained. After a land or drop, `git graft restore NAME --lease RECOVERY_LEASE` reopens the workspace. To reverse a land while its destination still points to the landed commit, `git graft undo-land NAME --lease RECOVERY_LEASE` appends a reversal commit. Recovery data has no automatic expiry. [File safety, permissions and recovery](docs/safety.md)

## Develop and verify

```sh
cargo test --locked
npm test
npm run bench
npm run bench:workflow
npm run coverage
```

The TypeScript end-to-end suite builds the release binary, drives real Git repositories and MCP clients, and writes a JSON report under `target/e2e/`. It requires the package managers it exercises. The ordinary CLI and MCP server require only the binary, Git and whichever tools a workspace uses. [Test prerequisites and coverage definitions](docs/verification.md)

[Operations and recovery](docs/operations.md) · [Review of the proposed architecture](docs/research.md) · [Contributing](CONTRIBUTING.md) · [MIT license](LICENSE)
