import assert from 'node:assert/strict';
import { spawn as spawnProcess } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import type { Scenario } from './harness.ts';

type Context = Parameters<Scenario['run']>[0];

function record(ctx: Context, name: string): string {
  return join(ctx.repo, '.git', 'graft', 'records', `${name}.json`);
}
function object(value: unknown): Record<string, unknown> {
  assert.ok(value && typeof value === 'object' && !Array.isArray(value));
  return value as Record<string, unknown>;
}
function array(value: unknown): unknown[] {
  assert.ok(Array.isArray(value));
  return value;
}
function editRecord(ctx: Context, name: string, change: (value: Record<string, unknown>) => void): void {
  const path = record(ctx, name);
  const parsed: unknown = JSON.parse(readFileSync(path, 'utf8'));
  const data = object(parsed);
  change(data);
  writeFileSync(path, JSON.stringify(data));
}
function drop(ctx: Context, name: string, lease: string, ...args: string[]): Record<string, unknown> {
  return ctx.cli(['drop', name, '--lease', lease, ...args]);
}
function land(ctx: Context, name: string, lease: string, ...args: string[]): Record<string, unknown> {
  return ctx.cli(['land', name, '--lease', lease, ...args]);
}
function commit(ctx: Context, cwd: string, message: string): void {
  ctx.git(['add', '-A'], cwd);
  ctx.git(['commit', '-m', message], cwd);
}
function names(value: unknown): unknown[] {
  return array(value).map((item) => object(item).name);
}

export const cases: Scenario[] = [
  {
    name: 'doctor list status and isolated workspace',
    run(ctx) {
      const doctor = ctx.cli(['doctor']);
      assert.ok(doctor.git && doctor.common_dir && doctor.source_storage);
      assert.equal(array(ctx.cli(['list']).workspaces).length, 0);
      const w = ctx.spawn('agent');
      assert.equal(ctx.git(['branch', '--show-current'], w.path), 'graft/agent');
      assert.equal(ctx.git(['branch', '--show-current']), 'main');
      assert.ok(names(ctx.cli(['list']).workspaces).includes('agent'));
      let status = ctx.cli(['status', 'agent']);
      assert.equal(status.registered, true);
      assert.equal(status.change_count, 0);
      writeFileSync(join(w.path, 'tracked.txt'), 'committed\n');
      commit(ctx, w.path, 'workspace commit');
      writeFileSync(join(w.path, 'tracked.txt'), 'dirty\n');
      writeFileSync(join(w.path, 'untracked.txt'), 'new');
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'initial\n');
      status = ctx.cli(['status', 'agent']);
      assert.ok(array(status.committed_paths).includes('tracked.txt'));
      const changes = array(status.changes).map(object);
      assert.ok(changes.some((v) => v.path === 'tracked.txt'));
      assert.ok(changes.some((v) => v.path === 'untracked.txt'));
      ctx.fail(['spawn', 'agent']);
      ctx.fail(['drop', 'agent', '--lease', w.lease]);
      assert.equal(readFileSync(join(w.path, 'tracked.txt'), 'utf8'), 'dirty\n');
      drop(ctx, 'agent', w.lease, '--discard');
      assert.equal(existsSync(w.path), false);
    },
  },
  {
    name: 'linked source shares registry and primary root',
    run(ctx) {
      const linked = join(ctx.root, 'linked');
      ctx.git(['worktree', 'add', '-b', 'other', linked]);
      const w = ctx.spawn('linked-agent', [], linked);
      assert.ok(existsSync(w.path));
      assert.equal(ctx.git(['branch', '--show-current'], w.path), 'graft/linked-agent');
      assert.ok(names(ctx.cli(['list']).workspaces).includes('linked-agent'));
      ctx.cli(['drop', 'linked-agent', '--lease', w.lease], linked);
      assert.equal(existsSync(w.path), false);
      ctx.git(['worktree', 'remove', linked]);
    },
  },
  {
    name: 'drop refuses untracked ignored and unmerged work',
    run(ctx) {
      const w = ctx.spawn('agent');
      writeFileSync(join(w.path, 'untracked'), 'keep');
      ctx.fail(['drop', 'agent', '--lease', w.lease]);
      assert.equal(readFileSync(join(w.path, 'untracked'), 'utf8'), 'keep');
      rmSync(join(w.path, 'untracked'));
      mkdirSync(join(w.path, 'ignored'));
      writeFileSync(join(w.path, 'ignored', 'data'), 'keep ignored');
      ctx.fail(['drop', 'agent', '--lease', w.lease]);
      assert.equal(readFileSync(join(w.path, 'ignored', 'data'), 'utf8'), 'keep ignored');
      rmSync(join(w.path, 'ignored'), { recursive: true });
      writeFileSync(join(w.path, 'tracked.txt'), 'committed\n');
      commit(ctx, w.path, 'unmerged');
      ctx.fail(['drop', 'agent', '--lease', w.lease]);
      assert.equal(ctx.git(['show', 'HEAD:tracked.txt'], w.path), 'committed');
      drop(ctx, 'agent', w.lease, '--discard');
    },
  },
  {
    name: 'landing refusal preserves data and commit lands',
    run(ctx) {
      const w = ctx.spawn('agent');
      writeFileSync(join(w.path, 'tracked.txt'), 'landed\n');
      ctx.fail(['land', 'agent', '--lease', w.lease]);
      assert.equal(readFileSync(join(w.path, 'tracked.txt'), 'utf8'), 'landed\n');
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'initial\n');
      mkdirSync(join(w.path, 'ignored'));
      writeFileSync(join(w.path, 'ignored', 'data'), 'keep ignored');
      ctx.fail(['land', 'agent', '--lease', w.lease, '--commit', 'agent changes']);
      assert.equal(readFileSync(join(w.path, 'ignored', 'data'), 'utf8'), 'keep ignored');
      const result = land(ctx, 'agent', w.lease, '--commit', 'agent changes', '--discard-ignored');
      assert.equal(result.commit, ctx.git(['rev-parse', 'main']));
      assert.equal(ctx.git(['show', 'main:tracked.txt']), 'landed');
      assert.equal(existsSync(w.path), false);
    },
  },
  {
    name: 'land invoked from retiring worktree',
    run(ctx) {
      const w = ctx.spawn('agent');
      writeFileSync(join(w.path, 'tracked.txt'), 'integrate\n');
      const result = ctx.cli(['land', 'agent', '--lease', w.lease, '--commit', 'integrate'], w.path);
      assert.equal(result.commit, ctx.git(['rev-parse', 'main']));
      assert.equal(existsSync(w.path), false);
      assert.equal(ctx.git(['branch', '--list', 'graft/agent']), '');
    },
  },
  {
    name: 'gc previews and retains stale dirty work',
    run(ctx) {
      const clean = ctx.spawn('clean');
      const dirty = ctx.spawn('dirty');
      writeFileSync(join(dirty.path, 'untracked'), 'keep');
      for (const name of ['clean', 'dirty']) editRecord(ctx, name, (r) => { r.heartbeat = 0; });
      const preview = ctx.cli(['gc']);
      assert.ok(array(preview.candidates).includes('clean'));
      assert.ok(names(preview.retained).includes('dirty'));
      assert.ok(existsSync(clean.path));
      const applied = ctx.cli(['gc', '--apply']);
      assert.ok(array(applied.removed).includes('clean'));
      assert.ok(names(applied.retained).includes('dirty'));
      assert.equal(existsSync(clean.path), false);
      assert.equal(readFileSync(join(dirty.path, 'untracked'), 'utf8'), 'keep');
      drop(ctx, 'dirty', dirty.lease, '--discard');
    },
  },
  {
    name: 'cache purge requires apply and no live workspace',
    run(ctx) {
      const w = ctx.spawn('agent');
      const cacheDir = join(ctx.repo, '.git', 'graft', 'cache');
      const layerDir = join(ctx.repo, '.git', 'graft', 'layers');
      writeFileSync(join(cacheDir, 'fixture'), 'cache');
      writeFileSync(join(layerDir, 'fixture'), 'layer');
      assert.equal(object(ctx.cli(['gc', '--purge-cache']).cache).eligible, false);
      assert.equal(object(ctx.cli(['gc', '--purge-cache', '--apply']).cache).purged, false);
      assert.ok(existsSync(join(cacheDir, 'fixture')));
      drop(ctx, 'agent', w.lease);
      const preview = object(ctx.cli(['gc', '--purge-cache']).cache);
      assert.equal(preview.eligible, true);
      assert.equal(preview.purged, false);
      assert.ok(existsSync(join(layerDir, 'fixture')));
      assert.equal(object(ctx.cli(['gc', '--purge-cache', '--apply']).cache).purged, true);
      assert.equal(existsSync(join(cacheDir, 'fixture')), false);
      assert.equal(existsSync(join(layerDir, 'fixture')), false);
    },
  },
  {
    name: 'exec holds workspace lock on every platform',
    async run(ctx) {
      const w = ctx.spawn('agent');
      const started = join(ctx.root, 'started');
      const release = join(ctx.root, 'release');
      const script = `const fs=require('node:fs'); fs.writeFileSync(process.argv[1],'started'); const end=Date.now()+15000; const timer=setInterval(()=>{if(fs.existsSync(process.argv[2])){clearInterval(timer);process.exit(0)} if(Date.now()>end){clearInterval(timer);process.exit(2)}},25)`;
      const child = spawnProcess(ctx.bin, ['--repo', ctx.repo, '--json', 'exec', 'agent', '--lease', w.lease, '--', process.execPath, '-e', script, started, release], { env: ctx.env, stdio: ['ignore', 'pipe', 'pipe'] });
      let stderr = '';
      child.stderr.setEncoding('utf8');
      child.stderr.on('data', (data: string) => { stderr += data; });
      child.stdout.resume();
      const exit = new Promise<number | null>((resolve, reject) => {
        child.once('error', reject);
        child.once('close', resolve);
      });
      try {
        const deadline = Date.now() + 10000;
        while (!existsSync(started) && Date.now() < deadline) await delay(25);
        assert.ok(existsSync(started), `exec did not start: ${stderr}`);
        ctx.fail(['drop', 'agent', '--lease', w.lease, '--discard']);
        assert.ok(existsSync(w.path));
        writeFileSync(release, 'release');
        assert.equal(await exit, 0, stderr);
        ctx.coverCli('exec');
        drop(ctx, 'agent', w.lease);
      } finally {
        if (!existsSync(release)) writeFileSync(release, 'release');
        if (child.exitCode === null && child.signalCode === null) child.kill();
        await exit.catch(() => undefined);
      }
    },
  },
  {
    name: 'changed branch and hidden index flags protect work',
    run(ctx) {
      const w = ctx.spawn('agent');
      ctx.git(['switch', '-c', 'foreign'], w.path);
      assert.equal(ctx.cli(['status', 'agent']).registered, false);
      ctx.fail(['drop', 'agent', '--lease', w.lease, '--discard']);
      assert.ok(existsSync(w.path));
      ctx.git(['switch', 'graft/agent'], w.path);
      for (const flag of ['assume-unchanged', 'skip-worktree']) {
        writeFileSync(join(w.path, 'tracked.txt'), `${flag}\n`);
        ctx.git(['update-index', `--${flag}`, 'tracked.txt'], w.path);
        ctx.fail(['drop', 'agent', '--lease', w.lease]);
        assert.equal(readFileSync(join(w.path, 'tracked.txt'), 'utf8'), `${flag}\n`);
        ctx.git(['update-index', `--no-${flag}`, 'tracked.txt'], w.path);
      }
      ctx.git(['worktree', 'lock', w.path]);
      ctx.fail(['drop', 'agent', '--lease', w.lease, '--discard']);
      ctx.git(['worktree', 'unlock', w.path]);
      drop(ctx, 'agent', w.lease, '--discard');
    },
  },
  {
    name: 'moved worktree is protected until restored',
    run(ctx) {
      const w = ctx.spawn('agent');
      const moved = join(ctx.root, 'moved');
      ctx.git(['worktree', 'move', w.path, moved]);
      ctx.fail(['drop', 'agent', '--lease', w.lease, '--discard']);
      assert.ok(existsSync(moved));
      ctx.git(['worktree', 'move', moved, w.path]);
      drop(ctx, 'agent', w.lease);
      assert.equal(existsSync(w.path), false);
    },
  },
  {
    name: 'symlink redirection is refused',
    run(ctx) {
      const w = ctx.spawn('agent');
      const moved = join(ctx.root, 'moved');
      ctx.git(['worktree', 'move', w.path, moved]);
      try {
        symlinkSync(moved, w.path, 'dir');
      } catch (error) {
        ctx.unavailable(`Cannot create directory symlink to verify redirection protection: ${String(error)}`);
      }
      try {
        ctx.fail(['drop', 'agent', '--lease', w.lease, '--discard']);
        assert.ok(existsSync(moved));
      } finally {
        unlinkSync(w.path);
      }
      ctx.git(['worktree', 'move', moved, w.path]);
      drop(ctx, 'agent', w.lease);
    },
  },
  {
    name: 'spawn request id survives disconnect and rejects adoption',
    run(ctx) {
      const id = 'b4826629-f8e7-4736-ac38-f2950d40f642';
      const other = '938fb238-a7d9-4c90-a572-13c701d535a1';
      const first = ctx.spawn('agent', ['--request-id', id]);
      assert.equal(first.lease, id);
      const original = ctx.git(['rev-parse', 'HEAD']);
      assert.equal(first.result.base, original);
      writeFileSync(join(ctx.repo, 'new.txt'), 'next\n');
      commit(ctx, ctx.repo, 'advance main');
      const retry = ctx.spawn('agent', ['--request-id', id]);
      assert.equal(retry.lease, id);
      assert.equal(retry.path, first.path);
      assert.equal(retry.result.base, original);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], first.path), original);
      ctx.fail(['spawn', 'agent', '--request-id', other]);
      ctx.fail(['spawn', 'agent', '--request-id', id, '--base', 'main']);
      ctx.fail(['spawn', 'agent', '--request-id', id, '--sparse', 'different']);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], first.path), original);
      drop(ctx, 'agent', id);
      ctx.fail(['spawn', 'another', '--request-id', id]);
    },
  },
  {
    name: 'drop replay and reused slug reject stale lease',
    run(ctx) {
      const first = ctx.spawn('agent');
      const result = drop(ctx, 'agent', first.lease);
      assert.deepEqual(drop(ctx, 'agent', first.lease), result);
      const next = ctx.spawn('agent');
      assert.notEqual(first.lease, next.lease);
      ctx.fail(['heartbeat', 'agent', '--lease', first.lease]);
      ctx.fail(['drop', 'agent', '--lease', first.lease, '--discard']);
      assert.ok(existsSync(next.path));
      ctx.cli(['heartbeat', 'agent', '--lease', next.lease]);
      drop(ctx, 'agent', next.lease);
    },
  },
  {
    name: 'squash land replay does not create another commit',
    run(ctx) {
      const w = ctx.spawn('agent');
      writeFileSync(join(w.path, 'tracked.txt'), 'landed\n');
      const args = ['land', 'agent', '--lease', w.lease, '--squash', '--commit', 'agent work'];
      const result = ctx.cli(args);
      const head = ctx.git(['rev-parse', 'main']);
      const count = ctx.git(['rev-list', '--count', 'main']);
      assert.deepEqual(ctx.cli(args), result);
      assert.equal(ctx.git(['rev-parse', 'main']), head);
      assert.equal(ctx.git(['rev-list', '--count', 'main']), count);
      const next = ctx.spawn('agent');
      ctx.fail(args);
      assert.ok(existsSync(next.path));
      drop(ctx, 'agent', next.lease);
    },
  },
  {
    name: 'drop resumes after interrupted worktree removal',
    run(ctx) {
      const w = ctx.spawn('agent');
      const tip = ctx.git(['rev-parse', 'HEAD'], w.path);
      editRecord(ctx, 'agent', (r) => { r.state = { phase: 'removing', tip, discard: false, discard_ignored: false, landed: null }; });
      ctx.git(['worktree', 'remove', w.path]);
      const result = drop(ctx, 'agent', w.lease);
      assert.equal(existsSync(record(ctx, 'agent')), false);
      assert.equal(ctx.git(['branch', '--list', 'graft/agent']), '');
      assert.deepEqual(drop(ctx, 'agent', w.lease), result);
    },
  },
  {
    name: 'landing resumes from persisted plan',
    run(ctx) {
      const w = ctx.spawn('agent');
      const old = ctx.git(['rev-parse', 'main']);
      writeFileSync(join(w.path, 'tracked.txt'), 'planned\n');
      commit(ctx, w.path, 'planned');
      const tip = ctx.git(['rev-parse', 'HEAD'], w.path);
      const plan = { target: 'main', old, new: tip, tip };
      editRecord(ctx, 'agent', (r) => { r.state = { phase: 'landing', plan, discard_ignored: false }; });
      const result = land(ctx, 'agent', w.lease);
      assert.equal(result.commit, tip);
      assert.equal(ctx.git(['rev-parse', 'main']), tip);
      assert.equal(ctx.git(['show', 'main:tracked.txt']), 'planned');
      assert.equal(existsSync(w.path), false);
      assert.deepEqual(land(ctx, 'agent', w.lease), result);
    },
  },
  {
    name: 'landing resumes after applied worktree removal',
    run(ctx) {
      const w = ctx.spawn('agent');
      const old = ctx.git(['rev-parse', 'main']);
      writeFileSync(join(w.path, 'tracked.txt'), 'applied\n');
      commit(ctx, w.path, 'applied');
      const tip = ctx.git(['rev-parse', 'HEAD'], w.path);
      ctx.git(['merge', '--ff-only', tip]);
      const plan = { target: 'main', old, new: tip, tip };
      editRecord(ctx, 'agent', (r) => { r.state = { phase: 'removing', tip, discard: false, discard_ignored: false, landed: plan }; });
      ctx.git(['worktree', 'remove', w.path]);
      const result = land(ctx, 'agent', w.lease);
      assert.equal(result.commit, tip);
      assert.equal(ctx.git(['rev-parse', 'main']), tip);
      assert.equal(ctx.git(['branch', '--list', 'graft/agent']), '');
      assert.equal(existsSync(record(ctx, 'agent')), false);
      assert.deepEqual(land(ctx, 'agent', w.lease), result);
    },
  },
  {
    name: 'expired completion receipts require gc apply',
    run(ctx) {
      const w = ctx.spawn('agent');
      const result = drop(ctx, 'agent', w.lease);
      const receipt = join(ctx.repo, '.git', 'graft', 'completed', `${w.lease}.json`);
      const parsed: unknown = JSON.parse(readFileSync(receipt, 'utf8'));
      const data = object(parsed);
      data.at = Math.floor(Date.now() / 1000) - 8 * 86400;
      writeFileSync(receipt, JSON.stringify(data));
      const preview = ctx.cli(['gc']);
      assert.equal(preview.expired_receipts, 1);
      assert.deepEqual(drop(ctx, 'agent', w.lease), result);
      const applied = ctx.cli(['gc', '--apply']);
      assert.equal(applied.expired_receipts, 1);
      assert.equal(existsSync(receipt), false);
      ctx.fail(['drop', 'agent', '--lease', w.lease]);
    },
  },
];
