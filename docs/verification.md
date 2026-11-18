# Platform verification and worktree disk measurements

## Platform status

From a fresh local clone, `npm test` built the macOS release binary and passed all 58 TypeScript end-to-end scenarios with zero unavailable cases. These include successful calls to all 13 CLI commands and all 12 MCP tools, Git's subcommand discovery, a bare repository, wire-protocol negotiation, write isolation, refusal paths and recovery after partial operations.

| Platform and filesystem | Full source checkout | Verification recorded on November 18, 2026 |
| --- | --- | --- |
| macOS arm64, APFS | `clonefile` | Release E2E passed on a real APFS volume. |
| Windows x64, NTFS | Ordinary Git checkout | `cargo check --all-targets --target x86_64-pc-windows-gnu` passed. The Mac lacked MinGW import libraries to link a Windows executable. Native Windows E2E is pending. |
| Linux, ext4 without reflinks | Ordinary Git checkout | Native Linux E2E is pending. |
| Linux, Btrfs | Linux `FICLONE` | Native Linux E2E is pending. |

The CI matrix provisions each of these filesystems and checks the expected backend. A new runner result carries its own OS, architecture, Git version, coverage and disk report. The current Windows implementation uses Git on ReFS as well. On ordinary-copy volumes a full source checkout can cost as much as a native worktree, and saving an installed layer can use extra disk space. [Native verification workflow](../.github/workflows/ci.yml)

## Measured savings on APFS

Five workspaces on macOS 26.5 arm64 used about 80% less disk in both release benchmarks. Each row shows both methods from the median-savings round of three independent rounds. Each round includes creation of the first snapshot or installed layer. [Measurements, scenario inventory and source revision](measurements/macos-arm64.json)

| Five workspaces | Native Git | Graft | Space saved | Total time, native / Graft |
| --- | ---: | ---: | ---: | ---: |
| Same commit with 64 MiB of source | 320.91 MiB | 64.33 MiB | 256.58 MiB (79.95%) | 0.586 s / 1.885 s |
| Same locked npm installation with a 32 MiB package | 161.36 MiB | 32.33 MiB | 129.03 MiB (79.96%) | 1.186 s / 2.036 s |

Across the three rounds, source savings ranged from 255.93 to 257.07 MiB. Installed-dependency savings ranged from 127.54 to 129.24 MiB. Graft took longer to create the workspaces in both comparisons. For source, the first managed spawn took 0.487 s in the median round and the next four took 1.399 s combined. The two rows are independent workloads and their savings should be read separately.

## What the E2E suite checks

| Area | Assertions through the CLI or stdio server |
| --- | --- |
| Workspace lifecycle | Branch, index, ignored-file and untracked-file isolation. Ownership, stale leases, duplicate requests, scoped locks, moved trees, replayable landing and removal receipts. |
| Checkout storage | Real filesystem probe and backend receipt, cold and warm snapshots, first-parent reuse, Git index and executable modes, symlinks, attributes, sparse paths and Git fallback. |
| Dependencies and processes | npm, pnpm, Bun, Cargo and uv exercise local packages. Offline hit and miss results, private installs and outputs, bounded child streams, package layer compatibility, integrity and forced copying. |
| Integration | Fast-forward, two-parent and squash history, checked-out and unchecked targets, Git hooks, ignored collisions, unmerged indexes, conflicts and limited offline npm lockfile reconciliation. |
| MCP | A client drives a real stdio server. Initialization, protocol versions, schemas, discovery, lease checks, rejected calls, positive tool effects and returned errors are asserted. |

The full run compares successful operations with both the shipped contract and live discovery. A missing command, missing tool, failed scenario or unavailable prerequisite produces a failing exit status. LLVM separately counts executed Rust lines, functions and regions for the non-test binary on each host. Platform-dependent code and fault conditions affect those numbers.

On macOS arm64 the same 58 scenarios exercised **2,717 of 2,999 Rust lines (90.60%)**, 229 of 251 functions (91.24%) and 4,755 of 5,645 regions (84.23%). LLVM collected 279 process profiles and mapped all 13 Rust source files. The 100% figure applies to the public CLI and MCP operation inventories. The 30 Rust component tests also passed independently.

## Reproduce a run

From a fresh clone, run the release E2E suite with one command.

```sh
npm test
```

The suite uses Node.js 24.5 or newer, npm, Git, Rust/Cargo, pnpm, Bun, uv and an installed Python interpreter. It uses Node's built-in TypeScript support. The recorded toolchain used Git 2.56, Node 24.5, npm 11.5, pnpm 11.10, Bun 1.4.2 and uv 0.11.6. The Windows npm adapter needs the Node distribution's adjacent `npm-cli.js`, pnpm needs a native `pnpm.exe`, and the file-symlink cases require symlink privileges. Give uv an ordinary temporary path. With uv 0.11.6, a newline in the fixture's home path caused interpreter discovery to fail.

| Command | Result |
| --- | --- |
| `npm test` | Builds the locked Rust release binary, runs the complete TypeScript suite and writes `target/e2e/<platform>-<arch>-all.json`. |
| `npm run bench` | Builds the release binary and runs three independent rounds of both disk comparisons. Writes raw readings to `target/e2e/<platform>-<arch>-space.json`. |
| `npm run coverage` | Builds a separately instrumented binary, runs the full suite, and writes a per-source LLVM report under `target/e2e/coverage-*/`. Install `llvm-tools-preview` with rustup or supply matching `LLVM_COV` and `LLVM_PROFDATA` paths. Append `-- --html` for annotated source. |
| `npm ci` followed by `npm run typecheck` | Installs the locked development type definitions and checks every TypeScript test in strict mode. |
| `cargo test --locked` | Runs the Rust component tests. |

Set `GIT_GRAFT_TEST_VOLUME` to an existing writable directory to select the fixture's filesystem. `GIT_GRAFT_FIXTURE_MIB` and `GIT_GRAFT_WORKSPACES` control the source workload. `GIT_GRAFT_DEP_FIXTURE_MIB` and `GIT_GRAFT_DEP_WORKSPACES` control the dependency workload. `GIT_GRAFT_BENCH_REPEATS` sets the number of benchmark rounds. Each fixture uses its own temporary repository.

## How disk use is measured

The source comparison creates five native detached worktrees at one commit and five managed workspaces at that commit. It includes the cold snapshot and four warm spawns. It reads every tracked file in every checkout, compares the contents and changes one checkout to test isolation.

The dependency comparison starts with two npm caches warmed with the same integrity-verified archive. The warmed caches and empty Graft registry are in place before timing starts. Each native worktree runs a real offline `npm ci`. The first managed workspace installs the identical locked package and saves an installed layer. Four more workspaces restore it. The test verifies installed payload digests and independent writes. Both methods include the cost of creating the worktrees.

For each method, the runner subtracts ending free bytes from starting free bytes on the same volume. Savings equal the native delta minus the managed delta. Negative values are retained. The JSON keeps per-round readings, elapsed time, workload sizes, backend receipts and write-isolation results. Shared-volume activity, metadata and delayed allocation can change these observations. Logical file or clone sizes are reported separately. Repeating on a quiet dedicated volume gives the best comparison for a particular repository.
