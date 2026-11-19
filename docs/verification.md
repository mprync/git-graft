# Verification and measurement

## Platform scope

`git graft doctor` probes the selected volume. Graft uses APFS cloning or Linux reflinks where supported and can use Git checkout or private copies elsewhere. The CI matrix defines macOS/APFS, Windows/NTFS, Linux/ext4 and Linux/Btrfs jobs. Check the results for the revision and filesystem you intend to use. [Native filesystem workflow](../.github/workflows/ci.yml)

## Local release checks on November 19, 2026

On macOS arm64, a clean local clone of commit `2db7f5a4ba3c` produced the installed release used for testing and benchmarking. Its SHA-256 begins with `054cc1f44f7f`. It passed all **134 TypeScript end-to-end scenarios** on both the minimum Git 2.46.0 and Git 2.56.0. Each run exercised all 23 advertised CLI operations and all 22 MCP tools. [Scenario results and executable identity](measurements/macos-arm64/verification.json)

All 125 Rust tests passed. A separate instrumented macOS build passed the same 134 scenarios and covered **10,247 of 11,863 mapped Rust lines (86.38%)**. The macOS build and the Linux and Windows GNU cross-targets passed strict Clippy for the application and test targets. Native Windows/NTFS, Linux/ext4 and Linux/Btrfs execution remains unverified for this release. [Platform test matrix](../.github/workflows/ci.yml)

Recovery tests terminate a real landing process, verify the original branch and staged edits, and retry with the same workspace lease. They also cover ignored-file collisions, ref and index contention, interrupted undo and edits in an unrelated native checkout. Privacy tests check the exact objects transferred into SHA-1 and SHA-256 grafts and check that squashed private commits and transient blobs stay outside the primary repository and its Git mirrors.

Dependency tests cover registration through CLI and MCP, reuse after the upstream package is removed, private writes, changed lockfiles, damaged caches and cleanup that preserves foreign data. Landing tests check ordinary source edits beside ignored installations, changed ignore rules and a Unicode case rename on APFS. They verify that the installed packages stay outside the primary repository. [Manager scenarios](../e2e/registered-managers.ts) · [Offline .NET proof](../e2e/nuget.ts)

The RustSec scan reported zero known vulnerabilities across 124 locked entries. It retained one maintenance advisory for the transitive `chrono` 0.4.45 dependency. The npm audit reported zero vulnerabilities across the three locked development dependencies. These results apply to the lockfiles and advisory data used on the date above. [RustSec database revision](https://github.com/RustSec/advisory-db/tree/7133866a6b44d6d3e1994ae6a48a296553c3c3da) · [Chrono maintenance advisory](https://rustsec.org/advisories/RUSTSEC-2026-0543.html)

## Toolchain requirements

Use Git 2.46 or newer and Rust 1.89 or newer. Git 2.46 introduced the symbolic-ref transactions Graft uses to guard checked-out branch updates. Graft also uses sparse-aware staging and disables lazy fetching so operations fail locally if required objects are missing. [Git 2.46 release notes](https://github.com/git/git/blob/v2.46.0/Documentation/RelNotes/2.46.0.txt) · [Git 2.45 release notes](https://github.com/git/git/blob/v2.45.0/Documentation/RelNotes/2.45.0.txt) · [Git sparse staging](https://github.com/git/git/blob/v2.36.0/Documentation/git-add.txt)

The locked build passed on macOS arm64 with Rust 1.89.0 and `rmcp` 3.11.0 using `cargo +1.89.0 build --offline --locked --all-targets`. Offline reproduction requires cached dependencies. Rust 1.89 stabilized the standard library file locks Graft uses. CI also defines a Rust 1.89 job on Ubuntu. [Rust file locks](https://doc.rust-lang.org/std/fs/struct.File.html#method.lock)

Check `git --version` and `git graft doctor` on the target host. Distribution packages and hosted runner images can have different Git versions. [Ubuntu 24.04 runner inventory](https://github.com/actions/runner-images/blob/main/images/ubuntu/Ubuntu2404-Readme.md)

## Reproduce checks

The end-to-end suite builds the locked release binary and runs it in disposable repositories.

```sh
npm test
```

It requires Node.js 24.5 or newer, npm, Git, Rust/Cargo, pnpm, Bun, uv and Python. Windows symlink cases require appropriate privileges. `npm ci` installs the locked development dependencies for `npm run typecheck`. The Rust suite runs with `cargo test --locked`. For instrumented coverage, install `llvm-tools-preview` or provide matching `LLVM_COV` and `LLVM_PROFDATA`, then run `npm run coverage`. Reports are written under `target/e2e/`.

The suite exercises workspace isolation, Git operations, dependency handling, integration, recovery and MCP stdio. Failures and missing prerequisites should be investigated on the target system. Set `GIT_GRAFT_TEST_VOLUME` to an existing writable directory to select the fixture filesystem.

For a real third-party manager check, build the release binary and run `node e2e/nuget.ts` with a .NET SDK 8 or newer and Python on the path. The fixture registers NuGet, removes a package from its offline feed, restores it from the project cache in a second graft, then upgrades one graft and builds both. [NuGet fixture](../e2e/nuget.ts)

## Measure a workflow

Build the release binary before running the benchmark scripts.

```sh
cargo build --locked --release
npm run bench:agents
npm run bench:source
npm run bench:source:real -- --repo /path/to/packed-repository
```

`bench:agents` and `bench:source` use synthetic fixtures. `bench:source:real` reads a full-history repository with packed objects and runs against disposable local clones. If you override the output path, choose a location outside the source repository. Record the revision, tool versions, filesystem and fixture settings with the reports. The two-task benchmark includes dependency changes, integration and recovery. Its `--mixed-history` option adds unrelated history to a pack.

Measure wall time in a separate run from `--apfs-file-data` because the observer can warm filesystem caches. The observer measures shared and private regular-file extents. Directory records, inode metadata, extended attributes and resource forks are outside its scope. Signed whole-volume free-space changes can include unrelated writes and delayed allocation. [Release benchmarks and raw samples](benchmarks.md)
