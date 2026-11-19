# Release benchmarks

## Disk use with two active workspaces

Each benchmark starts with one populated primary checkout. “Additional file data” is the increase in unique physical file blocks across the entire fixture. It includes both task checkouts, their private Git files, and Graft’s shared project caches. MB means 1,000,000 bytes.

| Homebrew checkout on APFS | Native worktrees | Graft | Graft savings |
| --- | ---: | ---: | ---: |
| First checkout, including base setup | 29.94 MB | 15.65 MB | 47.72% |
| Second checkout at the same commit, additional cost | 29.94 MB | 0.47 MB | 98.44% |
| Two active checkouts, total | 59.88 MB | 16.12 MB | 73.08% |
| Third checkout after a one-file commit, additional cost | 29.95 MB | 1.97 MB | 93.43% |

The two-checkout total includes the first checkout and the incremental cost of the second. The third checkout starts from a one-file commit already present in the primary while both earlier workspaces remain active. Each row is the median of two runs. [Homebrew disk samples](measurements/macos-arm64/homebrew-apfs.json)

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

| Median wall time, Homebrew | Native worktrees | Earlier Graft | Current Graft |
| --- | ---: | ---: | ---: |
| First checkout | 0.34 s | 3.86 s | 6.46 s |
| Second checkout at the same commit | 0.35 s | 0.57 s | 1.12 s |
| Third checkout after a one-file commit | 0.36 s | 1.57 s | 2.85 s |

The earlier and current Graft timings come from separate runs on the same Homebrew revision. Sharing small files and filtering mixed packs reduced disk use and increased checkout time on this fixture. During the first spawn it spent 0.71 s discovering reachable objects, 2.37 s transferring and indexing packs, and 0.95 s verifying the private graph. History setup took 4.21 s in total on the first spawn and 0.11 s on the repeat. The remaining time includes verifying and materializing source files. Each figure is the median for its stage. [Homebrew timing samples](measurements/macos-arm64/homebrew-timing.json)

| Median per-command resource use | Native worktrees | Graft |
| --- | ---: | ---: |
| First checkout, user / system CPU | 0.05 / 0.26 s | 6.86 / 2.74 s |
| First checkout, peak RSS | 52.2 MB | 289.2 MB |
| Repeat checkout, user / system CPU | 0.05 / 0.26 s | 0.23 / 1.38 s |
| Repeat checkout, peak RSS | 52.4 MB | 17.9 MB |

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

These runs used macOS 26.5 arm64 on APFS, Git 2.56.0, Rust 1.99.1, Node.js 24.5.0 and npm 11.5.1. The release executable was 4,031,920 bytes, with SHA-256 `054cc1f44f7fd52d7a453f3312dc9d8504eb0cf319961df5855fe0e3331edbfb`. It was installed from a clean local clone with `cargo install --locked --path .`. All seven current reports identify that executable.

```sh
cargo build --locked --release
npm run bench:source:real -- --repo /path/to/Homebrew/brew --rounds 4 --advance-base
npm run bench:source -- --repeats 3
npm run bench:agents -- --repeats 3
npm run bench:workflow -- --mode shared --add-package --repeats 3
```

Run the first three benchmarks separately with `--apfs-file-data` and distinct `--output` paths to measure physical storage. The Homebrew disk run used two rounds. The public Homebrew reports used `--include-revisions`. [Tool and filesystem prerequisites](verification.md)
