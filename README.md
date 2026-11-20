# git-graft

**Copy-on-write Git workspaces for people and coding agents.** Run ten agents on ten branches of the same repository without ten copies of it on your disk.

[![CI](https://github.com/mprync/git-graft/actions/workflows/ci.yml/badge.svg)](https://github.com/mprync/git-graft/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`git graft spawn` gives each task a standalone Git repository next to your checkout. Its files, index, branches, stash and object database are its own, but unchanged files share their disk blocks with your checkout through filesystem copy-on-write. A workspace only takes space for what you change in it. When the task is done, `git graft land` brings its commits back to the branch you choose, and the whole workspace stays recoverable.

It replaces `git worktree`. Grafts don't share a live object database, lock file or branch namespace with your checkout, so an agent can't break your repository, check out your branch or lose work when your checkout moves.

## Why not `git worktree`?

| | `git worktree add` | `git graft spawn` |
| --- | --- | --- |
| Working files | A full private copy of every file | Shared data blocks; only edited files take space |
| Disk per Homebrew checkout (3,318 files) | 28.55 MiB | **0.29 MiB** |
| First `git status` in a new checkout | Rehash-free | Rehash-free, **3–4× faster** |
| Branches, stash, reflog | Shared with the primary; one branch per checkout | Private to each workspace; any branch name |
| Object database | The primary's live store | A copy-on-write snapshot that survives primary `gc` and moves |
| Removing a checkout | Deletes it | Archives it; `restore` brings it back |
| Agent integration | None | Built-in MCP server with leases and retries |

## Performance

Medians on a Homebrew checkout (3,484 entries), macOS arm64, APFS, Git 2.56, against `git worktree add` on the same machine:

| | Previous release | git-graft | `git worktree add` |
| --- | ---: | ---: | ---: |
| First checkout | 544 ms | **428 ms** | 446 ms |
| Later checkouts | 397 ms | **294 ms** | 283 ms |
| First checkout + first `git status` | 892 ms | **454 ms** | 550 ms |
| Later checkout + first `git status` | 768 ms | **321 ms** | 383 ms |
| Unique data written per checkout | 0.31 MiB | **0.29 MiB** | 28.55 MiB |

Graft clones whole tracked directories with one filesystem call, proves each file's bytes with Git's own index stat test, and records stat data so Git never rehashes the new checkout. On small repositories a worktree is still quicker to create, because a standalone repository has a fixed setup cost of about 150–190 ms. [Benchmarks, method and raw samples](docs/benchmarks.md)

## Install

Graft is a single binary. Git 2.46 or newer must be on your `PATH`.

**macOS and Linux**

```sh
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/mprync/git-graft/main/install.sh | sh
```

**Windows** (PowerShell; the same `curl … | sh` line works in Git Bash)

```powershell
curl.exe -fsSL https://raw.githubusercontent.com/mprync/git-graft/main/install.ps1 -o install.ps1; powershell -ExecutionPolicy Bypass -File install.ps1
```

Both scripts verify the download's SHA-256 against the release `SHA256SUMS` before installing anything, and install for the current user without administrator rights.

**Package managers**

| Platform | Command |
| --- | --- |
| Homebrew (macOS, Linux) | `brew install mprync/tap/git-graft` |
| Scoop (Windows) | `scoop bucket add mprync https://github.com/mprync/scoop-bucket` then `scoop install git-graft` |
| Chocolatey (Windows) | `choco install git-graft` |
| winget (Windows) | `winget install mprync.git-graft` |
| Debian, Ubuntu | `sudo apt install ./git-graft_<version>_amd64.deb` from the release page |
| Fedora, RHEL | `sudo dnf install ./git-graft-<version>-1.x86_64.rpm` from the release page |
| Arch Linux (AUR) | `yay -S git-graft-bin` |
| From source | `cargo install --locked --path .` (Rust 1.89+) |

[All install options, checksum and provenance verification, upgrades and uninstalling](docs/install.md)

## Quick start

From any Git checkout:

```sh
git graft doctor                       # check the volume supports copy-on-write
git graft spawn fix-parser --base main # create a workspace
```

The JSON result includes the workspace `path` and a secret `lease`. Work there with ordinary Git and your editor:

```sh
cd ../myrepo.graft/fix-parser
git commit -am "Fix parser"
```

Back in your checkout, land the commits and archive the workspace:

```sh
git graft land fix-parser --into main --lease "<lease>"
```

`land` reports conflicts and local edits before it changes anything. A landed or dropped workspace is archived, not deleted: keep the returned `recovery_lease` to `git graft restore` it, and use `git graft undo-land` to append a reversal. `git graft list`, `status`, `drop`, `move`, `lock` and `gc` manage workspaces. [Operations and recovery](docs/operations.md)

## Use with coding agents (MCP)

Graft includes a stdio MCP server. Point your client at the binary and the repository:

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

An agent calls `workspace_spawn` with `{"name": "fix-parser"}`, works in the returned path and lands with `workspace_land_and_wipe`. Supplying a random UUIDv4 as `request_id` makes spawns safely retryable. Add `--allow-exec` to let agents run commands and package managers inside their workspace. Keep leases out of logs. [Agent workflow](skills/git-graft/SKILL.md)

## Dependencies without the copies

Installing dependencies is a one-time cost. Register an installed npm tree once, and every compatible graft gets writable `node_modules` backed by the same data blocks. A graft that adds a package only pays for that package.

```sh
git graft seed-layer --shared --key node24-npm11-default
git graft spawn test-parser --base main
```

`git graft add-dep` works with npm, pnpm, Bun, Cargo and uv, and any package manager that accepts a cache directory can be registered:

```sh
git graft register-manager dotnet --cache-env NUGET_PACKAGES
```

[Workspaces and dependency sharing](docs/workspaces.md)

## Platform support

| Platform and filesystem | Working files | Status |
| --- | --- | --- |
| macOS, APFS | Copy-on-write directory and file clones | Fully supported |
| Linux, Btrfs or XFS | Copy-on-write reflinks | Supported |
| Linux, ext4 | Git checkout (no shared data blocks) | Supported, no file savings |
| Windows, NTFS | Git checkout (no shared data blocks) | Supported, no file savings |

`git graft doctor` reports what your volume supports. On filesystems without copy-on-write, Graft still gives each task an independent repository, private refs and recoverable archives; working files are written by Git as a worktree would.

## How it stays safe

- Graft never writes to your checkout's files, index or refs while creating a workspace, and it never copies untracked or ignored files into one.
- Every reused file is proven to hold the selected commit's bytes, either by Git's own index stat test on an unchanged file or by hashing it. Anything Graft cannot prove falls back to `git checkout`.
- Destructive operations archive instead of deleting, refuse when they would discard uncommitted work, and resume after an interruption.
- Workspace state lives under your checkout's `.git/graft` with owner-only permissions.

Graft runs Git hooks, filters and package scripts with your permissions; use it with repositories and MCP clients you trust. [Safety model](docs/safety.md) · [Security policy](SECURITY.md)

## Development

```sh
cargo test --locked   # Rust unit and integration tests
npm ci && npm test    # end-to-end CLI and MCP scenarios in disposable repositories
```

[Verification and coverage](docs/verification.md) · [Contributing](CONTRIBUTING.md) · [MIT license](LICENSE)
