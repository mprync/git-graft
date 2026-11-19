# Release benchmarks

On a full Homebrew checkout, Graft took 3.86 seconds to create the first standalone repository and 0.57 seconds to create a second at the same commit. Native `git worktree add` took 0.36 and 0.36 seconds. Both commands produced the same commit, tree and source files. The standalone checkout also received a private copy of the selected commit’s object history.

## Homebrew checkout

Each of four rounds started with a fresh local copy of Homebrew/brew at `bc63375ac3a4`, with 53,716 commits reachable from `HEAD` and four source packs totaling 130.94 MB. The harness retained the source pack layout, added staged, unstaged, untracked and ignored data, and alternated which implementation ran first. The third checkout started after both primaries advanced by one identical commit. Each round checked that the previous workspaces stayed at their original revision and the Graft checkout excluded unrelated commits.

| Median wall time | Native Git | Graft |
| --- | ---: | ---: |
| First checkout in a fresh primary | 0.36 s | 3.86 s |
| Second checkout at the same commit | 0.36 s | 0.57 s |
| Checkout after a one-commit advance | 0.36 s | 1.57 s |

Two source packs contained only objects the workspace needed. Graft shared 16.06 MB from those packs through APFS copy-on-write. It exported the rest of the reachable history from mixed packs and verified the result in the new repository. On the first checkout, object discovery took 0.75 seconds, pack transfer and indexing took 1.42 seconds, and the independent graph check took 0.88 seconds. At the same commit, the entire history setup took 0.11 seconds. These are medians of individual stages.

| Median per-command resource use | Native Git | Graft |
| --- | ---: | ---: |
| First checkout, user / system CPU | 0.05 / 0.26 s | 8.36 / 1.19 s |
| First checkout, peak RSS | 52.3 MB | 301.9 MB |
| Repeat checkout, user / system CPU | 0.05 / 0.27 s | 0.20 / 0.59 s |
| Repeat checkout, peak RSS | 52.3 MB | 26.4 MB |

The measurements cover the complete CLI process. Fixture preparation and correctness checks ran outside the timed commands. The check after the first spawn can warm filesystem caches before the repeat. CPU time can exceed elapsed time when Git workers run in parallel. Peak RSS uses macOS `/usr/bin/time` child accounting and measures the largest reported resident set. [Homebrew timing samples](measurements/macos-arm64/homebrew-timing.json)

## Disk use with two active workspaces

The starting point was one populated primary checkout. These numbers show the additional, unique file data after creating two workspaces. They include Graft’s project caches and any new pack data in the fixture. MB means 1,000,000 bytes.

| Fixture | Native Git | Graft |
| --- | ---: | ---: |
| Homebrew, full history | 59.88 MB | 201.39 MB |
| Generated source, 4,000 small files and three 8 MiB files | 83.94 MB | 33.64 MB |
| Two tasks with an installed 8 MiB npm dependency | 70.64 MB | 3.53 MB |

Homebrew’s private history accounted for most of Graft’s extra storage. On the generated source fixture, Graft could share the entire verified source pack and the three large files. It used 60% less file data across two workspaces. The corresponding first and repeat checkout times were 0.98 and 0.60 seconds for Graft and 0.30 and 0.30 seconds for native Git. [Generated source timing](measurements/macos-arm64/source-timing.json)

The disk runs used a calibrated APFS observer in separate rounds. It counts each mapped regular-file allocation block once across the fixture. It excludes directories, inode metadata, extended attributes and resource forks. The raw reports also include signed, volume-wide free-space changes, which can include other processes’ writes. Each Homebrew disk result has two samples. The generated-source and task results each have three. [Homebrew disk samples](measurements/macos-arm64/homebrew-apfs.json) · [Generated source disk samples](measurements/macos-arm64/source-apfs.json) · [Task disk samples](measurements/macos-arm64/tasks-apfs.json)

## Two tasks with a populated npm installation

Each run started with the same committed source, installed package and warm npm cache. The two tasks changed source and dependencies, built and tested, committed, landed one task and dropped the other. The conventional native run removed the task checkouts. A second native run also exported the private commit and files to a separate local repository, restored the dropped task and retired it again. Graft ran those recovery operations through its public CLI. Its first-ready time includes registering the reusable installation.

| Median across three rounds | Conventional native Git | Native Git with private recovery | Graft |
| --- | ---: | ---: | ---: |
| First task ready, including setup after the populated primary | 0.24 s | 0.23 s | 0.95 s |
| Second task ready | 0.23 s | 0.23 s | 0.46 s |
| Complete workflow | 1.11 s | 1.40 s | 3.50 s |
| Commands’ user / system CPU | 0.66 / 0.33 s | 0.87 / 0.43 s | 1.59 / 1.02 s |
| Largest reported command RSS | 109.0 MB | 108.9 MB | 91.6 MB |
| Additional file data after retirement and GC | 0.07 MB | 50.52 MB | 70.86 MB |

The fixture had 400 small files, three 8 MiB source files, eight historical 1 MiB blobs and an 8 MiB npm dependency. Graft’s active checkouts shared the eligible source and installation. After retirement, Graft kept complete workspace archives. The native recovery run kept the commit and files needed to restore this fixture. In a separate three-round workflow, adding a small local npm tarball took 0.32 seconds through `git graft add-dep` and 0.16 seconds through native npm. [Task timing and resource samples](measurements/macos-arm64/tasks-timing.json) · [Package-add samples](measurements/macos-arm64/npm-add.json)

## Reproduce the measurements

These runs used macOS 26.5 arm64 on APFS, Git 2.56.0, Rust 1.99.1, Node.js 24.5.0 and npm 11.5.1. The stripped release executable was 3,782,384 bytes, with SHA-256 `14d7a90df28888562ed5c912d3bef08d1283ce362289c4787b004e89e7d48144`. The reports record individual rounds and their execution order.

```sh
cargo build --locked --release
npm run bench:source:real -- --repo /path/to/Homebrew/brew --rounds 4 --advance-base
npm run bench:source -- --repeats 3
npm run bench:agents -- --repeats 3
npm run bench:workflow -- --mode shared --add-package --repeats 3
```

For APFS disk measurements, run the first three benchmarks separately with `--apfs-file-data` and distinct `--output` paths. The Homebrew disk run used `--rounds 2`. The real-source report omits revision IDs by default. These public Homebrew reports used `--include-revisions`. [Tool and filesystem prerequisites](verification.md)
