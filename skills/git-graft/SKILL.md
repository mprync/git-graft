---
name: git-graft
description: Work in leased git-graft worktrees through the CLI or MCP server, including retrying a disconnected task, reusing project caches and landing reviewed changes.
---

# Use git-graft for an agent task

Discover the host's tools. Use `workspace_spawn` with a task name, committed base and a fresh client-generated UUID as `request_id`. Keep that request and its returned lease for retries. Use the returned `worktree_path` for all edits. A reused name can identify a different task instance.

Use `workspace_exec` with an argv array for builds and tests. Use `workspace_add_dependency` with an empty package list for a locked installation. The host can choose which execution tools to advertise. Direct commands in the checkout only receive the cache and port settings when run through Graft.

Read `workspace_status` before landing. Inspect both committed path changes and working-tree changes. Supply a commit message to land only when all nonignored local changes belong in the task. An ignored file can be a developer's data. Pass `discard_ignored` only when the task permits its removal.

On `merge_conflict`, inspect the reported paths and conflict types. Resolve in the owned worktree and repeat the relevant tests. When the task authorizes landing, pass the original lease and explicit destination. Keep the resulting commit ID. A transport disconnect during land or drop can be retried with the same lease. Verify the filesystem or Git state before repeating an arbitrary child command.

`workspace_busy` means another scoped command holds the lock. A lease mismatch or failed registration check means the name or path no longer proves ownership. Inspect status before proceeding. For long-lived work, renew with `workspace_heartbeat`. Use `workspace_gc` with its preview defaults to inspect eligible cleanup.
