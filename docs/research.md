# Storage and integration decisions for agent worktrees

*Zero-Duplication Git Worktrees and Dependency Layering* proposes sharing source files and installed dependencies across many agent worktrees. The design depends on three separate mechanisms. Git shares history and tracks checkouts, the filesystem can share file data, and package managers can reuse downloads. Each has different rules for what happens when a process writes a file.

## Use Git to own branches and indexes

Linked worktrees share the repository's object database and most refs. Each checkout has its own index, HEAD and working files. Graft registers a normal worktree so editors, Git commands and existing build tools see those structures. Git discovers the installed `git-graft` executable on `PATH` when a user runs `git graft`. [Git worktrees](https://git-scm.com/docs/git-worktree) · [Git subcommands](https://git-scm.com/docs/git#_git_commands)

## Share file data through copy-on-write

Hardlinks give two names to the same inode. A write through either name can change both worktrees, and changing permissions affects both names. A source symlink can also send an ordinary editor or formatter straight into another checkout. MCP cannot intercept writes made by other processes. Graft takes committed files from Git, preserves tracked symlink semantics, and gives each checkout independently writable files. [Hardlinks](https://man7.org/linux/man-pages/man2/link.2.html) · [POSIX open](https://pubs.opengroup.org/onlinepubs/9799919799/functions/open.html) · [Git index file modes](https://git-scm.com/docs/index-format)

APFS file cloning and Linux reflinks let distinct files share storage until a write changes a block. Graft probes the actual source and destination filesystems. Eligible files clone from a commit-keyed snapshot whose key also covers checkout settings. Sparse cones, gitlinks, external attributes and path-dependent filters use Git's ordinary checkout. Committed text and line-ending rules can use the snapshot path. [Apple File System](https://developer.apple.com/documentation/foundation/about-apple-file-system) · [Linux FICLONE](https://man7.org/linux/man-pages/man2/ioctl_ficlone.2.html)

The proposal also considers OverlayFS. It requires a stable lower tree for the lifetime of a mount and private writable upper and work directories. A live developer checkout can change at any time. The current Linux implementation uses reflinks where the volume supports them and an ordinary Git checkout elsewhere. [OverlayFS lower-layer changes](https://docs.kernel.org/filesystems/overlayfs.html#changes-to-underlying-filesystems) · [OverlayFS layer sharing](https://docs.kernel.org/filesystems/overlayfs.html#sharing-and-copying-layers)

## Reuse downloads and protect writable installations

A warm cache can satisfy a locked offline installation when it contains the required artifacts. Dependencies, lifecycle script outputs and compiled files can still differ by platform, flags, interpreter and compiler version. Graft supplies project-scoped package caches, private Cargo build directories and private Python environments. It also supports an explicitly keyed installed Node layer for relocatable projects, with a digest check before publication and restoration. [pnpm worktrees](https://pnpm.io/git-worktrees) · [pnpm installation options](https://pnpm.io/cli/install) · [Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html) · [uv caching](https://docs.astral.sh/uv/concepts/cache/)

Worktrees share a trust boundary. Local processes can access the repository and user credentials within their operating-system permissions. A lease coordinates ownership among clients. A suggested port avoids duplicate assignments among active workspaces in a project. Applications and external processes still control which ports they bind.

## Integrate a commit before retiring its workspace

`git merge-tree --write-tree` computes a candidate tree without changing a checkout. Landing into a checked-out branch also requires updating its files, index and ref. Graft records the plan, rechecks the target, asks Git to fast-forward the checkout, and retires the workspace after integration. For an unchecked branch it supplies the expected previous commit to `update-ref`. Conflict paths and interrupted operations remain inspectable. External Git processes and filesystem writers can race with a landing operation. [Git merge-tree](https://git-scm.com/docs/git-merge-tree) · [Git update-ref](https://git-scm.com/docs/git-update-ref) · [Recovery behavior](operations.md)

Dependency conflicts need an ecosystem-specific resolver. The opt-in npm resolver accepts a narrow root lockfile-v3 case with independent additive dependencies, works offline in a temporary directory, and verifies the result. Other conflicts return to the caller for resolution.

## Test the MCP revisions clients use

The stdio tests send `initialize` and `notifications/initialized`, discover tools through `tools/list`, and call them through `tools/call`. They exercise the `2025-06-18` and `2024-11-05` handshakes against the shipped SDK. Later protocol revisions have their own versioning rules. A version written in a design proposal needs a corresponding wire test before it becomes a compatibility claim. [June 2025 lifecycle](https://modelcontextprotocol.io/specification/2025-06-18/basic/lifecycle) · [November 2024 lifecycle](https://modelcontextprotocol.io/specification/2024-11-05/basic/lifecycle) · [July 2026 versioning](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)

## Measure physical savings on the target volume

The proposal's zero-byte, per-file metadata and sub-second estimates depend on the workload and filesystem. A full-sized file can share its storage with many clones. Conversely, metadata and the first snapshot consume blocks even before an agent edits a file. Graft reports the selected backend, logical cloned bytes and volume-wide observations. The release benchmarks compare the change in free bytes over equal workloads and report timing, variation and write isolation. [Filesystem statistics](https://man7.org/linux/man-pages/man2/statfs.2.html) · [GNU du](https://www.gnu.org/software/coreutils/manual/html_node/du-invocation.html) · [Measured results](verification.md)
