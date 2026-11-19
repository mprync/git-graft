# Verification and measurement

## Platform scope

`git graft doctor` probes the selected volume. Graft uses APFS cloning or Linux reflinks where supported and can use Git checkout or private copies elsewhere. The CI matrix defines macOS/APFS, Windows/NTFS, Linux/ext4 and Linux/Btrfs jobs. Check the results for the revision and filesystem you intend to use. [Native filesystem workflow](../.github/workflows/ci.yml)

## Local release checks on November 19, 2026

On macOS arm64, all 105 Rust tests passed. The release passed all 103 end-to-end scenarios on APFS with Git 2.56.0. They exercised all 20 advertised CLI operations and all 19 MCP tools, including restricted and execution-enabled hosts. The same scenarios passed on an instrumented build, covering 8,251 of 9,774 Rust source lines (84.42%). The native and Windows GNU targets passed strict Clippy. Native Windows/NTFS and Linux filesystem execution are separate checks in the CI matrix.

With Apple Git 2.50.1, a separate run exercised spawn, ordinary Git commits, checked-out landing, undo, restore and `git fsck` on repositories using both loose/packed refs and reftable. Runtime testing of the declared Git 2.46.0 minimum remains open.

The recovery tests cover partially staged files, ignored-file collisions, ref and index contention, interrupted land and undo, and native checkouts associated with a workspace. Privacy tests verify that a squashed private tip and its transient blob stay outside the primary repository and its Git mirrors. Dependency tests exercise real package managers in moved, nested and whitespace-named workspaces.

The dependency audit found three advisories in the previous `rmcp` version. After the upgrade to 3.11.0, the RustSec scan reported zero known vulnerabilities across 123 locked entries. It retained one maintenance advisory for the transitive `chrono` 0.4.45 dependency. The npm audit reported zero vulnerabilities across the three locked development dependencies. These results apply to the lockfiles and advisory data used on the date above. [RustSec database revision](https://github.com/RustSec/advisory-db/tree/472a4c9a785030434f15d7da84964d60fbe789ab) · [Chrono maintenance advisory](https://rustsec.org/advisories/RUSTSEC-2026-0543.html)

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
