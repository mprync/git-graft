# Operating Graft workspaces

## Identity and lifecycle

`git graft spawn NAME --base REV` creates a standalone repository with its own Git objects, refs, index and working files. The result gives its `path` and an active `lease`. A name can be reused, so keep the lease with the task and share it only with trusted local tools. Avoid including leases in shared logs or reports. CLI mutations accept `--lease` or `GIT_GRAFT_LEASE`; MCP mutations take `lease`. Use `git graft list` and `git graft status NAME` to inspect workspaces. Run `git graft --help` and `git graft COMMAND --help` for the available options.

Ordinary Git commands operate on the workspace's private repository. A branch or stash created there is local to it; landing transfers the selected committed work into the original repository. A nested spawn can use a parent workspace's private commit as its base. Shallow repositories retain the selected commit’s locally available history and relevant Git shallow boundaries. Graft does not fetch missing history. Landing uses the destination’s existing boundaries and fails before changing it if ancestry cannot be resolved. Replacement refs, legacy Git grafts and repository-local checkout filters are declined.

Use `lock` and `unlock` to protect an active workspace from moves and retirement. `move` changes its path on the same filesystem. After a manual rename, `repair` verifies its recorded identity before accepting the new path. `gc` previews eligible cleanup and `gc --apply` applies it.

## Run commands and install dependencies

```sh
git graft exec task --lease "<lease>" -- cargo test --locked
git graft add-dep task --lease "<lease>" --manager npm
git graft add-dep task --lease "<lease>" --manager pnpm zod
git graft env task
```

An empty package list requests a locked installation or fetch. Package managers run with project-scoped caches; runtime and build paths are private to the workspace. `--offline` can fail when the cache lacks required artifacts. Package scripts are local programs. MCP execution and dependency tools require the host's `--allow-exec` option.

## Reuse an npm installation

From a primary checkout with a trusted, relocatable install matching the committed lockfile, register it once:

```sh
git graft seed-layer --shared --key node-and-npm-version-and-install-options
git graft spawn task --base main
```

The key attests to the toolchain, options and relevant environment. The install must contain no private data and be usable after source-only edits. Compatible full spawns on a clone-capable volume attach a private writable view; inspect `storage.dependencies.status`. An unavailable or mismatched layer is not installed automatically. Use `--no-deps` for a source-only spawn. Sparse workspaces and MCP hosts without `--allow-exec` do not attach the install.

For an exact-source manual snapshot, use `seed-layer --key KEY` or `layer NAME save --key KEY`, then `layer NAME restore --key KEY`. Manual transfers require copy-on-write unless `--copy` is explicitly supplied. Review `--help` for leases and options. Treat installed layers as executable content shared within one trust domain.

## Land and recover

Land integrates committed work by fast-forward, merge or `--squash`, subject to the destination's state. `--commit MESSAGE` stages nonignored workspace changes first; use it only when all such changes belong in the task. If `--commit` detects a path with both staged and unstaged changes, it returns `partially_staged` without replacing the staged version. Commit the staged version or unstage it explicitly before retrying. This also applies to a staged deletion whose path has been recreated. Conflicts remain available for inspection. `--reconcile` supports a narrow offline npm lockfile conflict case; other conflicts need normal resolution. Ignored files block a normal land; `--discard-ignored` retains them in the archived workspace.

Retain the `recovery_lease` from land or drop. `restore NAME --lease TOKEN` reopens the archive and issues a fresh active lease. `undo-land NAME --lease TOKEN` appends a reversal commit only while the destination is still at the recorded landed commit. `list` includes recoverable workspaces. Archives retain repository and local file state, which can include credentials and other sensitive user data, and can occupy disk indefinitely. See [safety and recovery](safety.md).

For a disconnected spawn, retry with the same client-generated, cryptographically random UUIDv4 `request_id` and the same options. The request ID acts as the workspace lease; keep it secret and share it only with trusted local tools. `heartbeat` renews a task lease. Close external editors and builds before retirement, especially on Windows where open handles can prevent a move. Graft locks coordinate Graft operations, not arbitrary external processes.

## When two tasks edit the same file

Two grafts created from the same commit each get a writable checkout of that version. Saving or committing a file in one leaves the other graft and the primary at their own versions. When both tasks land into the same branch, the first successful landing updates that branch. Graft uses Git's three-way merge for a second, diverged task. Edits to separate parts of a file can merge automatically.

If Git reports conflicting edits, Graft returns the paths and keeps the second workspace. From that workspace, fetch the current destination and merge it locally. This example uses a destination named `main`.

```sh
git fetch --no-tags /absolute/path/to/primary refs/heads/main
git merge FETCH_HEAD
git status
```

Resolve any conflicts, stage the resolutions, commit the merge and run the project's tests. From the primary, retry `git graft land NAME --into main --lease TOKEN` with the same workspace lease.

## Interrupted checkout update

A checked-out destination interrupted during land or undo can return `checkout_recovery_required` with `target`, `previous`, `commit` and `recovery_path`. Before editing the destination, retry the same command with the same lease. Graft can finish a verified update or restart it from the saved original index and a clean previous checkout.

If recovery still stops, preserve copies of the current files and index before investigating. Inspect Git status and `index.before` in the reported recovery path (when an original index existed). Restore the previous clean checkout and saved index only after preserving any changes, then retry the same command. The workspace cannot be dropped while its checkout update is pending.
