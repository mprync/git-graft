# Safety and recovery

## Graft contents

The selected commit determines the initial source tree. Local edits, untracked files and ignored files in the original checkout stay there. Eligible files can be verified and shared using copy-on-write, with independently writable file identities. Other files come from Git. Tracked symbolic links retain normal Git semantics and may point outside the graft.

A reused file must be proven to hold the selected commit's bytes before Graft records it: either Git's own index stat test vouches for an unchanged primary file whose size matches its blob, or Graft hashes the bytes. Under `core.autocrlf=true`, a file Git expands from LF to CRLF at checkout is proven only by the stat test at its expanded size, the blob's size plus one byte per line feed: Git's clean step removes only carriage returns before line feeds, so a clean file of that size holds exactly Git's expanded bytes. A file Git calls clean with LF endings is checked out by Git. Files with flags or user extended attributes are checked out by Git instead. Graft records each installed file's stat data in the new index so the first `git status` reads no file contents. Git builds that compare timestamps in whole seconds cannot see a same-size edit that restores a file's modification time within the same second; Graft's own clean checks, used before merging or removing a graft, compare the nanosecond stamps the index records and hash any such file.

A graft owns its Git objects, refs, reflogs, stash and index. It does not automatically receive new refs from the original repository. Merging transfers the selected committed work back. Graft declines unsupported histories and checkout configurations rather than silently changing their meaning.

Ordinary CLI and MCP calls that create a graft and omit the history option honor the primary repository's `graft.history` setting. In an unconfigured project, local history is the default. Existing users who need the previous isolated behavior can set `git config --local graft.history isolated` in the primary repository.

Local history treats the invoking clone’s complete Git object database as trusted input. An agent can enumerate objects from other branches, staged files, deleted branches and dropped stashes if those objects still exist. Only the selected commit supplies the checkout. The snapshot has its own object files and remains readable if the primary runs garbage collection or moves. New commits and edits in one graft stay private until explicitly transferred. Use `--history isolated` when unrelated objects must be excluded and only the selected commit's reachable history should be readable. Use `--history tip` when the graft needs only the starting commit and its files. Git treats that commit as the shallow history boundary. If a local snapshot cannot be made, Graft uses the selected-history path and reports the actual source in `storage.objects.source`.

When reusing a mixed pack, Graft edits an unlinked copy, has Git index its contents, and verifies the object IDs before publishing the pack. On macOS, creating that copy briefly requires an owner-controlled, writable source pack directory. An interruption before unlinking can leave a private temporary directory there, eligible for Git’s normal expiry. The source pack stays intact. If the filesystem or directory cannot support this operation, Graft exports the selected objects through Git.

## Recover work

`merge`, `remove` and `prune` archive the graft: they move its private repository and every file in it, including untracked and ignored files, to `<repo>.graft/.retired/<id>`. Archives therefore keep credentials such as `.env` files, keys and tokens that the graft held. Graft creates graft and archive directories owner-only (mode 0700 on Unix) and keeps each file's own mode; on Windows they inherit the parent's ACLs. Your user account, and any backup of the disk, can still read them. To remove a secret for good, delete it from the graft before archiving it, or from the archive afterwards. Save the returned recovery lease:

| Situation | Action |
| --- | --- |
| Reopen a merged or removed graft | `git graft restore NAME --lease TOKEN`; save the new active lease. |
| Reverse a merge | `git graft merge NAME --undo --lease TOKEN` appends a reversal commit if the destination still points at the merge commit. |
| Destination advanced | Review subsequent commits and use an ordinary reviewed reversal. |
| `checkout_recovery_required` | Retry the same merge or undo with the same lease. Graft finishes or reverses the update from the files on disk and keeps untracked, ignored, edited and staged files. If it names `paths`, follow [checkout recovery](operations.md#interrupted-checkout-update). |
| `checkout_update_set_aside` | The branch moved or is no longer checked out after the update stopped. Graft changed no further file, and files the stopped update wrote stay as they are; check `git status` in the primary and retry the merge. |
| `primary_moved` | Every record names one other primary path, so the primary was moved or copied. Graft reads and writes nothing; move the primary back to `details.recorded_primary`. |
| Merge receipt with `"removed": false` | The graft gained commits after the merge applied and stays active. Merge it again to land them. |
| Name or path is occupied | Restore fails as `already_exists`. Inspect the collision and preserve the existing data before retrying. |
| `archive_damaged` | The archive at `details.archive_path` is missing or was changed by hand. Restore leaves it exactly as found. Copy what you need from it, or put back the missing Git files and retry. |

Archives and recovery records have no automatic expiry and can use disk. Deleting the original repository or Graft's recovery state can prevent recovery. Maintain independent backups for disk failure and accidental deletion.

The first merge prepares its history store privately before publishing it. After an interruption Graft can reclaim first-store scratch whose recorded identity and contents still match. It preserves unrecognized or changed scratch and reports its path on a later merge. Interrupted merge-planning directories can also be left for inspection.

## Trust and cleanup

Graft is not a sandbox. Git hooks, filters, merge drivers, package scripts and explicitly requested commands have the user's operating-system permissions. The MCP host must opt in to execution and dependency tools with `--allow-exec`. A graft's own Git configuration is writable by whoever works in it, so when Graft inspects a graft's status it disables every filter driver defined by repository-scoped configuration; only the user's global and system drivers run. Leases coordinate cooperating clients; they do not protect data from another process running as the same user. Share active and recovery leases only with trusted local tools. Treat leases as sensitive and remove them from logs, issue reports and other shared material. Shell history and process listings may expose command-line arguments. Installed dependency layers must be trusted, relocatable and free of private data.

Package manager registration stores cache routing and compatibility settings in the project’s private Graft state. Keep credentials out of these settings. Choose a manager cache designed for concurrent access within the project’s trust boundary. Package builds and installers control which files they write. A registered installation can attach only at an unoccupied checkout-relative path with matching committed inputs. [Manager registration](operations.md#register-a-package-manager)

After a power loss, a registered install cache can contain incomplete files. Graft verifies the saved generation before reuse and preserves a damaged one for inspection. The graft can run a fresh package restore. A cache purge interrupted after quarantine also retains its quarantine directory and reports it on the next preview.

Use `git graft prune` to preview cleanup before applying it with `git graft prune --apply`. Close editors and builds that can write into a graft before moving or removing it. If other native Git checkouts share the primary, finish any checkout creation or branch switch before merging. Git tracks their branch occupancy separately from the destination ref lock.

On macOS, Graft rejects extended allow ACL entries on its primary private registry and managed graft root, including entries inherited before publication. It does not change permission entries, and it does not walk ordinary source ACLs outside these gates.

On Windows, Graft relies on inherited ACLs and does not verify private DACLs. Use a private Git administrative directory and a graft parent whose ACLs allow only trusted users; apply the same care to custom graft destinations.

### Processes left running in a graft

`exec` shares the graft's lock with the process it starts, and that process passes the lock on to the processes it starts in turn. While any of them still holds it, `merge`, `remove`, `prune`, `move`, `lock` and dependency changes report `workspace_busy`, even after `exec` has returned. A job left running in the background, such as `nohup npm run dev &`, keeps the graft busy until it exits; further `exec` calls and lease renewals still work. Stop the job before you merge or archive the graft.

Graft cannot see a process that closes inherited files, such as a daemon, a child that Python's `subprocess` starts with its default `close_fds=True`, or a child that Node.js starts on macOS. It also cannot see processes it did not start, such as an editor or a terminal. If one of these is still writing when the graft is archived, it keeps writing into the archive under `.retired`, because the archive is the same directory renamed. Nothing is lost, but a later `restore` shows those writes. On Windows, an open file or a working directory inside the graft usually makes the archive rename fail, and the operation can be retried after the process exits.
