---
name: git-graft
description: Give an agent task its own graft of a Git repository through the CLI or MCP, then merge, recover or remove it. Covers lease handling, request retries, package-manager reuse and undoing a merge.
---

# Use Graft for an agent task

A graft is an independent copy of the repository for one task. For routine work in a trusted clone, omit `history` from `graft_add`. This honors the project setting and defaults to local history. Choose `isolated` when the graft should exclude staged blobs, dangling objects and history unique to other branches. Choose `tip` when only the starting commit and its files are needed.

1. Discover the host's tools. Create the graft at a committed base with `graft_add`, using a fresh cryptographically random UUIDv4 as `request_id`. The request ID acts as the lease; keep it secret. Keep the request and returned lease for retries; share leases only with trusted local tools and omit them from shared logs. A reused name may identify another task. Use the returned `path` for edits and ordinary Git commands.
2. Before installing dependencies, inspect `storage.dependencies` and `storage.registered_dependencies` in the `graft_add` result. Use an attached installation directly. Otherwise run the manager with `graft_exec` and an argv array (use its `manager` field for wrappers), or install packages with `graft_add_dependency`. Both need a host started with `--allow-exec`. Manager registration and dependency layers are CLI-only: ask the user to run `git graft deps manager add` when a suitable registration is missing. Check each command's exit status.
3. Inspect `graft_status` and the task's changes before merging. Commit with Git, or supply a commit message only when all nonignored changes belong to the task. Resolve reported conflicts and retry `graft_merge`. Ignored files may be user data; use `discard_ignored` only when authorized.
4. Keep the returned `recovery_lease` private. Archives can retain sensitive user files. Use `graft_restore` to reopen an archived graft and receive a new active lease. Use `graft_remove` to archive a graft you no longer need, and `graft_renew` to keep a long task's lease from expiring. Call `graft_merge` with `undo` only when asked to reverse the merge; it requires the destination still to point at the recorded merge commit.

Use cache-only registration for package downloads. For installed-file reuse, a user can register a trusted relocatable directory with every tracked install input and a key for external toolchain settings. Use the registration result and the next compatible `graft_add` attachment receipt to verify reuse. [Registration arguments and examples](../../docs/operations.md#register-a-package-manager)

## When Graft keeps work instead of finishing

Graft never deletes or overwrites work to finish a command. These results mean it kept something; read `code` and `details`, then act as below. Never delete or edit Graft's record, archive or journal files to get past one.

- `unmerged_work`: removing without `discard` found unmerged commits, branches, tags or a stash (`details.found`). Merge the graft, or ask the user before removing it with `discard`.
- `"removed": false` on a merge receipt: the graft gained commits (`graft_head`) after `graft_tip` merged. It stays active with the same lease; merge it again.
- `unmerged_commits: true` on a remove receipt: the archive holds commits made after an earlier merge (`head`). Keep the `recovery_lease` and tell the user.
- `graft_archived`: your lease belongs to an archived graft (`details.archived_by`). Restore it with `graft_restore` and use the new lease. If `details.name_in_use`, ask the user first.
- `record_damaged` or a `damaged_records` list: a record file is unreadable. Other grafts still work. Ask the user to repair it; do not touch the file.
- `primary_moved`: the primary was moved or copied. Ask the user to move it back to `details.recorded_primary`.
- `already_exists` or `archive_damaged` on restore: the name or path is taken, or the archive was changed by hand. Report it to the user; restore changed nothing.
- `checkout_recovery_required`: retry the same merge or undo with the same lease. If it names `paths`, ask the user how to keep those edits.
- `checkout_update_set_aside`: the destination branch moved after an interrupted update. Ask the user to check `git status` in the primary, then retry the merge.
- `git_busy` or `workspace_busy`: another Git or Graft command, or a background job started with `graft_exec`, is running. Wait or stop your own background job, then retry.
- `retained_path` and `kept` (CLI `deps layer remove` and `gc --purge-cache`): layers were kept, not deleted. Leave them for the user.

[Full table of codes and fields](../../docs/operations.md#safety-error-codes-and-receipt-fields)
