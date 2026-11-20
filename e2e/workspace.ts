/** Verify standalone repository isolation and ordinary Git behavior. */
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import {
  existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readlinkSync, realpathSync,
  renameSync, rmSync, statSync, symlinkSync, writeFileSync,
} from 'node:fs';
import { delimiter, dirname, isAbsolute, join } from 'node:path';
import { objectResult, type Context, type Scenario } from './harness.ts';

function spawn(ctx: Context, name: string, options: string[] = [], repo = ctx.repo, base: string | null = 'HEAD') {
  const result = ctx.cli(['new', name, ...(base === null ? [] : ['--base', base]), ...options], repo);
  assert.equal(result.name, name);
  const { path, lease } = result;
  assert(typeof path === 'string' && typeof lease === 'string' && lease.length > 0);
  return { name, path, lease, result };
}

function owned(action: string, workspace: { name: string; lease: string }, ...options: string[]) {
  return [action, workspace.name, '--lease', workspace.lease, ...options];
}

function write(root: string, name: string, bytes: string): void {
  const path = join(root, name);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, bytes);
}

function read(root: string, name: string): string {
  return readFileSync(join(root, name), 'utf8');
}

function commit(ctx: Context, repo: string, name: string, content: string): string {
  write(repo, name, content);
  ctx.git(['add', '--', name], repo);
  ctx.git(['commit', '-qm', name], repo);
  return ctx.git(['rev-parse', 'HEAD'], repo);
}

function assertRepository(ctx: Context, path: string): void {
  const gitdir = join(path, '.git');
  assert(lstatSync(gitdir).isDirectory());
  assert.equal(ctx.git(['rev-parse', '--is-bare-repository'], path), 'false');
  assert.equal(realpathSync(ctx.git(['rev-parse', '--absolute-git-dir'], path)), realpathSync(gitdir));
  assert.equal(realpathSync(ctx.git(['rev-parse', '--path-format=absolute', '--git-common-dir'], path)),
    realpathSync(gitdir));
}

// Record only fixed operation names. Git argv can also contain paths and config values.
const gitProbe = `#!/bin/sh
inspect_operation() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -C|-c|--config-env|--git-dir|--work-tree|--namespace)
        [ "$#" -ge 2 ] || break
        shift 2 ;;
      -*) shift ;;
      worktree|pack-objects|index-pack|fetch-pack)
        printf '%s\\n' "$1" >> "$GRAFT_E2E_GIT_LOG" || return 1
        if [ "$1" = worktree ]; then
          printf '%s\\n' 'git worktree is forbidden in the independent fixture' >&2
          return 1
        fi
        return 0 ;;
      *) break ;;
    esac
  done
  printf '%s\\n' other >> "$GRAFT_E2E_GIT_LOG"
}
inspect_operation "$@" || exit 1
exec "$GRAFT_E2E_REAL_GIT" "$@"
`;

function traceCli<T>(ctx: Context, run: () => T, expectObjectTransfer = false): T {
  const dir = mkdtempSync(join(ctx.root, 'git-probe-'));
  const trace = join(dir, 'trace2.jsonl');
  const log = join(dir, 'operations.log');
  const overrides = ['PATH', 'GIT_TRACE2_EVENT', 'GRAFT_E2E_GIT_LOG', 'GRAFT_E2E_REAL_GIT'];
  const original = Object.fromEntries(overrides.map(key => [key, ctx.env[key]]));
  const posix = process.platform !== 'win32';
  try {
    if (posix) {
      const resolved = ctx.command('/bin/sh', ['-c', 'command -v git']);
      assert.equal(resolved.status, 0, resolved.error?.message ?? resolved.stderr);
      const executable = resolved.stdout.trim();
      assert(isAbsolute(executable), 'Git must resolve to an absolute executable path');
      const wrapper = join(dir, 'git');
      writeFileSync(wrapper, gitProbe, { mode: 0o700 });
      ctx.env.GRAFT_E2E_GIT_LOG = log;
      ctx.env.GRAFT_E2E_REAL_GIT = realpathSync(executable);
      ctx.env.PATH = original.PATH ? `${dir}${delimiter}${original.PATH}` : dir;
    }
    ctx.env.GIT_TRACE2_EVENT = trace;
    const result = run();
    const events = readFileSync(trace, 'utf8').trim().split('\n').map(line => objectResult(JSON.parse(line)));
    const commands = events.filter(event => event.event === 'cmd_name');
    assert(commands.length > 0, 'Trace2 must capture Git subprocesses');
    assert(!commands.some(event => event.name === 'worktree'), 'Trace2 recorded an unexpected git worktree call');
    ctx.measure('trace2_git_commands', commands.length);
    ctx.measure('git_observation_scope', posix ? 'Trace2 and PATH Git operations' : 'Trace2-enabled Git operations');
    if (posix) {
      const operations = readFileSync(log, 'utf8').trim().split('\n');
      assert(!operations.includes('worktree'), 'Git probe rejected an unexpected git worktree call');
      if (expectObjectTransfer) {
        assert(operations.some(operation => ['pack-objects', 'index-pack', 'fetch-pack'].includes(operation)),
          'Git probe must capture the object transfer even when it clears Trace2 from its environment');
        ctx.measure('git_object_transfer_observed', true);
      }
    }
    return result;
  } finally {
    for (const [key, value] of Object.entries(original)) {
      if (value === undefined) delete ctx.env[key];
      else ctx.env[key] = value;
    }
    rmSync(dir, { recursive: true, force: true });
  }
}

export const cases: Scenario[] = [
  {
    name: 'checkout runs ordinary Git with private refs and a private index',
    run(ctx) {
      const base = ctx.git(['rev-parse', 'HEAD']);
      write(ctx.repo, 'tracked.txt', 'primary staged\n');
      ctx.git(['add', 'tracked.txt']);
      write(ctx.repo, 'tracked.txt', 'primary unstaged\n');
      write(ctx.repo, 'ignored/primary', 'primary ignored\n');

      // The transfer trace checks Git's selected-history export.
      const ws = traceCli(ctx, () => spawn(ctx, 'ordinary', ['--history', 'isolated']), true);
      traceCli(ctx, () => {
        const peer = spawn(ctx, 'peer', ['--branch', 'custom/peer']);
        assertRepository(ctx, ws.path);
        assertRepository(ctx, peer.path);
        assert.equal(ctx.git(['branch', '--show-current'], ws.path), 'graft/ordinary');
        assert.equal(ctx.git(['branch', '--show-current'], peer.path), 'custom/peer');
        assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), base);
        assert.equal(read(ws.path, 'tracked.txt'), 'initial\n');
        assert(!existsSync(join(ws.path, 'ignored', 'primary')));

        const tip = commit(ctx, ws.path, 'feature.txt', 'workspace feature\n');
        ctx.git(['branch', 'private-topic'], ws.path);
        ctx.git(['tag', 'private-tag'], ws.path);
        write(ws.path, 'tracked.txt', 'to stash\n');
        ctx.git(['stash', 'push', '-qm', 'private stash'], ws.path);
        write(ws.path, 'tracked.txt', 'workspace staged\n');
        ctx.git(['add', 'tracked.txt'], ws.path);
        write(ws.path, 'tracked.txt', 'workspace unstaged\n');
        assert.equal(ctx.git(['show', ':tracked.txt'], ws.path), 'workspace staged');
        assert.match(ctx.git(['status', '--porcelain'], ws.path), /^MM tracked\.txt$/m);
        assert.equal(ctx.git(['show', 'refs/stash:tracked.txt'], ws.path), 'to stash');
        for (const repo of [ctx.repo, peer.path]) {
          assert.equal(ctx.git(['for-each-ref', '--format=%(refname)',
            'refs/heads/private-topic', 'refs/tags/private-tag', 'refs/stash'], repo), '');
        }
        const status = ctx.cli(['status', ws.name]);
        assert.equal(status.head, tip);
        assert.equal(status.change_count, 1);
        assert.deepEqual(status.changes, [{ path: 'tracked.txt', status: 'MM' }]);
        const listing = ctx.cli(['list']);
        assert(Array.isArray(listing.workspaces));
        assert.deepEqual(listing.workspaces.map(ws => objectResult(ws).name).sort(), ['ordinary', 'peer']);
        assert(!JSON.stringify(listing).includes(ws.lease));

        const command = owned('exec', ws, '--', process.execPath, '-e',
          'process.stdout.write(JSON.stringify({cwd:process.cwd(),arg:process.argv[1],lease:process.env.GIT_GRAFT_LEASE}))',
          'literal ; $() & spaces');
        ctx.env.GIT_GRAFT_LEASE = ws.lease;
        const child = ctx.cli(command);
        delete ctx.env.GIT_GRAFT_LEASE;
        assert.equal(child.success, true);
        assert.equal(child.exit_code, 0);
        const output = objectResult(JSON.parse(String(child.stdout)));
        assert.equal(realpathSync(String(output.cwd)), realpathSync(ws.path));
        assert.equal(output.arg, 'literal ; $() & spaces');
        assert.equal(output.lease, undefined);
        const exit = ctx.fail(owned('exec', ws, '--', process.execPath, '-e', 'process.exit(17)'));
        assert.equal(exit.success, false);
        assert.equal(exit.exit_code, 17);
      });

      assert.equal(ctx.git(['rev-parse', 'HEAD']), base);
      assert.equal(ctx.git(['show', ':tracked.txt']), 'primary staged');
      assert.equal(read(ctx.repo, 'tracked.txt'), 'primary unstaged\n');
      assert.equal(read(ctx.repo, 'ignored/primary'), 'primary ignored\n');
      for (const path of [ctx.repo, ws.path]) ctx.git(['fsck', '--full'], path);
    },
  },
  {
    name: 'nested graft uses its private parent HEAD and isolates dirty bytes',
    run(ctx) {
      const file = 'nested/content.txt';
      const base = commit(ctx, ctx.repo, file, 'primary content\n');
      const parent = spawn(ctx, 'parent');
      const privateTip = commit(ctx, parent.path, file, 'private commit\n');
      write(parent.path, file, 'parent staged\n');
      ctx.git(['add', '--', file], parent.path);
      write(parent.path, file, 'parent unstaged\n');
      write(parent.path, 'nested/local.txt', 'parent untracked\n');

      const child = spawn(ctx, 'nested', [], join(parent.path, 'nested'), null);
      assertRepository(ctx, child.path);
      assert.equal(child.result.base, privateTip);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], child.path), privateTip);
      assert.equal(read(child.path, file), 'private commit\n');
      assert(!existsSync(join(child.path, 'nested', 'local.txt')));
      assert.equal(ctx.git(['status', '--porcelain'], child.path), '');

      const childTip = commit(ctx, child.path, file, 'child content\n');
      const missing = ctx.command('git', ['cat-file', '-e', `${privateTip}^{commit}`]);
      assert.ifError(missing.error);
      assert.notEqual(missing.status, 0, 'The private parent commit must stay out of the primary until land');
      assert.equal(ctx.git(['rev-parse', 'HEAD']), base);
      assert.equal(read(ctx.repo, file), 'primary content\n');
      assert.equal(read(parent.path, file), 'parent unstaged\n');

      const landed = ctx.cli(owned('land', child));
      assert.equal(landed.commit, childTip);
      assert.equal(ctx.git(['rev-parse', 'HEAD^']), privateTip);
      assert.equal(read(ctx.repo, file), 'child content\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], parent.path), privateTip);
      assert.equal(ctx.git(['show', `:${file}`], parent.path), 'parent staged');
      assert.equal(read(parent.path, file), 'parent unstaged\n');
      assert.equal(read(parent.path, 'nested/local.txt'), 'parent untracked\n');
      for (const path of [ctx.repo, parent.path]) ctx.git(['fsck', '--full'], path);
    },
  },
  {
    name: 'history survives primary pruning and workspace repacking',
    run(ctx) {
      const main = ctx.git(['rev-parse', 'HEAD']);
      const tree = ctx.git(['rev-parse', 'HEAD^{tree}']);
      const base = ctx.git(['commit-tree', tree, '-p', main, '-m', 'temporary primary history']);
      ctx.git(['branch', 'temporary', base]);
      const ws = spawn(ctx, 'maintenance', [], ctx.repo, 'temporary');
      const tip = commit(ctx, ws.path, 'private.txt', 'only in workspace\n');
      ctx.git(['tag', 'kept', tip], ws.path);
      // Drop the primary's only reference to the fetched base in this fixture.
      ctx.git(['update-ref', '-d', 'refs/heads/temporary', base]);
      ctx.git(['-c', 'gc.reflogExpire=now', '-c', 'gc.reflogExpireUnreachable=now', 'gc', '--prune=now']);
      assert.notEqual(ctx.command('git', ['cat-file', '-e', `${base}^{commit}`]).status, 0);
      ctx.git(['repack', '-ad'], ws.path);
      ctx.git(['fsck', '--full'], ws.path);
      assert.equal(ctx.git(['rev-parse', 'kept'], ws.path), tip);
      assert.equal(ctx.git(['rev-parse', 'HEAD^'], ws.path), base);
      assert.equal(ctx.git(['show', `${main}:tracked.txt`], ws.path), 'initial');
    },
  },
  {
    name: 'request retry, lock, move, repair, prune and restore keep repository identity',
    run(ctx) {
      const id = randomUUID();
      const options = ['--request-id', id, '--lock', '--reason', 'review in progress'];
      traceCli(ctx, () => {
        const ws = spawn(ctx, 'identity', options);
        assert.equal(ws.lease, id);
        assert.deepEqual(spawn(ctx, ws.name, options).result, ws.result);
        ctx.fail(['new', ws.name, '--base', 'main', ...options]);
        const moved = join(ctx.root, 'moved checkout');
        const repaired = join(ctx.root, 'manually moved checkout');
        const move = ['move', ws.name, moved, '--lease', ws.lease];
        const repair = ['repair', ws.name, repaired, '--lease', ws.lease];
        assert.equal(ctx.cli(['status', ws.name]).locked, 'review in progress');
        ctx.fail(move);
        ctx.fail(owned('prune', ws, '--discard'));
        ctx.fail(owned('land', ws));
        ctx.fail(owned('unlock', { ...ws, lease: randomUUID() }));
        assertRepository(ctx, ws.path);
        ctx.cli(owned('unlock', ws));
        assert.equal(ctx.cli(['status', ws.name]).locked, null);

        mkdirSync(moved);
        write(moved, 'foreign.txt', 'untouched');
        ctx.fail(move);
        assert.equal(read(moved, 'foreign.txt'), 'untouched');
        rmSync(moved, { recursive: true });
        ctx.cli(move);
        assert(!existsSync(ws.path));
        assert.equal(ctx.cli(['status', ws.name]).path, moved);
        assertRepository(ctx, moved);
        ctx.cli(owned('lock', ws, '--reason', 'packing up'));
        renameSync(moved, repaired);
        const wrongLease = [...repair.slice(0, -1), randomUUID()];
        ctx.fail(wrongLease);
        ctx.cli(repair);
        assert.equal(ctx.cli(['status', ws.name]).path, repaired);
        assertRepository(ctx, repaired);

        assert.equal(ctx.cli(['status', ws.name]).locked, 'packing up');
        ctx.cli(owned('unlock', ws));
        const drop = owned('prune', ws);
        const retired = ctx.cli(drop);
        assert.equal(retired.recovery_lease, ws.lease);
        assert(!existsSync(repaired));
        assert.deepEqual(ctx.cli(drop), retired);
        ctx.fail(['new', ws.name, '--base', 'HEAD', ...options]);
        const listing = ctx.cli(['list']);
        assert(Array.isArray(listing.recoverable));
        assert(listing.recoverable.some(workspace => objectResult(workspace).name === ws.name));
        assert(!JSON.stringify(listing).includes(ws.lease));

        mkdirSync(repaired);
        write(repaired, 'foreign.txt', 'occupied restore destination');
        ctx.fail(owned('restore', ws));
        assert.equal(read(repaired, 'foreign.txt'), 'occupied restore destination');
        rmSync(repaired, { recursive: true });
        const restored = ctx.cli(owned('restore', ws));
        assert.equal(restored.path, repaired);
        assert.equal(typeof restored.lease, 'string');
        assert.notEqual(restored.lease, ws.lease);
        assert.deepEqual(ctx.cli(owned('restore', ws)), restored);
        ctx.fail(owned('lock', ws));
        assertRepository(ctx, repaired);
      });
    },
  },
  {
    name: 'sparse checkout honors executable, symlink and ordinary file modes',
    run(ctx) {
      write(ctx.repo, 'selected/run.sh', '#!/bin/sh\nexit 0\n');
      write(ctx.repo, 'selected/text.txt', 'regular file\n');
      write(ctx.repo, 'omitted/skip.txt', 'outside the cone\n');
      const symlinks = ctx.command('git', ['config', '--bool', '--get', 'core.symlinks']);
      assert.ifError(symlinks.error);
      assert(symlinks.status === 0 || symlinks.status === 1, symlinks.stderr);
      const nativeLinks = symlinks.status === 0
        ? symlinks.stdout.trim() === 'true'
        : process.platform !== 'win32';
      if (nativeLinks) symlinkSync('text.txt', join(ctx.repo, 'selected', 'link'));
      ctx.git(['add', '-A']);
      ctx.git(['update-index', '--chmod=+x', 'selected/run.sh']);
      ctx.git(['commit', '-qm', 'sparse modes']);
      const ws = spawn(ctx, 'sparse', ['--sparse', 'selected']);
      assertRepository(ctx, ws.path);
      assert.equal(read(ws.path, 'selected/text.txt'), 'regular file\n');
      assert(!existsSync(join(ws.path, 'omitted', 'skip.txt')));
      assert.match(ctx.git(['ls-files', '--stage', 'selected/run.sh'], ws.path), /^100755 /);
      assert.match(ctx.git(['ls-files', '--stage', 'selected/text.txt'], ws.path), /^100644 /);
      if (process.platform !== 'win32') {
        assert(statSync(join(ws.path, 'selected/run.sh')).mode & 0o100);
        assert.equal(statSync(join(ws.path, 'selected/text.txt')).mode & 0o100, 0);
      }
      if (nativeLinks) {
        assert.match(ctx.git(['ls-files', '--stage', 'selected/link'], ws.path), /^120000 /);
        assert.equal(readlinkSync(join(ws.path, 'selected/link')), 'text.txt');
      }
      assert.equal(ctx.git(['sparse-checkout', 'list'], ws.path), 'selected');
      assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
    },
  },
  {
    name: 'no-checkout, detached and orphan starts use ordinary Git semantics',
    run(ctx) {
      const base = ctx.git(['rev-parse', 'HEAD']);
      const invalid = ctx.command(ctx.bin,
        ['--repo', ctx.repo, '--json', 'new', 'invalid', '--base', base, '--detach', '--branch', 'chosen']);
      assert.ifError(invalid.error);
      assert.notEqual(invalid.status, 0);
      assert.match(invalid.stderr, /cannot be used with/);

      const empty = spawn(ctx, 'empty', ['--no-checkout']);
      assertRepository(ctx, empty.path);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], empty.path), base);
      assert(!existsSync(join(empty.path, 'tracked.txt')));
      ctx.git(['restore', '--source=HEAD', '--staged', '--worktree', '--', '.'], empty.path);
      assert.equal(read(empty.path, 'tracked.txt'), 'initial\n');

      const detached = spawn(ctx, 'detached', ['--detach']);
      assert.equal(ctx.git(['branch', '--show-current'], detached.path), '');
      const detachedCommit = commit(ctx, detached.path, 'detached.txt', 'detached commit\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD^'], detached.path), base);
      assert.equal(ctx.cli(['status', detached.name]).head, detachedCommit);
      assert.equal(ctx.cli(owned('land', detached)).commit, detachedCommit);
      assert.equal(read(ctx.repo, 'detached.txt'), 'detached commit\n');
      ctx.cli(owned('restore', detached));
      assert.equal(ctx.git(['branch', '--show-current'], detached.path), '');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], detached.path), detachedCommit);

      const orphan = spawn(ctx, 'orphan', ['--orphan', 'new-root'], ctx.repo, null);
      assertRepository(ctx, orphan.path);
      assert.equal(ctx.git(['symbolic-ref', '--short', 'HEAD'], orphan.path), 'new-root');
      assert.notEqual(ctx.command('git', ['rev-parse', '--verify', 'HEAD'], orphan.path).status, 0);
      assert(!existsSync(join(orphan.path, 'tracked.txt')));
      const first = commit(ctx, orphan.path, 'root.txt', 'new root\n');
      assert.equal(ctx.git(['show', '-s', '--format=%P', first], orphan.path), '');
      assert.equal(ctx.git(['branch', '--list', 'new-root']), '');
    },
  },
  ...['sha1', 'sha256'].map((format): Scenario => ({
    name: `checkout and landing from a bare ${format} primary`,
    run(ctx) {
      const bare = join(ctx.root, 'bare primary.git');
      const source = join(ctx.root, `source-${format}`);
      const init = ctx.command('git', ['init', '-q', '-b', 'main', `--object-format=${format}`, source]);
      if (format === 'sha256' && init.status !== 0 && /unknown option|unsupported|unknown hash/i.test(init.stderr)) {
        ctx.unavailable(`Git does not support SHA-256: ${init.stderr.trim()}`);
      }
      assert.equal(init.status, 0, init.error?.message ?? init.stderr);
      const base = commit(ctx, source, 'base.txt', 'bare base\n');
      ctx.git(['clone', '-q', '--bare', source, bare]);
      const ws = spawn(ctx, 'bare', [], bare);
      assertRepository(ctx, ws.path);
      assert.equal(ctx.git(['rev-parse', '--show-object-format'], ws.path), format);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), base);
      assert.equal(read(ws.path, 'base.txt'), 'bare base\n');
      const tip = commit(ctx, ws.path, 'feature.txt', 'bare landing\n');
      assert.equal(ctx.cli(owned('land', ws), bare).commit, tip);
      assert.equal(ctx.git(['rev-parse', 'refs/heads/main'], bare), tip);
      assert.equal(ctx.git(['show', 'main:feature.txt'], bare), 'bare landing');
      const result = ctx.cli(owned('land', ws, '--undo'), bare);
      assert.equal(ctx.git(['rev-parse', 'refs/heads/main'], bare), result.commit);
      assert.equal(ctx.git(['rev-parse', 'main^{tree}'], bare), ctx.git(['rev-parse', `${base}^{tree}`], bare));
      ctx.git(['fsck', '--full'], bare);
    },
  })),
];
