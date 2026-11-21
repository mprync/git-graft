# Changelog

All notable changes to git-graft are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-21

First public release.

### Added

- Grafts: `git graft add` creates a standalone Git repository next to your checkout with its own objects, branches, reflogs, stash and index. On APFS, Btrfs and XFS the files are copy-on-write clones, so a graft stores only what you change. Each reused file is verified to hold the bytes `git checkout` would write; anything unproven comes from Git.
- History modes: `--history local` (default), `isolated` and `tip`, or the `graft.history` setting.
- Shared dependencies: `git graft deps share` shares an installed `node_modules` with new grafts as writable, space-sharing copies. `deps add` adds packages, and other package managers can be registered.
- Merging: `git graft merge` imports a graft's commits into `main` or `--into <branch>`, reports conflicts before changing anything, and archives the graft. `--undo` appends a reversal commit.
- Archive and restore: `merge`, `remove` and `prune` archive a graft instead of deleting it; `git graft restore` reopens it.
- Leases: a secret per graft proves ownership; leases can be renewed and expire, and `prune` archives expired grafts.
- Also: `list`, `status`, `exec`, `lock`, `unlock`, `move`, `repair` and `doctor`. Every command prints JSON and accepts `--help`; a manual page ships with the crate.
- MCP server (`git-graft mcp`, stdio) with nine tools: `graft_add`, `graft_status`, `graft_exec`, `graft_merge`, `graft_remove`, `graft_restore`, `graft_list`, `graft_renew` and `graft_add_dependency`. Execution and dependency tools require `--allow-exec`.
- An agent skill in `skills/git-graft/SKILL.md`.
- Platforms: macOS (Intel, Apple silicon), Linux (x86_64, aarch64; static musl) and Windows (x64, ARM64). Disk savings need APFS, Btrfs or XFS; on ext4 and NTFS grafts work but files are full copies. Requires Git 2.46 or newer.
- Installers: `install.sh` and `install.ps1` (checksum and provenance verified), Homebrew, Scoop, Chocolatey, winget, `.deb`, `.rpm`, `.apk` and Arch packages, and `cargo install --locked git-graft`. Releases publish `SHA256SUMS` and signed build provenance.

### Safety

- `remove` refuses unmerged commits, branches, tags and stashes (`unmerged_work`); `remove --discard` after a landed merge reports `unmerged_commits`, and a merge whose graft gained commits keeps it active (`"removed": false`).
- A damaged record (`record_damaged`) no longer blocks other grafts or prune, a moved primary fails closed (`primary_moved`), and archived leases name how to restore (`graft_archived`).
- Interrupted checkout updates resume or reverse from the files on disk, stale staging locks are reclaimed, and `exec` background jobs keep their graft busy.
- Merges refuse paths that the filesystem folds onto ignored files by Unicode case or normalization.
- Cache purges keep saved and registered layers, `deps layer remove` retires layers to `.git/graft/retained-layers/`, and restore reports `already_exists` and `archive_damaged` without touching the archive.

[Unreleased]: https://github.com/mprync/git-graft/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/mprync/git-graft/releases/tag/v0.1.0
