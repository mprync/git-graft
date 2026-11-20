# Benchmarks

These measurements compare `git graft add` with `git worktree add` the way a developer or agent uses them: create a checkout of a real repository, then run `git status` in it.

## Checkout speed

Homebrew/brew at `bc63375ac3a4` has 3,484 tracked entries: 3,318 regular files and 166 symbolic links. Measured on macOS arm64, APFS, Git 2.56.0, with the release build.

| Median | Previous release | git-graft | `git worktree add` |
| --- | ---: | ---: | ---: |
| First checkout from a fresh clone | 544 ms | **428 ms** | 446 ms |
| A later checkout from the same clone | 397 ms | **294 ms** | 283 ms |
| First `git status` in the new checkout | 352–366 ms | **26–28 ms** | 99–100 ms |
| First checkout + first `git status` | 892 ms | **454 ms** | 550 ms |
| Later checkout + first `git status` | 768 ms | **321 ms** | 383 ms |

Ten rounds measured first checkouts and sixteen measured later ones. Each round gave every arm its own APFS clone of the same checkout and ran `git status` in it first, so the clone's index matched its files and the data was warm, as after `git clone`. Arms rotated and alternated order between rounds, and every result had to report a clean `git status`. Other processes kept the host's load average between 4 and 9, so a single run can vary by tens of milliseconds; the paired medians are the comparison to use. [Raw samples](measurements/macos-arm64/homebrew-directory-clones.json)

### Small and freshly committed repositories

Creating a standalone repository has a fixed cost that a linked worktree does not: reserving a recoverable graft, snapshotting the object store and running Git's checks takes Graft about 150–190 ms even when there is almost nothing to check out. Below a few thousand files, `git worktree add` is therefore faster in absolute terms, although both finish in a fraction of a second. These first-checkout medians use synthetic repositories whose objects are all loose, as right after `git commit`:

| Repository | git-graft | `git worktree add` |
| --- | ---: | ---: |
| 50 files | 190 ms | 26 ms |
| 2,000 files | 343 ms | 163 ms |

The disk saving holds at every size, and from a few thousand files upward Graft's directory clones overtake the cost of writing every file.

## Disk use per checkout

Each value is the increase in physical APFS data extents under a directory holding the primary checkout and its checkouts, measured after each new checkout with [`e2e/apfs-file-data.py`](../e2e/apfs-file-data.py).

| | 1st checkout | 2nd checkout | 3rd checkout |
| --- | ---: | ---: | ---: |
| `git worktree add` | 28.55 MiB | 28.55 MiB | 28.55 MiB |
| git-graft | **0.31 MiB** | **0.29 MiB** | **0.29 MiB** |

A graft's unique data is its private Git index, refs and configuration. Working files and Git packs share their blocks with the primary checkout until they are edited, and an edit allocates only the blocks the editor writes. The measurement excludes directory records and inode metadata, which both methods create.

## A hundred checkouts

[`e2e/scale-bench.ts`](../e2e/scale-bench.ts) creates 100 checkouts of the same 3,484-entry repository one after another, once with `git worktree add` and once with `git graft add`, and measures the physical APFS file data they add. Git runs with default settings for both.

| 100 checkouts | Disk added | Per checkout | Time for all 100 |
| --- | ---: | ---: | ---: |
| `git worktree add` | 2,854.7 MiB | 28.55 MiB | 38.2 s |
| git-graft | **27.8 MiB** | **0.28 MiB** | 43.5 s |

Grafts use 99.0% less disk. Creating the checkouts alone takes about as long either way; Graft's lead is in the first `git status` and in dependencies, below.

## Checkouts with dependencies

[`e2e/dependency-bench.ts`](../e2e/dependency-bench.ts) builds a web project whose dependencies are a typical React, Vite, TypeScript, Vitest and ESLint toolchain (238 packages, an 88 MB `node_modules`) and installs them once in the primary checkout. Each task then needs a checkout it can test in:

| Median of 3 checkouts | Disk added | Ready to test |
| --- | ---: | ---: |
| `git worktree add`, then `npm ci` | 85.38 MiB | 742 ms |
| `git graft add`, with the install shared once by `git graft deps share` | **0.16 MiB** | **237 ms** |

A graft's `node_modules` shares its data with the primary's install, so it costs 99.8% less disk and the checkout is ready 3.1 times sooner. Both arms install from the same warm npm cache, offline.

## Where the time goes

A graft is a standalone repository, so creating one does more than a linked worktree: it reserves a recoverable graft, snapshots the Git object store, and proves every reused file. Graft keeps this ahead of `git worktree add` with four techniques:

- **Directory clones.** A directory whose tracked contents are all reusable, and which holds nothing else in the primary, is cloned with one `clonefileat` call. On Homebrew this covers 3,442 of 3,484 entries and takes about 30 ms, against about 160 ms for per-file clones. Every cloned entry is then verified and anything unexpected is discarded.
- **Git's stat test instead of hashing.** A primary file whose stat data matches its non-racy index entry and whose size matches its blob holds the staged bytes, by the same test `git status` relies on. Its stat must also be unchanged across the clone. Other files are hashed.
- **Recorded stat data.** Graft writes each cloned file's stat into the new index, exactly as Git's checkout does, so the first `git status` reads no file contents.
- **Concurrent Git queries.** Independent read-only Git commands run in parallel instead of one after another.

Set `GIT_GRAFT_TRACE=1` to print the duration of each phase of a command to standard error.

## Reproduce

Build the release binary, then run the real-repository benchmark against a local clone. It never writes to the source repository:

```sh
cargo build --locked --release
npm run bench:source:real -- --repo /path/to/clone --rounds 8
npm run bench:source:real -- --repo /path/to/clone --apfs-file-data   # physical data, separate run
```

Synthetic workflows that include dependency installs, merging and recovery:

```sh
npm run bench:agents
npm run bench:workflow -- --mode shared --add-package
node e2e/scale-bench.ts --repo /path/to/clone --workspaces 100   # disk for many checkouts
node e2e/dependency-bench.ts                                      # real npm dependencies
npm run bench        # three-round disk comparison used in CI
```

Measure wall time and physical data in separate runs, because the extent observer warms filesystem caches. Record the revision, Git version, filesystem and machine load with your results.
