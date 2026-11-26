<div align="center">

# git-graft

**Run hundreds of coding agents on one repository without filling your disk.**

[![CI](https://github.com/mprync/git-graft/actions/workflows/ci.yml/badge.svg)](https://github.com/mprync/git-graft/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/mprync/git-graft)](https://github.com/mprync/git-graft/releases)
[![crates.io](https://img.shields.io/crates/v/git-graft)](https://crates.io/crates/git-graft)
[![MSRV: Rust 1.89](https://img.shields.io/badge/MSRV-Rust%201.89-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

[Install](#install) · [Quick start](#quick-start) · [Coding agents](#use-it-with-coding-agents) · [Benchmarks](#benchmarks) · [Documentation](#documentation)

<br>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/scale.dark.svg">
  <img alt="100 checkouts of the same repository: git worktree uses 2.8 GiB, git graft uses 27.8 MiB, 99.0% less" src="docs/assets/scale.light.svg" width="520">
</picture>

</div>

<br>

git-graft is a faster, lighter alternative to `git worktree`. It gives each task, or each coding agent, its own complete Git repository next to your checkout. Unchanged files share their disk blocks with your checkout, so a graft stores only what you change.

## Why git-graft

- **Almost no disk.** A hundred grafts of a repository take 27.8 MiB, where a hundred worktrees take 2.8 GiB.
- **Dependencies included.** Install `node_modules` once and every graft gets a writable copy that takes almost no space, without running `npm ci` again.
- **Fully isolated.** Each graft is a standalone repository with its own branches, index, stash and reflog. Any branch name works, and your repository isn't written to while a graft is in use.
- **Built for agents.** Claude Code, Codex, Cursor and any other MCP client create, work in and merge their own grafts through the built-in MCP server. A secret lease stops agents touching each other's work.
- **Safe to undo.** Merges report conflicts before they change anything, and finished grafts are archived rather than deleted, so you can restore them.
- **Plain Git.** It works with Git 2.46 or newer and the tools you already use, on macOS, Linux and Windows.

|                      | `git worktree`                          | `git graft`                                  |
| -------------------- | --------------------------------------- | -------------------------------------------- |
| Working files        | A full copy of every file               | Only the files you change[^clone]            |
| Installed dependencies | Installed again in every checkout     | Shared, and still writable                   |
| Branches             | One checkout per branch                 | Any branch, in any number of grafts          |
| Repository state     | Shares your objects, refs and stash     | Its own objects, refs, stash and index       |
| Finished work        | Deleted by `git worktree remove`        | Archived, and restorable                     |
| Coding agents        | Need their own tooling                  | Built-in MCP server                          |

[^clone]: On a filesystem that can clone files: APFS, Btrfs or XFS. See [supported platforms](#supported-platforms).

## Install

macOS and Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/mprync/git-graft/main/install.sh | sh
```

Homebrew:

```sh
brew install mprync/tap/git-graft
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/mprync/git-graft/main/install.ps1 | iex
```

<details>
<summary><b>Scoop, Cargo and Linux packages</b></summary>

<br>

Scoop:

```powershell
scoop bucket add mprync https://github.com/mprync/scoop-bucket
scoop install git-graft
```

Cargo:

```sh
cargo install --locked git-graft
```

There are `.deb`, `.rpm`, `.apk` and Arch packages on the [releases page](https://github.com/mprync/git-graft/releases). winget (`winget install mprync.git-graft`) and Chocolatey (`choco install git-graft`) packages follow once their reviews finish.

</details>

git-graft needs Git 2.46 or newer. The install scripts verify every download against the release checksums; see [installation](docs/install.md) for upgrades, uninstalling and verifying provenance.

## Quick start

```sh
# Create a graft from any Git checkout. It prints the graft's path and a lease.
git graft add fix-parser

# Work in it like any other repository.
cd ../myrepo.graft/fix-parser
git commit -am "Fix the parser"

# Back in your checkout, merge it into main.
git graft merge fix-parser --lease <lease>
```

The lease is a secret that proves the graft is yours, so keep it out of logs. Merging archives the graft and returns a `recovery_lease`, and `git graft restore fix-parser --lease <recovery_lease>` brings it back. Use `--into <branch>` to merge somewhere else, and `git graft list` to see every graft.

### Share dependencies

Install your dependencies once in your checkout, then share them:

```sh
npm ci
git graft deps share --key node24
```

Every graft you create afterwards gets a writable `node_modules` that takes almost no extra space.

### Bring local files

A graft starts with only the files Git tracks. Name the untracked or ignored files it needs, such as `.env`, with `.gitignore`-style patterns:

```sh
git graft add fix-parser --include .env --include 'config/*.local.json'
```

`--include '*'` brings all of them, and `git graft local-files` lists what there is. Copies are clones where the filesystem supports them, and never replace a tracked file.

To set defaults, list patterns under `include` in a `.graftconfig` file at the root of your checkout, or in `.graftconfig.local` for your own. The same files can define [hooks](docs/operations.md#hooks) that set up and clean up each graft, which run once you have read them and run `git graft hook trust`. One file can also [configure several repositories](docs/operations.md#configure-several-repositories-from-one-directory).

## Use it with coding agents

git-graft includes an MCP server. Give your agent the path to your repository.

**Claude Code**

```sh
claude mcp add git-graft -- git-graft --repo /path/to/project mcp
```

<details>
<summary><b>Codex</b>, in <code>~/.codex/config.toml</code></summary>

```toml
[mcp_servers.git-graft]
command = "git-graft"
args = ["--repo", "/path/to/project", "mcp"]
```

</details>

<details>
<summary><b>Cursor</b>, in <code>.cursor/mcp.json</code></summary>

```json
{
  "mcpServers": {
    "git-graft": {
      "command": "git-graft",
      "args": ["--repo", "/path/to/project", "mcp"]
    }
  }
}
```

</details>

Any other MCP client works the same way: a stdio server whose command is `git-graft` with the arguments `--repo <path> mcp`.

Each agent creates, works in and merges its own graft, and proves ownership with its lease, so agents can't interfere with each other or with your checkout. Two options, each a separate opt-in placed after `mcp`, give them more:

| Option         | Lets agents                                                                     |
| -------------- | ------------------------------------------------------------------------------- |
| `--allow-exec` | Run commands and install packages inside their grafts                           |
| `--allow-push` | Push a graft's commits to your checkout's remotes, for example to open a pull request |

The [agent guide](skills/git-graft/SKILL.md) describes the tools.

## Benchmarks

<p align="center">
  <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/disk.dark.svg"><img alt="Disk per checkout: git worktree 28.6 MiB, git graft 0.29 MiB, 98.9% less" src="docs/assets/disk.light.svg" width="49%"></picture>
  <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/dependencies.dark.svg"><img alt="node_modules per checkout: git worktree 85.4 MiB, git graft 0.16 MiB, 99.8% less" src="docs/assets/dependencies.light.svg" width="49%"></picture>
  <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/speed.dark.svg"><img alt="Ready to work: git worktree 550 ms, git graft 454 ms, 17% faster" src="docs/assets/speed.light.svg" width="49%"></picture>
  <picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/speed-dependencies.dark.svg"><img alt="Ready to test with node_modules: git worktree 742 ms, git graft 237 ms, 3.1 times faster" src="docs/assets/speed-dependencies.light.svg" width="49%"></picture>
</p>

Medians from the release build on macOS with APFS, comparing `git worktree add` (plus `npm ci` where dependencies are involved) with `git graft add` on the same repositories. In very small repositories, under about two thousand files, a worktree is still quicker to create, but the disk savings hold at every size. The [benchmarks page](docs/benchmarks.md) has the method, the raw data and how to rerun each one.

## Supported platforms

git-graft runs on macOS, Linux and Windows. How much a graft shares depends on the filesystem:

| Filesystem                   | Working files              | Git history                 |
| ---------------------------- | -------------------------- | --------------------------- |
| APFS (macOS), Btrfs and XFS (Linux) | Shared, as clones   | Shared, as clones           |
| ext4 (Linux)                 | Full copies                | Shared, through hard links  |
| NTFS (Windows) and others    | Full copies                | Full copies                 |

Everything else works on every filesystem: private repositories, merging, archives and the MCP server. Run `git graft doctor` to see what yours supports.

## How it works

`git graft add` creates a standalone Git repository next to your checkout and asks the filesystem to clone its files instead of writing them. The clones share data blocks with your checkout until you edit them.

Before reusing a file, git-graft proves that Git would see it as unchanged: that Git's own line-ending conversion, the one `git status` and `git add` apply, gives exactly the committed content. Files with a filter, `ident` or a working-tree encoding, and anything it cannot prove, come from Git instead. It also records each file's metadata in the new index, so the first `git status` reads no file contents.

Creating a graft never changes your checkout, and copies untracked or ignored files only when you or the agent [ask for them](#bring-local-files). See [safety and recovery](docs/safety.md) for the details.

## FAQ

<details>
<summary><b>How is this different from <code>git worktree</code>?</b></summary>

<br>

A worktree writes a full copy of every file, shares your repository's object store, refs and stash, and allows one checkout per branch. A graft is a standalone repository with its own objects, branches, reflog, stash and index, so any branch name works and your repository isn't written to while the graft is in use. On a filesystem that can clone files, it stores only the files you change.

</details>

<details>
<summary><b>Why not <code>cp -c</code> or <code>cp --reflink</code>?</b></summary>

<br>

Cloning a directory copies everything in it, including untracked and ignored files, local edits and Git's internal state, and nothing checks the files against a commit. A graft starts from the commit you choose, brings only the untracked and ignored files you ask for, verifies every reused file, and adds merging, archives and the MCP server.

</details>

<details>
<summary><b>Why not <code>git clone --shared</code> or <code>--reference</code>?</b></summary>

<br>

Both borrow Git objects from the original repository but still write every working file in full, and the clone breaks if the original's objects move or are garbage-collected. A graft shares file data through the filesystem and keeps its own snapshot of the objects it needs.

</details>

<details>
<summary><b>What about Jujutsu workspaces?</b></summary>

<br>

They need `jj` and write a full working copy for each workspace. git-graft works with plain Git and the tools you already use.

</details>

<details>
<summary><b>Is my work safe if something crashes?</b></summary>

<br>

Every operation can be retried after an interruption, and archived grafts can be restored. Archives are not backups, so keep your own backups for disk failure. See [safety and recovery](docs/safety.md).

</details>

## Documentation

| Guide                                           | Covers                                                       |
| ----------------------------------------------- | ------------------------------------------------------------ |
| [Installation](docs/install.md)                 | Every install method, upgrades, uninstalling and provenance  |
| [Operating grafts](docs/operations.md)          | Commands, configuration, hooks, turns, merging and pushing   |
| [Grafts](docs/workspaces.md)                    | What takes space, history visibility and Git compatibility   |
| [Safety and recovery](docs/safety.md)           | What a graft contains, recovering work and trust             |
| [Benchmarks](docs/benchmarks.md)                | Method, raw data and how to rerun them                       |
| [Agent guide](skills/git-graft/SKILL.md)        | The MCP tools and how agents use them                        |

Every command also has `--help`, and `git graft --help` shows the full manual.

## Contributing

Bug reports and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) to build git-graft and run its tests, and [SECURITY.md](SECURITY.md) to report a vulnerability privately.

## License

git-graft is released under the [MIT license](LICENSE).
