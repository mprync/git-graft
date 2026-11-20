/** Exercise retirement and restore with complete standalone Git repositories. */
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, rmdirSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { objectResult, parseExact, type Context, type Scenario } from './harness.ts';

function drop(ctx: Context, name: string, lease: string) {
  return ctx.cli(['drop', name, '--lease', lease, '--discard']);
}

function restore(name: string, lease: string) {
  return ['restore', name, '--lease', lease];
}

function recordPath(ctx: Context, id: unknown): string {
  assert(typeof id === 'string' && /^[a-f0-9-]{36}$/i.test(id));
  return join(ctx.repo, '.git', 'graft', 'workspaces', 'records', `${id}.json`);
}

function archivePath(ctx: Context, id: unknown): string {
  const { recoverable } = ctx.cli(['list']);
  assert(Array.isArray(recoverable));
  const entry = recoverable.map(objectResult).find(row => row.id === id);
  assert(entry && typeof entry.archive_path === 'string');
  return entry.archive_path;
}

function history(ctx: Context, path: string, text: string) {
  writeFileSync(join(path, 'tracked.txt'), text);
  ctx.git(['add', 'tracked.txt'], path);
  ctx.git(['commit', '-qm', text], path);
  return ctx.git(['rev-parse', 'HEAD'], path);
}

export const cases: Scenario[] = [
  {
    name: 'destructive drop restores the exact index and local files after Git GC',
    run(ctx) {
      const name = 'dirty';
      const ws = ctx.spawn(name);
      const branch = 'refs/heads/graft/' + name;
      const base = ctx.git(['rev-parse', branch], ws.path);
      const branchHistory = ctx.git(['commit-tree', base + '^{tree}', '-p', base, '-m', 'branch reflog only'], ws.path);
      ctx.git(['update-ref', branch, branchHistory, base], ws.path);
      ctx.git(['update-ref', branch, base, branchHistory], ws.path);
      ctx.git(['switch', '--detach', base], ws.path);
      const headHistory = history(ctx, ws.path, 'HEAD reflog only');
      ctx.git(['switch', 'graft/' + name], ws.path);
      const tip = history(ctx, ws.path, 'unmerged commit');
      ctx.git(['tag', 'private-tag', tip], ws.path);
      ctx.git(['branch', 'private-branch', tip], ws.path);
      const env = ctx.cli(['env', name]);
      assert.equal(env.path, ws.path);
      assert(typeof env.runtime_dir === 'string');
      const runtime = join(env.runtime_dir, 'private.txt');
      ctx.cli(['exec', name, '--lease', ws.lease, '--', process.execPath, '-e', '']);
      writeFileSync(runtime, 'private runtime');
      writeFileSync(join(ws.path, 'tracked.txt'), 'stashed bytes');
      ctx.git(['stash', 'push', '-qm', 'private stash'], ws.path);
      const tracked = join(ws.path, 'tracked.txt');
      writeFileSync(tracked, 'staged bytes');
      ctx.git(['add', 'tracked.txt'], ws.path);
      writeFileSync(tracked, 'unstaged bytes');
      writeFileSync(join(ws.path, 'untracked.txt'), 'untracked bytes');
      mkdirSync(join(ws.path, 'ignored'));
      writeFileSync(join(ws.path, 'ignored', 'secret.txt'), 'ignored bytes');
      mkdirSync(join(ws.path, 'empty'));
      const previous = ctx.git(['status', '--porcelain', '--ignored'], ws.path);
      const index = readFileSync(join(ws.path, '.git', 'index'));
      const retired = drop(ctx, name, ws.lease);
      assert.equal(retired.recovery_lease, ws.lease);
      assert.equal(existsSync(ws.path), false);
      const archive = archivePath(ctx, ws.result.id);
      assert.deepEqual(readFileSync(join(archive, '.git', 'index')), index);
      assert.deepEqual(drop(ctx, name, ws.lease), retired);

      ctx.git(['-c', 'gc.reflogExpire=now', '-c', 'gc.reflogExpireUnreachable=now', 'gc', '--prune=now']);
      ctx.git(['fsck', '--full']);
      assert.notEqual(ctx.command('git', ['cat-file', '-e', `${tip}^{commit}`]).status, 0);
      ctx.cli(['gc', '--apply']);
      ctx.fail(['spawn', name, '--request-id', ws.lease]);

      const response = ctx.cli(restore(name, ws.lease));
      assert.equal(response.path, ws.path);
      assert.equal(response.name, name);
      assert.equal(typeof response.lease, 'string');
      assert.notEqual(response.lease, ws.lease);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip);
      assert.equal(ctx.git(['branch', '--show-current'], ws.path), 'graft/dirty');
      assert.deepEqual(readFileSync(join(ws.path, '.git', 'index')), index);
      for (const [ref, oid] of [['graft/dirty', branchHistory], ['HEAD', headHistory]]) {
        assert.ok(ctx.git(['reflog', 'show', '--format=%H', ref], ws.path).split('\n').includes(oid));
        assert.equal(ctx.git(['cat-file', '-t', oid], ws.path), 'commit');
      }
      assert.equal(ctx.git(['rev-parse', 'private-tag', 'private-branch'], ws.path), [tip, tip].join('\n'));
      assert.equal(ctx.git(['show', 'refs/stash:tracked.txt'], ws.path), 'stashed bytes');
      ctx.git(['fsck', '--full'], ws.path);
      assert.equal(ctx.git(['show', ':tracked.txt'], ws.path), 'staged bytes');
      assert.equal(ctx.git(['status', '--porcelain', '--ignored'], ws.path), previous);
      assert.equal(readFileSync(tracked, 'utf8'), 'unstaged bytes');
      assert.equal(readFileSync(join(ws.path, 'untracked.txt'), 'utf8'), 'untracked bytes');
      assert.equal(readFileSync(join(ws.path, 'ignored', 'secret.txt'), 'utf8'), 'ignored bytes');
      assert.equal(ctx.cli(['env', name]).runtime_dir, env.runtime_dir);
      assert.equal(readFileSync(runtime, 'utf8'), 'private runtime');
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
      const archive = archivePath(ctx, ws.result.id);
      ctx.git(['fsck', '--full'], archive);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], archive), tip);
      assert.equal(ctx.git(['branch', '--list', 'graft/clean']), '');
      ctx.git(['gc', '--prune=now']);
      ctx.cli(restore(name, ws.lease));
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip);
      assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
      assert.equal(readFileSync(join(ws.path, 'tracked.txt'), 'utf8'), 'clean but unmerged');
    },
  },
  {
    name: 'drop and restore replay after interrupted filesystem transitions',
    run(ctx) {
      const seed = ctx.spawn('archive-root');
      drop(ctx, 'archive-root', seed.lease);
      const ws = ctx.spawn('retry');
      const id = ws.result.id;
      const path = recordPath(ctx, id);
      const archive = join(dirname(archivePath(ctx, seed.result.id)), String(id));
      writeFileSync(join(ws.path, 'local.txt'), 'preserve');
      const active = objectResult(parseExact(readFileSync(path, 'utf8')));
      const command = ['drop', 'retry', '--lease', ws.lease, '--discard'];
      writeFileSync(path, JSON.stringify({
        ...active, archive, transition: { action: 'retire', status: 'dropped' },
      }));
      mkdirSync(archive);
      writeFileSync(join(archive, 'foreign.txt'), 'foreign archive');
      assert.match(String(ctx.fail(command).message), /occupied/i);
      assert.equal(readFileSync(join(ws.path, 'local.txt'), 'utf8'), 'preserve');
      assert.equal(readFileSync(join(archive, 'foreign.txt'), 'utf8'), 'foreign archive');
      renameSync(archive, join(ctx.root, 'foreign archive'));
      renameSync(ws.path, archive);

      mkdirSync(ws.path);
      writeFileSync(join(ws.path, 'foreign.txt'), 'foreign endpoint');
      const retired = ctx.cli(command);
      assert.equal(retired.recovery_lease, ws.lease);
      assert.deepEqual(ctx.cli(command), retired);
      const undo = restore('retry', ws.lease);
      assert.equal(ctx.fail(undo).code, 'already_exists');
      assert.equal(readFileSync(join(ws.path, 'foreign.txt'), 'utf8'), 'foreign endpoint');
      assert.equal(readFileSync(join(archive, 'local.txt'), 'utf8'), 'preserve');
      renameSync(ws.path, join(ctx.root, 'foreign endpoint'));
      ctx.git(['fsck', '--full'], archive);

      const dropped = objectResult(parseExact(readFileSync(path, 'utf8')));
      const lease = randomUUID();
      writeFileSync(path, JSON.stringify({
        ...dropped, transition: { action: 'restore', lease, port: ws.result.port },
      }));
      renameSync(archive, ws.path);

      const restored = ctx.cli(undo);
      assert.equal(restored.lease, lease);
      assert.equal(restored.path, ws.path);
      assert.deepEqual(ctx.cli(undo), restored);
      assert.equal(ctx.fail(['heartbeat', 'retry', '--lease', ws.lease]).code, 'lease_mismatch');
      ctx.cli(['heartbeat', 'retry', '--lease', lease]);
      assert.equal(readFileSync(join(ws.path, 'local.txt'), 'utf8'), 'preserve');
      ctx.git(['fsck', '--full'], ws.path);
    },
  },
  {
    name: 'landing with ignored files keeps the original checkout for restore',
    run(ctx) {
      const name = 'ignored-land';
      const ws = ctx.spawn(name);
      const tip = history(ctx, ws.path, 'landed work');
      mkdirSync(join(ws.path, 'ignored'));
      writeFileSync(join(ws.path, 'ignored', 'local.txt'), 'ignored work');
      const args = ['land', name, '--lease', ws.lease];
      assert.match(String(ctx.fail(args).message), /ignored files/i);
      assert.equal(ctx.git(['rev-parse', 'main']), ctx.git(['rev-parse', tip + '^'], ws.path));
      const landed = ctx.cli([...args, '--discard-ignored']);
      assert.equal(landed.commit, tip);
      assert.equal(ctx.git(['rev-parse', 'main']), tip);
      assert.equal(existsSync(ws.path), false);
      assert.equal(readFileSync(join(archivePath(ctx, ws.result.id), 'ignored', 'local.txt'), 'utf8'), 'ignored work');
      ctx.cli(restore(name, ws.lease));
      assert.equal(readFileSync(join(ws.path, 'ignored', 'local.txt'), 'utf8'), 'ignored work');
    },
  },
  {
    name: 'restore preserves occupied paths, unrelated primary refs, and reused names',
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
      assert.equal(ctx.git(['rev-parse', 'graft/occupied']), main);

      const next = ctx.spawn(name);
      writeFileSync(join(next.path, 'active.txt'), 'active');
      ctx.fail(command);
      assert.equal(readFileSync(join(next.path, 'active.txt'), 'utf8'), 'active');
      assert.equal(readFileSync(join(archivePath(ctx, original.result.id), 'retired.txt'), 'utf8'), 'original');
      drop(ctx, name, next.lease);
      ctx.cli(command);
      assert.equal(readFileSync(join(original.path, 'retired.txt'), 'utf8'), 'original');
      assert.equal(existsSync(join(original.path, 'active.txt')), false);
      assert.equal(ctx.git(['rev-parse', 'graft/occupied']), main);
      ctx.git(['fsck', '--full'], original.path);
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
