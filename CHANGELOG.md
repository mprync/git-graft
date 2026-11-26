# Changelog

All notable changes to git-graft are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `add --include PATTERN` copies chosen untracked or ignored files, such as `.env`, into a new graft. Nothing is copied by default, and `local-files` lists what there is to choose from.
- `.graftconfig`, which a project commits, and `.graftconfig.local`, your own, set a project's default includes and hooks. One directory above several repositories can configure them all.
- Hooks run your own commands after `add` and `restore` and before a graft is archived, for setup and cleanup such as a per-graft database.
- `push` sends a graft's commits to a branch on the primary's remote, for pull request work. It never forces and won't update the default branch unless you name it.
- `turn take`, `turn release` and `turn list` let your work take turns on something there is only one of, such as a development server, across repositories, and a `post-take` hook can prepare it for each holder.
- `prune --purge-archives DAYS` deletes archives older than DAYS days.
- `remotes` in `.graftconfig` gives each new graft remotes of the primary, such as `origin`, so `git fetch` and `git push` work inside it. Credentials are never copied, and grafts added over MCP push only with `--allow-push`.
- MCP tools for the above: `graft_local_files`, `graft_turn_take`, `graft_turn_release`, `graft_turn_list`, and `graft_push` for hosts started with `--allow-push`.

### Changed

- On ext4 and other Linux filesystems without file cloning, a graft with local history hard-links the primary's Git objects instead of copying them: `add` takes 1.0 s instead of 16 s on a 186 MB repository, and uses 80 MB instead of 324 MB. Set `graft.history` to `isolated` to keep copying.
- `doctor` reports whether grafts share the primary's Git objects.
- Graft names may start with a number, such as a ticket ID.

### Safety

- Hooks and remotes that come with a repository are used only after you approve them with `git graft hook trust`, and again after any change to them. No MCP tool can approve them.
- MCP hosts run hooks only when started with `--allow-exec`.

## [0.2.0] - 2026-11-22

**Upgrade from 0.1.0:** after a restart, 0.1.0 can refuse every command with `graft namespace ownership is unverified`. 0.2.0 fixes this and upgrades existing state automatically. Don't downgrade afterwards.

**Behaviour changes that can affect scripts:**

- Merges into the checked-out branch now go ahead around uncommitted edits to other files.
- Some refusals return specific codes instead of `operation_failed`: `destination_has_local_changes`, `local_changes` and `dependencies_not_shareable`.

### Changed

- Merge and undo keep your uncommitted edits, as `git merge` does, and refuse only when an edit touches a file the merge changes.
- `remove` and `merge` list a graft's uncommitted, untracked and ignored files with their status.
- Grafts follow the primary's line endings, so `core.autocrlf=true` projects clone files instead of copying them.
- `add` right after a commit is faster: about 294 ms instead of 441 ms on 3,000 files.
- Several `add` commands now run in parallel instead of queuing: four at once take 1.0 s each instead of 2.0 s.
- `deps share` explains why a project can't share its dependencies and what to do instead.

### Fixed

- Commands no longer fail after a restart changes the disk's device number.
- A merge or undo no longer overwrites an ignored file created during the merge.
- A retry of an interrupted merge or undo now finishes on Linux when a file replaced a directory, or the reverse.
- `remove` and `merge` no longer count Graft's own unchanged shared `node_modules` as local work.
- Files committed with CRLF line endings are no longer reported as modified.
- The first `git status` in a new graft no longer reads file contents.

## [0.1.0] - 2026-11-21

First public release.

### Added

- `git graft add` creates a standalone Git repository next to your checkout. On APFS, Btrfs and XFS its files are copy-on-write clones, so a graft stores only what you change.
- `git graft merge` merges a graft into `main` or another branch and archives it; `--undo` reverses the merge.
- `git graft deps share` shares an installed `node_modules` with new grafts without duplicating it on disk.
- Archived grafts can be reopened with `restore`; leases prove which agent owns a graft.
- Also: `list`, `status`, `exec`, `remove`, `prune`, `lock`, `unlock`, `move`, `repair` and `doctor`, all with JSON output.
- An MCP server (`git-graft mcp`) and an agent skill.
- Builds for macOS, Linux and Windows; requires Git 2.46 or newer.
- Installs with Homebrew, Scoop, Chocolatey, winget, Cargo, Linux packages and install scripts.

### Safety

- `remove` refuses to delete unmerged commits, branches, tags or stashes.
- Interrupted adds and merges resume or reverse safely from the files on disk.
- A damaged record or a moved primary checkout fails safely without affecting other grafts.

[Unreleased]: https://github.com/mprync/git-graft/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/mprync/git-graft/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mprync/git-graft/releases/tag/v0.1.0
