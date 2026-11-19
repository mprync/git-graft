# Release benchmarks

## Local history on a populated Homebrew clone

These runs started from disposable local copies of Homebrew `bc63375ac3a4` on macOS arm64/APFS with Git 2.56.0. Each primary had `graft.history=local`. Timing covers the ordinary release CLI process with dependency discovery enabled, including first-use initialization. No cache was seeded. Each workspace had its own Git repository and staging index.

| Clean primary, median of five fresh runs | Graft | Native worktree |
| --- | ---: | ---: |
| First checkout | 0.552 s | 0.339 s |
| Repeat at the same commit | 0.537 s | 0.366 s |
| Third checkout after a one-file commit | 0.533 s | 0.353 s |
| First checkout followed by first Git status | 0.968 s | 0.462 s |

The last row is the median of the paired command times. The first status checks the cloned files against Git’s index. Median first-spawn user and system CPU times were 0.10 s and 0.90 s for Graft, and 0.05 s and 0.27 s for Git. Peak RSS medians were 17.1 MB and 52.4 MB respectively. In a separate three-round run with staged, unstaged, untracked and ignored donor files, first checkout took 0.549 s for Graft and 0.329 s for a worktree. Both checkouts were clean and the donor retained its changes. [Clean timing samples](measurements/macos-arm64/homebrew-local-clean-timing.json) · [Dirty timing samples](measurements/macos-arm64/homebrew-local-dirty-timing.json)

| Additional APFS file data | Graft | Native worktree |
| --- | ---: | ---: |
| First checkout | 327,680 bytes | 29,941,760 bytes |
| Second checkout, incremental | 315,392 bytes | 29,941,760 bytes |
| Two active checkouts, total | 643,072 bytes | 59,883,520 bytes |
| Third checkout after a one-file commit, incremental | 315,392 bytes | 29,945,856 bytes |

All 3,318 regular source files and all four packs shared data blocks with the primary. For two grafts, the private Git v4 indexes accounted for 573,440 bytes and administrative files for the remaining 69,632 bytes. Git compresses common path prefixes in its v4 index. This saved 303,104 bytes across the two indexes compared with v2. The physical observer counts unique file extents after the initial status check. Filesystem metadata, resource forks and extended attributes are outside its scope. The physical results matched on two clean rounds and one dirty round. [Clean disk samples](measurements/macos-arm64/homebrew-local-clean-apfs.json) · [Dirty disk samples](measurements/macos-arm64/homebrew-local-dirty-apfs.json)

To reproduce the clean timing run, use `npm run bench:source:real -- --repo /path/to/Homebrew/brew --project-history local --clean-primary --rounds 5 --advance-base --include-revisions`. Run the physical observer separately with `--apfs-file-data` and a different `--output` path. The reports identify the executable by SHA-256.

The volume-wide first-checkout space change in the clean timing runs had a median of 2.04 MB for Graft and 31.58 MB for Git. This includes filesystem metadata for the new paths and any other activity on the volume. The 327,680-byte file-data measurement above counts blocks attributable to the fixture.

### Durable snapshot writes

The APFS local backend waits for writeback of each private object file and child directory, then issues one hardware flush before publishing the snapshot. In four alternating pairs of fresh Homebrew clones, this reduced median first spawn from 0.690 s to 0.623 s. In three pairs on a fixture with 256 newly committed files and 259 loose Git objects, it reduced median first spawn from 1.721 s to 0.317 s. Every run verified the checkout and the primary. [Paired release samples](measurements/macos-arm64/local-snapshot-writeback.json)

## How much does selecting one commit save?

From a populated Homebrew clone, `--history isolated` keeps the selected ancestry for log and blame. `--history tip` exports one commit, its trees and blobs. The release CLI was timed on three fresh primaries for isolated history and six for tip history. The APFS block measurements ran separately.

| Clean primary on APFS | Isolated ancestry | Single commit |
| --- | ---: | ---: |
| First spawn, median | 5.90 s | 0.636 s |
| Native worktree in the paired timing runs | 0.41 s | 0.366 s |
| Additional file data, first workspace | 15,482,880 bytes | 5,693,440 bytes |
| Additional file data, second workspace | 315,392 bytes | 319,488 bytes |
| Additional working file data, both workspaces | 0 bytes | 0 bytes |

On a cold tip spawn, Graft can verify the selected commit, trees and Git attributes in a temporary object store while Git builds the complete pack. This lets Graft check and clone eligible source files concurrently. Git finishes verifying the complete pack before native checkout or index publication. Cold spawn improved in all six alternating release pairs, from a median of 0.770 s to 0.636 s. Repeat spawn took 0.473 s versus 0.390 s for Git. Cold spawn plus the first status took 1.075 s versus 0.491 s. The temporary store added no retained file blocks in the measured workspace. [Paired timings, CPU, peak RSS and APFS allocation](measurements/macos-arm64/homebrew-tip-overlap.json)

When a shallow export fits in one pack and the primary has no reusable pack, Git uses path names from the selected revision to find similar objects for compression. On the preceding release this reduced median pack transfer from 253 ms to 188 ms. [Pack export measurements](measurements/macos-arm64/homebrew-tip-pack-export.json)

The two grafts shared a selected pack and pack index occupying 5,353,472 bytes. After a one-file commit, a third graft at the new tip allocated 5,672,960 additional bytes for the new selection and workspace. [APFS allocation samples](measurements/macos-arm64/homebrew-tip-pack-export.json) · [Previous tip release](measurements/macos-arm64/homebrew-tip-release.json) · [Isolated release](measurements/macos-arm64/homebrew-selected-history.json)

## Isolated history on the previous release

Each benchmark starts with one populated primary checkout. “Additional file data” is the increase in unique physical file blocks across the entire fixture. It includes both task checkouts, their private Git files, and Graft’s shared project caches. MB means 1,000,000 bytes.

| Homebrew checkout on APFS | Native worktrees | Graft | Graft savings |
| --- | ---: | ---: | ---: |
| First checkout, including base setup | 29.94 MB | 15.65 MB | 47.74% |
| Second checkout at the same commit, additional cost | 29.94 MB | 0.47 MB | 98.44% |
| Two active checkouts, total | 59.88 MB | 16.12 MB | 73.09% |
| Third checkout after a one-file commit, additional cost | 29.95 MB | 1.97 MB | 93.43% |

The two-checkout total includes the first checkout and the incremental cost of the second. The third checkout starts from a one-file commit already present in the primary while both earlier workspaces remain active. Each row is the median of two runs. The optimized build added 16,115,712 bytes for two checkouts, within one 4,096-byte block of the previous measurement. Both builds added exactly 466,944 bytes on the repeat. [Optimized disk samples](measurements/macos-arm64/homebrew-optimized-apfs.json) · [Previous disk samples](measurements/macos-arm64/homebrew-apfs.json)

### Where the 16.12 MB goes

Both grafts had clean Git status. The starting repository already contained the files and history on local disk. Graft shared the file blocks for all 3,318 eligible source files and built a private Git object database containing only the chosen commit and its history.

| Additional physical file data for both grafts | Bytes |
| --- | ---: |
| Git pack indexes mapping object IDs to locations in private packs | 9,338,880 |
| History pack blocks written to isolate the selected history | 4,323,328 |
| Graft snapshot, ownership and other administrative records | 1,576,960 |
| Two Git checkout indexes for staging and file status | 876,544 |
| Checked-out source file data and source snapshots | 0 |
| Change in the original checkout excluding Graft’s registry | 0 |
| **Total** | **16,115,712** |

The observer assigns blocks shared with the original clone to that baseline first, then counts each remaining block once across caches and workspaces. The table excludes filesystem metadata. A normal Git worktree can use the primary pack indexes directly. Graft’s independence from primary repacking and its exclusion of unrelated Git objects account for most of the first-checkout cost.

### What changed since the 201 MB result

An earlier build used **201.39 MB** for two Homebrew checkouts, compared with **59.88 MB** for native worktrees. Its source-size threshold excluded all 3,318 checked-out files from sharing. Its history setup produced 157.54 MB of packs and indexes, of which only 16.06 MB reused whole primary packs. The remaining history was exported to exclude objects from unrelated branches. The earlier build is identified by executable SHA-256 prefix `14d7a90df288`. [Earlier disk samples](measurements/macos-arm64/homebrew-before-apfs.json) · [Earlier timing samples](measurements/macos-arm64/homebrew-before-timing.json)

The current build shares all 3,318 eligible Homebrew source files (20.54 MB logical). It reused 16.06 MB of whole packs and indexes and retained 111.50 MB at the original offsets in a mixed pack. Git verified the filtered pack and built an index for it. The index, remaining exported objects and checkout records explain why the first graft still takes space. A repeat graft reuses that verified history.

| Additional file data for two active checkouts | Native worktrees | Graft | Graft savings |
| --- | ---: | ---: | ---: |
| Generated source, 4,000 small files and three 8 MiB files | 83.94 MB | 2.15 MB | 97.44% |
| Two npm tasks with an installed 8 MiB dependency | 70.64 MB | 0.31 MB | 99.56% |

The calibrated APFS observer counts each mapped regular-file allocation block once across the fixture. Directory records, inode metadata, extended attributes and resource forks are outside its scope. The raw reports also provide signed volume-wide free-space changes, which include unrelated activity. The source and npm disk results each have three samples. Filesystems that require private copies can consume substantially more space. [Generated source disk samples](measurements/macos-arm64/source-apfs.json) · [Task disk samples](measurements/macos-arm64/tasks-apfs.json)

## Checkout time and resources

Each of four rounds used a fresh, disposable copy of Homebrew/brew at `bc63375ac3a4` with 53,716 reachable commits and 4 source packs totaling 130.94 MB. The harness preserved the initial pack layout, added staged, unstaged, untracked and ignored data, and alternated which implementation ran first. It checked the commit, tree, source files, private-object exclusion and preservation of the primary.

| Median wall time, Homebrew | Graft, previously published | Same binary rerun | Optimized Graft | Native worktrees in optimized run |
| --- | ---: | ---: | ---: | ---: |
| First checkout | 6.46 s | 6.73 s | 6.06 s | 0.35 s |
| Second checkout at the same commit | 1.12 s | 1.12 s | 1.15 s | 0.37 s |
| Third checkout after a one-file commit | 2.85 s | 3.00 s | 2.04 s | 0.41 s |

We ran four two-round batches, starting and ending with the control binary and running the optimized binary in the middle two batches. Each binary had four fresh starts, with native Git timed on the same fixture in every round. The optimized binary reduced first-checkout time by 10.0% and one-file advance time by 32.1%. The repeat took about 22 ms longer. [Previous publication](measurements/macos-arm64/homebrew-timing.json) · [Control rerun](measurements/macos-arm64/homebrew-control-timing.json) · [Optimized release](measurements/macos-arm64/homebrew-optimized-timing.json)

Native worktrees use the primary object database directly. On its first spawn Graft spent 0.74 s discovering which objects belong to the selected commit, 2.09 s transferring and indexing packs, and 1.06 s verifying the complete private history. History setup totaled 4.13 s including cache publication. Graft now overlaps pack validation with export, verifies independent batches of source files in parallel and reuses the checkout index during snapshot creation. For a single-parent advance it bounds Git’s traversal at the already verified parent. The workspace retains the complete parent history. Each time is the median for its stage.

| Median per-command resource use | Native worktrees | Graft |
| --- | ---: | ---: |
| First checkout, user / system CPU | 0.05 / 0.28 s | 7.11 / 3.62 s |
| First checkout, peak RSS | 52.5 MB | 289.3 MB |
| Repeat checkout, user / system CPU | 0.05 / 0.28 s | 0.24 / 1.28 s |
| Repeat checkout, peak RSS | 52.3 MB | 17.8 MB |

In the generated source fixture, the first and repeat checkouts took 2.36 s and 1.11 s with Graft, and 0.30 s and 0.34 s with native Git. Timing runs exclude APFS scans. Fixture setup and correctness checks run outside the timed commands. Checking the first checkout can warm filesystem caches for the second. CPU time sums work across workers. Peak RSS uses macOS child-process accounting. [Generated source timing samples](measurements/macos-arm64/source-timing.json)

## Two tasks with a populated npm installation

The fixture starts with 400 small files, three 8 MiB source files, eight historical 1 MiB blobs, an installed 8 MiB package and a warm npm cache. Both tasks edit, build, test and commit. One lands and the other is dropped. The native recovery comparison also saves private data, restores the dropped task and retires it again. Graft performs those steps with its public CLI and retains complete workspace archives.

| Median across three rounds | Conventional native Git | Native Git with private recovery | Graft |
| --- | ---: | ---: | ---: |
| First task ready, including setup | 0.23 s | 0.24 s | 1.00 s |
| Second task ready | 0.23 s | 0.27 s | 0.45 s |
| Complete workflow | 1.16 s | 1.54 s | 3.29 s |
| Commands’ user / system CPU | 0.71 / 0.33 s | 0.90 / 0.48 s | 1.25 / 1.08 s |
| Largest reported command RSS | 108.9 MB | 109.0 MB | 91.4 MB |
| Additional file data after retirement and GC | 0.07 MB | 50.52 MB | 0.34 MB |

Adding a small local package to one active graft allocated 81,920 additional bytes, compared with 20,480 for native npm. In a separate three-round timing run the add took 0.29 s through `git graft add-dep` and 0.17 s through npm. The package-add measurement includes npm’s changes to project and install metadata. [Task timing and resource samples](measurements/macos-arm64/tasks-timing.json) · [Package-add samples](measurements/macos-arm64/npm-add.json)

## Reproduce the measurements

These runs used macOS 26.5 arm64 on APFS, Git 2.56.0, Rust 1.99.1, Node.js 24.5.0 and npm 11.5.1. The optimized Homebrew executable was 4,048,544 bytes, built from `bc88f56` with `cargo build --release --locked`, with SHA-256 `d71a90823520f779b52ee2acca433250acf1590242091735c5ec3d1050465020`. The previous Homebrew control and the generated-source and npm reports used the 4,031,920-byte executable identified by `054cc1f44f7f`.

```sh
cargo build --locked --release
npm run bench:source:real -- --repo /path/to/Homebrew/brew --rounds 4 --advance-base
npm run bench:source -- --repeats 3
npm run bench:agents -- --repeats 3
npm run bench:workflow -- --mode shared --add-package --repeats 3
```

Run the first three benchmarks separately with `--apfs-file-data` and distinct `--output` paths to measure physical storage. The Homebrew disk run used two rounds. The public Homebrew reports used `--include-revisions`. [Tool and filesystem prerequisites](verification.md)
