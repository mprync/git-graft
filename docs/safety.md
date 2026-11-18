# File safety and recovery

## What a new workspace contains

The requested Git commit determines the source tree. On a clone-capable volume, Graft opens tracked donor files through directory descriptors, makes private copy-on-write clones, and checks the resulting bytes against Git's checkout representation. A file that cannot pass the check comes from Git. Untracked files, ignored files and local edits in the primary checkout stay there. Source receipts report logical bytes reused from the primary checkout, a cached parent and the snapshot. Physical disk growth comes from measuring the volume.

Source cache entries carry a project-local authentication tag, the Git tree entries and recorded file identities. Graft uses an ordinary Git checkout when it cannot use a source snapshot. Garbage collection retains unknown or modified snapshots. A committed symbolic link has normal Git semantics. A program following that link can reach its target.

Installed dependency layers require a separate, explicit save or seed. The caller supplies the compatibility key and attests to the installed packages, build options, relocatability and absence of private data. Restore stages its files privately, verifies their content and publishes only to a vacant installation path.

## Recover a workspace or undo a land

| What happened | What to do |
| --- | --- |
| A task was dropped, landed or retired by GC | Use the task's lease with `git graft restore NAME --lease TOKEN`. A new active lease is returned. |
| The destination branch needs to be put back | Use `git graft undo-land NAME --lease TOKEN`. It appends a commit with the destination's previous tree, preserving the complete history of the integration. |
| The destination has advanced since land | Review the later commits and prepare a normal reviewed reversal. Automatic undo requires the recorded destination tip. |
| The old workspace name, branch or path is occupied | Inspect `git graft list` and native Git's worktree listing. Restore requires a vacant slot. A replacement workspace has its own lease. |
| A process stopped during retirement or restore | Retry the same command with the original token. Graft validates its recorded phase, refs and directory identities before resuming. |

Recovery pins protect committed data from Git's normal garbage collection. A workspace whose files, index or metadata need preservation is parked with its Git administration. A proven reconstructible checkout can instead be reopened from the pinned commit. Private runtime data is retained. On Windows, Graft conservatively parks the checkout including its filesystem metadata. Reconstructed checkouts have fresh filesystem timestamps.

An ordinary `drop` checks for local files and unmerged work. `--discard` allows retirement with those changes retained for recovery. After landing, `--discard-ignored` serves the same purpose for ignored files. The seven-day expiry on short-lived replay receipts does not expire recovery data. Parked installs, build outputs, pinned Git objects and any independent copies still occupy disk after retirement. To review or remove bulky outputs, restore the workspace with its receipt and inspect those files there.

## Processes, permissions and cleanup

Graft is designed for trusted local repositories and processes running as the same user. Git hooks, checkout filters, merge drivers, package scripts and explicitly requested child programs run with that account's privileges. The host must enable `mcp --allow-exec` for execution, package installation, seeding, layer operations and lockfile reconciliation. Leases prevent cooperating clients from interfering with another task. The local account can access its own private registry.

New Unix state and workspace-root directories are owner-only. On upgrade, `doctor`, `list`, `status` and GC preview allow inspection. If a write reports broad permissions on a legacy root, verify that it is the reported Graft-owned directory, review any intentional multi-user access, and set owner-only permissions on that specific directory. Graft leaves pre-existing directory modes to the owner.

Windows directories inherit their access controls from the parent. Use a repository and workspace parent whose ACLs permit access only to the intended users.

Use `gc` for a preview. Applying GC checks recorded ownership, active locks, unmerged work, directory types, nested repositories and filesystem boundaries. Package caches and installed layers require the separate `--purge-cache --apply` request. Keep irreplaceable artifacts in a workspace or a backup outside a package manager's cache. Deleting the repository or its private recovery store can also remove the information necessary to restore a task.

On Windows, dependency layers carry file data into new files. Forgetting a layer parks it and frees its key. The `retained` receipt records that it still occupies disk. GC keeps stored layers when their Windows metadata cannot be verified.

Close external editors and builds before retiring a workspace. Graft coordinates its own clients with locks. An unrelated native Git process can switch a checkout independently, and an open file descriptor can continue writing after a directory is moved. Use `git worktree lock` for externally active workspaces. Git's reference transactions, worktree registration and filesystem renames are separate operations. Ambiguous partial states need inspection. A backup on an independent volume remains necessary for disk failure.
