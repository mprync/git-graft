# Platform verification and worktree disk measurements

## Platform status

From a fresh local clone, `npm test` built the macOS release binary and passed all 88 TypeScript end-to-end scenarios with zero unavailable cases. The suite exercised all 16 CLI commands and 15 MCP tools, including dependency seeding, workspace restoration and reversal of completed integrations. [Release measurements and operation inventory](measurements/macos-arm64-primary-reuse.json)

| Platform and filesystem | Full source checkout | Verification recorded on November 18, 2026 |
| --- | --- | --- |
| macOS arm64, APFS | `clonefile` | Release E2E passed on a real APFS volume. |
| Windows x64, NTFS | Ordinary Git checkout | All-targets Windows GNU Clippy passed with warnings denied. Native Windows E2E is pending. |
| Linux, ext4 without reflinks | Ordinary Git checkout | Native Linux E2E is pending. |
| Linux, Btrfs | Linux `FICLONE` | Native Linux E2E is pending. |

The CI matrix provisions each of these filesystems and checks the expected backend. A new runner result carries its own OS, architecture, Git version, coverage and disk report. The current Windows implementation uses Git on ReFS as well. On ordinary-copy volumes a full source checkout can cost as much as a native worktree, and saving an installed layer can use extra disk space. [Native verification workflow](../.github/workflows/ci.yml)

## Two agents starting from one populated checkout

The starting checkout contained 400 small files, three distinct 8 MiB binary files, a locked npm install with an 8 MiB package and a warm npm cache. The Graft arm used the Git subcommand for workspace creation, dependency operations, build/test execution, integration, retirement and GC. Native Git ran the same workload in an independent repository. The primary checkout kept its tracked edit and untracked and ignored files throughout the task. The integration went to a separate local branch.

| While both agents were active | Additional disk, Graft | Additional disk, native Git + npm | Space saved |
| --- | ---: | ---: | ---: |
| Earlier Graft, first workspace installs packages | 42.62 MiB | 67.86 MiB | 37.12% |
| Current Graft, first workspace installs packages | 16.66 MiB | 67.77 MiB | 75.43% |
| Current Graft, seed the existing primary install | **0.69 MiB** | **67.77 MiB** | **98.99%** |

Each cell is a three-round median. Percentages come from paired differences within each round. Seeded savings ranged from 98.72% to 99.01% on this shared APFS volume. The first Graft workspace reused 25,173,503 logical source bytes across 409 files. Git supplied the eight committed bytes for the file with the developer's local edit. Layer restore shared an additional 8,389,053 logical bytes. Filesystem metadata, indexes, leases and writes account for the remaining disk use. [Per-round timings, byte readings and receipts](measurements/macos-arm64-primary-reuse.json)

| Median wall time | Earlier Graft | Current Graft, seeded | Native Git + npm, seeded comparison |
| --- | ---: | ---: | ---: |
| First spawn alone | 0.395 s | 0.461 s | 0.069 s |
| Second spawn alone | 0.328 s | 0.279 s | 0.067 s |
| First agent ready, including setup and package preparation | 0.979 s | 0.742 s | 0.233 s |
| Entire workflow through cleanup | 2.387 s | 2.570 s | 0.839 s |

Graft verifies the first donor, prepares the Git index and records durable ownership. Batching Git queries and reusing open directories reduced the cost of later spawns. Keeping the final index refresh matters to Git clients that call `git diff-files` immediately. After cleanup the seeded Graft run retained 0.63 MiB above its starting point, including recovery state. Native Git retained 0.08 MiB after explicitly removing its fixture worktrees and branches.

## Five-workspace comparison with the earlier benchmark

| Five workspaces on APFS | Native Git | Graft | Space saved | Total time, native / Graft |
| --- | ---: | ---: | ---: | ---: |
| Same commit with 64 MiB of source | 320.26 MiB | 0.45 MiB | 99.86% | 0.269 s / 1.950 s |
| Same locked npm installation, first workspace installs a 32 MiB package | 160.48 MiB | 32.24 MiB | 79.91% | 1.133 s / 1.796 s |

These rows use the median absolute-savings round of three paired trials. The earlier source implementation first wrote a complete 64 MiB snapshot, then shared it across all five agents. Paying for one extra source copy instead of five accounts for the earlier 79.95% result. The current first checkout can already share the primary's blocks. The unseeded package workload still pays for one independent install. [Earlier measurements](measurements/macos-arm64.json) · [Current measurements](measurements/macos-arm64-primary-reuse.json)

## What the E2E suite checks

| Area | Assertions through the CLI or stdio server |
| --- | --- |
| Workspace lifecycle | Branch, index, ignored-file and untracked-file isolation. Ownership, stale leases, duplicate requests, scoped locks, moved trees, replayable landing and removal receipts. |
| Checkout storage | Real filesystem probe and backend receipt, cold and warm snapshots, first-parent reuse, Git index and executable modes, symlinks, attributes, sparse paths and Git fallback. |
| Dependencies and processes | npm, pnpm, Bun, Cargo and uv exercise local packages. Offline hit and miss results, private installs and outputs, bounded child streams, package layer compatibility, integrity and forced copying. |
| Integration | Fast-forward, two-parent and squash history, checked-out and unchecked targets, Git hooks, ignored collisions, unmerged indexes, conflicts and limited offline npm lockfile reconciliation. |
| MCP | A client drives a real stdio server. Initialization, protocol versions, schemas, discovery, lease checks, rejected calls, positive tool effects and returned errors are asserted. |

The full run compares successful operations with both the shipped contract and live discovery. A missing command, missing tool, failed scenario or unavailable prerequisite produces a failing exit status. LLVM separately counts executed Rust lines, functions and regions for the non-test binary on each host. Platform-dependent code and fault conditions affect those numbers.

On macOS arm64 the 88 scenarios exercised **4,826 of 5,260 mapped Rust lines (91.75%)**, 414 of 445 functions (93.03%) and 8,647 of 10,231 regions (84.52%). LLVM collected 409 process profiles and mapped all 18 Rust source files. The full CLI and MCP inventories had positive end-to-end coverage. All 33 Rust component tests passed independently. The offline RustSec audit found zero advisories against the lockfile in database revision `999ec89d` dated October 17, 2026. A database refresh was unavailable during verification.

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
| `npm run bench:workflow` | Measures a complete two-agent workflow three times from a populated checkout, including seeding the existing install. Writes `target/e2e/workflow-bench.json`. Use `-- --mode cold` to charge the first workspace for installation. |
| `npm run coverage` | Builds a separately instrumented binary, runs the full suite, and writes a per-source LLVM report under `target/e2e/coverage-*/`. Install `llvm-tools-preview` with rustup or supply matching `LLVM_COV` and `LLVM_PROFDATA` paths. Append `-- --html` for annotated source. |
| `npm ci` followed by `npm run typecheck` | Installs the locked development type definitions and checks every TypeScript test in strict mode. |
| `cargo test --locked` | Runs the Rust component tests. |

Set `GIT_GRAFT_TEST_VOLUME` to an existing writable directory to select the fixture's filesystem. `GIT_GRAFT_FIXTURE_MIB` and `GIT_GRAFT_WORKSPACES` control the source workload. `GIT_GRAFT_DEP_FIXTURE_MIB` and `GIT_GRAFT_DEP_WORKSPACES` control the dependency workload. `GIT_GRAFT_BENCH_REPEATS` sets the number of benchmark rounds. Each fixture uses its own temporary repository.

## How disk use is measured

The source comparison creates five native detached worktrees at one commit and five managed workspaces at that commit. It includes the cold snapshot and four warm spawns. It reads every tracked file in every checkout, compares the contents and changes one checkout to test isolation.

The dependency comparison starts with two npm caches warmed with the same integrity-verified archive. The warmed caches and empty Graft registry are in place before timing starts. Each native worktree runs a real offline `npm ci`. The first managed workspace installs the identical locked package and saves an installed layer. Four more workspaces restore it. The test verifies installed payload digests and independent writes. Both methods include the cost of creating the worktrees.

For each method, the runner subtracts ending free bytes from starting free bytes on the same volume. Savings equal the native delta minus the managed delta. Negative values are retained. The JSON keeps per-round readings, elapsed time, workload sizes, backend receipts and write-isolation results. Shared-volume activity, metadata and delayed allocation can change these observations. Logical file or clone sizes are reported separately. Repeating on a quiet dedicated volume gives the best comparison for a particular repository.
