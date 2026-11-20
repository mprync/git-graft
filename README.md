<div align="center">

# git-graft

**Run hundreds of coding agents on one repository without filling your disk.**

A faster, lighter replacement for `git worktree`. Each graft is a full Git repository that stores only what you change.

[![CI](https://github.com/mprync/git-graft/actions/workflows/ci.yml/badge.svg)](https://github.com/mprync/git-graft/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/mprync/git-graft)](https://github.com/mprync/git-graft/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![macOS · Linux · Windows](https://img.shields.io/badge/platforms-macOS%20·%20Linux%20·%20Windows-lightgrey)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/scale.dark.svg">
  <img alt="100 checkouts of the same repository: git worktree uses 2.8 GiB, git graft uses 27.8 MiB, 99.0% less" src="docs/assets/scale.light.svg" width="520">
</picture>

</div>

## Why git-graft

- **99% less disk.** A graft shares every unchanged file with your checkout and stores only the files you edit. A hundred grafts cost less than one worktree.
- **No copied dependencies.** Install `node_modules` once; every graft gets a writable copy that takes almost no extra space and skips `npm ci`.
- **Faster than a worktree.** A graft is ready to work 17% sooner, and ready to run tests 3.1× sooner when dependencies are involved.
- **Built for agents.** A built-in MCP server lets agents create, work in and land their own grafts, each in a private repository that can't touch yours.
- **Nothing gets lost.** Landing and pruning archive a graft instead of deleting it, so any graft can be restored.

## Install

**macOS and Linux**

```sh
curl -fsSL https://raw.githubusercontent.com/mprync/git-graft/main/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm https://raw.githubusercontent.com/mprync/git-graft/main/install.ps1 | iex
```

Or use a package manager:

| | |
| --- | --- |
| Homebrew | `brew install mprync/tap/git-graft` |
| Scoop | `scoop bucket add mprync https://github.com/mprync/scoop-bucket` then `scoop install git-graft` |
| winget | `winget install mprync.git-graft` |
| Chocolatey | `choco install git-graft` |
| Debian, Ubuntu, Fedora, Alpine, Arch | `.deb`, `.rpm`, `.apk` and Arch packages on the [releases page](https://github.com/mprync/git-graft/releases) |
| Cargo | `cargo install --locked --git https://github.com/mprync/git-graft` |

Graft needs Git 2.46 or newer. The install scripts check every download against the release checksums. [More install options](docs/install.md)

## Quick start

```sh
git graft new fix-parser                 # prints the graft's path and its lease
cd ../myrepo.graft/fix-parser            # a normal Git repository: edit, test, commit
git graft land fix-parser --lease <lease>  # bring the commits back to main
```

That's it. `git graft list` shows your grafts, and `git graft restore` brings back any graft you landed or pruned.

**Share dependencies once, use them everywhere:**

```sh
npm ci                                   # in your checkout, as usual
git graft seed --shared --key node24     # register this install for new grafts
git graft new add-tests                  # node_modules is already there
```

## The numbers

<table>
  <tr>
    <td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/disk.dark.svg"><img alt="Disk per checkout: git worktree 28.6 MiB, git graft 0.29 MiB, 98.9% less" src="docs/assets/disk.light.svg"></picture></td>
    <td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/dependencies.dark.svg"><img alt="node_modules per checkout: git worktree 85.4 MiB, git graft 0.16 MiB, 99.8% less" src="docs/assets/dependencies.light.svg"></picture></td>
  </tr>
  <tr>
    <td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/speed.dark.svg"><img alt="Ready to work: git worktree 550 ms, git graft 454 ms, 17% faster" src="docs/assets/speed.light.svg"></picture></td>
    <td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/speed-dependencies.dark.svg"><img alt="Ready to test with node_modules: git worktree 742 ms, git graft 237 ms, 3.1 times faster" src="docs/assets/speed-dependencies.light.svg"></picture></td>
  </tr>
</table>

Medians from the release build on macOS with APFS, comparing `git worktree add` (plus `npm ci` where dependencies are involved) with `git graft new` on the same repositories. In very small repositories, under about two thousand files, a worktree is still quicker to create; the disk savings hold at every size. [Method, raw data and how to rerun every benchmark](docs/benchmarks.md)

## Use it with coding agents

Graft includes an MCP server. Add it to your agent's MCP configuration:

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

Agents get nine tools: `graft_new`, `graft_status`, `graft_exec`, `graft_land`, `graft_prune`, `graft_restore`, `graft_list`, `graft_renew` and `graft_add_dependency`. Each agent works in its own graft and proves ownership with a secret lease, so agents can't interfere with each other or with your checkout. [Agent guide](skills/git-graft/SKILL.md)

## Graft vs worktree

| | `git worktree` | `git graft` |
| --- | --- | --- |
| Disk per checkout | A full copy of every file | Only the files you change |
| Dependencies | Installed again in every checkout | Installed once, shared by every graft |
| Branches, stash, reflog | Shared with your checkout; one branch per worktree | Private to each graft; any branch name |
| Your repository | Shared object store and lock files | Never written to while a graft works |
| Removing a checkout | Deleted | Archived; restore it any time |
| Agent support | None | Built-in MCP server with leases and retries |

## Commands

| Command | What it does |
| --- | --- |
| `git graft new <name>` | Create a graft from a commit (default: your current `HEAD`) |
| `git graft list` / `status` | Show grafts, or one graft's branch and changed files |
| `git graft land <name>` | Merge a graft's commits into a branch and archive it; `--undo` reverts a landing |
| `git graft prune <name>` | Archive a graft; `--expired` archives grafts whose lease ran out |
| `git graft restore <name>` | Reopen an archived graft |
| `git graft seed` / `deps` | Share installed dependencies; add packages or register other package managers |
| `git graft exec` | Run a command inside a graft |
| `git graft lock` / `unlock` / `move` / `repair` | Protect, relocate or reconnect a graft |
| `git graft doctor` | Check that Git and your filesystem support sharing |

Every command prints JSON and accepts `--help`. [Full reference](docs/operations.md)

## How it works

`git graft new` creates a standalone Git repository next to your checkout. Instead of writing every file, it asks the filesystem to clone them: the graft's files point at the same data blocks as your checkout's, and only a file you edit gets blocks of its own. Graft proves each reused file holds exactly the bytes `git checkout` would write, using Git's own index check or a hash, and falls back to Git for anything it cannot prove. It records file metadata in the new index, so the first `git status` reads no file contents. [Workspaces and dependency sharing](docs/workspaces.md)

| Platform | Disk savings |
| --- | --- |
| macOS (APFS) | Yes |
| Linux (Btrfs, XFS) | Yes |
| Linux (ext4), Windows (NTFS) | No: grafts work, but files are full copies, as with a worktree |

## Safety

- Creating a graft never changes your checkout's files, index or branches, and never copies untracked or ignored files.
- Landing reports conflicts before changing anything, and every destructive step archives first, so it can be undone.
- Graft runs Git hooks and package scripts with your permissions; use it with repositories and agents you trust.

[Safety model](docs/safety.md) · [Security policy](SECURITY.md)

## Contributing

```sh
cargo test --locked   # unit and integration tests
npm ci && npm test    # end-to-end CLI and MCP scenarios
```

[Contributing guide](CONTRIBUTING.md) · [Verification](docs/verification.md) · [MIT license](LICENSE)
