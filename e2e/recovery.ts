/** Exercise retirement and restore with complete standalone Git repositories. */
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { chmodSync, existsSync, lstatSync, mkdirSync, readFileSync, readdirSync, readlinkSync, renameSync, rmdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { objectResult, parseExact, type Context, type Scenario } from './harness.ts';

function drop(ctx: Context, name: string, lease: string) {
  return ctx.cli(['remove', name, '--lease', lease, '--discard']);
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

/** Every entry under `root` with its type, mode and content hash, to prove nothing changed. */
function snapshot(root: string): string[] {
  return readdirSync(root, { recursive: true }).map(String).sort().map(name => {
    const path = join(root, name);
    const entry = lstatSync(path);
    const mode = process.platform === 'win32' ? '' : (entry.mode & 0o777).toString(8);
    if (entry.isSymbolicLink()) return `${name} link ${readlinkSync(path)}`;
    if (!entry.isFile()) return `${name} dir ${mode}`;
    return `${name} file ${mode} ${createHash('sha256').update(readFileSync(path)).digest('hex')}`;
  });
}

/** Run the CLI without waiting, so two invocations can overlap. */
function cliAsync(ctx: Context, args: string[]): Promise<{ status: number | null; stdout: string; stderr: string }> {
  ctx.coverCli(args[0]);
  return new Promise((resolve, reject) => {
    const child = spawn(ctx.bin, ['--repo', ctx.repo, '--json', ...args], { cwd: ctx.repo, env: ctx.env, windowsHide: true });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', chunk => { stdout += String(chunk); });
    child.stderr.on('data', chunk => { stderr += String(chunk); });
    child.on('error', reject);
    child.on('close', status => resolve({ status, stdout, stderr }));
  });
}

function history(ctx: Context, path: string, text: string) {
  writeFileSync(join(path, 'tracked.txt'), text);
  ctx.git(['add', 'tracked.txt'], path);
  ctx.git(['commit', '-qm', text], path);
  return ctx.git(['rev-parse', 'HEAD'], path);
}

export const cases: Scenario[] = [
  {
    name: 'destructive prune restores the exact index and local files after Git GC',
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
      const env = ctx.cli(['status', name, '--env']);
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
      ctx.cli(['prune', '--apply']);
      ctx.fail(['add', name, '--request-id', ws.lease]);

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
      assert.equal(ctx.cli(['status', name, '--env']).runtime_dir, env.runtime_dir);
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
    name: 'prune and restore replay after interrupted filesystem transitions',
    run(ctx) {
      const seed = ctx.spawn('archive-root');
      drop(ctx, 'archive-root', seed.lease);
      const ws = ctx.spawn('retry');
      const id = ws.result.id;
      const path = recordPath(ctx, id);
      const archive = join(dirname(archivePath(ctx, seed.result.id)), String(id));
      writeFileSync(join(ws.path, 'local.txt'), 'preserve');
      const active = objectResult(parseExact(readFileSync(path, 'utf8')));
      const command = ['remove', 'retry', '--lease', ws.lease, '--discard'];
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
      const args = ['merge', name, '--lease', ws.lease];
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

  {
    name: 'restore refuses an active name and a damaged archive without deleting anything',
    run(ctx) {
      const original = ctx.spawn('taken');
      writeFileSync(join(original.path, 'retired.txt'), 'retired work');
      drop(ctx, 'taken', original.lease);
      const archive = archivePath(ctx, original.result.id);
      const archived = snapshot(archive);
      const next = ctx.spawn('taken');
      writeFileSync(join(next.path, 'active.txt'), 'active work');
      const active = snapshot(next.path);
      const taken = ctx.fail(restore('taken', original.lease));
      assert.equal(taken.code, 'already_exists');
      assert.equal(objectResult(taken.details).archive_path, archive);
      assert.deepEqual(snapshot(next.path), active);
      assert.deepEqual(snapshot(archive), archived);

      // Hand-deleted work tree files: restore returns what is left and deletes nothing else.
      const partial = ctx.spawn('partial');
      for (const file of ['kept.txt', 'deleted.txt']) writeFileSync(join(partial.path, file), file);
      drop(ctx, 'partial', partial.lease);
      const partialArchive = archivePath(ctx, partial.result.id);
      rmSync(join(partialArchive, 'deleted.txt'));
      const left = snapshot(partialArchive);
      ctx.cli(restore('partial', partial.lease));
      assert.deepEqual(snapshot(partial.path), left);
      assert.equal(readFileSync(join(partial.path, 'kept.txt'), 'utf8'), 'kept.txt');
      assert.equal(existsSync(partialArchive), false);

      // Damaged Git metadata: a clear error, and the archive stays exactly as found.
      const damaged = ctx.spawn('damaged');
      writeFileSync(join(damaged.path, 'work.txt'), 'unsaved work');
      const tip = history(ctx, damaged.path, 'damaged archive commit');
      drop(ctx, 'damaged', damaged.lease);
      const damagedArchive = archivePath(ctx, damaged.result.id);
      const head = readFileSync(join(damagedArchive, '.git', 'HEAD'));
      rmSync(join(damagedArchive, '.git', 'HEAD'));
      const remaining = snapshot(damagedArchive);
      for (let attempt = 0; attempt < 2; attempt++) {
        const failure = ctx.fail(restore('damaged', damaged.lease));
        assert.equal(failure.code, 'archive_damaged');
        assert.match(String(failure.message), /missing or damaged/);
        assert.equal(objectResult(failure.details).archive_path, damagedArchive);
        assert.deepEqual(snapshot(damagedArchive), remaining);
        assert.equal(existsSync(damaged.path), false);
      }
      writeFileSync(join(damagedArchive, '.git', 'HEAD'), head);
      ctx.cli(restore('damaged', damaged.lease));
      assert.equal(ctx.git(['rev-parse', 'HEAD'], damaged.path), tip);
      assert.equal(readFileSync(join(damaged.path, 'work.txt'), 'utf8'), 'unsaved work');

      // A deleted archive keeps its record, so the failure names where it was.
      const gone = ctx.spawn('gone');
      drop(ctx, 'gone', gone.lease);
      const goneArchive = archivePath(ctx, gone.result.id);
      rmSync(goneArchive, { recursive: true, force: true });
      const missing = ctx.fail(restore('gone', gone.lease));
      assert.equal(missing.code, 'archive_damaged');
      assert.equal(objectResult(missing.details).archive_path, goneArchive);
      assert.equal(archivePath(ctx, gone.result.id), goneArchive);
      assert.equal(existsSync(gone.path), false);
    },
  },
  {
    name: 'restore after the primary branch moved keeps the graft commit and merges both lines',
    run(ctx) {
      const ws = ctx.spawn('behind');
      writeFileSync(join(ws.path, 'graft.txt'), 'graft line');
      ctx.git(['add', 'graft.txt'], ws.path);
      ctx.git(['commit', '-qm', 'graft line'], ws.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], ws.path);
      writeFileSync(join(ws.path, 'untracked.txt'), 'untracked');
      drop(ctx, 'behind', ws.lease);
      writeFileSync(join(ctx.repo, 'primary.txt'), 'primary line');
      ctx.git(['add', 'primary.txt']);
      ctx.git(['commit', '-qm', 'primary moved on']);
      const main = ctx.git(['rev-parse', 'main']);
      const restored = ctx.cli(restore('behind', ws.lease));
      assert.equal(ctx.git(['rev-parse', 'main']), main);
      assert.equal(ctx.git(['status', '--porcelain']), '');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip);
      assert.equal(existsSync(join(ws.path, 'primary.txt')), false);
      assert.equal(readFileSync(join(ws.path, 'untracked.txt'), 'utf8'), 'untracked');
      rmSync(join(ws.path, 'untracked.txt'));
      const merged = ctx.cli(['merge', 'behind', '--lease', String(restored.lease)]);
      assert.equal(merged.action, 'merge');
      assert.equal(ctx.git(['show', 'main:graft.txt']), 'graft line');
      assert.equal(ctx.git(['show', 'main:primary.txt']), 'primary line');
      ctx.git(['merge-base', '--is-ancestor', main, 'main']);
      ctx.git(['merge-base', '--is-ancestor', tip, 'main']);
    },
  },
  {
    name: 'two concurrent restores of one archive restore it exactly once',
    async run(ctx) {
      const ws = ctx.spawn('race');
      writeFileSync(join(ws.path, 'work.txt'), 'race work');
      drop(ctx, 'race', ws.lease);
      const archived = snapshot(archivePath(ctx, ws.result.id));
      const results = await Promise.all([0, 1].map(() => cliAsync(ctx, restore('race', ws.lease))));
      const receipts = results.map(result => {
        assert.equal(result.status, 0, result.stderr || result.stdout);
        return objectResult(JSON.parse(result.stdout));
      });
      assert.deepEqual(receipts[0], receipts[1]);
      assert.equal(receipts[0].path, ws.path);
      assert.deepEqual(snapshot(ws.path), archived);
      ctx.cli(['heartbeat', 'race', '--lease', String(receipts[0].lease)]);
      const { workspaces } = ctx.cli(['list']);
      assert(Array.isArray(workspaces));
      assert.equal(workspaces.map(objectResult).filter(row => row.name === 'race').length, 1);
    },
  },
  {
    name: 'archives survive prune, cache purge and aggressive primary garbage collection',
    run(ctx) {
      const ws = ctx.spawn('survivor');
      const tip = history(ctx, ws.path, 'unmerged survivor');
      ctx.cli(['exec', 'survivor', '--lease', ws.lease, '--', process.execPath, '-e', '']);
      writeFileSync(join(ws.path, 'untracked.txt'), 'untracked');
      mkdirSync(join(ws.path, 'ignored'));
      writeFileSync(join(ws.path, 'ignored', 'local.txt'), 'ignored');
      drop(ctx, 'survivor', ws.lease);
      const archive = archivePath(ctx, ws.result.id);
      const archived = snapshot(archive);
      ctx.git(['-c', 'gc.reflogExpire=now', '-c', 'gc.reflogExpireUnreachable=now', 'gc', '--aggressive', '--prune=now']);
      const purged = ctx.cli(['prune', '--apply', '--purge-cache']);
      assert.equal(objectResult(purged.cache).purged, true);
      ctx.cli(['prune', '--apply']);
      assert.deepEqual(snapshot(archive), archived);
      ctx.cli(restore('survivor', ws.lease));
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip);
      assert.equal(readFileSync(join(ws.path, 'untracked.txt'), 'utf8'), 'untracked');
      assert.equal(readFileSync(join(ws.path, 'ignored', 'local.txt'), 'utf8'), 'ignored');
      ctx.git(['fsck', '--full'], ws.path);
    },
  },
  {
    name: 'each recovery lease restores only its own archive',
    run(ctx) {
      const first = ctx.spawn('same');
      writeFileSync(join(first.path, 'owner.txt'), 'first');
      drop(ctx, 'same', first.lease);
      const second = ctx.spawn('same');
      writeFileSync(join(second.path, 'owner.txt'), 'second');
      drop(ctx, 'same', second.lease);
      const other = ctx.spawn('other');
      writeFileSync(join(other.path, 'owner.txt'), 'other');
      drop(ctx, 'other', other.lease);
      const archives = [first, second, other].map(ws => archivePath(ctx, ws.result.id));
      const before = archives.map(snapshot);
      for (const [name, lease] of [['same', other.lease], ['same', randomUUID()], ['other', first.lease], ['other', second.lease]]) {
        assert.equal(ctx.fail(restore(name, lease)).code, 'lease_mismatch');
      }
      assert.deepEqual(archives.map(snapshot), before);
      assert.equal(existsSync(first.path), false);

      const reopened = ctx.cli(restore('same', second.lease));
      assert.equal(readFileSync(join(second.path, 'owner.txt'), 'utf8'), 'second');
      assert.deepEqual(snapshot(archives[0]), before[0]);
      assert.equal(ctx.fail(restore('same', first.lease)).code, 'already_exists');
      assert.equal(readFileSync(join(second.path, 'owner.txt'), 'utf8'), 'second');
      assert.deepEqual(snapshot(archives[0]), before[0]);

      drop(ctx, 'same', String(reopened.lease));
      ctx.cli(restore('same', first.lease));
      assert.equal(readFileSync(join(first.path, 'owner.txt'), 'utf8'), 'first');
      assert.deepEqual(snapshot(archives[1]), before[1]);
      assert.deepEqual(snapshot(archives[2]), before[2]);
      ctx.cli(restore('other', other.lease));
      assert.equal(readFileSync(join(other.path, 'owner.txt'), 'utf8'), 'other');
    },
  },
  {
    name: 'archives keep ignored credentials in owner-only directories',
    run(ctx) {
      const ws = ctx.spawn('secrets');
      mkdirSync(join(ws.path, '.git', 'info'), { recursive: true });
      writeFileSync(join(ws.path, '.git', 'info', 'exclude'), '.env\n');
      const secret = join(ws.path, '.env');
      writeFileSync(secret, 'TOKEN=private\n');
      if (process.platform !== 'win32') chmodSync(secret, 0o600);
      assert.equal(ctx.git(['status', '--porcelain', '--ignored', '.env'], ws.path), '!! .env');
      drop(ctx, 'secrets', ws.lease);
      const archive = archivePath(ctx, ws.result.id);
      assert.equal(readFileSync(join(archive, '.env'), 'utf8'), 'TOKEN=private\n');
      if (process.platform !== 'win32') {
        for (const path of [dirname(archive), archive]) assert.equal(statSync(path).mode & 0o777, 0o700, path);
        assert.equal(statSync(join(archive, '.env')).mode & 0o777, 0o600);
      }
      ctx.cli(restore('secrets', ws.lease));
      assert.equal(readFileSync(secret, 'utf8'), 'TOKEN=private\n');
      if (process.platform !== 'win32') {
        assert.equal(statSync(ws.path).mode & 0o777, 0o700);
        assert.equal(statSync(secret).mode & 0o777, 0o600);
      }
    },
  },
];
