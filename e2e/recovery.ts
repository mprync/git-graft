/** Exercise retirement and restore against a native Git object store and worktree. */
import assert from 'node:assert/strict';
import { chmodSync, existsSync, mkdirSync, readFileSync, readdirSync, rmdirSync, statSync, unlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { objectResult, type Context, type Scenario } from './harness.ts';

function drop(ctx: Context, name: string, lease: string) {
  return ctx.cli(['drop', name, '--lease', lease, '--discard']);
}

function restore(name: string, lease: string) {
  return ['restore', name, '--lease', lease];
}

function archives(ctx: Context) {
  const root = ctx.repo + '.graft';
  return readdirSync(root).filter(name => name.startsWith('.graft-recovery-')).map(name => join(root, name));
}

function pinnedRef(ctx: Context) {
  const refs = ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft/recovery']);
  const tip = refs.split('\n').find(ref => ref.endsWith('/tip'));
  assert(tip);
  return tip;
}

function history(ctx: Context, worktree: string, text: string) {
  writeFileSync(join(worktree, 'tracked.txt'), text);
  ctx.git(['add', 'tracked.txt'], worktree);
  ctx.git(['commit', '-qm', text], worktree);
  return ctx.git(['rev-parse', 'HEAD'], worktree);
}

export const cases: Scenario[] = [
  {
    name: 'destructive drop restores the exact index and local files after Git GC',
    run(ctx) {
      const name = 'dirty';
      const ws = ctx.spawn(name);
      const droppedHistory = history(ctx, ws.path, 'work from an earlier branch tip');
      ctx.git(['reset', '--hard', 'HEAD^'], ws.path);
      const tip = history(ctx, ws.path, 'unmerged commit');
      const runtime = String(ctx.cli(['env', name]).runtime_dir);
      mkdirSync(runtime);
      writeFileSync(join(runtime, 'private.txt'), 'private runtime');
      const tracked = join(ws.path, 'tracked.txt');
      writeFileSync(tracked, 'staged bytes');
      ctx.git(['add', 'tracked.txt'], ws.path);
      writeFileSync(tracked, 'unstaged bytes');
      writeFileSync(join(ws.path, 'untracked.txt'), 'untracked bytes');
      mkdirSync(join(ws.path, 'ignored'));
      writeFileSync(join(ws.path, 'ignored', 'secret.txt'), 'ignored bytes');
      mkdirSync(join(ws.path, 'empty'));
      const previous = ctx.git(['status', '--porcelain', '--ignored'], ws.path);
      const retired = drop(ctx, name, ws.lease);
      assert.equal(retired.recovery_lease, ws.lease);
      assert.equal(existsSync(ws.path), false);
      assert.equal(existsSync(runtime), false);
      assert.ok(!ctx.git(['worktree', 'list', '--porcelain']).includes(ws.lease));
      assert.ok(!ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft/recovery']).includes(ws.lease));
      assert.deepEqual(drop(ctx, name, ws.lease), retired);

      ctx.git(['-c', 'gc.reflogExpire=now', '-c', 'gc.reflogExpireUnreachable=now', 'gc', '--prune=now']);
      ctx.git(['worktree', 'prune', '--expire', 'now']);
      const receipt = join(ctx.repo, '.git', 'graft', 'completed', ws.lease + '.json');
      const expired = objectResult(JSON.parse(readFileSync(receipt, 'utf8')));
      expired.at = 0;
      writeFileSync(receipt, JSON.stringify(expired));
      ctx.cli(['gc', '--apply']);
      assert.equal(existsSync(receipt), false);
      ctx.fail(['spawn', name, '--request-id', ws.lease]);

      const response = ctx.cli(restore(name, ws.lease));
      assert.equal(response.worktree_path, ws.path);
      assert.equal(response.name, name);
      assert.equal(typeof response.lease, 'string');
      assert.notEqual(response.lease, ws.lease);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip);
      assert.equal(ctx.git(['branch', '--show-current'], ws.path), 'graft/dirty');
      assert.equal(ctx.git(['cat-file', '-t', droppedHistory]), 'commit');
      assert.equal(ctx.git(['show', ':tracked.txt'], ws.path), 'staged bytes');
      assert.equal(ctx.git(['status', '--porcelain', '--ignored'], ws.path), previous);
      assert.equal(readFileSync(tracked, 'utf8'), 'unstaged bytes');
      assert.equal(readFileSync(join(ws.path, 'untracked.txt'), 'utf8'), 'untracked bytes');
      assert.equal(readFileSync(join(ws.path, 'ignored', 'secret.txt'), 'utf8'), 'ignored bytes');
      assert.equal(readFileSync(join(runtime, 'private.txt'), 'utf8'), 'private runtime');
      assert.ok(existsSync(join(ws.path, 'empty')));
      assert.deepEqual(ctx.cli(restore(name, ws.lease)), response);
      ctx.fail(['heartbeat', name, '--lease', ws.lease]);
      ctx.cli(['heartbeat', name, '--lease', String(response.lease)]);
    },
  },
  {
    name: 'clean checkout restores from preserved data and keeps its unmerged commit',
    run(ctx) {
      const name = 'clean';
      const ws = ctx.spawn(name);
      const tip = history(ctx, ws.path, 'clean but unmerged');
      drop(ctx, name, ws.lease);
      // Keep the original Windows tree until ACLs and alternate streams can
      // be ruled out. Ordinary POSIX files are reproducible from Git blobs.
      assert.equal(archives(ctx).length, process.platform === 'win32' ? 1 : 0);
      assert.equal(ctx.git(['branch', '--list', 'graft/clean']), '');
      const reference = pinnedRef(ctx);
      assert.equal(ctx.git(['rev-parse', reference]), tip);
      ctx.git(['gc', '--prune=now']);
      if (process.platform !== 'win32') {
        const id = reference.split('/').at(-2);
        const partial = join(ctx.repo + '.graft', '.graft-rebuild-' + id + '-0');
        // Git can register the staging checkout before writing its index or files.
        ctx.git(['worktree', 'add', '--detach', '--no-checkout', '--lock',
          '--reason', 'git-graft recovery ' + id, '--', partial, tip]);
        assert.equal(existsSync(join(partial, 'tracked.txt')), false);
      }
      ctx.cli(restore(name, ws.lease));
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip);
      assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
      assert.equal(readFileSync(join(ws.path, 'tracked.txt'), 'utf8'), 'clean but unmerged');
    },
  },
  {
    name: 'a pending retirement and restoration replay after independent Git ref locks',
    run(ctx) {
      const name = 'retry';
      const ws = ctx.spawn(name);
      writeFileSync(join(ws.path, 'local.txt'), 'preserve');
      const branchLock = join(ctx.repo, '.git', 'refs', 'heads', 'graft', name + '.lock');
      writeFileSync(branchLock, '');
      const command = ['drop', name, '--lease', ws.lease, '--discard'];
      ctx.fail(command);
      assert.equal(readFileSync(join(archives(ctx)[0], 'local.txt'), 'utf8'), 'preserve');
      unlinkSync(branchLock);
      const retired = ctx.cli(command);
      assert.deepEqual(ctx.cli(command), retired);
      const restoreLock = join(ctx.repo, '.git', ...pinnedRef(ctx).replace(/tip$/, 'restoring.lock').split('/'));
      writeFileSync(restoreLock, '');
      const undo = restore(name, ws.lease);
      ctx.fail(undo);
      assert.equal(readFileSync(join(ws.path, 'local.txt'), 'utf8'), 'preserve');
      unlinkSync(restoreLock);
      const restored = ctx.cli(undo);
      assert.deepEqual(ctx.cli(undo), restored);
      assert.equal(readFileSync(join(ws.path, 'local.txt'), 'utf8'), 'preserve');
    },
  },
  {
    name: 'restore preserves occupied paths, branch refs, and reused workspace names',
    run(ctx) {
      const name = 'occupied';
      const original = ctx.spawn(name);
      writeFileSync(join(original.path, 'retired.txt'), 'original');
      drop(ctx, name, original.lease);
      const command = restore(name, original.lease);

      mkdirSync(original.path);
      ctx.fail(command);
      assert.ok(existsSync(original.path));
      rmdirSync(original.path);

      const main = ctx.git(['rev-parse', 'main']);
      ctx.git(['branch', 'graft/occupied', main]);
      ctx.fail(command);
      assert.equal(ctx.git(['rev-parse', 'graft/occupied']), main);
      ctx.git(['branch', '-d', 'graft/occupied']);

      const next = ctx.spawn(name);
      writeFileSync(join(next.path, 'active.txt'), 'active');
      ctx.fail(command);
      assert.equal(readFileSync(join(next.path, 'active.txt'), 'utf8'), 'active');
      assert.equal(readFileSync(join(archives(ctx)[0], 'retired.txt'), 'utf8'), 'original');
      drop(ctx, name, next.lease);
      ctx.cli(command);
      assert.equal(readFileSync(join(original.path, 'retired.txt'), 'utf8'), 'original');
      assert.equal(existsSync(join(original.path, 'active.txt')), false);
    },
  },
  {
    name: 'a same-tip symbolic workspace ref is rejected and preserved',
    run(ctx) {
      const name = 'symbolic';
      const ws = ctx.spawn(name);
      ctx.git(['symbolic-ref', 'refs/heads/graft/' + name, 'refs/heads/main']);
      ctx.fail(['drop', name, '--lease', ws.lease, '--discard']);
      assert.ok(existsSync(ws.path));
      assert.equal(ctx.git(['symbolic-ref', 'refs/heads/graft/' + name]), 'refs/heads/main');
    },
  },
  {
    name: 'filesystem metadata and sparse checkout survive retirement',
    run(ctx) {
      mkdirSync(join(ctx.repo, 'inside'));
      mkdirSync(join(ctx.repo, 'outside'));
      writeFileSync(join(ctx.repo, 'inside', 'work.txt'), 'inside');
      writeFileSync(join(ctx.repo, 'outside', 'work.txt'), 'outside');
      ctx.git(['add', 'inside', 'outside']);
      ctx.git(['commit', '-qm', 'sparse fixture']);
      const ws = ctx.spawn('sparse', ['--sparse', 'inside']);
      const file = join(ws.path, 'inside', 'work.txt');
      if (process.platform !== 'win32') chmodSync(file, 0o600);
      assert.equal(existsSync(join(ws.path, 'outside', 'work.txt')), false);
      drop(ctx, 'sparse', ws.lease);
      ctx.cli(restore('sparse', ws.lease));
      assert.equal(ctx.git(['sparse-checkout', 'list'], ws.path), 'inside');
      assert.equal(existsSync(join(ws.path, 'outside', 'work.txt')), false);
      assert.equal(readFileSync(file, 'utf8'), 'inside');
      if (process.platform !== 'win32') assert.equal(statSync(file).mode & 0o777, 0o600);

      const ordinary = ctx.spawn('mode');
      const tracked = join(ordinary.path, 'tracked.txt');
      if (process.platform !== 'win32') chmodSync(tracked, 0o600);
      drop(ctx, 'mode', ordinary.lease);
      ctx.cli(restore('mode', ordinary.lease));
      if (process.platform !== 'win32') assert.equal(statSync(tracked).mode & 0o777, 0o600);
    },
  },
];
