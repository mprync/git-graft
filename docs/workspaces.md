# Graft workspaces

Each Graft workspace is a standalone local Git repository with its own object database, branches, reflogs, stash, index and working files. Use `git graft spawn NAME --base REV` to create one and use the returned `path` for ordinary Git commands. Refs remain local until work is explicitly transferred; `land` imports the selected committed work into a local destination branch.

`spawn` also supports a named branch, detached HEAD, an orphan branch, sparse checkout, an empty checkout and an initial lock. `list`, `status`, `lock`, `unlock`, `move`, `repair`, `drop`, `restore` and `gc` manage the workspace. `add`, `remove` and `prune` are aliases for spawn, drop and gc respectively. Consult command help for options.

Graft can share verified source and immutable pack data on filesystems that support copy-on-write. An exact pack may be reused; otherwise the required history can be exported and later reused. A trusted registered npm installation can be attached to compatible workspaces. Receipts describe the chosen storage. Shallow repositories retain the selected commit’s locally available history and relevant Git shallow boundaries; missing history is not fetched. Landing fails before destination mutation when ancestry cannot be resolved. Replacement refs, legacy Git grafts and repository-local checkout filters are declined. Git configuration and remote tracking in the standalone repository differ from the original checkout.

[Operations and dependencies](operations.md) · [Safety and recovery](safety.md) · [Verification](verification.md)
