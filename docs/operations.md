# Operating Graft workspaces

## Identity and lifecycle

`git graft spawn NAME --base REV` creates a standalone repository with its own Git objects, refs, index and working files. Routine CLI and MCP spawns honor the primary repository's `graft.history` setting and default to local history. Set `graft.history=isolated` when unrelated objects must be excluded, or choose `tip` for a shallow workspace containing only the starting commit and its files. The result gives its `path` and an active `lease`. A name can be reused, so keep the lease with the task and share it only with trusted local tools. Avoid including leases in shared logs or reports. CLI mutations accept `--lease` or `GIT_GRAFT_LEASE`; MCP mutations take `lease`. Use `git graft list` and `git graft status NAME` to inspect workspaces. Run `git graft --help` (the manual page) or `git graft -h`, and `git graft COMMAND --help`, for the available options.

Ordinary Git commands operate on the workspace's private repository. A branch or stash created there is local to it; landing transfers the selected committed work into the original repository. A nested spawn can use a parent workspace's private commit as its base. Shallow repositories retain the selected commit’s locally available history and relevant Git shallow boundaries. Graft does not fetch missing history. Landing uses the destination’s existing boundaries and fails before changing it if ancestry cannot be resolved. Replacement refs, legacy Git grafts and repository-local checkout filters are declined.

Use `lock` and `unlock` to protect an active workspace from moves and retirement. `move` changes its path on the same filesystem. After a manual rename, `repair` verifies its recorded identity before accepting the new path. `gc` previews eligible cleanup and `gc --apply` applies it.

## Run commands and install dependencies

```sh
git graft exec task --lease "<lease>" -- cargo test --locked
git graft add-dep task --lease "<lease>" --manager npm
git graft add-dep task --lease "<lease>" --manager pnpm zod
git graft env task
```

An empty package list requests a locked installation or fetch. `git graft add-dep` supports npm, pnpm, Bun, Cargo and uv. Graft configures project-scoped caches for its managed commands. `--offline` requires the requested artifacts to be available locally. Package scripts run with the user's permissions. MCP execution and dependency tools require the host's `--allow-exec` option.

| Package manager | Reuse across grafts | Per-workspace work |
| --- | --- | --- |
| npm | A compatible registered installation can attach automatically on a copy-on-write volume. The npm download cache is shared across managed commands. | Each graft has its own manifest, lockfile and writable installation. An addition can change transitive packages and metadata. |
| Cargo | `git graft exec` and `add-dep` use a project-level `CARGO_HOME` for crate downloads and Git sources. This needs no registration. | Each graft has its own manifests, lockfile, Cargo target and build directories. A build can compile the same crate in two grafts. |
| NuGet | Register `dotnet` with `NUGET_PACKAGES` to give SDK-style projects one package store per Graft project. NuGet can reuse several cached versions side by side. | Restore generates each project's `obj/project.assets.json`. Builds normally write under `bin/` and `obj/`. Legacy `packages.config` projects can also copy packages into a project directory. |

Adding a package in one graft changes that workspace’s declarations and resolved versions. Other grafts use a cached version when their own project files request it. Programs run directly from a terminal use the terminal’s environment. [Cargo cache](https://doc.rust-lang.org/cargo/guide/cargo-home.html) · [NuGet package storage](https://learn.microsoft.com/en-us/nuget/consume-packages/managing-the-global-packages-and-cache-folders) · [NuGet restore](https://learn.microsoft.com/en-us/nuget/consume-packages/package-restore)

## Register a package manager

An agent can teach Graft how a tool locates its cache once per project. Choose the executable name and supply the manager’s documented cache environment variable or command-line option. Graft creates an owned cache directory and passes its path on matching `git graft exec` calls. The first restore can populate this directory even if the same packages already exist in a machine-wide cache.

```sh
git graft register-manager dotnet --cache-env NUGET_PACKAGES
git graft register-manager mvn '--cache-arg=-Dmaven.repo.local={cache}'
git graft list-managers
git graft exec task --lease "<lease>" -- dotnet restore
git graft exec task --lease "<lease>" --manager mvn -- ./mvnw verify
```

The default variable is `GRAFT_PACKAGE_CACHE`. A registration can also set one manager-specific variable and prepend ordered arguments. Each `--cache-arg` represents one argument and `{cache}` expands to the cache directory. Executable matching uses the basename. Use `--manager` to select the same registration for a wrapper. The package manager controls version resolution, concurrent cache writes and the work needed to complete a restore. Supply cache settings suitable for shared use. [Maven local repository property](https://maven.apache.org/components/ref/4.0.0-rc-7/configuration.html)

### Reuse an installed directory

An agent with a trusted installed tree can register its location and every tracked file that determines the install. For example, a Ruby project that installs under `vendor/bundle` can register that tree from its primary checkout. Replace the example key with the actual runtime, package manager version, ABI and install settings.

```sh
git graft register-manager bundle --install-dir vendor/bundle \
  --input Gemfile --input Gemfile.lock --key ruby-3.4-bundler-2.6-default
```

The installed tree must be relocatable, free of private data and independent of unlisted inputs. Use a committed ignore rule for the entire installation directory so each graft inherits it. To register it from an existing graft, also supply `--workspace NAME --lease TOKEN`. Run registration again after a compatible install with new committed inputs to cache that version as well. Register a new key when external toolchain or install settings change.

During a full spawn, Graft checks the declared inputs, platform, key and destination ignore rules, then attaches verified files into a vacant path using native copy-on-write. Inspect each entry in `storage.registered_dependencies`. An `attached` status means the installed files are ready. With `inputs_mismatch` or `unavailable`, run the manager’s ordinary install in that graft. If a later ignore-rule edit exposes an attached package, `land --commit` asks you to restore the rule or commit the intended changes explicitly with Git. Cache routing still works on a filesystem that cannot clone files. Sparse, empty and `--no-deps` checkouts skip attachment.

`git graft unregister-manager NAME` removes the settings and retains cached files and existing installs. When all grafts are retired, `git graft gc --purge-cache` previews eligible caches and registered installations. Add `--apply` to remove verified entries. Changed or unrecognized generations remain available for inspection.

MCP exposes `workspace_list_managers`, `workspace_register_manager` and `workspace_unregister_manager`. Registration changes, command execution and automatic installation attachment require a server started with `--allow-exec`.

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
