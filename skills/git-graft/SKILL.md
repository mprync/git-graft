---
name: git-graft
description: Manage an agent task in a Graft workspace through CLI or MCP, including spawn retries, dependency reuse, landing and recovery.
---

# Use Graft for an agent task

1. Discover the host's tools. Spawn a workspace at a committed base using a fresh cryptographically random UUIDv4 as `request_id`. The request ID acts as the lease; keep it secret. Keep the request and returned lease for retries; share leases only with trusted local tools and omit them from shared logs. A reused name may identify another task. Use the returned `path` for edits and ordinary Git commands.
2. Run builds and tests with `workspace_exec` and an argv array when the host enables execution. Check `storage.dependencies.status` before installing packages. An attached shared install is already ready; dependency additions are private to the workspace.
3. Inspect `workspace_status` and the task's changes before landing. Commit with Git, or supply a commit message only when all nonignored changes belong to the task. Resolve reported conflicts and retry. Ignored files may be user data; use `discard_ignored` only when authorized.
4. Keep the returned `recovery_lease` private. Archives can retain sensitive user files. Use `workspace_restore` to reopen an archived repository and receive a new active lease. Use `workspace_undo_land` only when asked to reverse the integration; it requires the destination still to point at the recorded landed commit.

Seed a shared dependency layer only when the user trusts the install and can attest that it is relocatable, matches the committed lockfile, contains no private data and can be reused across source-only edits. Consult [operations](../../docs/operations.md) for dependency and recovery details.
