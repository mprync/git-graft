# Graft workspaces

Each Graft workspace is a standalone local Git repository with its own object database, branches, reflogs, stash, index and working files. Use `git graft spawn NAME --base REV` to create one and use the returned `path` for ordinary Git commands. Refs remain local until work is explicitly transferred; `land` imports the selected committed work into a local destination branch.

`spawn` also supports a named branch, detached HEAD, an orphan branch, sparse checkout, an empty checkout and an initial lock. `list`, `status`, `lock`, `unlock`, `move`, `repair`, `drop`, `restore` and `gc` manage the workspace. `add`, `remove` and `prune` are aliases for spawn, drop and gc respectively. Consult command help for options.

### What takes space

On a compatible volume, an unchanged tracked file can have a separate writable name in every workspace while its contents occupy the same physical blocks. This applies to small files too. An edit allocates blocks for the writes the editor performs. An editor that replaces a whole file can allocate the size of that file. New files, Git indexes and workspace records also take space.

Git stores earlier revisions separately from checked-out files, often with several branches mixed into one pack. Graft reuses whole eligible packs. For a suitable mixed pack it keeps the selected records at their existing offsets, rebuilds the index and asks Git to verify the result. Layouts outside its time, memory or filesystem limits use Git’s selective export. The first graft pays for any private base data. Later grafts at the same revision can share that verified base.

A compatible registered npm installation works the same way. Each graft gets writable files backed by the existing installation’s blocks. Adding a package writes the new package and whatever manifests, lockfiles or existing dependencies npm changes. The spawn receipt reports the storage backend, source counts and dependency attachment. [Dependency setup](operations.md#reuse-an-npm-installation) · [Measured space per checkout](benchmarks.md#disk-use-with-two-active-workspaces)

### Git compatibility

Shallow repositories retain the selected commit’s locally available history and relevant Git shallow boundaries. Missing history is not fetched. Landing stops before changing the destination when ancestry cannot be resolved. Graft declines replacement refs, legacy Git grafts and repository-local checkout filters. Git configuration and remote tracking in the standalone repository differ from the original checkout.

[Operations and dependencies](operations.md) · [Safety and recovery](safety.md) · [Verification](verification.md)
