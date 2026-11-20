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
