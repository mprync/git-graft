# Verification

## What the test suites cover

- **Rust tests** (`cargo test --locked`) cover the storage, index, registry, landing and recovery primitives, including interrupted operations, foreign data at workspace paths, same-size edits with restored timestamps, SHA-1 and SHA-256 repositories, and shallow histories.
- **End-to-end scenarios** (`npm test`) build the release binary and drive every CLI command and every MCP tool in disposable repositories. They cover workspace isolation, ordinary Git use inside a graft, dependency sharing, landing and conflict reporting, undo, archive and restore, interruption recovery, edge-case paths and modes, and disk use against `git worktree add`. The run fails if any advertised CLI operation or MCP tool goes unexercised.
- **Coverage** (`npm run coverage`) runs the end-to-end suite against an instrumented build and reports the Rust lines it reaches.

## Continuous integration

Every push runs the suites on four native filesystems, each with the copy-on-write backend it must use. [Workflow](../.github/workflows/ci.yml)

| Runner | Filesystem | Expected backend |
| --- | --- | --- |
| macOS 15 | APFS | `clonefile` |
| Ubuntu 24.04 | Btrfs (dedicated loop volume) | `reflink` |
| Ubuntu 24.04 | ext4 (dedicated loop volume) | Git checkout, with Git 2.46.0 built from source to test the minimum version |
| Windows Server 2025 | NTFS | Git checkout |

A separate job builds the locked sources with Rust 1.89, the minimum supported version. Tagged releases build and smoke-test the installers on macOS, Linux and Windows. [Release workflow](../.github/workflows/release.yml)

## Latest local results

On macOS arm64 (APFS), the release built from this repository passed all 166 Rust tests and all 149 end-to-end scenarios on both Git 2.46.0 and Git 2.56.0, exercising all 23 CLI operations and 22 MCP tools. Strict Clippy passed for macOS and the Linux and Windows GNU cross-targets. Native Linux and Windows execution is verified by CI.

## Toolchain requirements

Graft needs Git 2.46 or newer: it relies on the symbolic-ref transactions introduced in 2.46 to guard updates to checked-out branches, and disables lazy fetching so a missing object fails locally instead of reaching a remote. Building needs Rust 1.89 or newer for the standard library's file locks. [Git 2.46 release notes](https://github.com/git/git/blob/v2.46.0/Documentation/RelNotes/2.46.0.txt) · [Rust file locks](https://doc.rust-lang.org/std/fs/struct.File.html#method.lock)

Distribution Git packages can be older than 2.46; `git graft doctor` reports the Git version it found and whether it is supported.

## Run the checks

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
npm ci
npm run typecheck
npm test
```

The end-to-end suite needs Node.js 24.5 or newer, npm, Git, Rust, pnpm, Bun, uv and Python. Set `GIT_GRAFT_TEST_VOLUME` to a writable directory to choose the filesystem under test, and run a single group with `node e2e/run.ts --group <name>`. Reports are written to `target/e2e/`. Windows symlink cases need Developer Mode or equivalent privileges.

For a real third-party package manager, run `node e2e/nuget.ts` with a .NET 8 SDK: it registers NuGet, removes a package from its offline feed, restores it from the project cache in a second graft, then upgrades one graft and builds both. [NuGet scenario](../e2e/nuget.ts)

[Benchmarks](benchmarks.md)
