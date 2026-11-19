# Safety and recovery

## Workspace contents

The selected commit determines the initial source tree. Local edits, untracked files and ignored files in the original checkout stay there. Eligible files can be verified and shared using copy-on-write, with independently writable file identities. Other files come from Git. Tracked symbolic links retain normal Git semantics and may point outside the workspace.

A standalone workspace owns its Git objects, refs, reflogs, stash and index. It does not automatically receive new refs from the original repository. Landing transfers the selected committed work back. Graft declines unsupported histories and checkout configurations rather than silently changing their meaning.

## Recover work

Land and drop archive the workspace, including its private repository and files. Archives can retain ignored files, credentials and other sensitive user data; protect them as you would the workspace. Save the returned recovery lease:

| Situation | Action |
| --- | --- |
| Reopen a landed or dropped workspace | `git graft restore NAME --lease TOKEN`; save the new active lease. |
| Reverse a landing | `git graft undo-land NAME --lease TOKEN` appends a reversal commit if the destination still points at the landed commit. |
| Destination advanced | Review subsequent commits and use an ordinary reviewed reversal. |
| `checkout_recovery_required` | Retry the same land or undo with the same lease before editing. If it still stops, preserve files and the index and follow [checkout recovery](operations.md#interrupted-checkout-update). |
| Name or path is occupied | Inspect the collision and preserve the existing data before retrying. |

Archives and recovery records have no automatic expiry and can use disk. Deleting the original repository or Graft's recovery state can prevent recovery. Maintain independent backups for disk failure and accidental deletion.

## Trust and cleanup

Graft is not a sandbox. Git hooks, filters, merge drivers, package scripts and explicitly requested commands have the user's operating-system permissions. The MCP host must opt in to execution and dependency tools with `--allow-exec`. Leases coordinate cooperating clients; they do not protect data from another process running as the same user. Share active and recovery leases only with trusted local tools. Treat leases as sensitive and remove them from logs, issue reports and other shared material. Shell history and process listings may expose command-line arguments. Installed dependency layers must be trusted, relocatable and free of private data.

Use `git graft gc` to preview cleanup before applying it with `git graft gc --apply`. Close editors and builds that can write into a workspace before moving or retiring it. If other native Git checkouts share the primary, finish any checkout creation or branch switch before landing. Git tracks their branch occupancy separately from the destination ref lock.

On macOS, Graft rejects extended allow ACL entries on its primary private registry and managed workspace root, including entries inherited before publication. It does not change permission entries, and it does not walk ordinary source ACLs outside these gates.

On Windows, Graft relies on inherited ACLs and does not verify private DACLs. Use a private Git administrative directory and a workspace parent whose ACLs allow only trusted users; apply the same care to custom workspace destinations.
