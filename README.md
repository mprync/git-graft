<div align="center">

# git-graft

**Run hundreds of coding agents on one repository without filling your disk.**

A faster, lighter git worktree alternative for coding agents such as Claude Code, Codex and Cursor. Each graft is a full Git repository that stores only what you change.

[![CI](https://github.com/mprync/git-graft/actions/workflows/ci.yml/badge.svg)](https://github.com/mprync/git-graft/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/mprync/git-graft)](https://github.com/mprync/git-graft/releases)
[![crates.io](https://img.shields.io/crates/v/git-graft)](https://crates.io/crates/git-graft)
[![MSRV: Rust 1.89](https://img.shields.io/badge/MSRV-Rust%201.89-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/scale.dark.svg">
  <img alt="100 checkouts of the same repository: git worktree uses 2.8 GiB, git graft uses 27.8 MiB, 99.0% less" src="docs/assets/scale.light.svg" width="520">
</picture>

</div>

## Why git-graft

- **99% less disk.** A graft shares every unchanged file with your checkout and stores only the files you edit. A hundred grafts cost less than one worktree.
- **No copied dependencies.** Install `node_modules` once; every graft gets a writable copy that takes almost no extra space and skips `npm ci`.
- **Faster than a worktree.** A graft is ready to work 17% sooner, and ready to run tests 3.1× sooner when dependencies are involved.
- **Built for agents.** A built-in MCP server lets agents create, work in and merge their own grafts, each in a private repository that can't touch yours.
- **Nothing gets lost.** Merging and removing archive a graft instead of deleting it, so any graft can be restored.

### Where the disk savings apply

| Platform | Disk savings |
| --- | --- |
| macOS (APFS) | Yes |
| Linux (Btrfs, XFS) | Yes |
| Linux (ext4), Windows (NTFS) | No: grafts work, but files are full copies, as with a worktree |

## Who it's for

- Anyone running several coding agents at once, each needing its own checkout.
- People reviewing pull requests while an agent works, without disturbing their own checkout.
- Large monorepos, where a full checkout per task is slow and expensive.

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
| Cargo | `cargo install --locked git-graft`, or from source: `cargo install --locked --git https://github.com/mprync/git-graft` |

Graft needs Git 2.46 or newer. The install scripts check every download against the release checksums. [More install options](docs/install.md)

## Quick start

```sh
git graft add fix-parser                  # prints the graft's path and its lease
cd ../myrepo.graft/fix-parser             # a normal Git repository: edit, test, commit
git graft merge fix-parser --lease <lease> # merge the commits into main
```

The **lease** is a secret that proves the graft is yours; keep it out of logs. `git graft add` prints it with the graft's path (trimmed):

```json
{
  "name": "fix-parser",
  "branch": "graft/fix-parser",
  "path": "/home/you/myrepo.graft/fix-parser",
  "lease": "b0ff586d-107c-443a-8b97-067c9becb1da",
  "status": "active"
}
```

That's it. `git graft list` shows your grafts, `git graft merge --into <branch>` merges somewhere other than `main`, and `git graft restore` brings back any graft you merged or removed.

**Share dependencies once, use them everywhere:**

```sh
npm ci                                   # in your checkout, as usual
git graft deps share --key node24           # share this install with new grafts
git graft add add-tests                     # node_modules is already there
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

Medians from the release build on macOS with APFS, comparing `git worktree add` (plus `npm ci` where dependencies are involved) with `git graft add` on the same repositories. In very small repositories, under about two thousand files, a worktree is still quicker to create; the disk savings hold at every size. [Method, raw data and how to rerun every benchmark](docs/benchmarks.md)

## Use it with coding agents

Graft includes an MCP server. Point your agent at it with the repository path. The examples use `/absolute/path/to/project`.

**Claude Code**

```sh
claude mcp add git-graft -- git-graft --repo /path/to/project mcp
```

**Codex** (`~/.codex/config.toml`)

```toml
[mcp_servers.git-graft]
command = "git-graft"
args = ["--repo", "/absolute/path/to/project", "mcp"]
```

**Cursor** (`.cursor/mcp.json` in the project, or `~/.cursor/mcp.json` for every project)

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

**Any other MCP client** uses the same JSON: a stdio server whose command is `git-graft` and whose arguments are `--repo <path> mcp`.

Agents get nine tools: `graft_add`, `graft_status`, `graft_exec`, `graft_merge`, `graft_remove`, `graft_restore`, `graft_list`, `graft_renew` and `graft_add_dependency`. Each agent works in its own graft and proves ownership with a secret lease, so agents can't interfere with each other or with your checkout. `graft_exec` and `graft_add_dependency` run commands, so they appear only when you add `--allow-exec` after `mcp`. [Agent guide](skills/git-graft/SKILL.md)

## Graft vs worktree

| | `git worktree` | `git graft` |
| --- | --- | --- |
| Disk per checkout | A full copy of every file | Only the files you change |
| Dependencies | Installed again in every checkout | Installed once, shared by every graft |
| Branches, stash, reflog | Shared with your checkout; one branch per worktree | Private to each graft; any branch name |
| Your repository | Shared object store and lock files | Never written to while a graft works |
| Removing a checkout | Deleted | Archived; restore it any time |
| Agent support | None | Built-in MCP server with leases and retries |

## FAQ

**How is this different from `git worktree`?**
A worktree shares your repository's object store, refs and stash, allows one checkout per branch, and writes a full copy of every file. A graft is a standalone repository with its own objects, branches, reflogs, stash and index, so any branch name works and your repository is not written to while the graft is in use. On a copy-on-write filesystem it stores only the files you change. See [Graft vs worktree](#graft-vs-worktree).

**Why not `cp -c` or `cp --reflink`?**
Cloning a directory copies everything in it: untracked and ignored files, local edits and Git's internal state, with nothing proving the files match a commit. A graft starts from a commit you choose, never copies untracked or ignored files, checks that each reused file holds exactly the bytes `git checkout` would write, and falls back to Git when it can't prove that or the filesystem can't clone. It also adds leases, merge, archive and restore, and the MCP server.

**Why not `git clone --shared` or `--reference`?**
Both borrow objects from the original repository but still write every working file in full, so they save no checkout space, and the clone depends on the original's object store staying put. A graft shares file data on the filesystem, and its default local history takes a snapshot of the objects in Git's own format, so it stays readable if the original runs garbage collection or moves.

**What about jj workspaces?**
Jujutsu workspaces need `jj` and still write a full working copy for each workspace. git-graft works in an ordinary Git repository with your existing Git tools, and shares unchanged file data between checkouts.

**What happens on ext4 and NTFS?**
Grafts work, but those filesystems can't share file data, so each graft's files are full copies, as with a worktree. You still get private repositories, leases, archive and restore, and the MCP server. `git graft doctor` reports whether your filesystem supports sharing.

**Does it work with monorepos and large repositories?**
Yes. The disk savings hold at every size, and sparse checkout and `--history tip` keep each graft small. In very small repositories, under about two thousand files, `git worktree add` is still quicker to create. See the [benchmarks](docs/benchmarks.md).

**Is my work safe if something crashes?**
Creating a graft never changes your checkout. Merging reports conflicts before changing anything, and `merge`, `remove` and `prune` archive a graft rather than deleting it, so you can restore it or undo a merge. An interrupted operation can be retried with the same lease. Archives are not backups: keep your own backups for disk failure and accidental deletion. See [safety and recovery](docs/safety.md).

## Commands

| Command | What it does |
| --- | --- |
| `git graft add <name>` | Create a graft from a commit (default: your current `HEAD`) |
| `git graft list` / `status` | Show grafts, or one graft's branch and changed files |
| `git graft merge <name>` | Merge a graft's commits into `main` (or `--into <branch>`) and archive it; `--undo` reverts it |
| `git graft remove <name>` | Archive a graft |
| `git graft prune` | Archive grafts whose lease ran out |
| `git graft restore <name>` | Reopen an archived graft |
| `git graft deps` | Share installed dependencies, add packages or register other package managers |
| `git graft exec` | Run a command inside a graft |
| `git graft lock` / `unlock` / `move` / `repair` | Protect, relocate or reconnect a graft |
| `git graft doctor` | Check that Git and your filesystem support sharing |

Every command prints JSON and accepts `--help`. [Full reference](docs/operations.md)

## How it works

`git graft add` creates a standalone Git repository next to your checkout. Instead of writing every file, it asks the filesystem to clone them: the graft's files point at the same data blocks as your checkout's, and only a file you edit gets blocks of its own. Graft proves each reused file holds exactly the bytes `git checkout` would write, using Git's own index check or a hash, and falls back to Git for anything it cannot prove. It records file metadata in the new index, so the first `git status` reads no file contents. [Workspaces and dependency sharing](docs/workspaces.md)

## Safety

- Creating a graft never changes your checkout's files, index or branches, and never copies untracked or ignored files.
- Merging reports conflicts before changing anything, and every destructive step archives first, so it can be undone.
- Graft runs Git hooks and package scripts with your permissions; use it with repositories and agents you trust.

[Safety model](docs/safety.md) · [Security policy](SECURITY.md)

## Contributing

```sh
cargo test --locked   # unit and integration tests
npm ci && npm test    # end-to-end CLI and MCP scenarios
```

[Contributing guide](CONTRIBUTING.md) · [Verification](docs/verification.md) · [MIT license](LICENSE)
