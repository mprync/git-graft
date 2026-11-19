# Graft workspaces

Each Graft workspace is a standalone local Git repository with its own object database, branches, reflogs, stash, index and working files. Use `git graft spawn NAME --base REV` to create one and use the returned `path` for ordinary Git commands. Refs remain local until work is explicitly transferred; `land` imports the selected committed work into a local destination branch.

`spawn` also supports a named branch, detached HEAD, an orphan branch, sparse checkout, an empty checkout and an initial lock. `list`, `status`, `lock`, `unlock`, `move`, `repair`, `drop`, `restore` and `gc` manage the workspace. `add`, `remove` and `prune` are aliases for spawn, drop and gc respectively. Consult command help for options.

### What takes space

On a compatible volume, a regular tracked file gets its own writable file identity in each workspace. APFS cloning or Linux reflinks let those files share the same physical data blocks, including for small files. Graft creates no symlink or hard link back to the primary file. Edit `src/config.ts` in a graft and the filesystem allocates blocks for the writes the editor performs. The primary and sibling grafts keep their original contents. An editor that replaces a whole file can allocate the size of that file. New files, Git indexes and workspace records also take space.

If the selected commit itself tracks a symlink, Git's normal checkout rules apply. A trusted npm layer can also contain relative symlinks such as a package's command in `node_modules/.bin`. The layer checks that these links resolve inside the installation.

Git stores earlier revisions separately from checked-out files, often with several branches mixed into one pack. With `--history local`, an eligible full clone supplies its existing packs, pack indexes and loose objects through copy-on-write. The new repository can read even unreferenced objects already in that clone. Repacking the primary leaves the snapshot intact.

With the default isolated history, Graft reuses whole eligible packs. For a suitable mixed pack it keeps the selected records at their existing offsets, rebuilds the index and asks Git to verify the result. Other layouts use Git’s selective export. Later isolated grafts at the same revision can share the verified base.

An agent can register a manager’s package cache, an installed directory or both. A cache stores packages per project and can hold multiple versions. A compatible installed directory gives each graft writable files backed by the existing data blocks. Adding a package writes its new data and whatever manifests, lockfiles or existing packages the resolver changes. The spawn receipt reports the source backend and each installation attachment. [Dependency setup](operations.md#register-a-package-manager) · [Measured space per checkout](benchmarks.md#disk-use-with-two-active-workspaces)

### Git compatibility

Shallow repositories retain the selected commit’s locally available history and relevant Git shallow boundaries. Missing history is not fetched. Landing stops before changing the destination when ancestry cannot be resolved. Graft declines replacement refs, legacy Git grafts and repository-local checkout filters. Git configuration and remote tracking in the standalone repository differ from the original checkout.

[Operations and dependencies](operations.md) · [Safety and recovery](safety.md) · [Verification](verification.md)
