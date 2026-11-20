import assert from 'node:assert/strict';
import { randomBytes, randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { leaveIndexedCheckoutJournal } from './checkout-journal.ts';
import { objectResult, type Context, type Scenario } from './harness.ts';

type Workspace = ReturnType<Context['spawn']>;

function commit(ctx: Context, path: string, file: string, content: string): string {
  writeFileSync(join(path, file), content);
  ctx.git(['add', '--', file], path);
  ctx.git(['commit', '-qm', file], path);
  return ctx.git(['rev-parse', 'HEAD'], path);
}

function land(ctx: Context, name: string, workspace: Workspace, ...options: string[]): string {
  const result = ctx.cli(['land', name, '--lease', workspace.lease, ...options]);
  assert.equal(result.recovery_lease, workspace.lease);
  assert(typeof result.commit === 'string');
  return result.commit;
}

function undo(name: string, lease: string): string[] {
  return ['land', name, '--undo', '--lease', lease];
}

function planStore(ctx: Context): string {
  const admin = ctx.git(['rev-parse', '--absolute-git-dir']);
  const format = ctx.git(['rev-parse', '--show-object-format']);
  return join(admin, 'graft', 'land-plans', format + '-full');
}

function undoPins(ctx: Context, store: string): string[] {
  return ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft/workspaces'], store)
    .split('\n').filter(ref => ref.includes('/undo/'));
}

function assertUndo(ctx: Context, dest: string, previous: string, landed: string, reverted: unknown) {
  assert.equal(typeof reverted, 'string');
  assert.equal(ctx.git(['rev-parse', dest]), reverted);
  assert.equal(ctx.git(['show', '-s', '--format=%P', dest]), landed);
  assert.equal(ctx.git(['rev-parse', `${dest}^{tree}`]), ctx.git(['rev-parse', `${previous}^{tree}`]));
}

export const cases: Scenario[] = [
  {
    name: 'landing falls back when history cache ownership is unrecognized',
    run(ctx) {
      const ws = ctx.spawn('foreign-cache');
      const admin = ctx.git(['rev-parse', '--absolute-git-dir']);
      const marker = join(admin, 'graft', 'independent-packs-v2', 'graft-pack-cache');
      mkdirSync(dirname(marker), { recursive: true });
      writeFileSync(marker, 'foreign cache marker\n');
      const tip = commit(ctx, ws.path, 'feature.txt', 'preserved feature\n');
      assert.equal(land(ctx, 'foreign-cache', ws), tip);
      assert.equal(readFileSync(marker, 'utf8'), 'foreign cache marker\n');
      ctx.git(['fsck', '--full'], planStore(ctx));
      ctx.cli(undo('foreign-cache', ws.lease));
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
    },
  },
  {
    name: 'landing uses the available history backend without duplicate objects and survives recovery',
    run(ctx) {
      ctx.git(['config', '--local', 'graft.history', 'isolated']);
      writeFileSync(join(ctx.repo, 'history.bin'), randomBytes(1024 * 1024));
      ctx.git(['add', 'history.bin']);
      ctx.git(['commit', '-qm', 'history']);
      ctx.git(['gc']);
      const first = ctx.spawn('cache-base');
      commit(ctx, ctx.repo, 'base-change.txt', 'base change\n');
      const old = ctx.git(['rev-parse', 'HEAD']);
      const ws = ctx.spawn('cache-delta');
      const admin = ctx.git(['rev-parse', '--absolute-git-dir']);
      const format = ctx.git(['rev-parse', '--show-object-format']);
      const cache = join(admin, 'graft', 'independent-packs-v2', `${format}-${old}`);
      const canClone = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
      assert.equal(typeof canClone, 'boolean');
      assert.equal(existsSync(cache), canClone, 'verified generation must match this volume’s clone capability');
      const cachedPacks = canClone ? readdirSync(cache).filter(name => name.endsWith('.pack')) : [];
      if (canClone) assert(cachedPacks.length >= 2, 'expected inherited and delta packs');
      const tip = commit(ctx, ws.path, 'feature.txt', 'landed change\n');
      assert.equal(land(ctx, 'cache-delta', ws), tip);
      const store = planStore(ctx);
      const packDir = join(store, 'objects', 'pack');
      const packs = readdirSync(packDir).filter(name => name.endsWith('.pack'));
      for (const name of cachedPacks) assert(packs.includes(name), `missing cached pack ${name}`);
      const ids: string[] = [];
      for (const pack of packs) {
        const output = ctx.git(['verify-pack', '-v', join(packDir, pack.replace(/\.pack$/, '.idx'))], store);
        ids.push(...output.split('\n').map(line => line.match(/^([0-9a-f]{40,64}) (?:commit|tree|blob|tag) /)?.[1]).filter((id): id is string => !!id));
      }
      const historyBlob = ctx.git(['rev-parse', `${old}:history.bin`]);
      assert.equal(ids.filter(id => id === historyBlob).length, 1, 'plan store duplicated the large history blob');
      ctx.cli(['prune','--expired', '--apply']);
      ctx.git(['gc', '--prune=now']);
      ctx.git(['fsck', '--full'], store);
      ctx.git(['fsck', '--full']);
      const restored = ctx.cli(['restore', 'cache-delta', '--lease', ws.lease]);
      assert.notEqual(restored.lease, ws.lease);
      ctx.cli(undo('cache-delta', ws.lease));
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
      assert.equal(readFileSync(join(ws.path, 'feature.txt'), 'utf8'), 'landed change\n');
      ctx.cli(['prune', 'cache-base', '--lease', first.lease]);
    },
  },
  {
    name: 'undo reverses an entire fast-forward after restore and garbage collection',
    run(ctx) {
      const name = 'multiple-commits';
      const old = ctx.git(['rev-parse', 'HEAD']);
      const ws = ctx.spawn(name);
      const first = commit(ctx, ws.path, 'feature.txt', 'first commit\n');
      const last = commit(ctx, ws.path, 'tracked.txt', 'second commit\n');
      assert.equal(land(ctx, name, ws), last);
      const restored = ctx.cli(['restore', name, '--lease', ws.lease]);
      assert.notEqual(restored.lease, ws.lease);
      const privateFile = join(ws.path, 'local.txt');
      writeFileSync(privateFile, 'restored workspace data\n');

      ctx.cli(['prune','--expired', '--apply']);
      ctx.git(['gc', '--prune=now']);
      ctx.git(['fsck', '--full']);
      ctx.git(['fsck', '--full'], ws.path);

      const args = undo(name, ws.lease);
      const result = ctx.cli(args);
      assert.deepEqual(result, {
        action: 'undo', name, target: 'main', previous: last, commit: result.commit,
      });
      assertUndo(ctx, 'HEAD', old, last, result.commit);
      assert.equal(ctx.git(['rev-list', '--ancestry-path', `${old}..HEAD`]),
        [result.commit, last, first].join('\n'));
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'initial\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), last);
      assert.equal(readFileSync(privateFile, 'utf8'), 'restored workspace data\n');
      assert.deepEqual(ctx.cli(args), result);
    },
  },
  {
    name: 'undo replay clears its indexed checkout journal and unblocks the next landing',
    run(ctx) {
      const name = 'indexed-undo';
      const base = ctx.git(['rev-parse', 'HEAD']);
      const ws = ctx.spawn(name);
      const tip = commit(ctx, ws.path, 'feature.txt', 'workspace feature\n');
      assert.equal(land(ctx, name, ws), tip);
      const args = undo(name, ws.lease);
      const journal = leaveIndexedCheckoutJournal(ctx, args);
      const reversed = ctx.git(['rev-parse', 'HEAD']);
      assert.deepEqual(journal.plan, { target: 'main', old: tip, new: reversed, tip });
      assertUndo(ctx, 'HEAD', base, tip, reversed);
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
      const index = readFileSync(join(ctx.repo, '.git', 'index'));

      const replayed = ctx.cli(args);
      assert.deepEqual(replayed, { action: 'undo', name, target: 'main', previous: tip, commit: reversed });
      assert.equal(existsSync(journal.path), false);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), reversed);
      assert.deepEqual(readFileSync(join(ctx.repo, '.git', 'index')), index);
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'initial\n');

      const followup = ctx.spawn('after-undo');
      assert.equal(followup.result.base, reversed);
      const next = commit(ctx, followup.path, 'followup.txt', 'next landing\n');
      assert.equal(land(ctx, 'after-undo', followup), next);
      assert.equal(ctx.git(['rev-parse', 'HEAD^']), reversed);
      assert.equal(readFileSync(join(ctx.repo, 'followup.txt'), 'utf8'), 'next landing\n');
      assert.equal(existsSync(journal.path), false);
      ctx.git(['fsck', '--full']);
    },
  },
  ...['merge', 'squash'].map((strategy): Scenario => ({
    name: `undo restores the exact destination tree after a ${strategy} landing`,
    run(ctx) {
      const ws = ctx.spawn(strategy);
      const tip = commit(ctx, ws.path, 'feature.txt', 'workspace changes\n');
      const old = commit(ctx, ctx.repo, 'native.txt', 'destination changes\n');
      const oldGitParents = strategy === 'merge' ? `${old} ${tip}` : old;
      const landed = land(ctx, strategy, ws, ...(strategy === 'squash' ? ['--squash'] : []));
      assert.equal(ctx.git(['show', '-s', '--format=%P', 'HEAD']), oldGitParents);
      const result = ctx.cli(undo(strategy, ws.lease));
      assertUndo(ctx, 'HEAD', old, landed, result.commit);
      assert.equal(readFileSync(join(ctx.repo, 'native.txt'), 'utf8'), 'destination changes\n');
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
    },
  })),
  {
    name: 'undo leaves dirty target and an ignored path collision intact',
    run(ctx) {
      const name = 'dirty-target';
      const old = commit(ctx, ctx.repo, 'removed.txt', 'present before landing\n');
      const ws = ctx.spawn(name);
      ctx.git(['rm', 'removed.txt'], ws.path);
      ctx.git(['commit', '-qm', 'remove tracked file'], ws.path);
      const landed = land(ctx, name, ws);
      const args = undo(name, ws.lease);
      const tracked = join(ctx.repo, 'tracked.txt');
      const untracked = join(ctx.repo, 'private.txt');
      writeFileSync(tracked, 'staged user edit\n');
      ctx.git(['add', '--', 'tracked.txt']);
      writeFileSync(tracked, 'unstaged user edit\n');
      writeFileSync(untracked, 'untracked user file\n');
      assert.match(String(ctx.fail(args).message), /local changes/i);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), landed);
      assert.equal(ctx.git(['show', ':tracked.txt']), 'staged user edit');
      assert.equal(readFileSync(tracked, 'utf8'), 'unstaged user edit\n');
      assert.equal(readFileSync(untracked, 'utf8'), 'untracked user file\n');

      ctx.git(['restore', '--source=HEAD', '--staged', '--worktree', '--', 'tracked.txt']);
      rmSync(untracked);
      const common = ctx.git(['rev-parse', '--path-format=absolute', '--git-common-dir']);
      mkdirSync(join(common, 'info'), { recursive: true });
      writeFileSync(join(common, 'info', 'exclude'), '/removed.txt\n');
      const collision = join(ctx.repo, 'removed.txt');
      writeFileSync(collision, 'ignored user file\n');
      assert.match(String(ctx.fail(args).message), /removed\.txt/i);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), landed);
      assert.equal(readFileSync(collision, 'utf8'), 'ignored user file\n');
      rmSync(collision);
      assertUndo(ctx, 'HEAD', old, landed, ctx.cli(args).commit);
    },
  },
  {
    name: 'undo waits for a native worktree to release the destination and preserves its edits',
    run(ctx) {
      const name = 'native-undo';
      const old = ctx.git(['rev-parse', 'HEAD']);
      ctx.git(['branch', 'target']);
      const ws = ctx.spawn(name);
      const tip = commit(ctx, ws.path, 'feature.txt', 'workspace feature\n');
      const landed = land(ctx, name, ws, '--into', 'target');
      assert.equal(landed, tip);
      const dest = join(ctx.root, 'native destination');
      ctx.git(['worktree', 'add', dest, 'target']);
      const local = join(dest, 'tracked.txt');
      writeFileSync(local, 'native staged edit\n');
      ctx.git(['add', '--', 'tracked.txt'], dest);
      writeFileSync(local, 'native unstaged edit\n');
      const args = undo(name, ws.lease);
      assert.match(String(ctx.fail(args).message), /external linked worktree/i);
      assert.equal(ctx.git(['rev-parse', 'target']), landed);
      assert.equal(ctx.git(['show', ':tracked.txt'], dest), 'native staged edit');
      assert.equal(readFileSync(local, 'utf8'), 'native unstaged edit\n');
      assert.equal(readFileSync(join(dest, 'feature.txt'), 'utf8'), 'workspace feature\n');
      // Detaching at the same commit leaves both staged and unstaged edits intact.
      ctx.git(['switch', '--detach'], dest);
      assertUndo(ctx, 'target', old, landed, ctx.cli(args).commit);
      assert.equal(ctx.git(['show', ':tracked.txt'], dest), 'native staged edit');
      assert.equal(readFileSync(local, 'utf8'), 'native unstaged edit\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], dest), landed);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
      ctx.git(['fsck', '--full']);
    },
  },
  {
    name: 'undo rejects a moved target before creating an undo commit',
    run(ctx) {
      const name = 'advanced';
      const ws = ctx.spawn(name);
      commit(ctx, ws.path, 'feature.txt', 'feature landed\n');
      const landed = land(ctx, name, ws);
      const advanced = commit(ctx, ctx.repo, 'later.txt', 'later user commit\n');
      const failure = ctx.fail(undo(name, ws.lease));
      assert.equal(failure.code, 'target_advanced');
      assert.deepEqual(failure.details, { target: 'main', expected: landed, actual: advanced });
      assert.equal(ctx.git(['rev-parse', 'HEAD']), advanced);
      assert.equal(readFileSync(join(ctx.repo, 'later.txt'), 'utf8'), 'later user commit\n');
      assert.deepEqual(undoPins(ctx, planStore(ctx)), []);
      assert.equal(ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft']), '');
    },
  },
  {
    name: 'undo pins before ref contention, retries the same commit and rejects later advances',
    run(ctx) {
      const name = 'contended';
      const old = ctx.git(['rev-parse', 'HEAD']);
      ctx.git(['branch', 'target']);
      const ws = ctx.spawn(name);
      commit(ctx, ws.path, 'feature.txt', 'feature landed\n');
      const landed = land(ctx, name, ws, '--into', 'target');
      const args = undo(name, ws.lease);
      const common = ctx.git(['rev-parse', '--path-format=absolute', '--git-common-dir']);
      const lock = join(common, 'refs', 'heads', 'target.lock');
      writeFileSync(lock, 'native Git owns the branch lock\n', { flag: 'wx' });
      try {
        assert.match(String(ctx.fail(args).message), /lock|reference/i);
        assert.equal(ctx.git(['rev-parse', 'target']), landed);
        assert.equal(readFileSync(lock, 'utf8'), 'native Git owns the branch lock\n');
      } finally {
        rmSync(lock);
      }
      const store = planStore(ctx);
      const pins = undoPins(ctx, store);
      assert.equal(pins.length, 1);
      const [pinned] = pins;
      assert.ok(!pinned.includes(ws.lease));
      assert.equal(ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft']), '');
      const candidate = ctx.git(['rev-parse', pinned], store);
      assert.equal(ctx.git(['rev-parse', candidate + '^'], store), landed);
      assert.equal(ctx.git(['rev-parse', candidate + '^{tree}'], store), ctx.git(['rev-parse', old + '^{tree}']));
      ctx.env.GIT_COMMITTER_DATE = '2025-01-01T00:00:00Z';
      const result = ctx.cli(args);
      assert.equal(result.commit, candidate);
      assertUndo(ctx, 'target', old, landed, candidate);
      assert.deepEqual(ctx.cli(args), result);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
      assert.equal(ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft']), '');
      ctx.git(['fsck', '--full'], store);
      ctx.git(['fsck', '--full']);

      const tree = ctx.git(['rev-parse', 'target^{tree}']);
      const later = ctx.git(['commit-tree', tree, '-p', candidate, '-m', 'later native commit']);
      ctx.git(['update-ref', 'refs/heads/target', later, candidate]);
      assert.equal(ctx.fail(args).code, 'target_advanced');
      assert.equal(ctx.git(['rev-parse', 'target']), later);
    },
  },
  {
    name: 'undo authenticates the retired landing and rejects prune and no-op',
    run(ctx) {
      const dropped = ctx.spawn('dropped');
      ctx.cli(['prune', 'dropped', '--lease', dropped.lease]);
      assert.match(String(ctx.fail(undo('dropped', dropped.lease)).message), /not retired by landing/i);
      const unchanged = ctx.spawn('unchanged');
      const old = ctx.git(['rev-parse', 'HEAD']);
      assert.equal(land(ctx, 'unchanged', unchanged), old);
      assert.equal(ctx.fail(undo('other', unchanged.lease)).code, 'lease_mismatch');
      assert.equal(ctx.fail(undo('unchanged', randomUUID())).code, 'lease_mismatch');
      assert.equal(ctx.fail(undo('unchanged', unchanged.lease)).code, 'nothing_to_undo');
      assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
    },
  },
];
