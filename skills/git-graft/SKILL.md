---
name: git-graft
description: Manage a Graft workspace through CLI or MCP, register unfamiliar package managers for reuse, and handle spawn retries, landing and recovery.
---

# Use Graft for an agent task

1. Discover the host's tools. Spawn a workspace at a committed base using a fresh cryptographically random UUIDv4 as `request_id`. The request ID acts as the lease; keep it secret. Keep the request and returned lease for retries; share leases only with trusted local tools and omit them from shared logs. A reused name may identify another task. Use the returned `path` for edits and ordinary Git commands.
2. Before installing dependencies, inspect `storage.dependencies` and `storage.registered_dependencies` in the spawn result. Use an attached installation directly. For an unfamiliar manager, call `workspace_list_managers`. When a suitable registration is missing, identify the manager’s documented cache variable or argument and use `workspace_register_manager` once. Run the manager with `workspace_exec` and an argv array. Use its `manager` field for wrappers. On an installation cache miss, run the ordinary restore in the graft. Check the command’s exit status.
3. Inspect `workspace_status` and the task's changes before landing. Commit with Git, or supply a commit message only when all nonignored changes belong to the task. Resolve reported conflicts and retry. Ignored files may be user data; use `discard_ignored` only when authorized.
4. Keep the returned `recovery_lease` private. Archives can retain sensitive user files. Use `workspace_restore` to reopen an archived repository and receive a new active lease. Use `workspace_undo_land` only when asked to reverse the integration; it requires the destination still to point at the recorded landed commit.

Use cache-only registration for package downloads. For installed-file reuse, register a trusted relocatable directory with every tracked install input and a key for external toolchain settings. Use the registration result and the next compatible spawn’s attachment receipt to verify reuse. [Registration arguments and examples](../../docs/operations.md#register-a-package-manager)
