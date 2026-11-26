# Operating grafts

## Identity and lifecycle

`git graft add NAME --base REV` creates a graft: a standalone repository with its own Git objects, refs, index and working files. Routine CLI and MCP calls honor the primary repository's `graft.history` setting and default to local history. Set `graft.history=isolated` when unrelated objects must be excluded, or choose `tip` for a shallow graft containing only the starting commit and its files. The result gives its `path` and an active `lease`. A name can be reused, so keep the lease with the task and share it only with trusted local tools. Avoid including leases in shared logs or reports. CLI commands that change a graft accept `--lease` or `GIT_GRAFT_LEASE`; MCP tools take `lease`. Use `git graft list` and `git graft status NAME` to inspect grafts, and `git graft status NAME --env` to see a graft's runtime and cache paths. Run `git graft --help` (the manual page) or `git graft -h`, and `git graft COMMAND --help`, for the available options.

Ordinary Git commands operate on the graft's private repository. A branch or stash created there is local to it; `merge` transfers the selected committed work into the original repository, and `push` sends it to one of that repository's remotes instead. A nested `add` can use a parent graft's private commit as its base. Shallow repositories retain the selected commit’s locally available history and relevant Git shallow boundaries. Graft does not fetch missing history. Merging uses the destination’s existing boundaries and fails before changing it if ancestry cannot be resolved. Replacement refs, legacy Git grafts and repository-local checkout filters are declined.

Use `lock` and `unlock` to protect an active graft from moves, removal and pruning. `move` changes its path on the same filesystem. After a manual rename, `repair` verifies its recorded identity before accepting the new path. `remove NAME` archives one graft. `prune` previews the expired grafts that can be archived and `prune --apply` archives them.

## Configure a project

Two JSON files at the root of the primary checkout configure Graft for a project. `.graftconfig` is the project's: commit it and every clone shares it. `.graftconfig.local` is yours: add it to `.gitignore`. Either may be missing, and a missing file sets nothing. Both may set:

- `include`: the [untracked and ignored files](#copy-untracked-and-ignored-files) a new graft copies when the request names none.
- `hooks`: the [commands](#hooks) Graft runs when a graft is added, about to be archived or restored, keyed by a name of up to 64 letters, digits, `-`, `_` or `.`.
- `turns`, at the top level only: the [shared resources](#share-a-resource-between-repositories) whose turns every repository the file configures shares.
- `remotes`: names of the primary's remotes that every new graft gets, such as `["origin"]`. See [remotes in a graft](#remotes-in-a-graft).

```json
{
  "include": [".env", "config/*.local.json"],
  "hooks": {
    "database": {"event": "post-add", "argv": ["/opt/project/bin/graft-setup"]},
    "reopen": {"event": "post-restore", "argv": ["/opt/project/bin/graft-setup"]},
    "teardown": {"event": "pre-archive", "argv": ["/opt/project/bin/graft-teardown"]}
  }
}
```

`.graftconfig.local` overrides `.graftconfig` setting by setting:

```json
{
  "include": ["!config/*.local.json", "fixtures/large/"],
  "hooks": {
    "database": {"event": "post-add", "argv": ["/opt/project/bin/graft-setup", "--small"]},
    "reopen": null,
    "notify": {"event": "post-add", "argv": ["notify-send", "graft ready"]}
  }
}
```

- `include` lists are joined, the shared patterns first. A local `!pattern` removes the same pattern from the shared list. A `!pattern` that matches no shared pattern stays in the list as a `.gitignore` negation, which leaves out files that the patterns before it match. With the two files above, a graft copies `.env` and `fixtures/large/`.
- A local hook with the name of a shared hook replaces it whole, program, arguments and event together, and keeps its place in the order. A local `null` turns the shared hook off. A new name adds a hook, which runs after the shared ones. With the two files above, `post-add` runs `graft-setup --small` and then `notify-send`, `post-restore` runs nothing, and `pre-archive` runs `graft-teardown`.

Hooks for the same event run in the order they are written. `null` is only meaningful in `.graftconfig.local`; `.graftconfig` refuses it. Graft reads both files from the primary checkout's working tree, or from a [directory above it](#configure-several-repositories-from-one-directory), every time it needs them, never from a graft: a graft's own copy of `.graftconfig`, edited or committed, changes nothing until it is merged into the primary. `git graft hook list` shows the hooks in effect, in the order they run, each with the file that defined it and whether it is [trusted](#hooks). A file that is not valid JSON, holds anything but a JSON object, has a key Graft does not know or a hook it would refuse, stops every command that reads it with `invalid_config`, naming the file.

### Configure several repositories from one directory

Some projects keep several repositories side by side in one directory, which may itself be a repository, and use them together. When the primary checkout's root holds neither `.graftconfig` nor `.graftconfig.local`, Graft reads them from the nearest directory above it that holds either. When the checkout is inside your home directory, the search stops at home, which it still reads, as Git's `GIT_CEILING_DIRECTORIES` stops Git's own search; otherwise it goes up to the root of the filesystem. Home is `HOME` on Unix and `USERPROFILE` on Windows. There, a `repositories` object holds the same settings for each repository below, keyed by its path relative to that directory, with `/` between parts:

```json
{
  "repositories": {
    "app": {
      "include": [".env", "storage/*.key"],
      "hooks": {
        "prepare": {"event": "post-add", "argv": ["./scripts/graft-hook.sh"]},
        "cleanup": {"event": "pre-archive", "argv": ["./scripts/graft-hook.sh"]}
      }
    },
    "services/router": {"include": ["config.local.json"]}
  }
}
```

- A checkout's own files always come first: once its root holds either file, the directory above is not read, except for the [`turns`](#share-a-resource-between-repositories) it lists. Of the directories above, up to home, only the nearest that holds either file is read, and a repository it does not name under `repositories` gets nothing from it. The directory's own top-level `include` and `hooks` are for the repository whose root it is, if any, and are not passed down.
- `.graftconfig.local` in that directory overrides `.graftconfig` for each repository as described above. Reports name the files by their path from the primary checkout, such as `../.graftconfig`.
- A hook there may name its program by a path relative to that directory, such as `./scripts/graft-hook.sh`, which keeps a committed file free of absolute paths. No graft of the repository holds that file, so the graft's work cannot change it. The path must stay inside the directory, without `..`, and outside the repository and every directory of grafts: for `app`, a program under `app/`, `app.graft/` or any other directory whose name ends in `.graft` is refused. Graft follows the links on the path before it checks, and on macOS and Windows it ignores case, so `App/bin/setup` is refused there as well. `git graft hook list` shows such a hook's resolved `program`.
- On Unix, both files there must belong to you or to the superuser, as must the directory and any link to them; otherwise every command that reads them refuses with `invalid_config`. A file in a shared directory such as `/tmp` could otherwise run another user's commands as you. Windows does not check who owns them, so there every hook from either file in that directory needs trust, whether Git tracks the file or not.
- Hooks from that directory's `.graftconfig`, and from its `.graftconfig.local` while the directory's own repository tracks that file or on Windows, need [trust](#hooks) as the primary's own do. The trust is recorded for each repository in its own Graft state, so run `git graft hook trust` in each.
- A key under `repositories` that names no repository, such as a mistyped path, configures nothing. `git graft hook list` reports each one in `missing_repositories`, with the file that has it.

### Remotes in a graft

A graft has no remotes unless the configuration names some. With `"remotes": ["origin"]`, `add` configures `origin` in the new graft with the primary's `remote.origin.url`, any `pushurl`, and its `fetch` refspecs, so `git fetch origin` and `git push origin <branch>` work in the graft as in any clone, and `push` is not needed. With local history it also copies the primary's `refs/remotes/origin/*`, whose objects the graft already holds; with isolated or tip history it copies none, which would bring other history in, and a `git fetch` in the graft gets them. The primary's `refs/remotes/origin/HEAD` is not copied as a ref: where it is a symbolic ref, the graft gets a symbolic ref to the same branch, so `origin` follows that branch after a `git fetch`. `restore` gives a reopened graft any named remote it lacks.

- Only those keys are copied. Credentials stay in the user's credential helpers and SSH configuration, and a remote whose URL names a user and password, such as `https://user:token@host/repo`, is left out with the reason `url_has_credentials`; an SSH user, as in `git@host:repo`, is no credential. A name the primary does not configure is left out with `not_configured`. A remote the graft already has is `kept` with its URLs, except that a caller that may not push makes Git refuse to push through it too (see below); a later caller that may push does not undo that.
- Remotes that `.graftconfig` lists, or a `.graftconfig.local` that Git tracks, come with the repository and need [trust](#hooks) by the same rule as its hooks: `git graft hook trust` records a digest of the remotes a graft would get from those files, each with the file that brings it, beside the hooks' digest, and `git graft hook list` reports each remote in effect with `trusted`. Until then, and again after any change to those remotes or the files that bring them, `add` and `restore` leave every remote they list out of the graft and report it as `"status": "skipped"`, `"reason": "untrusted"`, with a `hint` that names `git graft hook trust`; the command goes on, as a graft works without its remotes. The two digests are compared apart, so a change to the remotes does not stop the hooks, and a change to a hook does not hold back the remotes. What an untracked `.graftconfig.local` lists, or removes with `!name`, is your own and needs no trust, except on Windows in a [directory above the checkout](#configure-several-repositories-from-one-directory), as for hooks.
- Over MCP, a host started without `--allow-push` gives each remote the push URL `graft-push-disabled::` in place of the primary's `pushurl`, and forbids that transport in the graft's configuration (`protocol.graft-push-disabled.allow=never`). `git fetch` uses `url` and works; `git push` through the remote fails with `fatal: transport 'graft-push-disabled' not allowed`. Leaving `pushurl` out would not do, as Git pushes to `url` when a remote has none. The same applies to a remote a restored graft already has, such as one made on the command line and restored over MCP. The command line and a host started with `--allow-push` get the primary's `pushurl`.
- A plain `git push` from the graft is Git's own, so it does not have the check against the remote's default branch that [`push`](#push-to-a-remote) makes: `git push origin HEAD:main` updates `main` if the remote allows it.
- The receipt of `add` and `restore` reports each as `remotes`: `{"name": "origin", "status": "configured", "refs": 12, "push": true}`, where `push` says whether Git can push through it, or `"status": "skipped"` with a `reason`, or `"status": "kept"` with `push`. A remote that cannot be set up does not fail the command: it is reported by name as `"status": "failed"` with a `reason`, its keys and copied refs are taken out of the graft again, and the other remotes are still set up. A later `restore` tries it again.
- Lists from `.graftconfig` and `.graftconfig.local` are joined; a local `!name` removes a shared one. `.graftconfig` refuses `!name` with `invalid_config`, as it refuses a `null` hook.
- It is opt-in: a project that wants every graft to push and fetch by itself names its remotes, and a graft of any other stays without them. `merge`, `remove` and `prune` still count only work that a local branch of the primary contains as merged, whatever a graft pushed.

## Copy untracked and ignored files

A graft starts with only the files of its commit. Untracked and ignored files in the checkout, such as `.env`, local configuration or build output, stay behind unless the request names them:

```sh
git graft add api --include .env --include 'config/*.local.json'
git graft add api --include '*'      # every untracked and ignored file
```

Over MCP, `graft_add` takes the same patterns as `include`, and `graft_local_files` (CLI `git graft local-files`) lists the checkout's untracked and ignored files and directories, so an agent can choose what its task needs. Patterns use `.gitignore` syntax and match only files Git does not track; a nested repository is skipped. `*` also brings build output and environments such as `target/` and `.venv/`, which can be large where files cannot be cloned, so name what the task needs where you can. To leave a directory out of `*`, exclude both the directory and its contents: `--include '*' --include '!target/' --include '!target/**'`.

When a request names no patterns, the `include` lists of the project's [configuration](#configure-a-project) supply them, so a project can set a default once. Without any, nothing is copied. Patterns in a request replace the configured ones, and an empty MCP `include` list copies nothing. The patterns come from the primary checkout's files, and the files are copied from the checkout `add` runs in, which is a parent graft for a nested `add`. `local-files` reports the configured patterns as `defaults`, with `patterns` and the `files` that set them.

Copies are clones on a filesystem that supports them, and full copies elsewhere. A copy never replaces a file of the checkout, nothing is copied into an attached dependency install, and a copy is never written through a symlink; such paths are listed in the receipt's `skipped` with a reason (`exists`, `blocked`, `tracked`, `attached_install` (one entry for the whole install), `nested_repository`, `vanished`, `unreadable`, `symlink` on Windows, or `special`). The `add` receipt records them as `storage.included`: `source` (`request`, or `config` with the `config_files` that set the patterns), `patterns`, `files`, `bytes`, `skipped_count` and up to 50 `skipped` entries.

A copied file that is untracked in the graft is added to the graft's `.git/info/exclude`, so `merge --commit` never commits it; commit one deliberately with `git add -f`. `status`, `merge` and `remove` do not count a copy as a change while it is the file Graft installed. Once the graft edits, replaces or deletes it, it is listed as `ignored` again. Copies stay in the graft's archive, like every other file.

## Run commands and install dependencies

```sh
git graft exec task --lease "<lease>" -- cargo test --locked
git graft deps add task --lease "<lease>" --manager npm
git graft deps add task --lease "<lease>" --manager pnpm zod
git graft status task --env
```

An empty package list requests a locked installation or fetch. `git graft deps add` supports npm, pnpm, Bun, Cargo and uv. Graft configures project-scoped caches for its managed commands. `--offline` requires the requested artifacts to be available locally. Package scripts run with the user's permissions. The MCP tools `graft_exec` and `graft_add_dependency` require the host's `--allow-exec` option.

| Package manager | Reuse across grafts | Per-graft work |
| --- | --- | --- |
| npm | A compatible registered installation can attach automatically on a copy-on-write volume. The npm download cache is shared across managed commands. | Each graft has its own manifest, lockfile and writable installation. An addition can change transitive packages and metadata. |
| Cargo | `git graft exec` and `deps add` use a project-level `CARGO_HOME` for crate downloads and Git sources. This needs no registration. | Each graft has its own manifests, lockfile, Cargo target and build directories. A build can compile the same crate in two grafts. |
| NuGet | Register `dotnet` with `NUGET_PACKAGES` to give SDK-style projects one package store per Graft project. NuGet can reuse several cached versions side by side. | Restore generates each project's `obj/project.assets.json`. Builds normally write under `bin/` and `obj/`. Legacy `packages.config` projects can also copy packages into a project directory. |

Adding a package in one graft changes that graft’s declarations and resolved versions. Other grafts use a cached version when their own project files request it. Programs run directly from a terminal use the terminal’s environment. [Cargo cache](https://doc.rust-lang.org/cargo/guide/cargo-home.html) · [NuGet package storage](https://learn.microsoft.com/en-us/nuget/consume-packages/managing-the-global-packages-and-cache-folders) · [NuGet restore](https://learn.microsoft.com/en-us/nuget/consume-packages/package-restore)

## Register a package manager

You can teach Graft how a tool locates its cache once per project. Choose the executable name and supply the manager’s documented cache environment variable or command-line option. Graft creates an owned cache directory and passes its path on matching `git graft exec` calls. The first restore can populate this directory even if the same packages already exist in a machine-wide cache.

```sh
git graft deps manager add dotnet --cache-env NUGET_PACKAGES
git graft deps manager add mvn '--cache-arg=-Dmaven.repo.local={cache}'
git graft deps manager list
git graft exec task --lease "<lease>" -- dotnet restore
git graft exec task --lease "<lease>" --manager mvn -- ./mvnw verify
```

The default variable is `GRAFT_PACKAGE_CACHE`. A registration can also set one manager-specific variable and prepend ordered arguments. Each `--cache-arg` represents one argument and `{cache}` expands to the cache directory. Executable matching uses the basename. Use `--manager` to select the same registration for a wrapper. The package manager controls version resolution, concurrent cache writes and the work needed to complete a restore. Supply cache settings suitable for shared use. [Maven local repository property](https://maven.apache.org/components/ref/4.0.0-rc-7/configuration.html)

### Reuse an installed directory

With a trusted installed tree, you can register its location and every tracked file that determines the install. For example, a Ruby project that installs under `vendor/bundle` can register that tree from its primary checkout. Replace the example key with the actual runtime, package manager version, ABI and install settings.

```sh
git graft deps manager add bundle --install-dir vendor/bundle \
  --input Gemfile --input Gemfile.lock --key ruby-3.4-bundler-2.6-default
```

The installed tree must be relocatable, free of private data and independent of unlisted inputs. Use a committed ignore rule for the entire installation directory so each graft inherits it. To register it from an existing graft, also supply `--graft NAME --lease TOKEN`. Run registration again after a compatible install with new committed inputs to cache that version as well. Register a new key when external toolchain or install settings change.

During a full `add`, Graft checks the declared inputs, platform, key and destination ignore rules, then attaches verified files into a vacant path using native copy-on-write. Inspect each entry in `storage.registered_dependencies`. An `attached` status means the installed files are ready. With `inputs_mismatch` or `unavailable`, run the manager’s ordinary install in that graft. If a later ignore-rule edit exposes an attached package, `merge --commit` asks you to restore the rule or commit the intended changes explicitly with Git. Cache routing still works on a filesystem that cannot clone files. Sparse, empty and `--no-deps` checkouts skip attachment.

`git graft deps manager remove NAME` removes the settings and retains cached files and existing installs. When all grafts are archived, `git graft prune --purge-cache` previews eligible caches, registered installations and shared dependency layers that a later `deps share` replaced. Add `--apply` to remove verified entries. Layers saved with `deps layer save` or `deps share --snapshot`, and the layer `deps share` currently registers, are reported under `kept` and stay until `deps layer remove`. Changed or unrecognized generations remain available for inspection.

Manager registration and dependency layers are CLI-only. MCP hosts started with `--allow-exec` expose `graft_exec` and `graft_add_dependency` and attach registered installations on `graft_add`; without `--allow-exec` those tools are unavailable and installations are not attached.

## Reuse an npm installation

From a primary checkout with a trusted, relocatable install matching the committed lockfile, register it once:

```sh
git graft deps share --key node-and-npm-version-and-install-options
git graft add task --base main
```

The key attests to the toolchain, options and relevant environment. The install must contain no private data and be usable after source-only edits. Compatible full `add` calls on a clone-capable volume attach a private writable view; inspect `storage.dependencies.status`. An unavailable or mismatched layer is not installed automatically. Use `--no-deps` for a source-only graft. Sparse grafts and MCP hosts without `--allow-exec` do not attach the install.

### What can be shared

`deps share` registers a project only when its whole install is plain registry packages that the lockfile pins:

- npm lockfile version 2 or 3, with `package.json` and the lockfile's root entry agreeing on every dependency section.
- Every dependency is a registry version or an `npm:` alias, and every locked package has an HTTPS `resolved` URL and a SHA-256 or stronger `integrity` hash. Git, `file:`, `link:`, `workspace:` and tarball dependencies are refused.
- No `workspaces`, `overrides`, `bundleDependencies` or `bundledDependencies` in `package.json` or the lockfile root, and no `link` or `inBundle` package in the lockfile.
- `packageManager`, if present, names npm.

A project outside these rules is refused with `dependencies_not_shareable`. `details.reason` says why, `details.field` names the offending key (such as `workspaces` or `dependencies.NAME`), `details.path` names the file or lockfile entry that holds it when there is one, and `details.fallback` gives the way forward. Nothing is registered. Install in each graft instead: run `npm ci` there, or `git graft deps add NAME --lease TOKEN --manager npm`. Neither needs a shared layer, so neither clones the primary's files.

npm workspaces stay refused. npm installs each workspace member as a symlink in `node_modules` to the member's own folder, such as `node_modules/app -> ../packages/app`, and puts conflicting versions in that folder's own `node_modules`. A layer holds one `node_modules` and verifies that every link in it resolves inside it. These links leave it by design, and the members' nested installs lie outside it. Sharing them would mean verifying links against tracked source and layering several directories, which would loosen the check that keeps a shared layer from reaching outside its tree. Graft does not take that on for this case.

### Remove and merge after attaching

A shared install that Graft attached counts as the graft's own, not as local work, while it is unchanged. The attach receipt, `storage.dependencies`, records the `contents` digest of the files Graft verified and attached. `remove` and `merge` hash the graft's `node_modules` again. If it matches, the install is left out of the `local_changes` check, and the archive still holds every file. If any file was added, edited or deleted, or the receipt has no `contents` because the graft predates this check, `node_modules/` is reported as `ignored` as before. Other ignored files always count.

For an exact-source manual snapshot, use `deps share --snapshot --key KEY` (from the primary checkout) or `deps layer save NAME --key KEY` (from a graft), then `deps layer restore NAME --key KEY`. `deps layer remove NAME --key KEY` retires a saved layer: on every platform it moves the layer to `.git/graft/retained-layers/` and reports `retained_path`, so a patched install is never lost. Delete that directory yourself once you no longer need it; Graft never deletes it. Manual transfers require copy-on-write unless `--copy` is explicitly supplied. Review `--help` for leases and options. Treat installed layers as executable content shared within one trust domain.

## Take turns on a shared resource

Grafts separate source files, but a project can have something that exists once and cannot be copied for each graft: a development server on a fixed port, a local database, a simulator or device, a licensed tool. The work that needs it takes turns. A resource is a name the project chooses, such as `web` or `database`: 1 to 64 lowercase letters, numbers or hyphens, starting with a letter or number. Any other name is refused with `invalid_resource`. Graft attaches no meaning to the name and does nothing to the resource itself. It does not start, stop or lock anything. A turn is a promise between cooperating clients, like a lease: it records whose turn it is, and each agent has to ask before it uses the resource.

A turn is held by a holder: the work that needs the resource, such as an agent or a task, rather than one graft, so work that uses grafts of several repositories together holds one turn. A holder has a name of 1 to 64 letters, digits, `-`, `_` or `.`, not starting with `-`, and a lease: a random UUIDv4 it chooses for its first `take` and gives to every later `take` and `release` under that name. Anything else is refused with `invalid_holder`. While a holder holds or waits for a turn, its name on that resource belongs to its lease: another lease gets `lease_mismatch`, with the name in `details.holder`.

```sh
lease=$(uuidgen)
git graft turn take web --holder fix-parser --lease "$lease"
git graft turn take web --holder fix-parser --lease "$lease" --wait 600
git graft turn take web --holder fix-parser --lease "$lease" --pid "$PPID" --graft ../app.graft/fix-parser
git graft turn release web --holder fix-parser --lease "$lease"
git graft turn list
```

`turn take` exits with status 0 whether or not the turn was granted, because waiting in the queue is not a failure. Read `granted` in the reply. The first reply below went to `task-a`, the second to `task-b`:

```json
{"expires_at":1795501800,"granted":true,"grafts":["/work/app.graft/fix-parser"],"holder":"task-a","queue":[],"resource":"web","since":1795500000}
{"expires_at":1795501800,"granted":false,"grafts":["/work/app.graft/fix-parser"],"holder":"task-a","position":1,"queue":["task-b"],"resource":"web","since":1795500000}
```

`holder` and `queue` are holder names, `grafts` are the paths of the holder's grafts, `since` and `expires_at` are Unix times in seconds, and `position` is the caller's place in the queue, with 1 next in line. While nobody holds the turn, `holder`, `since` and `expires_at` are `null` and `grafts` is empty. Replies never contain a lease, and the state stores only a SHA-256 digest of each.

Holders get the turn in the order they first asked. A `take` on a free resource with an empty queue is granted. Any other holder joins the end of the queue and keeps its place each time it asks again. When the holder releases the turn, the first in the queue becomes the holder the next time it calls `take`; one further back is not granted while one ahead of it is still waiting. A queued holder must ask again within 300 seconds to keep its place. After that it is dropped, and joins the end of the queue when it next asks.

A turn lasts for its ttl, counted from the last time its holder took it: 1800 seconds unless `--ttl` gives another value from 1 to 86400. The holder renews the turn by calling `take` again, which keeps the turn's ttl unless `--ttl` is given again. A turn that is not renewed by `expires_at` is free, and a holder that asks after that queues like any other.

Two options end a holder's turn and its place in the queue sooner, so a holder that stops without releasing does not keep others waiting for its whole ttl:

- `--pid PID` names a process whose life bounds the holder, such as the agent's own. Graft records the process's start time with its ID, so a later process that reuses the ID does not count. Once the process ends, the holder is dropped. On Linux, macOS and Windows the start time is checked; on other systems, and on macOS for another user's process, which cannot be inspected there, only whether the ID is in use. A process that is not running is refused with `invalid_holder`.
- `--graft PATH`, repeatable, names a graft the holder works in, in any repository. Once none of the named grafts is active, because each was merged, removed or pruned, the holder is dropped. A path that is not an active graft is refused with `invalid_holder`. Naming a graft needs no lease, since it can only shorten the holder's own turn.

A later `take` that gives `--pid` or `--graft` replaces what the holder named before; one that gives neither keeps it. A holder may name at most 64 grafts. The grafts of another repository are read without its lock, as each of their records is replaced whole, and a repository whose grafts cannot be read counts them as active.

`--wait N`, up to 3600, asks about once a second for up to N seconds and returns as soon as the turn is granted. It holds no lock between asks, so other commands are not kept waiting. If a waiting `take` is interrupted, the holder stays in the queue: run `turn release` when it no longer wants the turn, or those behind it wait until its 300 seconds have passed.

`turn release` gives up the caller's turn or takes it out of the queue, and replies `{"next":"task-b","released":true,"resource":"web"}`. `next` names the holder now first in line, or is `null`. `released` is `false` when the holder neither held the turn nor waited for it, which is not an error. `turn list` replies with `turns`: for each resource that has a holder or a queue, its `resource`, `holder`, `since`, `expires_at`, `grafts` and `queue`. It needs no lease and creates no Graft state in a project that has none.

Turn commands authenticate no graft and change no graft's record, so they work while a process started with `exec` is still running in a graft, such as the server the turn is for, and they do not wait for grafts that are being built. `merge`, `remove` and `prune` do not change turn state; a holder whose grafts are no longer active is left out the next time its resource is read.

### Share a resource between repositories

Turn state is kept in `.git/graft/turns/` of the primary, one file for each resource, so by default each repository has its own turns. When several repositories under one directory share a resource, such as a development stack that serves all of them, list it under `turns` at the top level of that directory's [configuration](#configure-several-repositories-from-one-directory):

```json
{
  "turns": ["stack"],
  "repositories": {"app": {}, "services/router": {}}
}
```

A `turn` command in any repository below that directory then keeps that resource's turn in the Graft state of the repository at the directory itself, so there is one queue for all of them. The directory must be the root of a Git checkout; otherwise turn commands refuse with `invalid_config`. `turns` lists from `.graftconfig` and `.graftconfig.local` are joined. Resources not listed stay each repository's own, and `turn list` shows both.

Graft reads `turns` from the nearest directory above the repository whose files list any, up to home as for the configuration, passing over directories whose files list none, even when the repository has files of its own, so a repository's own configuration cannot take it out of the queue. Those files still govern everything else. If the repository's own files also list `turns`, which they do to share resources with repositories below it, they must list the same resources as the directory above; otherwise turn commands refuse with `invalid_config` and name both files. When the directory above is itself a checkout whose own directory above lists the same `turns`, the turns are kept there, so a queue is never split between levels.

The MCP tools `graft_turn_take`, `graft_turn_release` and `graft_turn_list` serve the same turns. They do not need the host's `--allow-exec` option. `graft_turn_release` and `graft_turn_list` run nothing. `graft_turn_take` runs the project's [`post-take` hooks](#hooks-for-turns) only when the host has `--allow-exec`; without it, it still takes the turn and reports them as skipped. Over MCP the session is the holder: the server holds every turn its client takes under a lease of its own and with its own process ID, so the agent gives neither, and the turns end when the session ends. `graft_turn_take` takes `resource`, `holder`, optional `grafts` (paths, relative to the repository the server serves) and optional `wait` and `ttl`; `graft_turn_release` takes `resource` and `holder`. Give `wait` a value shorter than the client's limit for one tool call. A call the client cancels stops asking, and gives up a turn it was granted after the cancellation unless it only renewed a turn the holder already held.

If a turn state file cannot be read, commands for that resource fail, name the file and leave it unchanged; move it aside to start that resource's queue again. A state file written before holders replaced grafts as the holders of turns is reported the same way.

## Hooks

A hook is a command that Graft runs at a point in a graft's life, defined in the project's [configuration](#configure-a-project). Use one for setup and cleanup that belongs to the project and not to Git: creating a database for the graft and dropping it again, writing a settings file that names the graft's port, or deleting a secret before the graft is archived.

```json
{"hooks": {"database": {"event": "post-add", "argv": ["/opt/project/bin/graft-setup"]}}}
```

`argv` is the program and its arguments. An event can have any number of hooks, which run in the order the configuration gives; once one fails, the rest for that command are skipped. `git graft hook list` shows them. There is no command or MCP tool that defines a hook: edit the files.

A hook that comes with the repository runs only once you have trusted it: every hook in `.graftconfig`, and every hook in `.graftconfig.local` once Git tracks that file. `git graft hook list` shows such a hook with `"trusted": false` until then. Read the hooks, then run `git graft hook trust`. It records a SHA-256 digest of those hooks, each by event, name and `argv`, in `.git/graft/trusted-hooks.json`, and replies with the hooks it trusted and the digest. For a hook whose program is a path relative to a [directory above the checkout](#configure-several-repositories-from-one-directory), the digest also covers the resolved path and the program's contents, so an edit to the program needs trust again. Until you trust them, and again after any change to a hook in those files, such as one merged from a graft or pulled from a remote, `add`, `remove`, `merge`, `restore`, `prune --apply` and `turn take` refuse with `hooks_untrusted` before they run or change anything, when they would run a hook for their event. A `prune` preview runs no hook, so it needs no trust. What an untracked `.graftconfig.local` adds, replaces or turns off is your own and needs no trust, except on Windows in a [directory above the checkout](#configure-several-repositories-from-one-directory); `include` patterns run nothing and need none either. The [remotes](#remotes-in-a-graft) those files list need trust by the same rule, recorded as a digest of their own beside the hooks' digest, which is the same as before Graft read remotes: an untrusted remote is left out of a new graft rather than refused, and a change to the remotes does not stop the hooks. There is no MCP tool that trusts hooks or remotes. See [safety](safety.md).

| Event | When the hook runs |
| --- | --- |
| `post-add` | After `add` has created the graft and recorded it active, before `add` replies. |
| `pre-archive` | Before `remove`, `merge` or `prune --apply` archives a graft: after the lease is checked and before Graft looks for untracked or ignored files, which the hook may remove. A plain `remove` refuses a pending merge, a Git operation in progress, a change to a tracked file and unmerged work before the hook runs. `merge --undo` runs no hook. |
| `post-restore` | After `restore` has reopened an archived graft, before it replies. |
| `post-take` | After `turn take` has granted a turn the holder did not hold, or changed the grafts of one it holds, before it replies. See [hooks for turns](#hooks-for-turns). |

Graft starts the program directly, without a shell, with the graft as its working directory, or the resolved `program` for a hook from a directory above the checkout. It waits for the hook to exit, and for its standard output and standard error to close, and sets no time limit. A job the hook starts in the background must redirect both streams, for example `nohup server >server.log 2>&1 &`, or the command does not return until that job exits. Name the program by an absolute path outside the grafts, or by a bare name that is looked up in `PATH`; in a directory above the checkout, a path relative to that directory also works. The configuration refuses a relative path with a directory in it, such as `./scripts/setup`, everywhere else: on Unix it would name the graft's own copy, so the graft's files would decide what runs, and on Windows it would name a program in the caller's directory instead. The hook gets the environment of the `git graft` command, or of the MCP host, without `GIT_GRAFT_LEASE` and without the variables that point Git at another repository, such as `GIT_DIR`. A hook never receives a lease. Graft adds these variables:

| Variable | Value |
| --- | --- |
| `GIT_GRAFT_HOOK` | The event: `post-add`, `pre-archive`, `post-restore` or `post-take`. |
| `GIT_GRAFT_HOOK_NAME` | The hook's name in the configuration. |
| `GIT_GRAFT_NAME` | The graft's name. |
| `GIT_GRAFT_PATH` | The graft's path, which is also the working directory. |
| `GIT_GRAFT_PRIMARY` | The primary checkout's path. |
| `GIT_GRAFT_CONFIG_DIR` | The directory that holds the file defining the hook: the primary checkout, or the directory above it. |
| `GIT_GRAFT_BRANCH` | The branch the graft was created on. Empty for a graft created with `--detach`. |
| `GIT_GRAFT_BASE` | The commit the graft was created from. Empty for a graft created with `--orphan`. |
| `GIT_GRAFT_PORT` | The graft's port, the one `exec` passes as `PORT`. `restore` can assign a different one. |

For example, a setup script configured for both `post-add` and `post-restore`, and a teardown script for `pre-archive`, as in the [configuration example](#configure-a-project):

```sh
#!/bin/sh
# graft-setup: a database and a settings file for this graft.
set -e
createdb "app_$GIT_GRAFT_NAME"
printf 'DATABASE=app_%s\nPORT=%s\n' "$GIT_GRAFT_NAME" "$GIT_GRAFT_PORT" > settings.local
```

```sh
#!/bin/sh
# graft-teardown: safe to run more than once.
set -e
dropdb --if-exists "app_$GIT_GRAFT_NAME"
rm -f settings.local
```

If the project ignores `settings.local`, a plain `remove` would refuse the graft with `local_changes` because of that file. The teardown script deletes it first, so `remove` proceeds.

A command that triggers hooks reports each in a `hooks` array, in the order they ran. With no hook configured for the event, the result has no `hooks` field.

```json
{"name": "database", "event": "post-add", "file": ".graftconfig", "status": "ok", "exit_code": 0, "stdout": "", "stderr": "", "stdout_truncated": false, "stderr_truncated": false}
```

`status` is `ok` for exit code 0, `failed` for any other exit, and `skipped` when the hook was not run, because the MCP host does not allow it or an earlier hook for the command failed. `stdout` and `stderr` are captured as for `exec` and cut at 256 KiB each. A `failed` entry whose hook could not be started, and every `skipped` entry, has a `reason`; `exit_code` is then `null`, as it is for a hook ended by a signal. `prune --apply` reports the hooks in the `hooks` array of that graft's entry.

What a failure means depends on the event:

- A failing `post-add` or `post-restore` hook does not fail the command, though the hooks after it are skipped. The graft exists, is active and keeps its lease; Graft never deletes it. Read the entry, then repair the setup by hand or archive the graft.
- A failing `pre-archive` hook refuses `remove` and `merge` with `hook_failed`, and `details.hooks` carries every entry, the skipped ones too. Graft itself has changed nothing: the graft, its files and the primary are as the hook left them. `prune --apply` keeps that graft with the reason `hook_failed` and goes on with the others.

A `pre-archive` hook must be safe to run more than once for the same graft. It can run before Graft knows whether the command will succeed. If `merge` then stops on a conflict, or `remove` on untracked files the hook left, the graft stays active with the hook's cleanup already done, and the hook runs again on the retry. A plain `remove` of a graft with unmerged commits or a changed tracked file refuses with `unmerged_work` or `local_changes` without running the hook, so the graft keeps its setup. `prune --apply` runs it for a graft it would archive and for one held back only by untracked or ignored files, which the hook may remove; a graft kept for a change to a tracked file, a Git operation in progress, its commits, refs or stash is left alone, hook unrun. A `prune` preview runs no hook, so it can report `dirty` for a graft whose hook would remove the files.

A retried command that only repeats an earlier result runs no hook and has no `hooks` field: an `add` with the same `request_id` that returns the graft already created, a repeated `restore`, or a repeated `remove` or `merge` that returns its receipt. If `add` or `restore` is interrupted while its hook runs, the retry returns the graft without running the hook again, so run the setup by hand.

While a hook runs, Graft holds that graft's lock and no project-wide lock. Other commands on the same graft report `workspace_busy`, and other grafts can be added, merged and removed meanwhile. A process that a hook leaves running is not tracked.

Over MCP, hooks run on `graft_add`, `graft_remove`, `graft_merge`, `graft_restore` and `graft_turn_take` only when the host was started with `--allow-exec`. Without it they are not run, the entry has `"status": "skipped"`, and the command goes on, so `graft_remove` and `graft_merge` archive the graft without its `pre-archive` hook, and `graft_turn_take` grants the turn without its `post-take` hook. With it, hooks that need trust and lack it refuse the call with `hooks_untrusted`, as on the CLI. The CLI always runs hooks once they are trusted.

If the configuration cannot be read, `add`, `remove`, `merge` and `restore` refuse with `invalid_config` before changing anything, and `prune --apply` keeps each graft it would have archived, with the reason `error`.

### Hooks for turns

A `post-take` hook acts on the resource for the holder that has just been granted it: it can point a development server at the holder's grafts, for example. It runs when `turn take` grants a turn the holder did not hold, and when a holder that already holds it names a different set of grafts; a renewal, a queued ask and a release run nothing. The hooks come from the configuration of the project that keeps the turn: the primary's own, or, for a [shared resource](#share-a-resource-between-repositories), the top-level `hooks` of the directory's configuration. They run in that project's checkout with no Graft lock held, after the turn is recorded. A top-level `post-take` hook runs for every resource whose turn that project keeps: those it shares under `turns` and its own, such as `web`. A hook meant for one resource should check `GIT_GRAFT_TURN_RESOURCE` and exit 0 for the others. Since they never run in a graft, a `post-take` hook may name its program by a path relative to the configuration's directory, such as `./scripts/stack`, without `..`, wherever it is defined; trust covers that program's contents, as for hooks [in a directory above the checkout](#configure-several-repositories-from-one-directory). Besides `GIT_GRAFT_HOOK`, `GIT_GRAFT_HOOK_NAME`, `GIT_GRAFT_CONFIG_DIR` and `GIT_GRAFT_PRIMARY`, which names the checkout of the project that keeps the turn, a `post-take` hook receives:

| Variable | Value |
| --- | --- |
| `GIT_GRAFT_TURN_RESOURCE` | The resource. |
| `GIT_GRAFT_TURN_HOLDER` | The holder's name. |
| `GIT_GRAFT_TURN_GRAFTS` | The paths of the holder's grafts, one per line, or empty. |

```sh
#!/bin/sh
# post-take: only the stack resource is switched; any other turn needs nothing.
[ "$GIT_GRAFT_TURN_RESOURCE" = stack ] || exit 0
# Tell the stack which grafts to serve, one path per line.
printf '%s\n' "$GIT_GRAFT_TURN_GRAFTS" > stack-grafts.txt
```

The reply carries the `hooks` array. If a `post-take` hook fails, the turn is given up again, the rest are skipped, and `turn take` refuses with `hook_failed`, with `details.resource`, `details.holder` and `details.hooks`. Trust applies as for other hooks, but only to a `take` that would run one: after a trusted `post-take` hook is edited, a renewal and a queued ask still succeed, and the `take` that would run the hook refuses with `hooks_untrusted`, changing nothing, until you trust it again. An MCP host runs them only with `--allow-exec`, reporting them as skipped otherwise while the turn is still taken. A waiting `take` runs them once, when it is granted.

## Merge and recover

`merge` integrates committed work into a local branch by fast-forward, a merge commit or `--squash`, subject to the destination's state. `--commit MESSAGE` stages nonignored changes in the graft first; use it only when all such changes belong in the task. If `--commit` detects a path with both staged and unstaged changes, it returns `partially_staged` without replacing the staged version. Commit the staged version or unstage it explicitly before retrying. This also applies to a staged deletion whose path has been recreated. Conflicts remain available for inspection. `--reconcile` supports a narrow offline npm lockfile conflict case; other conflicts need normal resolution. Ignored files block a normal merge; `--discard-ignored` retains them in the archived graft.

The receipt from `merge` has `"action":"merge"`, the `target` branch, its `previous` and new `commit`, and `graft_tip`; the receipt from `remove` has `"action":"remove"`. Both include `recovery_lease`. Retain it. The default merge commit message is `Merge graft NAME into TARGET`.

If the graft gains commits after its merge reaches the destination, for example from a hook or a second agent, `merge` does not archive it: the archive would present those commits as merged. The receipt then has `"removed": false`, the merged `graft_tip` and the newer `graft_head`. The graft stays active with the same lease and every file, and the next `merge` lands the newer work. `--undo` applies only to a merge that archived its graft.

If `merge --commit` is killed while it stages the graft, it leaves the graft's `.git/index.lock`. Retry the same merge: Graft releases a lock that carries its own staging marker, because it then holds the graft's workspace lock and Git can still read the index. A lock that Graft did not create is reported as `git_busy` and kept.

`restore NAME --lease TOKEN` reopens the archive and issues a fresh active lease. `merge NAME --undo --lease TOKEN` appends a reversal commit only while the destination is still at the recorded merge commit. `list` includes recoverable grafts. Archives retain repository and local file state, which can include credentials and other sensitive user data, and occupy disk until they are deleted. See [safety and recovery](safety.md).

`prune --purge-archives DAYS` previews the archives of grafts archived at least DAYS days ago, under `archives.purged`, each with its `name`, `id`, `archived_at` (Unix seconds) and `path`; with `--apply` it deletes each archive and its record, after which the graft cannot be restored and its recovery lease finds nothing. `0` takes every archive. The archives are chosen before prune archives anything, so `--apply` deletes those the preview names and never a graft the same run archived. Graft records when it archives a graft; for a graft archived by an earlier version, its last heartbeat stands in. Archives whose record is damaged or in the middle of an operation, and those whose graft was restored after they were chosen, are left alone. Graft refuses a `.retired` that is not its own directory, such as a symbolic link to another. A deletion is set aside first under `.retired/.purging-ID` and the record removed, so an interrupted one never leaves a record pointing at half an archive; the next `--purge-archives --apply`, whatever its DAYS, deletes what is left, and the record too if the purge stopped before removing it. The preview names such a graft. Only directories by that name are deleted; a file or link is left alone. If a deletion fails, the command fails with `purge_failed` even when prune has archived grafts: `details` holds the whole prune reply, and its `archives.purged` lists the archives deleted before the failure. There is no MCP tool for it.

For a disconnected `add`, retry with the same client-generated, cryptographically random UUIDv4 `request_id` and the same options. The request ID acts as the graft lease; keep it secret and share it only with trusted local tools.

### Interrupted add

If `add` stops before it finishes, for example because it was killed during the checkout, it reports `spawn_incomplete` or leaves a graft with `"status": "pending"`. Until the checkout's index is published, the unfinished graft holds `.git/index.lock`, which Graft writes when it reserves the graft, so Git cannot commit the files that are not checked out yet as deletions. The `spawn_incomplete` recovery message says whether the lock is still held. Do not delete that lock or work in a pending graft.

Retry the same request. Graft deletes nothing: it archives the whole unfinished attempt under a new lease, then builds the graft again at the same name and path. The reply lists each archived attempt in `archived_attempts`, with its `archive_path` and `lease`, and so do the graft's later `status`, merge and remove receipts, `restore` replies and `prune` entries. Copy anything you need from `archive_path`; its files are kept byte for byte, including untracked, ignored and edited files. A retry while the first attempt is still running waits for it and returns its graft. On Windows, a process that still has a file open in the attempt can block the archive move; the retry then reports `workspace_busy` and leaves the attempt in place, so retry again once that process exits.

An archived creation that never finished cannot be restored: `restore` with its lease, or with the lease of a pending graft you archived with `remove --discard`, fails with `unfinished_creation` and leaves the archive exactly as it was. `details.archive_path` names it; copy what you need from there, or run `add` again.

## Uncommitted edits in the primary checkout

A merge or undo into the branch checked out in the primary works like `git merge` with local edits. Your uncommitted edits stay exactly as they were, as long as none of them touches a path the merge changes. Edits are staged and unstaged changes and untracked files. Staged changes stay staged, unstaged edits keep their bytes, line endings included, and untracked files stay where they are. A retry of a merge that was stopped before it wrote any file follows the same rule.

If any edit touches a path the merge changes, Graft refuses with `destination_has_local_changes` and changes nothing. `details.paths` lists the overlapping paths, and `details.checkout`, `target`, `previous` and `commit` name the checkout and the update. Commit those edits or move them aside, then retry. Edits to other paths can stay.

These edits count as touching a changed path:

- an edit, deletion, staged version or type change (file, symlink or directory) of a file the merge changes, including a staged version that already equals the merged one;
- an untracked file where the merge adds a file, including one whose name differs only by case or Unicode normalization on a filesystem that treats both spellings as one name;
- either side of a rename, because a rename is a deletion and an addition;
- a file inside a path the merge turns into a file, or a file the merge turns into a directory;
- a submodule with new commits or edits, when the merge moves that submodule. A merge that leaves the submodule alone never touches it.

Just before Git writes any file, its dry run checks every file the merge replaces again, so an edit made while the merge prepares is refused the same way and leaves no recovery state behind. An ignored file the merge would overwrite or write into also stops it with `destination_has_local_changes`, naming the paths the merge would write; move that file aside, then retry. A file that appears later, even while Git writes, is never overwritten either: Git creates each new file exclusively and refuses a path that is already taken, and Graft then returns every other file to the previous commit and reports `destination_has_local_changes` naming that path. As with `git merge`, an edit to a tracked file the merge changes, made in the instant between Git's final check of that file and its write, can still be replaced. Unresolved conflicts, assume-unchanged or skip-worktree flags and sparse checkouts in the primary still block the merge.

## When two tasks edit the same file

Two grafts created from the same commit each get a writable checkout of that version. Saving or committing a file in one leaves the other graft and the primary at their own versions. When both tasks merge into the same branch, the first successful merge updates that branch. Graft uses Git's three-way merge for a second, diverged task. Edits to separate parts of a file can merge automatically.

If Git reports conflicting edits, Graft returns the paths and keeps the second graft. From that graft, fetch the current destination and merge it locally. This example uses a destination named `main`.

```sh
git fetch --no-tags /absolute/path/to/primary refs/heads/main
git merge FETCH_HEAD
git status
```

Resolve any conflicts, stage the resolutions, commit the merge and run the project's tests. From the primary, retry `git graft merge NAME --into main --lease TOKEN` with the same graft lease.

## Interrupted checkout update

A merge or undo into the branch checked out in the primary can stop while Git rewrites its files, for example when a checkout filter fails, the disk fills or the process is killed. It returns `checkout_recovery_required` with `target`, `previous`, `commit`, `graft_tip` and `recovery_path`, and keeps a journal there with the original index as `index.before`. Retry the same command with the same lease. Nothing needs to be cleaned up first.

On retry, Graft compares each file the update changes with its previous and merged versions and lets Git's two-tree update finish from there. A merge from another graft first reverses the stopped update in the same way, returning those files to the previous commit, and the stopped graft keeps its plan for its own retry. Neither direction deletes or overwrites untracked or ignored files, edits to other files, or changes staged since the interruption. While the native Git update of the stopped attempt is still running, the retry waits and reports it.

Recovery stops before writing any file, and names the paths in `paths`, when a changed file holds neither version or a changed path has a newly staged version. It also stops, naming them, when files appear where it writes; Git keeps them as they are. Keep a copy of those edits, return each file to either version or record and unstage the staged version, then retry. If the process tree was killed, Git can leave its own `HEAD.lock` or branch lock; Graft reports the lock path and does not remove it. Follow Git's advice once no Git command is running, then retry.

If the destination branch moved or is no longer checked out, the update can no longer finish. Graft returns `checkout_update_set_aside`, changes no further file and keeps the journal at the reported `recovery_path`. Files the stopped update had already written stay as they are and show in `git status`. Check `git status` in the primary, then retry the merge, which plans again against the current branch. The graft cannot be pruned while its checkout update is pending.

## Push to a remote

A graft has no remotes of its own unless the configuration [names some](#remotes-in-a-graft). `push` sends its commits to a remote of the primary repository without merging them into a local branch:

```sh
git graft push task --lease "<lease>"
git graft push task --lease "<lease>" --to fix/parser
git graft push task --lease "<lease>" --remote fork --to fix/parser
```

Use `merge` when the work should land on a local branch of the primary. Use `push` when a branch on a remote is the destination, for example to open a pull request from it.

The source is the graft's `HEAD` commit. The destination is the branch named by `--to`, or otherwise a branch with the name of the graft's current branch, which is `graft/NAME` unless `add --branch` chose another. A graft with a detached `HEAD` is refused with `detached_head` unless `--to` is given, and a graft with no commits is refused with `nothing_to_push`. A graft whose branch has the name of the remote's default branch, as the primary records it in `refs/remotes/REMOTE/HEAD`, is refused with `default_branch` unless `--to` names that branch: a graft added with `--branch main` does not update the remote's `main` on a plain `push`. `git clone` and `git remote set-head` write that record; without it, no branch is treated as the default. `--remote` takes the name of a remote configured in the primary and defaults to `origin`. It does not take a URL or a path: any other value is refused with `remote_not_found`, and `details.remotes` lists the primary's remote names.

Graft copies the commits into the primary's object store under a private ref, runs `git push REMOTE COMMIT:refs/heads/BRANCH` in the primary, then removes the private ref whether or not the push succeeded. The copy is the same protected transfer a merge uses and runs no hook. The push is the primary's own Git with your configuration and environment, so the remote's URL, `pushurl` and `insteadOf` rewrites, credential helpers, SSH settings and the primary's `pre-push` hook all apply. The lease is not passed to them. Git is run with `GIT_TERMINAL_PROMPT=0`, so Git's own terminal prompt for a username or password fails instead of waiting. Unless `GIT_SSH_COMMAND`, `GIT_SSH` or `core.sshCommand` names an SSH command, Git is also given `GIT_SSH_COMMAND='ssh -o BatchMode=yes'`, so an SSH passphrase, password or unknown host key fails the push instead of waiting. Other prompts are not suppressed: Git still runs a program named by `GIT_ASKPASS`, `core.askPass` or `SSH_ASKPASS`, a credential helper such as Git Credential Manager can open its own dialog, and an SSH command you configured runs as configured. An MCP server started by an IDE can inherit such a program, and the push then waits on a dialog nobody sees while it holds the graft's lock. Configure credentials that need no answer, such as a stored token or an SSH agent, before pushing from an unattended agent. Tags and submodules are not pushed, even where `push.followTags` or `push.recurseSubmodules` is set. The primary's branches, `HEAD`, index and files do not change. Git updates the primary's remote-tracking ref for the pushed branch, such as `refs/remotes/origin/graft/task`, as it does after any push.

Nothing is forced. If the remote branch has commits the graft lacks, or the remote refuses the update for another reason such as a protected branch or a server hook, the error is `push_rejected` and `details.reason` gives the reason. Any other failure, such as an unreachable remote, failed authentication or a `pre-push` hook that declines, is `push_failed`. Both carry `details.remote`, `details.branch` and Git's messages in `details.stderr`, with the user and password part of any URL removed. Graft itself never prints a remote's URL. After a rejection the remote, the primary's branches and the graft are as they were.

```json
{"action":"push","name":"task","remote":"origin","branch":"graft/task","commit":"<oid>","uncommitted_changes":0}
```

Only commits are pushed. `uncommitted_changes` counts the staged, unstaged and untracked files that stayed in the graft. Commit them and push again if they belong to the work.

The graft stays active with the same lease. To continue after a push, for example to answer a review, commit in the graft and run the same `push` again; it fast-forwards the remote branch. To take in commits that reached the remote branch from elsewhere, fetch the remote in the primary, then fetch its remote-tracking ref from the primary into the graft and merge it:

```sh
git -C /absolute/path/to/primary fetch origin
git fetch --no-tags /absolute/path/to/primary refs/remotes/origin/fix/parser
git merge FETCH_HEAD
```

Graft does not treat pushed commits as merged, because no local branch of the primary contains them. `remove` therefore still reports `unmerged_work` after a push. To finish, archive the graft with `remove --discard` and keep the `recovery_lease`; `restore` reopens it if more work is needed. Plain `remove` works once a local branch of the primary contains the pushed commits, for example after the pull request was merged and the primary pulled it. A squash or rebase merge creates other commits, so it does not count.

While a push runs, `merge`, `remove`, `exec` and lease renewal on the same graft report `workspace_busy`, and a graft with a background job started by `exec` cannot be pushed until the job exits. Other grafts are not held while Git talks to the remote. If a push is killed, its private ref can stay in the primary under `refs/graft/`. It only keeps the pushed commits from garbage collection, and the graft's next push removes it.

Over MCP the tool is `graft_push`, with `name`, `lease` and optional `remote` and `to`. It is offered only by a host started with `--allow-push`, which is separate from `--allow-exec`.

## Safety error codes and receipt fields

These codes and fields tell you that Graft kept something instead of deleting or overwriting it. Every error here leaves the graft, its archive and the primary's files as they were. Error codes appear as `code` in CLI JSON errors and MCP tool results.

| Code or field | Meaning | What to do |
| --- | --- | --- |
| `unmerged_work` | `remove` without `--discard` found work a merge never took: commits, a moved or detached HEAD, other branches, tags, a stash or other refs. `details.found` lists them. | Merge the graft, or archive it with `remove --discard` and keep the `recovery_lease`. |
| `unmerged_commits` (with `head`) | A `remove --discard` receipt with `"action":"remove"` after an earlier merge had landed. `commit` and `graft_tip` name that merge; `head` is the archived graft HEAD whose later commits were never merged. | Keep the `recovery_lease`. Restore the graft to merge the later commits. |
| `"removed": false` (with `graft_head`) | A merge receipt: the graft gained commits after `graft_tip` was merged, so it stays active with the same lease and files. | Merge it again to land `graft_head`. |
| `graft_archived` | The lease belongs to an archived graft. `details.archived_by` is `merge`, `remove` or `remove_or_prune`; `details.name_in_use` says whether another graft now holds the name. | Run `restore NAME --lease TOKEN`. If the name is in use, archive that graft first. |
| `record_damaged` | The record file of this graft cannot be read or conflicts with another record. `details.file` names it. Graft never rewrites, moves or deletes it. | Repair the file from a backup or move it aside yourself. Other grafts keep working. |
| `damaged_records` | A `list` and `prune` field naming each damaged record file and its error. Prune skips them. | As for `record_damaged`. |
| `primary_moved` | Every record names one other primary path, so the primary was moved or copied. `details.recorded_primary` is that path. Nothing is read or written. | Move the primary back to `recorded_primary`. Do not edit or move the record files. |
| `already_exists` | `restore` found another graft using the name, or a file or directory at the graft's path. | Merge or remove the other graft, or move the existing path aside after saving its data, then retry. |
| `archive_damaged` | The archive at `details.archive_path` is missing or was changed by hand. Restore leaves it exactly as found. | Copy what you need from it, or put back the missing Git files and retry. |
| `spawn_incomplete` | `add` stopped before the graft was built. `details.path` is the pending graft, which holds its index lock so Git cannot commit it. | Retry the same request. See [interrupted add](#interrupted-add). |
| `archived_attempts` | Named in `add`, `status`, merge, remove, `restore` and `prune` replies about a graft whose request was retried: archives of its unfinished attempts, each with `archive_path` and `lease`. | Copy what you need from `archive_path`. |
| `unfinished_creation` | `restore` was asked for an archive of a creation that never finished. `details.archive_path` names it; nothing was changed. | Copy what you need from the archive, or run `add` again. |
| `destination_has_local_changes` | Uncommitted edits in the primary checkout touch paths the merge or undo changes. `details.paths` lists them; nothing changed. | Commit those edits or move them aside, then retry. See [uncommitted edits in the primary checkout](#uncommitted-edits-in-the-primary-checkout). |
| `local_changes` | `merge` or `remove` found uncommitted, untracked or ignored files in the graft. `details.paths` lists up to 200 of them, each with `status` `added`, `deleted`, `modified`, `untracked` or `ignored`, and `details.count` gives the total. On `remove`, `details.discard_archives` is `true`: `remove --discard` archives every one of them with the graft. | Commit them and retry, or archive the graft with `remove --discard` and keep the `recovery_lease`. A shared install that Graft attached, and files `add` copied with `include`, are not listed while the graft has not changed them. |
| `dependencies_not_shareable` | `deps share` found a project whose install is not plain, lockfile-pinned registry packages, such as npm workspaces. `details.reason` says why, `details.field` and `details.path` name the offending key and file when known, and `details.fallback` gives the alternative. Nothing was registered. | Install in each graft: run `npm ci` there, or `git graft deps add NAME --lease TOKEN --manager npm`. See [what can be shared](#what-can-be-shared). |
| `checkout_recovery_required` | A checkout update of the primary was interrupted. `details.paths` lists files that need attention. | Retry the same merge or undo with the same lease. See [interrupted checkout update](#interrupted-checkout-update). |
| `checkout_update_set_aside` | An interrupted checkout update can no longer finish because its branch moved or is no longer checked out. The journal stays at `details.recovery_path`. | Check `git status` in the primary, then retry the merge. |
| `push_rejected` | `push` was refused by the remote: the remote branch has commits the graft lacks, the branch is protected or a server hook declined. `details.reason` gives the reason, with `details.remote`, `details.branch` and Git's messages in `details.stderr`. Nothing was pushed or forced. | Merge the remote branch into the graft and push again, or push to another branch with `--to`. See [push to a remote](#push-to-a-remote). |
| `push_failed` | `push` could not be completed: the remote was unreachable, authentication failed, the primary's `pre-push` hook declined, or Git failed in another way. `details.stderr` has Git's messages. If the connection was lost after the remote accepted the update, the remote branch may already hold the commit. | Fix the cause in the primary, where the remote and its credentials are configured, then push again. Pushing a commit the remote already has succeeds. |
| `remote_not_found` | `push` was given a remote name the primary does not configure. `details.remotes` lists the names it has. | Choose one of them, or add the remote in the primary with `git remote add`. |
| `detached_head` | `push` found the graft's `HEAD` detached, so there is no branch name to push. `details.commit` is that commit. | Name the remote branch with `--to`. |
| `nothing_to_push` | `push` found a graft with no commits. | Commit in the graft, then push again. |
| `default_branch` | `push` without `--to` would update the remote's default branch, because the graft's branch has its name. `details.remote` and `details.branch` name them. Nothing was pushed. | Push to another branch with `--to`, or name the default branch with `--to` if it is meant to be updated. |
| `git_busy` | A Git lock or an unreadable index in the graft or primary is held by a command Graft did not start, or cannot prove has exited. | Wait for the Git command to finish. Remove a lock only once no Git command is running. |
| `workspace_busy` | Another Graft command, a running hook, or a process started by `exec` (including background jobs), holds the graft. | Wait, or stop the background job, then retry. |
| `"granted": false` (with `position`) | A `turn take` reply, not an error: another holder holds the turn on the resource or is ahead in its queue. | Leave the resource alone. Ask again within 300 seconds, or use `--wait`. See [take turns on a shared resource](#take-turns-on-a-shared-resource). |
| `invalid_resource` | A `turn` command was given a resource name that is not 1 to 64 lowercase letters, numbers or hyphens starting with a letter or number. Nothing changed. | Use a name of that form. |
| `invalid_holder` | A `turn` command was given a holder name that is not 1 to 64 letters, digits, `-`, `_` or `.`, a lease that is not a random UUIDv4, a `--pid` that names no running process, or a `--graft` path that is not an active graft. Nothing changed. | Fix the argument. See [take turns on a shared resource](#take-turns-on-a-shared-resource). |
| `lease_mismatch` (from `turn`) | Another lease holds or waits for the turn under this holder name. `details.holder` names it. Nothing changed. | Use the lease the name was first given, or another name. |
| `invalid_config` | `.graftconfig` or `.graftconfig.local` is not valid JSON, is not a JSON object, has a key Graft does not know, or defines a hook or pattern Graft refuses. `details.file` names the file, and the message says what is wrong. Nothing was changed. | Fix the file and retry. See [configure a project](#configure-a-project). |
| `hook_failed` | One of the project's `pre-archive` hooks exited with a non-zero code or could not be started, so `remove` or `merge` did not archive the graft. `details.hooks` carries the exit code and output of each hook, those skipped after it too. Graft changed nothing; the hook may have. As a prune `reason`, the same for one expired graft; prune went on with the others. | Fix what the hook reports, then retry. The hook runs again. See [hooks](#hooks). |
| `purge_failed` | `prune --apply --purge-archives` could not delete an archive. Unlike the other codes here, the command has changed things: prune archived what `details.retained` reports, and `details.archives.purged` lists the archives deleted before the failure. `message` names the cause. | Fix the named problem, then rerun the command. It finishes a deletion it had begun. |
| `hooks_untrusted` | The command would run a hook from `.graftconfig`, or from a `.graftconfig.local` that Git tracks, and those hooks have never been trusted or have changed since `git graft hook trust` last ran. `details.hooks` lists the hooks that need trust and `details.trusted` those last trusted, or `null`. Graft ran nothing and changed nothing. | Read the hooks, then run `git graft hook trust` in a terminal and retry. No MCP tool can trust hooks. See [hooks](#hooks). |
| `kept` | A `gc --purge-cache` field listing saved layers (`saved_layer`) and the registered shared layer (`registered_shared_layer`) it did not delete. | Retire them with `deps layer remove` when no longer needed. |
| `retained`, `retained_path` | `deps layer remove` moved the layer to `.git/graft/retained-layers/` instead of deleting it. | Delete `retained_path` yourself once you no longer need the install. |
| prune `reason` `error`, `archive_occupied`, `busy` | Prune could not check or archive this graft (`error` carries the message), found its archive path occupied, or found it busy. It went on with the others. | Fix the named problem, then rerun `prune`. |
