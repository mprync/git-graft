# Graft workspaces

Each Graft workspace is a standalone local Git repository with its own object database, branches, reflogs, stash, index and working files. Use `git graft spawn NAME --base REV` to create one and use the returned `path` for ordinary Git commands. Refs remain local until work is explicitly transferred; `land` imports the selected committed work into a local destination branch.

`spawn` also supports a named branch, detached HEAD, an orphan branch, sparse checkout, an empty checkout and an initial lock. `list`, `status`, `lock`, `unlock`, `move`, `repair`, `drop`, `restore` and `gc` manage the workspace. `add`, `remove` and `prune` are aliases for spawn, drop and gc respectively. Consult command help for options.

### What takes space

On a compatible volume, a regular tracked file gets its own writable file identity in each workspace. APFS cloning or Linux reflinks let those files share the same physical data blocks, including for small files. Graft creates no symlink or hard link back to the primary file. Edit `src/config.ts` in a graft and the filesystem allocates blocks for the writes the editor performs. The primary and sibling grafts keep their original contents. An editor that replaces a whole file can allocate the size of that file. New files, Git indexes and workspace records also take space.

If the selected commit itself tracks a symlink, Git's normal checkout rules apply. A trusted npm layer can also contain relative symlinks such as a package's command in `node_modules/.bin`. The layer checks that these links resolve inside the installation.

On macOS and Linux, local and tip grafts can share verified files when Git checks them out with LF endings. This includes repositories using `core.autocrlf=input` or `* text=auto` with native LF output. Paths that require CRLF expansion, filters or working-tree encoding go through Git.

With local or tip history, a checkout can share ordinary files alongside a submodule. The private Git index records the selected submodule commit. Its directory starts empty, even when the primary has initialized the submodule and edited its files. Enabling recursive submodule checkout in the workspace’s effective Git configuration delegates checkout to Git.

### History visibility and defaults

For ordinary CLI and MCP spawns, omit the history option to honor the primary repository's `graft.history` setting. In a project without that setting, local history is the default. Existing users who want the previous isolated behavior can set it with `git config --local graft.history isolated` in the primary repository.

Git stores earlier revisions separately from checked-out files, often with several branches mixed into one pack. With `--history local`, an eligible full clone supplies its existing packs, pack indexes and loose objects through copy-on-write. A linked worktree reads the primary’s live object database. Graft takes a snapshot in Git’s own format so the workspace can still read its commits if the primary moves or runs garbage collection. Objects from other branches, staged blobs and dangling objects present at creation are also readable.

On APFS, a directory whose tracked entries are all reusable, and which holds nothing else in the primary, is cloned with one call; Graft lists the clone again and discards it if anything else appeared. Each reused file must hold the selected commit's bytes. Graft accepts a primary file when its stat data matches the primary's non-racy index entry, its size matches the blob and its stat is unchanged across the clone, which is the test `git status` itself relies on. Other files are hashed, and anything that fails is checked out by Git. The workspace's Git store must also resolve every required blob, so a donor file with the right contents cannot conceal a missing object. Graft records each installed file's stat data in the new index, so the first `git status` reads no file contents.

Choose isolated history when the workspace should receive only objects reachable from its selected commit. Graft reuses whole eligible packs. For a suitable mixed pack it keeps the selected records at their existing offsets, rebuilds the index and asks Git to verify the result. Other layouts use Git’s selective export. Later isolated grafts at the same revision can share the verified base.

With `--history tip`, Graft gives Git a shallow boundary at the selected commit. Only that commit and the trees and blobs for its checkout are readable at creation. Log and blame stop at the base. New commits retain their real parent IDs, so landing reconnects them to the primary’s history. A mixed primary pack can require a separate pack of selected objects. Additional grafts at the same base can share that pack. When the branch advances, Graft can reuse blocks from the first parent’s cached pack and export the remaining objects. Git verifies the resulting pack set against the new tip before Graft publishes it.

An agent can register a manager’s package cache, an installed directory or both. A cache stores packages per project and can hold multiple versions. A compatible installed directory gives each graft writable files backed by the existing data blocks. Adding a package writes its new data and whatever manifests, lockfiles or existing packages the resolver changes. The spawn receipt reports the source backend and each installation attachment. [Dependency setup](operations.md#register-a-package-manager) · [Measured space per checkout](benchmarks.md#disk-use-per-checkout)

### Git compatibility

Shallow repositories retain the selected commit’s locally available history and relevant Git shallow boundaries. Missing history is not fetched. Landing stops before changing the destination when ancestry cannot be resolved. Graft declines replacement refs, legacy Git grafts and repository-local checkout filters. Git configuration and remote tracking in the standalone repository differ from the original checkout.

[Operations and dependencies](operations.md) · [Safety and recovery](safety.md) · [Verification](verification.md)
