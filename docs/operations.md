# Operating managed worktrees

## Workspace identity and paths

For a primary repository at `/code/project`, workspaces live at `/code/project.graft/NAME`. All entry points, including linked checkouts and bare repositories, discover the common Git directory. The shared registry is in `$GIT_COMMON_DIR/graft`. Git owns branch registration, the per-worktree index and worktree removal. Graft checks the registration, branch, private Git directory and ownership marker before operating on a workspace.

A name identifies a slot. A lease identifies one use of that slot. Keep the lease returned by spawn with the task. CLI mutations accept `--lease` or `GIT_GRAFT_LEASE`. MCP mutations take a `lease` argument. Leases coordinate clients that share the same local account. That account can read the registry.

## Run and install

```sh
git graft exec task -- cargo test --locked
git graft add-dep task --manager npm
git graft add-dep task --manager pnpm zod
git graft add-dep task --manager uv --offline
git graft env task
```

Use argv arrays over MCP. Child stdout and stderr are captured separately with a 256 KiB limit per stream and truncation flags. A failed child yields its exit code, output and `success: false`. The CLI propagates the exit status and MCP sets `isError`. Commands should finish and close their output streams.

| Manager | Empty package list | Private output | Shared storage |
| --- | --- | --- | --- |
| npm | `ci` | Worktree `node_modules` | npm content cache |
| pnpm | Frozen install | Worktree installation using clone-or-copy imports | pnpm store |
| Bun | Frozen install | Worktree installation using the copyfile backend | Bun package cache |
| Cargo | `fetch --locked` | Workspace-specific target and intermediate directories | Cargo home, optional sccache |
| uv | `sync --locked` | Workspace-specific virtual environment using copy mode | uv cache |

Supplying packages uses the manager's add/install operation. `--offline` requests offline resolution. Missing cache entries can make it fail. Install scripts are ordinary local programs. Bun's strict offline mode requires a version that advertises it. On Windows, the npm adapter uses the Node installation's adjacent `npm-cli.js`, and the pnpm adapter uses a native `pnpm.exe`.

## Reuse an installed Node tree

If the primary checkout already has the install you want to reuse, run the following while its tracked source is clean. The supplied key attests that the installed files match the committed lockfile, are relocatable, contain no private data, and were built with the toolchain and options represented by the key.

```sh
git graft seed-layer --key node24-npm11-production
git graft spawn task
git graft layer task restore --key node24-npm11-production --lease "<task lease>"
```

To publish an install prepared in a managed workspace instead, use its lease.

```sh
git graft layer task save --key node24-npm11-production
git graft layer other restore --key node24-npm11-production --lease "<other lease>"
git graft layer task forget --key node24-npm11-production
```

Save after installing a known relocatable package tree. Each operation requires clean tracked source and a single tracked root manifest and lockfile. The digest includes the committed source tree, manifest and lock content, OS, CPU architecture and your key. Include ABI, manager version, install flags and relevant environment in that key. It is conservative across source changes. Workspace monorepos, external package links and nonrelocatable outputs should use their package manager.

Save verifies package links, file contents and modes. Restore validates the saved tree, prepares the installation privately and publishes it into an absent `node_modules`. `--copy` requests ordinary independent files. Treat cached installations as executable content shared by members of the same trust domain.

## Land and resolve conflicts

Land computes a merge from commit IDs. It fast-forwards when possible, creates a two-parent commit for clean divergence, or creates a single-parent result with `--squash`. A checked-out destination must pass Git's working-tree checks before a native fast-forward. An unchecked local branch advances through a compare-and-swap ref update. Finish any merge/rebase in the destination first. Git's hooks still apply. Coordinate external branch switches, edits and ref updates with the landing operation.

A `merge_conflict` includes paths and Git conflict records. Resolve the conflict in the task branch and retry. With `land --reconcile` a supported root npm lockfile-v3 conflict can be reconciled offline for independent, additive changes. It uses a private index and a temporary resolver directory, disables npm scripts, and checks the generated resolution. Incompatible versions, unsupported project layouts, unrelated conflicts and incomplete cache data remain conflicts. Other ecosystems use their normal resolver and review process.

Ignored files block a normal land. `land --discard-ignored` permits parking them with the retired workspace. A private runtime directory is retained for restoration. These files may continue to occupy disk after retirement.

On Windows, move terminals out of the workspace and close programs holding its files before cleanup. Windows can keep a directory open for as long as another process uses it as its working directory. Retry with the same lease after releasing the handle. [Windows current-directory behavior](https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-setcurrentdirectory)

## Recover a disconnected task

- Use a client-generated UUID with `spawn --request-id` or MCP `request_id`. Retry with the same name, UUID, base spelling and sparse paths to recover that instance.
- `status` reports the phase and registration health. `heartbeat` renews the 24-hour lease. Scoped commands hold a per-workspace OS lock, refresh the heartbeat and allow other workspaces to run concurrently.
- Retry a partially completed land with the same lease and target. An already integrated plan can finish cleanup. A terminal receipt lets a client replay a completed land or drop while the name is vacant. Receipts become eligible for GC after seven days.
- Keep the `recovery_lease` from land or drop. Run `git graft list` to see recoverable workspaces and `git graft restore NAME --lease TOKEN` to reopen one. The restored workspace gets a fresh active lease. To reverse the integration itself, run `git graft undo-land NAME --lease TOKEN` while the destination is still at the landed commit. [Recovery guarantees and boundaries](safety.md)
- If a tree was moved or detached with native Git, restore its recorded path and branch before retrying. If creation stopped before registration could be authenticated, inspect the path and branch with `git worktree list --porcelain`. Verify ownership before manually retiring either. A repository owner who lost a lease can inspect the corresponding private record in `graft/records`.
- `gc` previews. `gc --apply` can retire a stale workspace only after checking its Git state, merge reachability, locks and ownership. External editors and programs started outside Graft do not hold its locks. Renew the lease or use `git worktree lock` for a workspace that needs to outlive an unattended task.
