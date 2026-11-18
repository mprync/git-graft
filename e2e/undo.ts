import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
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
  return ['undo-land', name, '--lease', lease];
}

function assertUndo(ctx: Context, dest: string, previous: string, landed: string, reverted: unknown) {
  assert.equal(typeof reverted, 'string');
  assert.equal(ctx.git(['rev-parse', dest]), reverted);
  assert.equal(ctx.git(['show', '-s', '--format=%P', dest]), landed);
  assert.equal(ctx.git(['rev-parse', `${dest}^{tree}`]), ctx.git(['rev-parse', `${previous}^{tree}`]));
}

export const cases: Scenario[] = [
  {
    name: 'undo reverses an entire fast-forward after restoration and receipt expiry',
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

      const common = ctx.git(['rev-parse', '--path-format=absolute', '--git-common-dir']);
      const receiptPath = join(common, 'graft', 'completed', `${ws.lease}.json`);
      const receipt = objectResult(JSON.parse(readFileSync(receiptPath, 'utf8')));
      writeFileSync(receiptPath, JSON.stringify({ ...receipt, at: 0 }));
      ctx.cli(['gc', '--apply']);
      assert.equal(existsSync(receiptPath), false);

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
      assert.doesNotMatch(ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft/recovery']), /\/undo$/m);
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
      const refs = ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft/recovery']);
      const pinned = refs.split('\n').find(ref => ref.endsWith('/undo'));
      assert.ok(pinned);
      assert.ok(!refs.includes(ws.lease));
      const candidate = ctx.git(['rev-parse', pinned]);
      ctx.env.GIT_COMMITTER_DATE = '2025-01-01T00:00:00Z';
      const result = ctx.cli(args);
      assert.equal(result.commit, candidate);
      assertUndo(ctx, 'target', old, landed, candidate);
      assert.deepEqual(ctx.cli(args), result);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);

      const tree = ctx.git(['rev-parse', 'target^{tree}']);
      const later = ctx.git(['commit-tree', tree, '-p', candidate, '-m', 'later native commit']);
      ctx.git(['update-ref', 'refs/heads/target', later, candidate]);
      assert.equal(ctx.fail(args).code, 'target_advanced');
      assert.equal(ctx.git(['rev-parse', 'target']), later);
    },
  },
  {
    name: 'undo authenticates the retired landing and rejects drop and no-op',
    run(ctx) {
      const dropped = ctx.spawn('dropped');
      ctx.cli(['drop', 'dropped', '--lease', dropped.lease]);
      assert.equal(ctx.fail(undo('dropped', dropped.lease)).code, 'landing_not_found');
      const unchanged = ctx.spawn('unchanged');
      const old = ctx.git(['rev-parse', 'HEAD']);
      assert.equal(land(ctx, 'unchanged', unchanged), old);
      assert.equal(ctx.fail(undo('other', unchanged.lease)).code, 'landing_not_found');
      assert.equal(ctx.fail(undo('unchanged', randomUUID())).code, 'landing_not_found');
      assert.equal(ctx.fail(undo('unchanged', unchanged.lease)).code, 'nothing_to_undo');
      assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
    },
  },
];
