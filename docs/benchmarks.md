# Release benchmarks

## Local history on a populated Homebrew clone

An ordinary spawn uses local history unless the project configures another mode. The following medians cover six rotating comparisons per row on populated APFS clones of Homebrew `bc63375ac3a4`. For the established-project row, each clone already had one empty orphan workspace.

| Local project state | Previous Graft | Current Graft | Linked worktree | Current Graft through first status | Worktree through first status |
| --- | ---: | ---: | ---: | ---: | ---: |
| First use | 421 ms | 436 ms | 318 ms | 855 ms | 413 ms |
| One existing workspace | 490 ms | 394 ms | 330 ms | 833 ms | 444 ms |

Graft now gives file discovery until the workspace’s Git objects are installed. In an established project, this produced a prepared file list in six of six matched trials, up from three of six at the earlier deadline. Both schedules prepared all six on first use. The release also checks for missing blobs when discovery falls back. All eight missing-blob fixtures stayed pending with the donor file intact. The first checkout added 311,296 bytes of physical file data for Graft and 29,941,760 for the worktree. These figures exclude filesystem metadata. [Release samples, recovery and allocation](measurements/macos-arm64/local-discovery-deadline.json)

In eighteen earlier fresh APFS checkout comparisons at Homebrew `bc63375ac3a4`, the release took **547 ms** at the median, its predecessor took **665 ms** and a linked worktree took **356 ms**. Median checkout plus first status took **955 ms**, **1,090 ms** and **488 ms**, respectively. The release won 12 of 18 pairs with a median paired saving of 70 ms. Host load varied substantially across rounds. The first Graft used 311,296 bytes of additional physical file data on APFS and shared all 3,318 regular working files. [Release comparisons and physical allocation](measurements/macos-arm64/local-concurrent-object-checks.json)

For planned local checkouts with at least 128 blob entries, Graft checks blob types while Git walks the installed tree and checks the commit and attributes. Publication waits for both results. Running both schedules in the same diagnostic release reduced median verification time from 163 ms to 106 ms and median checkout from 438 ms to 428 ms. A separate two-command batch verification trial gave inconsistent checkout timings across orders. [Matched timings, changed-object tests and interrupted checkout recovery](measurements/macos-arm64/local-concurrent-object-checks.json)

Reading the primary Git index for discovery matched native Git in 32 cases, but gave a median paired checkout saving of 2 ms in one order and a 17 ms loss in reverse. Splitting blob-type checks between two Git processes shortened that phase from 134 to 100 ms in six traced pairs. It sped up only six of twelve confirmation checkouts. Asking APFS to read cloned Git indexes ahead of use saved a median 36 ms per pair in six initial rounds. In twelve reversed rounds it sped up four checkouts and cost a median 10 ms per pair. Six traced checkouts confirmed that the kernel accepted hints for all four indexes, totaling 10,431,880 bytes each time. The standard release keeps its existing read schedule. [Individual checkout timings, first status and recovery cases](measurements/macos-arm64/local-git-read-scheduling.json)

In an earlier eighteen-pair series, buffering the input and output of Git’s blob-type check yielded median checkouts of **461 ms** for that release, **479 ms** for its predecessor and **351 ms** for a linked worktree. Median checkout plus first status took **931 ms**, **935 ms** and **458 ms**, respectively. The Git command alone returned the same 3,484 rows and saved 1.0–1.4 ms at the median. Whole-command paired medians varied from a 10 ms loss to a 10 ms saving in the two release series. [Release samples, verification contrasts and allocation](measurements/macos-arm64/local-checkout-buffered-verification.json)

Six instrumented spawns timed the regular-file workers and the Git subprocess installing symlinks. Link checkout finished 112–127 ms after the workers started. The last regular-file worker finished 146–157 ms after launch and finished last in all six. Git’s installed-tree listing took 56–60 ms and its blob-type check took 80–86 ms while other checkout work ran. [Per-phase timings](measurements/macos-arm64/local-checkout-buffered-verification.json)

A single persistent Git process checked the installed object types, metadata sizes and exact tree, commit and attribute bytes in 128 ms at the median, compared with 98 ms for the release's concurrent checks. A separate trial kept those concurrent checks and asked Git for the same blobs in pack-offset order. That reduced the type check from 97 ms to 68 ms. Link checkout finished 27 ms earlier, but the last regular-file worker finished at 158 ms in both schedules. The ordering trial won ten of twelve initial pairs and two of six four-checkout confirmation groups. The release keeps its existing request order. [Release timings, native Git queries, missing-object checks and recovery](measurements/macos-arm64/local-git-verification-order.json)

Assigning 64 index entries at a time reduced the median spread in worker completion from 28.8 ms to 2.7 ms. The last worker finished at 151 ms versus 159 ms. The candidate won nine of twelve initial release pairs. In a confirmation that alternated the schedules within groups of four fresh clones, it improved two of six groups and cost a median 15 ms per group comparison. Giving each worker the same number of eligible files also produced opposite results when the run order was reversed. Graft retains the fixed slices. All Graft timing samples shared all 3,318 regular files. Eight dirty-donor and collision cases checked Git fallback order and preservation of user files. [Release samples, worker times and recovery checks](measurements/macos-arm64/local-clone-worker-batches.json)

Preparing destination directories on separate threads slowed the median checkout in both orders, from 430 to 465 ms and from 434 to 458 ms. Reusing a directory handle for adjacent files within each existing worker improved five of twelve matched checkouts. All 48 Graft checkouts shared the 3,318 regular files. Both prototypes preserved a newly created user file on collision across Git 2.46 and 2.56 with SHA-1 and SHA-256. Separately, the durable registry write that marks placement complete took 6.7 ms at the median across six spawns. [Individual timings, resource use and recovery results](measurements/macos-arm64/local-checkout-parent-preparation.json)

Two experiments tested the remaining file costs. Cloning verified files into private staging and moving each top-level entry took 499 ms at the median, versus 460 ms for the release and 350 ms for a worktree. A separate trial filled Git’s index stat fields after hashing the installed files. Median first status fell from 429 ms to 34 ms, but spawn rose from 469 ms to 589 ms. In both Git 2.46 and 2.56, with SHA-1 and SHA-256, that first status missed a same-size edit whose modification time had been restored. The release detected the edit in all four cases. [Timing samples and edit checks](measurements/macos-arm64/local-checkout-staging-and-stat.json)

Graft prepares symlink parent directories before handing those links to Git. Previously, a regular-file worker could create a directory between Git’s lookup and its `mkdir`, causing an otherwise clean spawn to fail. In twelve earlier comparisons, median spawn times were **563 ms** for the directory fix, **495 ms** for its predecessor and **390 ms** for a linked worktree. Each Graft shared all 3,318 regular working files. The first graft added 311,296 bytes of physical file data and the worktree added 29,941,760 bytes, excluding filesystem metadata. [Individual timings, physical allocation and recovery checks](measurements/macos-arm64/local-checkout-link-parents.json)

In an earlier eighteen-round comparison, overlapping local checkout planning with reservation took **414 ms**, its predecessor took **479 ms** and `git worktree add` took **294 ms**. Git status was checked after timing. These runs used the same commit and recorded 270,336 bytes for Graft’s private checkout index. [Per-run timings, CPU, peak memory and allocation](measurements/macos-arm64/homebrew-local-checkout-overlap.json)

In six additional comparisons from that earlier series, median checkout plus first `git status --porcelain` took **786 ms**, **819 ms** and **397 ms**, respectively. The first status checks the cloned files because their Git index entries start with empty stat fields. [Paired checkout and first-status samples](measurements/macos-arm64/homebrew-local-checkout-overlap.json)

Graft checks candidate source files during workspace reservation and uses the installed object store to verify the checkout before publishing its index. A recursive APFS directory clone approached worktree speed in a prototype, but an ignored file created after the initial scan appeared in interrupted staging. The release clones individual tracked files. [Directory clone experiment and interruption test](measurements/macos-arm64/homebrew-local-directory-clone.json)

Moving verified files from temporary staging into the final checkout increased median paired spawn time by **64 ms** with four early readers. All 3,318 regular files used the staged path in each of six runs. With the release’s two-reader limit, 1,711 files used the staged path per run and the median paired cost was **52 ms**. Both trials received empty staging directories before timing. The release continues to clone verified files directly into the workspace. [Local staging trials and four-reader control](measurements/macos-arm64/local-early-file-staging.json)

### How much time is spent before checking out files?

Eight alternating pairs on the standard release tracked the first and last regular files in each of Graft’s four checkout batches. The first sampled file appeared at **246 ms** for Graft and **24 ms** for Git. From there to the last sampled file took **172 ms** and **271 ms**, respectively. The programs returned **36 ms** and **5 ms** later. Whole-command medians were **466 ms** and **299 ms**. Each interval has its own median. Git uses its own checkout order, and sampling at 2 ms observes path visibility. In a separate eight-run Graft trace, the median interval from project ID creation to workspace placement was 106 ms. The interval from checkout index lock creation to the first sampled file was 56 ms. [Per-path observations, setup milestones and executable identity](measurements/macos-arm64/local-git-read-scheduling.json)

Eight matched rounds on the current release measured the effect of an existing reservation. Every fixture first stopped an explicit request immediately before private Git initialization. With the project and workspace reservation kept, retry took **360 ms**. With both moved aside, a fresh spawn took **424 ms**. The linked worktree took **314 ms**. Paying for the stopped attempt and the retry took **526 ms** at the median. With file checkout disabled the corresponding command times were **102 ms**, **166 ms** and **15 ms**. All markers and preparation used disposable clones. A separate replay confirmed that a missing or foreign root preserved the pending journal, and that restoring the owned root let the same lease complete on Git 2.46 and 2.56 with both hashes. [Setup timings and recovery cases](measurements/macos-arm64/local-placement-controls.json)

A release trial created the empty records directory inside the staged registry namespace so its ownership marker and directory could be published together. The median paired checkout cost rose by 14 ms in forward order and fell by 18 ms in reverse order. Across both orders the median saving was 3 ms in twelve comparisons. The current release keeps the existing publication sequence. [Individual trial timings](measurements/macos-arm64/local-placement-controls.json)

Preparing the first workspace marker while creating the registry sped up two of twelve full checkouts. It cost a median 7 ms per pair in one order and 45 ms in reverse. Interruptions before the placement journal could leave a private staging directory. Retrying the same request preserved a file added there and created one active workspace. [First-use timings and recovery checks on Git 2.46 and 2.56](measurements/macos-arm64/local-first-root-staging.json)

A second trial kept the placement order and grouped the marker writes under one final device flush. The marker operation fell from 14.4 ms to 7.0 ms, and 21 of 24 empty orphan spawns improved. In twelve confirmation rounds on populated clones, only four improved. The median paired cost rose by 15 ms. Two earlier runs also stopped at the working-file sharing check, once on the standard release and once on the candidate. All 84 follow-up Graft checkouts shared all 3,318 regular files. The cause of the earlier exceptions remains unresolved. The standard release keeps its existing flush sequence. [Checkout samples, storage receipts and durability limits](measurements/macos-arm64/local-first-root-staging.json)

On the same Homebrew commit, creating a local graft with `--no-checkout` took **179 ms**, against **15 ms** for a linked worktree with `--no-checkout`. An empty orphan graft took **127 ms** and copied no Git history. Preparing one other orphan before timing reduced those two Graft medians to **141 ms** and **86 ms**, respectively. All five commands were measured ten times. The orphan also skips selecting and installing a base commit, so the difference between local and orphan includes more than object copying. [Setup controls and individual samples](measurements/macos-arm64/local-checkout-setup-and-file-count.json)

On single-commit repositories containing about 20 MiB of data, the gap narrowed as the same payload was spread across more files. Each row shows the median first checkout in six alternating pairs, with a fresh populated clone per command.

| Regular files | Payload | Default Graft | Linked worktree |
| ---: | ---: | ---: | ---: |
| 64 | 20 MiB | 239 ms | 42 ms |
| 512 | 20 MiB | 254 ms | 69 ms |
| 3,328 | 20 MiB | 393 ms | 245 ms |
| 512 | 128 MiB | 291 ms | 139 ms |

Both schedules tested on the Homebrew checkout remain experimental. Preparing the object snapshot during Git initialization saved 2 ms and 3 ms per pair at the median in two six-round series. Creating the two first-use registry directories concurrently saved 15 ms in the first series and cost 40 ms in the reversed series. Each local graft shared all 3,318 regular files. [Release identities, paired samples and recovery checks](measurements/macos-arm64/local-checkout-setup-and-file-count.json)

### Earlier default checkout measurements

Changing the unconfigured default from isolated to local history reduced median checkout from 3.620 s to 0.474 s in six rotating comparisons. A linked worktree took 0.292 s. With `--no-checkout`, that local release took 164 ms and Git took 15 ms. [Default policy comparison](measurements/macos-arm64/homebrew-local-default.json)

Six instrumented cold spawns put the median cost of preparing, checking and publishing the local Git object snapshot at 35 ms. Reserving the workspace took 42 ms and checking out working files took 287 ms. Three release prototypes tested changes to file checkout. Starting donor verification during reservation saved 30 ms per pair in one nine-round series and 26 ms with the order reversed. Planning from the destination Git index saved 8 ms and 9 ms. Checking out symlinks alongside regular files produced less consistent results, saving 3 ms and 53 ms. The standard release remains the reference for the default checkout timings above. [Phase measurements and paired samples](measurements/macos-arm64/homebrew-local-setup-phases.json)

### Earlier runs with local history configured

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

From a populated Homebrew clone, `--history isolated` keeps the selected ancestry for log and blame. `--history tip` exports one commit, its trees and blobs. The table uses three fresh primaries for isolated history and twelve for tip history. The APFS block measurements ran separately.

| Clean primary on APFS | Isolated ancestry | Single commit |
| --- | ---: | ---: |
| First spawn, median | 5.90 s | 0.551 s |
| Native worktree in the paired timing runs | 0.41 s | 0.344 s |
| Additional file data, first workspace | 15,482,880 bytes | 5,693,440 bytes |
| Additional file data, second workspace | 315,392 bytes | 319,488 bytes |
| Additional working file data, both workspaces | 0 bytes | 0 bytes |

The first workspace starts with Homebrew already checked out and an empty Graft history cache. The primary's packs mix the selected commit with other history. To keep only that commit and its files in an independent repository, Graft writes a selected pack. In two runs the pack used 5,246,976 new bytes on APFS and its pack index used 106,496. The second graft at the same commit shared both in full. Each graft used 286,720 bytes for its own Git staging index. Other Git and Graft records used 53,248 bytes for the first graft and 32,768 for the second. These counts cover file data after the first status and exclude filesystem metadata.

On a cold tip spawn, Graft verifies the selected tree and committed attributes while reserving the workspace. After reservation, Git accepts the entries into a private index and Graft checks them against the verified listing. Graft can then clone eligible files while Git builds the complete pack. Git verifies the complete pack before checking out the remaining paths or publishing the index.

Reading the selected files’ checkout attributes during donor discovery reduced median cold startup from 0.566 s to 0.551 s and won nine of twelve pairs. Repeat checkout took 0.434 s for Graft and 0.345 s for a worktree. First checkout plus first status took 0.929 s and 0.458 s respectively. All 3,318 regular working files shared their data blocks with the primary. [Paired release timings and physical allocation](measurements/macos-arm64/homebrew-tip-parallel-attributes.json)

Preparing the metadata during reservation reduced median cold startup from 0.646 s to 0.589 s, improving five of six release pairs. Repeat startup took 0.452 s for Graft and 0.380 s for Git. Cold startup plus the first status took 1.017 s for Graft and 0.463 s for Git. All 3,318 regular files shared their existing APFS data blocks. The previous release had already reduced cold startup from 0.770 s to 0.636 s by overlapping checkout and pack creation. [Release timings and allocation](measurements/macos-arm64/homebrew-tip-reservation.json) · [Previous checkout measurements](measurements/macos-arm64/homebrew-tip-overlap.json)

Graft now asks Git which attributes are set before copying source files. On Homebrew's 3,484 tracked paths, the report shrank from 1,579,954 bytes to 80,128 bytes. This also preserves checkout filters literally named `unspecified`. In six fresh release pairs, the preceding and corrected binaries took 0.642 s and 0.631 s respectively for first checkout. Repeat checkout took 0.476 s versus 0.384 s for a worktree. First checkout plus first status took 1.099 s versus 0.489 s. [Paired timings, CPU, memory and APFS allocation](measurements/macos-arm64/homebrew-tip-attributes.json)

When a shallow export fits in one pack and the primary has no reusable pack, Git uses path names from the selected revision to find similar objects for compression. On the preceding release this reduced median pack transfer from 253 ms to 188 ms. [Pack export measurements](measurements/macos-arm64/homebrew-tip-pack-export.json)

### Why does the first tip graft take longer than git worktree add?

Four rotating rounds used fresh populated clones at the same Homebrew commit. Times run from command start to exit. Clean status, the selected commit and the tree were checked afterward.

| Release command, median | Full checkout | With `--no-checkout` |
| --- | ---: | ---: |
| `git worktree add` | 306 ms | 15 ms |
| `git graft spawn --history tip` | 560 ms | 416 ms |

Git shared the primary object database, created the branch and invoked `reset` about 14 ms after starting. Git’s “Updating files” region took 277 ms. Graft created an independent repository, recorded how to recover it, and validated the selected commit and its checkout attributes. In eight release pairs with timing markers in one binary, Graft’s file workers started 223 ms after entering the program. They took 258 ms to verify and clone 3,318 regular files. Worker completion to activation took another 36 ms, including the remaining Git checkout and publication of the index and registry entry.

The full selected-history operation took 280 ms in the uninstrumented cold runs. It overlapped file checkout. In the instrumented runs, the wait for history and the final identity check after the workers finished took 5 ms together. With checkout disabled, Graft still exports and verifies the selected objects, and the file workers never start.

Two controls changed what already existed before running the release CLI. In six alternating triples, a fresh project took 520 ms, a project with one orphan workspace and an empty pack cache took 465 ms, and a project with one prior workspace at the same tip took 381 ms. In a separate four-run trace of fresh projects, 47 ms of a 54 ms workspace reservation went to ordering and flushing ownership markers, recovery records and directories. These observations put the first optimization target on the work before file checkout, particularly durable reservation and preparation of the private checkout. [Release samples, phase timings, controls and Git commands](measurements/macos-arm64/homebrew-worktree-gap.json)

Graft verifies the selected paths, modes and file IDs against the commit’s root tree while reserving the workspace. Git reads the committed attributes from the resulting private index. This removes a separate metadata pack and its indexing step from cold checkout. Graft still verifies the complete selected object store before publishing the workspace index.

| Fresh Homebrew checkout, median of 12 | Previous Graft | With verified index | `git worktree add` |
| --- | ---: | ---: | ---: |
| Spawn | 539 ms | 491 ms | 294 ms |
| First clean status | 332 ms | 329 ms | 133 ms |
| Spawn plus first status | 865 ms | 830 ms | 432 ms |

Cold spawn improved in 10 of 12 pairs, with a median paired saving of 40 ms. All 3,318 regular files shared their data blocks. A separate APFS measurement found 5,677,056 new file-data bytes for the first graft, including 5,246,976 for the selected history pack. A second graft at that commit added 319,488 bytes. [Release timings and file-data breakdown](measurements/macos-arm64/homebrew-tip-verified-index.json)

A follow-up experiment let donor checks start while Git wrote and verified the private checkout index. Across 24 fresh triples, median spawn took 487 ms with the retained release, 482 ms with the candidate and 301 ms with `git worktree add`. The candidate won 14 pairs with a median paired saving of 3 ms. In six separately traced triples, Git started its worktree reset at 16 ms. Graft finished checkout attribute lookup at 164 ms with sequential verification and 167 ms with concurrent verification. The release retains sequential index verification. [Paired measurements and publication checks](measurements/macos-arm64/homebrew-tip-index-readback.json)

An experiment started exporting checkout metadata before opening the project. It kept the selected commit check and verified the exported bytes inside the reserved workspace. In the first 12 fresh comparisons it won eight, with a median paired saving of 30 ms. Reversing the order on 12 more fresh comparisons gave six wins and a median paired saving of 0.3 ms. Each Graft checkout shared all 3,318 regular files. The confirmation did not establish a repeatable startup gain. [Timings and moving-branch checks](measurements/macos-arm64/homebrew-tip-early-metadata.json)

On macOS, Graft now orders each new directory's ownership marker before the exclusive rename and flushes the parent directory before returning. In 12 fresh Homebrew pairs, the two directory publications took 20.8 ms with separate full flushes and 15.4 ms with the shared flush, saving a median of 4.9 ms per pair. The whole-command comparison was inconclusive. In the standard release confirmation, median first checkout was 544 ms before the change, 551 ms after and 308 ms for `git worktree add`. Process-exit tests interrupted both directory publications before and after rename, and verified retry and foreign-directory preservation. [Paired timings, allocation and recovery checks](measurements/macos-arm64/homebrew-tip-directory-publication.json)

A prototype started indexing the checkout metadata after verifying ownership of the pending workspace, while Git initialized the destination. In eight instrumented pairs it started the file workers a median 15 ms earlier. Verification of the metadata still preceded use of any paths or attributes. In twelve uninstrumented pairs, first checkout took 550 ms with the standard release, 538 ms with the prototype and 308 ms with a worktree. The median within-pair saving was 3 ms, with seven wins. The standard release keeps the simpler sequence. An interrupted temporary bare repository blocked recovery. Keeping just the temporary pack and index allowed drop and restore on Git 2.46 and 2.56 with SHA-1 and SHA-256. [Release pairs, phase times and recovery checks](measurements/macos-arm64/homebrew-tip-init-overlap.json)

The standard release also prepares the Git tree listing and private checkout index before starting the file workers. Overlapping the listing with HEAD setup and the index with donor checks reduced this planning interval from 79 to 63 ms in eight instrumented rounds. The isolated prototype won nine of twelve fresh comparisons. The integrated CLI won five of twelve, then eight of twelve with execution order reversed. Across those 24 rounds, median first checkout was 589 ms for the standard release, 619 ms for the candidate and 355 ms for a worktree. The production CLI retains sequential planning. [Paired release timings and APFS allocation](measurements/macos-arm64/homebrew-tip-checkout-planning.json)

### When do the working files appear?

Eight alternating release pairs measured the inode change times of all 3,318 regular files after checkout. Four additional pairs independently watched a sample of 64 paths during checkout. Median observation lag was 1.9–3.5 ms across those runs.

| Fresh Homebrew checkout | First regular file | Half of regular files | Last regular file | Command exit |
| --- | ---: | ---: | ---: | ---: |
| Graft, selected tip | 182 ms | 257 ms | 417 ms | 453 ms |
| Linked Git worktree | 24 ms | 164 ms | 293 ms | 299 ms |

Graft's median first-to-last interval was 233 ms versus 270 ms for Git. Graft spent another 36 ms after its last regular file, including native checkout of the 166 symlinks and workspace activation. Git finished in another 6 ms. A four-worker trial distributed files in batches of 64. It won 13 of 24 pairs and saved a median 3.3 ms per pair. In a separate eight-pair comparison the time to install the final 5% of regular files changed from 43 to 41 ms. The release keeps its fixed worker ranges. [File times, observer checks and release samples](measurements/macos-arm64/homebrew-tip-file-arrival.json)

A separate trial used Git to prepare the private index during reservation. In six traced pairs the index write finished at 121 ms versus 197 ms, and the first top-level working entry appeared at 227 ms in both arms. Across 24 uninstrumented pairs the trial took a median 3.9 ms longer per pair. Its default indexes were byte-identical, but it bypassed the destination's post-index-change hook and ignored its checksum preference. The release creates the index in the destination repository. [Release timings and Git compatibility checks](measurements/macos-arm64/homebrew-tip-early-native-index.json)

#### What does starting symlink checkout earlier save?

The previous release waited for all regular-file workers before asking Git to create the 166 symlinks. For a verified tip checkout with at least 256 entries, Graft now lets Git start on symlinks once the selected pack and workspace identity are verified, provided every regular file is eligible for cloning and there are no submodules. Workers still check donor content. Graft checks the live HEAD again after the workers finish and before publishing the index.

| Fresh Homebrew checkout, 36 comparisons | Previous release | Earlier symlink checkout | Linked Git worktree |
| --- | ---: | ---: | ---: |
| Median time to command exit | 474 ms | 471 ms | 291 ms |

The change won 25 of 36 paired runs. Median savings per pair were −3, 25 and 11 ms in three balanced sets of twelve, or **6.7 ms** across all pairs. In eight separately timed pairs, Git began creating symlinks before the last regular file in three runs. Median time from the last regular file to exit fell from 35.3 to 32.8 ms. All 166 link targets matched Git. The first graft allocated 5,677,056 bytes of file data and shared every regular working file. [Release timings, file creation times, allocation and recovery checks](measurements/macos-arm64/homebrew-tip-native-symlinks.json)

### How much time does an existing reservation save?

Eight rounds timed the standard release at the same Homebrew commit. An earlier attempt stopped just before private Git initialization, leaving only the durable reservation and ownership marker. The matching retry took **397 ms**, compared with **434 ms** for a fresh Graft, **418 ms** for a new name in the already registered project and **298 ms** for a linked worktree. The matching reservation saved 39 ms per pair at the median. Each graft still built its first selected pack and shared 3,318 working files.

Ten further rounds varied the reservation and the pack cache independently. With the matching workspace reserved, a cached pack reduced median history preparation from **280 ms to 19 ms** and complete checkout from **396 ms to 350 ms**. The paired worktree median was **300 ms**. Much of the cold pack work already overlaps working-file creation. Cache seeding and reservation were outside these control timings.

A separate trial changed how Graft flushes the one-commit boundary to disk. The median isolated publication fell from 8.7 to 5.4 ms. The full release command improved in 13 of 24 pairs and by only 0.3 ms per pair in the reversed-order series. The standard release retains its existing publication sequence. [Raw release timings, cache controls and recovery checks](measurements/macos-arm64/homebrew-tip-pending-start.json)

Three writes precede the placement record. Graft persists the workspace ownership marker, its staging directory and the workspace root. In 24 instrumented pairs on APFS, those operations together took 9.66 ms in sequence and 9.61 ms concurrently. Starting them together increased the median marker write from 5.62 to 9.59 ms. Across 24 fresh Homebrew release pairs the concurrent version won 14 times, with median paired savings of 6.11 ms in the first set and 0.21 ms in the reversed set. The release keeps the sequential writes.

Twelve further cold checkouts timed the code around private repository initialization. Median time from entry to the first file workers was 157 ms. The commit check took 4.6 ms, `git init` took 5.7 ms, and persisting the shallow boundary took 8.5 ms. Setting HEAD and applying checkout configuration took another 26.5 ms. Preparing the private index and choosing eligible files before launching workers took 29.0 ms. The paired, uninstrumented CLI took 455 ms and the linked worktree took 286 ms. [Timings and reservation interruption checks](measurements/macos-arm64/homebrew-tip-reservation-flushes.json)

### How much does saving the selected pack for later grafts cost?

Twelve comparisons used the same release and an already registered Homebrew project with an empty cache. Saving the verified pack for later grafts took a median 45 ms. The full checkout took 434 ms. When a foreign cache directory prevented publication, the same checkout took 395 ms and a linked worktree took 293 ms. The median saving within pairs was 35 ms. Both grafts still exported the same private pack and shared all 3,318 regular working files.

A prototype let checkout use the verified workspace pack while Graft finished saving the reusable copy. In six observed pairs the workspace pack index appeared at 346 ms versus 374 ms, and the first symlink appeared at 374 ms versus 399 ms. All 166 link targets matched, and a subsequent graft successfully reused the cache in every run. Across 24 fresh-project comparisons, the prototype won eight and took 7 ms longer per pair at the median. Median command times were 448 ms for the retained release, 449 ms for the prototype and 288 ms for the worktree. [Release timings and publication order](measurements/macos-arm64/homebrew-tip-cache-publication.json)

### How much of the gap depends on the number of files?

Six untraced release pairs per row used fresh populated clones of one Homebrew repository. The smaller commits selected nested subsets of the same tracked paths and preserved their attribute files. Every graft shared all eligible working-file data.

| Tracked paths | Graft tip checkout | `git worktree add` | Difference of medians |
| ---: | ---: | ---: | ---: |
| 43 | 248 ms | 28 ms | 220 ms |
| 904 | 319 ms | 108 ms | 211 ms |
| 1,810 | 370 ms | 185 ms | 185 ms |
| 3,484 | 481 ms | 302 ms | 179 ms |

In six separate diagnostic pairs on the full tree, Graft completed workspace reservation at 88 ms, finished Git configuration at 137 ms and started its file workers at 164 ms. The selected pack and its index were durable at 379 ms. File workers finished at 416 ms. The paired worktree completed at 296 ms. Both Graft operations finished after the paired worktree in every round. On the 43-path tree, reservation alone took 80 ms and the complete worktree took 27 ms.

A second control used native Git to initialize an independent repository, fetch exactly one commit over the local file transport, and check out its tree. This took 68 ms for 43 paths and 461 ms for the full tree. It wrote fresh working files and omitted Graft's workspace registration and recovery records. A Graft candidate changed flushing of temporary pack files while retaining the final durability checks. On the full tree it lost nine of twelve pairs, with a median paired slowdown of 28 ms. The standard release keeps its pack flushes. [Release samples, native Git commands and phase timestamps](measurements/macos-arm64/homebrew-tip-file-count.json)

Reading source configuration, destination configuration and the source attribute-file location concurrently shortened the interval between the first and last Git process in six traced pairs from 14.6 to 0.8 ms. These diagnostic runs included a tracing wrapper. Across 24 untraced release comparisons, median checkout took 497 ms for Graft, 495 ms for the candidate and 299 ms for a worktree. The candidate won 12 pairs, with a median paired saving of 0.5 ms. Graft retains sequential reads. [Release timings and Git process traces](measurements/macos-arm64/homebrew-tip-config-reads.json)

### Does packed size matter when the file count stays fixed?

Six fresh pairs per fixture used the same 512 paths in 16 directories and no extra Git settings. The two 32 MiB fixtures had identical file lengths. One repeated a different 256-byte pattern within each file, and the other used deterministic random bytes. Each Graft shared all 512 regular working files, passed strict Git object verification and exposed only the selected commit.

| Working data | Selected pack | Graft median | Linked worktree median | Graft history median |
| --- | ---: | ---: | ---: | ---: |
| 128 KiB, random | 148 KiB | 237 ms | 46 ms | 90 ms |
| 32 MiB, repeating | 318 KiB | 234 ms | 63 ms | 116 ms |
| 32 MiB, random | 32.0 MiB | 337 ms | 63 ms | 211 ms |

At the same 32 MiB working size, median history transfer rose from 70 to 143 ms and cache publication from 26 to 49 ms. The linked worktree used the primary object store in both cases. Even the 148 KiB pack left a substantial startup gap. History and file checkout overlap, so phase durations cannot be added to the command time. [Release samples and private pack sizes](measurements/macos-arm64/checkout-fixed-path-payload.json)

A release trial saved the cache pack and its index concurrently, then published the verified receipt. In twelve fresh Homebrew triples, median cache publication fell from 45 to 42 ms. End-to-end medians were 428 ms for both Graft versions and 295 ms for the linked worktree. All Grafts shared the 3,318 regular files. The standard release retains sequential cache writes.

The 32 MiB random fixture followed a different transfer path from Homebrew. Graft kept compressed records from the primary pack at their original offsets and asked Git to build an index from the filtered bytes. Across six diagnostic runs, median transfer took 145 ms, including 118 ms in `git index-pack`, 7 ms reading the pack into memory and 12 ms computing its checksum and sealing it. Native indexing took 37 ms at 8 MiB and 220 ms at 64 MiB, with the same 512 files throughout. Homebrew used a fresh selected-pack export, so these indexing times apply to the filtered-pack fixtures. [Paired release samples and filtered-pack phase timings](measurements/macos-arm64/checkout-pack-publication-phases.json)

### How much does copying Git configuration cost?

A linked worktree shares the repository configuration. Graft copies checkout and identity settings into the new repository so they survive moves and later edits to the primary. On fresh Homebrew clones, the previous release took a median 439 ms with zero extra settings, 447 ms with three and 490 ms with nine.

| Extra settings copied, 12 fresh comparisons each | Previous release | Current release | Linked Git worktree | Median paired saving |
| --- | ---: | ---: | ---: | ---: |
| 0 | 425 ms | 432 ms | 290 ms | −1 ms |
| 4 | 453 ms | 440 ms | 288 ms | 11 ms |
| 9 | 481 ms | 439 ms | 297 ms | 41 ms |

On macOS and Linux, Graft can write four or more eligible settings under one Git config lock when Git has initialized a simple core config. It still evaluates branch- and path-dependent configuration after establishing HEAD. The final release won all 12 comparisons with nine settings and 11 of 12 with four. The zero-setting control skips this code path. Checks on Git 2.46 and 2.56 covered changed conditional includes, quoted values, CRLF checkout, a preexisting config lock and moving the workspace. When checkout failed immediately after saving the configuration, archiving and restoring the pending workspace preserved both its config and a user-created file. [Release samples, configuration checks and APFS allocation](measurements/macos-arm64/homebrew-checkout-config-writes.json)

### Does verifying donor files earlier reduce checkout time?

On macOS, two readers can verify regular donor files while Graft prepares the private workspace. Each reader retains an open descriptor. Checkout checks that the selected path still names the verified file, that its metadata still matches, and that Git permits raw checkout for the path. A changed file goes through the ordinary checkout path. Speculative reads have a combined 64 MiB budget and require enough free descriptors.

In 24 fresh Homebrew triples, median first checkout fell from **488 ms to 476 ms**, compared with **293 ms** for a worktree. The release won 18 pairs with a median paired saving of 16.5 ms. Six pairs each at 176 and 904 paths showed less than 1 ms median paired improvement. The first full graft allocated 5,677,056 file-data bytes and a second at the same commit added 319,488 bytes. Both shared all 3,318 regular files.

Eight diagnostic pairs explain the worker limit. Four readers finished verifying all 3,318 files before cloning began in every run. Median cloning time fell from 268 to 230 ms, but cloning started at 190 ms instead of 170 ms. Four readers saved only 4.5 ms per pair in uninstrumented checkout timings. Two readers gave a larger whole-command improvement. [Release comparisons, phase timings, allocation and mutation checks](measurements/macos-arm64/homebrew-tip-donor-verification.json)

In six diagnostic pairs, closing all 3,318 pinned donor descriptors took a median 2.4 ms. Transferring each descriptor to its checkout worker reduced final cleanup to 0.06 ms. Across 24 uninstrumented release pairs, the change made checkout 4.6 ms slower per pair at the median. The standard release took 463 ms, the candidate 470 ms and the linked worktree 296 ms.

On the standard implementation, the last worker to finish spent a median 179 of its 232 ms creating APFS file clones, including destination lookup. Eight separate diagnostic rounds held early verification at two readers and varied only the workers creating files. Whole-command medians were 646 ms with one worker, 514 ms with two, 440 ms with four and 472 ms with eight. Graft retains four checkout workers. [Release samples, descriptor lifetimes and worker timings](measurements/macos-arm64/homebrew-tip-descriptor-handoff.json)

### How much does pack creation slow file checkout?

Eight comparisons kept the primary’s complete history and working tree identical and added a pack containing exactly the selected tip to half the fresh clones. Graft reused that pack on the first checkout in every such clone. Median history setup fell from 345 ms to 170 ms. Median whole-command time was 551 ms with the original packs and 597 ms with the extra pack. In a separate eight-round traced run, the corresponding command times were 498 ms and 480 ms. The history saving did not translate into a consistent startup saving.

Another eight traced rounds kept the original packs and prepared only an empty orphan graft before timing. Git’s checkout attribute query finished at 175 ms in a fresh project and 142 ms in the prepared project. A native worktree started its reset at 18 ms. Both Graft arms built the selected pack from scratch. First-use registry setup explains part of the delay before file cloning. [Source pack and registry controls](measurements/macos-arm64/homebrew-tip-source-pack-layout.json)

In eight paired cold runs, verifying and cloning the 3,318 regular files took 220 ms when the file workers started after history creation had finished, and 301 ms when both jobs started together. The median paired difference was 81 ms. Serializing the jobs increased median command time from 542 to 618 ms. The standard release took 546 ms and `git worktree add` took 329 ms in these rounds.

Moving the two simultaneous jobs to the moment Graft selected the commit would give an estimated median of 367 ms. This estimate subtracts all intervening setup and keeps the measured concurrent durations and publication time. It missed the paired worktree time in all eight rounds, by a median of 42 ms. An earlier eight-round series gave a median paired gap of 56 ms under the same assumption.

Restricting only `pack-objects` to one thread shortened the file-worker phase from 293 to 246 ms, but history creation grew from 273 to 453 ms. The complete command lost all ten pairs. Restricting only `index-pack` won five of ten with a median paired loss of 0.8 ms. Using two threads in each Git process also won five of ten. These experiments retained Git's checks, the selected tree and file sharing. [Individual command and phase timings](measurements/macos-arm64/homebrew-tip-history-contention.json)

Changing the number of file workers also affected the critical path. On the 16-core Mac, eight fresh rounds measured median file verification and cloning times of 512 ms with two workers, 401 ms with four and 615 ms with eight. In eight reversed-order rounds using the standard four-worker release as the control, whole-command medians were 707, 645 and 809 ms respectively. Native worktrees took 361 ms. The standard four-worker limit won six of eight pairs against each alternative. The filesystem was in active use during these measurements. [Release samples and worker phase timings](measurements/macos-arm64/homebrew-tip-file-workers.json)

### Can Graft reuse the compressed objects already in the primary?

The Homebrew tip needed 3,758 objects. The primary already had usable compressed records for 2,494 of them, including 540 deltas whose base also belonged to the tip. A prototype copied those records and encoded the remaining 1,264 objects into the same private pack. Git verified exact object membership and the shallow repository passed `fsck --strict` with four compression levels.

Using the macOS compression library reduced median pack transfer from 193 to 155 ms in eight fresh pairs. In twelve more pairs, standard Graft took 562 ms, the prototype took 559 ms and `git worktree add` took 309 ms. The prototype won seven pairs. Eight separately instrumented pairs showed why the transfer saving had limited impact. History finished before the file workers in every run with either writer. Changing the writer shortened median history creation from 272 to 239 ms. Verifying and cloning the files still took 311 ms with the prototype and 294 ms with the standard writer. These phases overlap.

The standard first graft used 5,693,440 additional APFS file-data bytes and the prototype used 5,758,976. A second graft at the same tip added 319,488 bytes with either writer. After adding one tracked file, the next graft used 458,752 bytes with the standard writer and 1,916,928 with the prototype. The prototype writes deltas that refer to their base by object ID. Graft’s parent-pack filter exports those deltas again when preparing the new tip. The standard release retains its existing writer. [Individual timings, exact pack checks and allocation breakdown](measurements/macos-arm64/homebrew-tip-compressed-records.json)

### Does using one pack start checkout sooner?

A prototype put the selected commit, its trees and its committed attributes first in the full pack. Git could verify all 433 metadata objects after reading 126 KB. In six traced runs, the full history was ready 29 ms earlier than with the ordinary writer, measured from program entry. The file workers started 22 ms later because they had to wait for the prefix before planning checkout.

Moving private repository initialization into that wait let indexing start as soon as the prefix arrived. Across twelve fresh release comparisons, median command time was 568 ms for standard Graft, 591 ms for the prototype and 317 ms for `git worktree add`. The prototype lost nine pairs, with a median paired penalty of 14 ms. Both Graft versions shared all 3,318 regular files. [Release comparisons, phase timings and allocation](measurements/macos-arm64/homebrew-tip-pack-prefix.json)

### Can checkout supply the Git pack while it verifies the files?

The checkout workers produced 3,307 unique blobs from the 3,318 regular Homebrew files they already read. Compressing them added a median 21 ms to the worker phase across eight pairs with history already cached. Git independently verified the resulting 3,758-object repository, including the remaining 451 objects from the primary.

On a cold checkout, streaming every verified blob into Git’s indexer took 573 ms end to end, against 533 ms for standard Graft and 298 ms for a worktree. A second prototype reused compressed records already in the primary and had checkout supply only the remaining 1,194 blobs. In twelve fresh comparisons, that took 556 ms against 539 ms for standard Graft and 312 ms for a worktree. Git took another 24 ms to finish indexing after it received the required file records. Checking and durably publishing the pack followed. The prototype added 1,921,024 physical file-data bytes after a one-file advance, compared with 458,752 for the standard writer. [Paired release timings, exact object checks and physical allocation](measurements/macos-arm64/homebrew-tip-checkout-fed-pack.json)

### External Git attributes

With the user attribute `*.rb linguist-language=Ruby`, the previous release wrote private copies of all 3,318 regular Homebrew files. The updated release shares all 3,318. Additional APFS file data for two single-commit grafts fell from 64,954,368 to 6,012,928 bytes, including their Git data and Graft records. Working files accounted for zero new file-data blocks in both measured rounds. Two native worktrees used 59,883,520 bytes. Reproduce with `npm run bench:source:real -- --history tip --clean-primary --rounds 2 --apfs-file-data --global-attribute '*.rb linguist-language=Ruby'`.

Six separate timing pairs without the external rule put median first spawn at 0.596 s before and 0.591 s after. The paired native worktrees took 0.346 s in the updated release runs. [Timing samples and APFS allocation](measurements/macos-arm64/homebrew-tip-external-attributes.json)

### Directory traversal on macOS

Each checkout worker can retain up to six ancestor directory descriptors per source and destination. It rechecks each directory’s identity before using it for a new sibling path. Twelve rounds timed the previous release, this change and native Git back-to-back on fresh Homebrew clones in all six orders. Median first creation was 574 ms, 563 ms and 326 ms respectively. The change won eight rounds, with a median paired saving of 9 ms. [Individual checkout timings](measurements/macos-arm64/homebrew-tip-directory-traversal.json)

### Text checkout on a populated clone

On Homebrew with `* text=auto` and `core.autocrlf=false`, the previous release delegated all 3,318 regular files to Git. Sharing the verified files reduced median first checkout from 641 ms to 614 ms across twelve fresh clones. Native worktree creation took 321 ms. The new release won nine pairs, saving a median of 31 ms per pair. Without the text rule, both Graft releases took 619 ms and Git took 312 ms.

With `core.autocrlf=input` and the same text rule, the first graft used 5,693,440 additional APFS file-data bytes, down from 35,164,160. Working files accounted for the entire reduction. The second graft added 319,488 bytes. To reproduce the disk measurement, run `npm run bench:source:real -- --history tip --clean-primary --autocrlf input --global-attribute '* text=auto' --rounds 1 --apfs-file-data` on macOS. [Release samples and allocation by purpose](measurements/macos-arm64/homebrew-tip-text-checkout.json)

### A checkout containing a submodule

This fixture adds one submodule entry and a tracked `.gitmodules` file to Homebrew `bc63375ac3a4`. With `--history tip`, all 3,319 regular files shared the primary’s APFS data blocks. Previously, a submodule anywhere in the tree caused Git to write private copies of every working file. The new workspace contains an empty submodule directory and records the child commit ID in its private index.

| Six alternating release pairs | Previous | With file sharing |
| --- | ---: | ---: |
| First graft, median | 621 ms | 555 ms |
| Repeat graft, median | 357 ms | 423 ms |
| First Git status after first graft, median | 130 ms | 379 ms |
| Native worktree creation, median | 297 ms | 327 ms |
| First graft, additional physical file data | 35,168,256 bytes | 5,693,440 bytes |
| Repeat graft, additional physical file data | 29,794,304 bytes | 319,488 bytes |

Disk measurements ran separately. Sharing saved 29,474,816 working-file bytes per graft. Git checks the cloned files on first status because their staging index starts without cached file timestamps. [Release timing and allocation samples](measurements/macos-arm64/homebrew-tip-submodule.json)

### What happens after a one-file commit?

The first two grafts share a selected pack and pack index occupying 5,353,472 bytes. When the primary advances by one file, Graft can share existing blocks from the parent’s cached pack while excluding the parent commit and superseded objects. On Homebrew, the third checkout allocated **458,752 bytes**, down from **5,672,960 bytes** on the previous release. The first two grafts added zero working-file data blocks. These measurements count physical file data across the fixture, including Git indexes and Graft records. They exclude filesystem metadata. [APFS measurements and release samples](measurements/macos-arm64/homebrew-tip-parent-reuse.json)

| Clean Homebrew checkout | Graft spawn | Worktree add | Additional Graft file data | Additional worktree file data |
| --- | ---: | ---: | ---: | ---: |
| First at selected commit | 0.673 s | 0.439 s | 5,693,440 bytes | 29,941,760 bytes |
| Second at same commit | 0.639 s | 0.465 s | 319,488 bytes | 29,941,760 bytes |
| Third after one-file advance | 0.686 s | 0.436 s | 458,752 bytes | 29,945,856 bytes |

Times are medians from four fresh primaries using the installed release. After the advance, median Git pack transfer fell from 231 ms to 130 ms. The full checkout took 0.743 s on the previous release. Disk observations ran separately and agreed in both rounds. [Timing and allocation samples](measurements/macos-arm64/homebrew-tip-parent-reuse.json)

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

### Clean checkout with explicitly isolated history

The pinned Homebrew commit has 53,716 commits and 371,804 reachable objects. A linked worktree reads the primary’s existing packs. Graft builds an independent store containing only the objects reachable from the selected commit. On this fixture it filters a 112 MB pack and verifies its index.

| Median public CLI time on APFS | Before overlapping history verification | With overlapping history verification | Linked Git worktree |
| --- | ---: | ---: | ---: |
| First populated checkout, six fresh clones per command | 4.616 s | 4.123 s | 0.288 s |
| Same checkout with an unrelated sibling directory changing every 200 ms, six per command | 4.657 s | 4.068 s | 0.288 s |
| First workspace with working-file checkout disabled, three per command | 3.500 s | 2.919 s | 0.015 s |

On eligible APFS checkouts, Git walks the private history while it independently verifies the large pack and its proposed index. Both operations use the same pack bytes. Graft holds the verification directories open and uses their filesystem IDs, so creating a sibling directory leaves the walk usable. Git verified the complete selected history in every comparison. Private history setup fell from 3.291 s to 2.761 s in the quiet series. Median user CPU rose from 6.72 s to 7.85 s and peak RSS held at 289 MB.

All 3,318 regular working files shared data with the populated primary. A separate allocation run measured 15,486,976 additional file-data bytes for the first graft, 315,392 for another at the same commit and 1,818,624 after adding one file. The linked worktree added 29,941,760 bytes on first checkout. These counts exclude filesystem metadata. [Release binaries, per-run timings, APFS allocation and recovery checks](measurements/macos-arm64/homebrew-isolated-directory-id.json)

#### Working files after history setup

The linked worktree checks files out directly and fills its index as it writes them. On its first isolated checkout, Graft clones eligible files from the primary into a reusable snapshot, verifies their contents and clones them again into the workspace. Git then reads the installed files to populate the index's file statistics. In three diagnostic release runs, building the snapshot took 498 ms, cloning it into the workspace took 263 ms, and the final Git index refresh took 340 ms at the median. Omitting the refresh caused `git diff-files` to report all 3,484 clean paths as changed.

On macOS, Graft's checkout workers now read newly cloned small files before Git refreshes the index. This applies to snapshot checkouts with at least 256 entries and reads at most 1 MiB per file and 8 MiB per worker. Git still checks every installed file. In six diagnostic pairs, these reads increased the clone phase from 260 to 368 ms and reduced the subsequent refresh from 360 to 137 ms.

| First populated Homebrew checkout, nine fresh primaries per command | Before reading clones in parallel | With parallel reads | Linked Git worktree |
| --- | ---: | ---: | ---: |
| Median time | 4.138 s | 4.028 s | 0.298 s |
| Median user / system CPU | 7.83 / 3.91 s | 7.81 / 3.93 s | 0.05 / 0.23 s |
| Median peak RSS | 289 MB | 289 MB | 52 MB |

The standard release finished sooner in all nine pairs, with a median paired saving of 105 ms. Private history setup took 2.770 s. The APFS observer counted 15,482,880 additional regular-file bytes for the first graft and 315,392 for the repeat. The working files contributed zero additional file-data blocks on the first graft. [Release comparisons, phase samples, allocation and validation](measurements/macos-arm64/homebrew-isolated-index-refresh.json)

#### Reusing Git packs on a trusted primary

Six fresh Homebrew primaries per command compared the same release with both history policies. The local snapshot shared all four pack files and their indexes through APFS. The isolated store contained exactly the 371,804 objects reachable from the selected commit. An unrelated loose object planted in each primary was readable from the local snapshot and absent from every isolated store.

| Cold checkout, median | Time |
| --- | ---: |
| Linked Git worktree using the primary object store | 0.317 s |
| Graft with `--history local` | 0.477 s |
| Graft with isolated history | 3.748 s |

Local history removed 3.271 s from the median for this fixture. A local graft still initializes a standalone repository, reserves a recoverable workspace and checks which working files it can clone. The linked worktree uses the primary repository throughout its lifetime. [Release samples and object visibility checks](measurements/macos-arm64/homebrew-isolated-signed-staging.json)

#### Staging files during signed snapshot creation

On an eligible cold APFS checkout, Graft compares each regular file with the bytes streamed by Git. After a match it clones from the same open file into workspace staging. Before installation it checks the file against the published snapshot record. Git refreshes the final index. A direct-install trial saved 829 ms per pair at the median, but an immediate `git diff-files` reported changes on clean files. Even with the index refresh restored, bypassing the reusable snapshot lost block sharing on a later checkout after a same-size donor edit.

| Cold checkout, median | Previous release | With verified staging | Linked Git worktree |
| --- | ---: | ---: | ---: |
| Nine rotating triples | 3.631 s | 3.524 s | 0.283 s |
| Six reversed triples | 4.112 s | 4.769 s | 0.345 s |
| User / system CPU, nine-triple series | 7.12 / 3.79 s | 7.07 / 3.55 s | 0.05 / 0.22 s |
| Peak RSS, nine-triple series | 289 MB | 289 MB | 52 MB |

Verified staging won nine of nine pairs in the first series and one of six in the second. The median time outside private history setup fell from 1.214 to 1.120 s and from 1.432 to 1.354 s, respectively. The end-to-end benefit varied with history setup. In the second series its median rose from 2.651 to 3.332 s.

The first graft added 15,486,976 bytes of regular-file data, a repeat added 315,392 bytes, and a one-file advance added 1,814,528 bytes. All 3,318 regular working files shared blocks with the primary. A separate interruption test reproduced an archive failure on the previous release with Git 2.46 and 2.56. Removing the unused object directory from the temporary Git context allowed both versions to archive and restore the user's note after an interrupted checkout. [Paired release samples, APFS allocation and recovery checks](measurements/macos-arm64/homebrew-isolated-signed-staging.json)

#### Discovering the selected history

On a first graft at the pinned Homebrew commit, Graft must find the exact set of objects to put in the private repository. A linked worktree can immediately use the primary repository's object database. The standard release now asks Git for the selected commits and their root trees, assigns batches of distinct trees to four Git processes and combines their object IDs. Git still verifies the resulting private store before publication. Small histories, repositories with reachability bitmaps and unsuccessful discovery attempts use the ordinary Git traversal.

| Cold checkout on a populated primary, nine fresh clones per command | Previous Graft release | With parallel tree discovery | Linked Git worktree |
| --- | ---: | ---: | ---: |
| Median end-to-end time | 3.978 s | 3.661 s | 0.294 s |
| Median source object discovery | 593 ms | 275 ms | — |
| Median total private history setup, including discovery | 2.749 s | 2.435 s | — |
| Median user / system CPU | 7.80 / 3.88 s | 7.89 / 3.88 s | 0.05 / 0.22 s |
| Median peak RSS | 289 MB | 289 MB | 52 MB |

Graft finished sooner in all nine pairs. The median paired saving was 309 ms. Each graft contained exactly the selected 371,804 objects, including 53,716 commits, and shared all 3,318 regular working files with the primary. In a separate APFS run the first graft added 15,486,976 bytes of file data. Of those, 9,338,880 bytes were history indexes and 4,321,280 bytes were history packs. Working files added zero private data blocks. A repeat at the same commit added 315,392 bytes and a checkout after adding one file added 1,814,528 bytes. These figures exclude filesystem metadata.

The largest remaining history cost is Git's verification of the filtered pack. In three diagnostic runs, reading and indexing 329,692 records took 949 ms and resolving 236,258 deltas took another 594 ms at the median. An experiment split independent delta families into eight temporary packs. It added 121 MB of temporary files and checked the original installed pack after the checkout timer stopped. The release keeps the original pack verification inside the timer. [Release samples, object-set comparisons, allocation and failure checks](measurements/macos-arm64/homebrew-isolated-tree-discovery.json)

#### Verifying whole packs once during a cold checkout

Git's process trace showed that Graft verified two complete primary packs after cloning them, then verified the same two packs again through the file descriptors used for the private history walk. The rebuilt release keeps the descriptor-based verification when that walk is available. On other paths it runs the existing whole-pack verifier. Across the four installed packs, the traced first checkout went from six `git index-pack --verify` invocations to four.

| Cold median on nine fresh Homebrew clones per command | Previous release | Rebuilt release | Linked worktree |
| --- | ---: | ---: | ---: |
| Elapsed time | 3.634 s | 3.637 s | 0.296 s |
| User CPU | 7.85 s | 7.10 s | 0.05 s |
| System CPU | 3.90 s | 3.77 s | 0.22 s |
| Peak RSS | 289 MB | 289 MB | 52 MB |

The repeated checks overlapped the larger filtered-pack verification. Removing them reduced CPU use. The nine-pair release confirmation had a median paired slowdown of 17 ms. The earlier twelve-pair trial had a median paired saving of 13 ms. These comparisons establish no repeatable checkout latency improvement.

All nine rebuilt checkouts contained exactly 371,804 selected objects and shared all 3,318 regular working files. A separate APFS measurement counted 15,503,360 additional file-data bytes on the first graft, 315,392 on repeat and 1,814,528 after one new file. Working files added zero private data blocks on first checkout. On Git 2.46 and 2.56, a failed descriptor-based verification retried once, then required fresh serial verification before publication. A custom delta-cache setting used ordinary verification from the start. [Release samples, verification traces and fallback checks](measurements/macos-arm64/homebrew-isolated-companion-verification.json)

The largest filtered pack still required about 1.57 s of native Git verification in the checkout trace. On the same pack and index in isolation, median verification took 1.562 s with eight workers, 1.547 s with twelve and 1.712 s with sixteen. All used the same aggregate 384 MiB delta-cache budget. Moving from eight to twelve raised median system CPU from 1.18 s to 2.63 s and peak RSS from 88 MB to 102 MB. The release retains eight workers. [All eighteen verification samples](measurements/macos-arm64/homebrew-isolated-companion-verification.json)

#### Verifying the largest pack by delta family

A linked worktree can read the primary's existing pack immediately. Graft's isolated history path filters that pack to the selected objects and verifies the result. On the pinned Homebrew clone, the filtered pack contained 329,692 records in 111.6 MB. With the pack bytes held constant, native Git verified it in 1.549 s. A bounded Rust verifier divided independent delta chains among eight workers, reconstructed and hashed every object, and checked each compressed record's length and CRC. Its median was 0.558 s. Standalone peak RSS rose from 88 MB to 200 MB.

| Median cold checkout, six fresh clones per command | Standard Graft | Experimental Graft | Linked worktree |
| --- | ---: | ---: | ---: |
| Separate verifier process on the standard release | 3.691 s | 3.272 s | 0.296 s |
| Verifier compiled into the CLI | 3.599 s | 3.555 s | 0.291 s |
| Compiled CLI with compression-library CPU detection enabled, reversed order | 3.624 s | 3.296 s | 0.293 s |

In the last series private history took 2.411 s with standard Graft and 2.100 s with the trial. The trial finished sooner in all six pairs, with a median paired saving of 319 ms. All six destinations had exactly 371,804 objects, a clean index and 3,318 shared regular files. A separate physical allocation run measured 15,486,976 bytes of additional file data on first checkout, 315,392 on repeat and 1,818,624 after a one-file commit. The first checkout added zero private working-file data blocks.

The trial replaced the release's native verification of the filtered pack. Git then independently verified every installed pack after the timer stopped. The standard release retains native verification before publication. [Per-run wall time, CPU, memory, allocation and Git verification results](measurements/macos-arm64/homebrew-isolated-delta-tree-verification.json)

#### Preparing working files during isolated history verification

On six fresh Homebrew clones per command, the standard Graft release spent 2.428 s of its 3.622 s median building and verifying private history. A linked worktree used the primary's object database and completed its entire checkout in 0.282 s. The Graft trial prepared working files in private scratch during history verification, then checked both the reusable snapshot and the staged files against the final repository before publication.

| Cold checkout, median of six per command in each series | Standard release | Experimental release | Linked worktree |
| --- | ---: | ---: | ---: |
| Prepare reusable snapshot early | 3.666 s | 3.512 s | 0.296 s |
| Also prepare the working files early | 3.673 s | 3.002 s | 0.291 s |
| Retain verified history files through publication | 3.636 s | 2.988 s | 0.286 s |
| Verify both sets of staged bytes against final checkout settings | 3.622 s | 3.152 s | 0.282 s |

In the final series the trial finished sooner in all six pairs. It used 8.56 s of user CPU and 4.93 s of system CPU, compared with 7.88 s and 3.90 s for the standard release. Both peaked at 289 MB RSS. Each graft contained the exact 371,804 objects reachable from the selected commit. All 3,318 regular working files shared data with the primary. A separate APFS measurement counted 15,486,976 additional file-data bytes on first checkout, 315,392 on repeat, and 1,818,624 after adding one file. The working files accounted for zero private data blocks on the first checkout. The scan excludes filesystem metadata.

The trial remains experimental. In a controlled delay, holding the optional Git verification open kept the workspace pending for the full 1.007 s observation after the destination reached its ref update. Releasing it allowed checkout to finish cleanly. The worker join currently lets that optional operation delay fallback. The trial passed 148 CLI and MCP scenarios on each of Git 2.46 and 2.56, including separate coverage of dirty donors, changed settings, SHA-1 and SHA-256, and interruption recovery in the focused fixtures. [Raw release timings, allocation, scheduling experiments and validation](measurements/macos-arm64/homebrew-isolated-snapshot-overlap.json)

#### Cancelling optional checkout work and recovering interrupted copies

In six fresh Homebrew comparisons, the standard release took 3.616 s and the linked worktree took 0.284 s. The standard release spent 2.421 s creating and verifying private history. The trial copied donor files into private scratch inside the pending workspace during history verification and checked the staged files against the final Git checkout before publishing them.

| Cold median, six fresh primaries per command | Standard Graft | Trial | Linked worktree |
| --- | ---: | ---: | ---: |
| Stage donor copies with a cancellable Git tree listing | 3.605 s | 3.348 s | 0.285 s |
| Hash staged copies during history verification | 3.616 s | 3.160 s | 0.286 s |
| Bound reads and verify final file modes | 3.617 s | 3.177 s | 0.290 s |
| Track ownership and publish by APFS file identity inside the pending workspace | 3.616 s | 3.328 s | 0.284 s |

The last trial beat standard Graft in all six pairs. Checkout finished 148 ms after the destination ref update while the optional Git listing was held open. All 3,318 regular working files shared source data. The APFS scan counted 15,491,072 additional file-data bytes on first checkout, 315,392 on repeat and 1,818,624 after a one-file advance.

The release retains ordinary snapshot preparation. Interrupting the last trial after it had copied an edited donor file left the uncommitted bytes inside the workspace's private Git directory. Drop and restore retained those bytes in all four combinations of Git 2.46 and 2.56 and SHA-1 and SHA-256. The working file had never been published. A subsequent clean spawn succeeded.

A separate APFS probe cloned a 4 MiB regular file, opened the clone, unlinked it, and used its open descriptor as the source for a second clone. The second clone matched the original bytes and shared all 1,024 allocation blocks. Passing `COPYFILE_CLONE_FORCE` to `fcopyfile` with an anonymous destination reproduced the bytes with zero shared blocks. The descriptor result covers one file and has no end-to-end timing. [Release samples, allocation, recovery checks and descriptor probe](measurements/macos-arm64/homebrew-isolated-cancellable-staging.json)

#### Earlier measurements

In two cold traces before overlapping verification, rebuilding the filtered pack index was the longest Git command at 1.83 s each. Disabling working-file checkout still left a 3.73–3.81 s spawn.

On the 16-CPU Mac, eight Git workers indexed that same filtered pack in 1.62 s at the median, compared with 1.89 s with four. All 32 indexes in the worker-count comparison matched byte for byte. Graft now uses eight workers for filtered packs with at least 100,000 selected objects when at least eight CPUs are available and the private repository has no explicit delta-cache limit. Halving Git’s per-worker cache allowance keeps its aggregate default allowance at 384 MiB.

| Median of eight fresh checkouts | Four-worker release | Eight-worker release | Linked Git worktree |
| --- | ---: | ---: | ---: |
| Time to create the workspace | 4.98 s | 4.76 s | 0.305 s |
| Pack transfer and indexing | 2.01 s | 1.74 s | — |
| User / system CPU | 6.62 / 2.96 s | 6.87 / 3.78 s | 0.05 / 0.23 s |
| Peak RSS | 289 MB | 289 MB | 52 MB |

The release was faster in seven pairs, with a median paired saving of 228 ms. Every isolated repository contained exactly the selected object set. A separate APFS run measured 15,478,784 additional file-data bytes for the first graft, 315,392 for the second and 1,814,528 after a one-file advance. All 3,318 regular working files shared their data with the primary on first checkout. [Release samples, verified indexes and allocation](measurements/macos-arm64/homebrew-isolated-delta-workers.json)

Two history walks contribute to the cold-start gap. Git's existing commit graph let Graft enumerate the source objects in 564 ms, compared with 829 ms without it. The release then walked the private store to verify that it contained exactly the selected history. The two walks serve different purposes because the source can include unrelated objects.

In a separate experiment on already prepared pack files, indexing the large filtered pack and then walking the private history took 2.305 s. Building a temporary pack index and running the walk while Git independently indexed the same bytes took 1.625 s. The temporary index was ready in 118 ms. All six paired runs produced byte-identical indexes and the same 371,804 reachable IDs. The paired median saving of 684 ms covers those two phases only. [Phase samples and Git 2.46/2.56 descriptor checks](measurements/macos-arm64/homebrew-isolated-closure-overlap.json)

An experimental release let Git walk the isolated history while it verified the filtered pack against a proposed index. Each workspace contained exactly the 371,804 reachable objects and shared all 3,318 regular working files. Timings below cover the complete command.

| Median cold checkout on APFS | Eight-worker release | Overlapping verification trial | Linked Git worktree |
| --- | ---: | ---: | ---: |
| Quiet fixture directory, six rotating comparisons | 4.60 s | 4.01 s | 0.288 s |
| Unrelated sibling directory created and removed every 200 ms, three rotating comparisons | 4.62 s | 6.43 s | 0.293 s |

The trial checked every ancestor directory's change timestamp to detect moves during verification. Creating an unrelated sibling changed one of those timestamps. All three affected checkouts discarded the early result and repeated verification. That regression ruled out this implementation for the standard release. [Per-run wall time, CPU, memory and verification results](measurements/macos-arm64/homebrew-isolated-closure-publication.json)

### Checkout from a dirty primary

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

Starting the committed-tree and attribute reads before Graft opened its registry gave a median local spawn of 444 ms across twelve fresh clones. The standard release took 426 ms and linked worktrees took 314 ms. The candidate improved four matched pairs. Four additional diagnostic runs confirmed that the candidate used its early result. When the primary branch moved during discovery, it checked out the newly selected commit on both Git versions and in both object formats. [Per-checkout timings, CPU, memory, sharing and branch-movement checks](measurements/macos-arm64/local-early-source-discovery.json)

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
