import assert from 'node:assert/strict';
import { execFile, spawn as spawnProcess } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, renameSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { promisify } from 'node:util';
import { objectResult, type Scenario } from './harness.ts';

type Context = Parameters<Scenario['run']>[0];

function object(value: unknown): Record<string, unknown> {
  assert.ok(value && typeof value === 'object' && !Array.isArray(value));
  return value as Record<string, unknown>;
}
function array(value: unknown): unknown[] {
  assert.ok(Array.isArray(value));
  return value;
}
function drop(ctx: Context, name: string, lease: string, ...args: string[]): Record<string, unknown> {
  return ctx.cli(['remove', name, '--lease', lease, ...args]);
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
      assert.equal(typeof objectResult(doctor.file_sharing).shared, 'boolean', 'doctor reports whether grafts share files');
      assert.equal(doctor.git, ctx.git(['--version']));
      assert.equal(doctor.git_minimum, '2.46.0');
      assert.equal(doctor.git_supported, true);
      assert.equal(array(ctx.cli(['list']).workspaces).length, 0);
      const w = ctx.spawn('agent');
      assert.equal(JSON.stringify(ctx.cli(['doctor'])).includes(w.lease), false, 'doctor exposed the active lease');
      assert.equal(ctx.git(['branch', '--show-current'], w.path), 'graft/agent');
      assert.equal(ctx.git(['branch', '--show-current']), 'main');
      assert.ok(names(ctx.cli(['list']).workspaces).includes('agent'));
      let status = ctx.cli(['status', 'agent']);
      assert.equal(status.path, w.path);
      assert.equal(status.phase, 'ready');
      assert.equal(status.change_count, 0);
      writeFileSync(join(w.path, 'tracked.txt'), 'committed\n');
      commit(ctx, w.path, 'workspace commit');
      writeFileSync(join(w.path, 'tracked.txt'), 'dirty\n');
      writeFileSync(join(w.path, 'untracked.txt'), 'new');
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'initial\n');
      status = ctx.cli(['status', 'agent']);
      assert.equal(status.head, ctx.git(['rev-parse', 'HEAD'], w.path));
      const changes = array(status.changes).map(object);
      assert.ok(changes.some((v) => v.path === 'tracked.txt'));
      assert.ok(changes.some((v) => v.path === 'untracked.txt'));
      ctx.fail(['add', 'agent']);
      ctx.fail(['remove', 'agent', '--lease', w.lease]);
      assert.equal(readFileSync(join(w.path, 'tracked.txt'), 'utf8'), 'dirty\n');
      drop(ctx, 'agent', w.lease, '--discard');
      assert.equal(existsSync(w.path), false);
      assert.equal(JSON.stringify(ctx.cli(['doctor'])).includes(w.lease), false, 'doctor exposed the retired lease');
    },
  },
  {
    name: 'a native linked checkout stays intact while Graft uses the primary',
    run(ctx) {
      const linked = join(ctx.root, 'native linked checkout');
      ctx.git(['worktree', 'add', '-qb', 'other', linked]);
      writeFileSync(join(linked, 'tracked.txt'), 'native user edit\n');
      const head = ctx.git(['rev-parse', 'HEAD'], linked);
      assert.match(String(ctx.fail(['add', 'native-input'], linked).message), /primary repository/i);
      const w = ctx.spawn('agent');
      assert.ok(names(ctx.cli(['list']).workspaces).includes('agent'));
      assert.equal(ctx.git(['rev-parse', 'HEAD'], w.path), head);
      drop(ctx, 'agent', w.lease);
      assert.equal(existsSync(w.path), false);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], linked), head);
      assert.equal(readFileSync(join(linked, 'tracked.txt'), 'utf8'), 'native user edit\n');
    },
  },
  {
    name: 'prune refuses untracked ignored and unmerged work',
    run(ctx) {
      const w = ctx.spawn('agent');
      writeFileSync(join(w.path, 'untracked'), 'keep');
      ctx.fail(['remove', 'agent', '--lease', w.lease]);
      assert.equal(readFileSync(join(w.path, 'untracked'), 'utf8'), 'keep');
      rmSync(join(w.path, 'untracked'));
      mkdirSync(join(w.path, 'ignored'));
      writeFileSync(join(w.path, 'ignored', 'data'), 'keep ignored');
      ctx.fail(['remove', 'agent', '--lease', w.lease]);
      assert.equal(readFileSync(join(w.path, 'ignored', 'data'), 'utf8'), 'keep ignored');
      rmSync(join(w.path, 'ignored'), { recursive: true });
      writeFileSync(join(w.path, 'tracked.txt'), 'committed\n');
      commit(ctx, w.path, 'unmerged');
      ctx.fail(['remove', 'agent', '--lease', w.lease]);
      assert.equal(ctx.git(['show', 'HEAD:tracked.txt'], w.path), 'committed');
      drop(ctx, 'agent', w.lease, '--discard');
    },
  },
  {
    name: 'land invoked from the workspace it retires',
    run(ctx) {
      const w = ctx.spawn('agent');
      writeFileSync(join(w.path, 'tracked.txt'), 'integrate\n');
      const result = ctx.cli(['merge', 'agent', '--lease', w.lease, '--commit', 'integrate'], w.path);
      assert.equal(result.commit, ctx.git(['rev-parse', 'main']));
      assert.equal(existsSync(w.path), false);
      assert.equal(ctx.git(['branch', '--list', 'graft/agent']), '');
    },
  },
  {
    name: 'cache purge requires apply, preserves unknown data and validates layers',
    run(ctx) {
      writeFileSync(join(ctx.repo, 'package.json'), JSON.stringify({ name: 'fixture', version: '1.0.0' }));
      writeFileSync(join(ctx.repo, 'package-lock.json'), JSON.stringify({
        name: 'fixture', version: '1.0.0', lockfileVersion: 3, packages: { '': { name: 'fixture', version: '1.0.0' } },
      }));
      commit(ctx, ctx.repo, 'Layer fixture');
      const w = ctx.spawn('agent');
      ctx.cli(['exec', 'agent', '--lease', w.lease, '--', process.execPath, '-e', '']);
      const cacheDir = join(ctx.repo, '.git', 'graft', 'cache');
      const layerDir = join(ctx.repo, '.git', 'graft', 'layers');
      const npm = join(cacheDir, 'npm');
      writeFileSync(join(npm, 'entry'), 'package cache');
      const foreignCache = join(cacheDir, 'fixture');
      const foreignLayer = join(layerDir, 'fixture');
      writeFileSync(foreignCache, 'user cache note');
      writeFileSync(foreignLayer, 'user layer note');
      const modules = join(w.path, 'node_modules', 'example');
      mkdirSync(modules, { recursive: true });
      writeFileSync(join(modules, 'index.js'), 'module.exports = 1;\n');
      function save(key: string): string {
        const layer = ctx.cli(['deps','layer','save','agent','--lease',w.lease, '--key', key, '--copy']);
        assert.equal(typeof layer.digest, 'string');
        return join(layerDir, String(layer.digest));
      }
      const verified = save('verified');
      const changed = save('changed');
      const change = join(changed, 'user-note.txt');
      writeFileSync(change, 'preserve changed layer');
      assert.equal(object(ctx.cli(['prune', '--purge-cache']).cache).eligible, false);
      assert.equal(object(ctx.cli(['prune', '--purge-cache', '--apply']).cache).purged, false);
      assert.ok(existsSync(npm));
      assert.ok(existsSync(verified));
      drop(ctx, 'agent', w.lease, '--discard');
      ctx.cli(['prune', '--apply']);
      assert.ok(existsSync(npm));
      assert.ok(existsSync(verified));
      const preview = object(ctx.cli(['prune', '--purge-cache']).cache);
      assert.equal(preview.eligible, true);
      assert.equal(preview.purged, false);
      assert.ok(existsSync(verified));
      // Layer deletion requires metadata verification unavailable on Windows.
      const canPurgeLayer = process.platform !== 'win32';
      assert(typeof w.result.base === 'string');
      const base = join(ctx.repo, '.git', 'graft', 'bases', w.result.base);
      const packages = ['npm', 'pnpm', 'bun', 'cargo', 'uv', 'sccache'].map(name => join(cacheDir, name));
      const removable = [...packages, ...(canPurgeLayer ? [verified] : []), ...(existsSync(base) ? [base] : [])];
      const blocked = [foreignCache, foreignLayer, changed, ...(canPurgeLayer ? [] : [verified])];
      const candidates = array(preview.candidates);
      for (const path of removable) assert(candidates.includes(path), `Expected a purge candidate at ${path}`);
      for (const path of blocked) assert(!candidates.includes(path), `Unexpected purge candidate at ${path}`);
      const applied = object(ctx.cli(['prune', '--purge-cache', '--apply']).cache);
      assert.equal(applied.purged, true);
      assert.deepEqual(array(applied.removed).sort(), candidates.sort());
      for (const report of [preview, applied]) {
        const retained = array(report.blocked).map(row => object(row).path);
        for (const path of blocked) assert(retained.includes(path), `Expected blocked cache at ${path}`);
      }
      assert.equal(existsSync(base), false);
      assert.equal(existsSync(npm), false);
      assert.equal(existsSync(verified), !canPurgeLayer);
      assert.equal(readFileSync(foreignCache, 'utf8'), 'user cache note');
      assert.equal(readFileSync(foreignLayer, 'utf8'), 'user layer note');
      assert.equal(readFileSync(change, 'utf8'), 'preserve changed layer');
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
        assert.equal(ctx.fail(['remove', 'agent', '--lease', w.lease, '--discard']).code, 'workspace_busy');
        assert.equal(ctx.fail(['move', 'agent', join(ctx.root, 'busy move'), '--lease', w.lease]).code, 'workspace_busy');
        assert.ok(existsSync(w.path));
        const peer = ctx.spawn('peer');
        drop(ctx, 'peer', peer.lease);
        assert.equal(existsSync(peer.path), false);
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
    name: 'hidden index flags protect local edits from prune and land',
    run(ctx) {
      const w = ctx.spawn('agent');
      const base = ctx.git(['rev-parse', 'HEAD']);
      for (const flag of ['assume-unchanged', 'skip-worktree']) {
        writeFileSync(join(w.path, 'tracked.txt'), flag + '\n');
        ctx.git(['update-index', '--' + flag, 'tracked.txt'], w.path);
        assert.equal(ctx.fail(['remove', 'agent', '--lease', w.lease]).code, 'hidden_changes');
        assert.equal(ctx.fail(['merge', 'agent', '--lease', w.lease]).code, 'hidden_changes');
        assert.equal(readFileSync(join(w.path, 'tracked.txt'), 'utf8'), flag + '\n');
        assert.equal(ctx.git(['rev-parse', 'HEAD']), base);
        ctx.git(['update-index', '--no-' + flag, 'tracked.txt'], w.path);
      }
      drop(ctx, 'agent', w.lease, '--discard');
      ctx.cli(['restore', 'agent', '--lease', w.lease]);
      assert.equal(readFileSync(join(w.path, 'tracked.txt'), 'utf8'), 'skip-worktree\n');
    },
  },
  {
    name: 'symlink redirection is refused',
    run(ctx) {
      const w = ctx.spawn('agent');
      const moved = join(ctx.root, 'moved');
      renameSync(w.path, moved);
      try {
        symlinkSync(moved, w.path, process.platform === 'win32' ? 'junction' : 'dir');
      } catch (error) {
        ctx.unavailable(`Cannot create directory symlink to verify redirection protection: ${String(error)}`);
      }
      try {
        ctx.fail(['remove', 'agent', '--lease', w.lease, '--discard']);
        assert.ok(existsSync(moved));
      } finally {
        unlinkSync(w.path);
      }
      renameSync(moved, w.path);
      drop(ctx, 'agent', w.lease);
    },
  },
  {
    name: 'graft request id survives disconnect and rejects adoption',
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
      ctx.fail(['add', 'agent', '--request-id', other]);
      ctx.fail(['add', 'agent', '--request-id', id, '--base', 'main']);
      ctx.fail(['add', 'agent', '--request-id', id, '--sparse', 'different']);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], first.path), original);
      drop(ctx, 'agent', id);
      ctx.fail(['add', 'another', '--request-id', id]);
    },
  },
  {
    name: 'concurrent retries of the same graft creation reserve one identity',
    async run(ctx) {
      const id = '80dd84ae-2182-42d5-ad09-1b14d6d9480a';
      const args = ['--repo', ctx.repo, '--json', 'add', 'concurrent', '--request-id', id];
      const spawn = promisify(execFile);
      const attempts = await Promise.allSettled(Array.from({ length: 3 }, () =>
        spawn(ctx.bin, args, { cwd: ctx.repo, env: ctx.env, timeout: 30_000 })));
      const results = attempts.map(attempt => {
        assert.equal(attempt.status, 'fulfilled', attempt.status === 'rejected' ? String(attempt.reason) : undefined);
        return object(JSON.parse(attempt.value.stdout));
      });
      const [first, ...retries] = results;
      assert.equal(first.lease, id);
      assert(typeof first.path === 'string');
      for (const retry of retries) assert.deepEqual(retry, first);
      assert.deepEqual(names(ctx.cli(['list']).workspaces), ['concurrent']);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], first.path), ctx.git(['rev-parse', 'HEAD']));
      ctx.git(['fsck', '--full'], first.path);
      drop(ctx, 'concurrent', id);
      ctx.coverCli('add');
    },
  },
  {
    name: 'prune replay and reused slug reject stale lease',
    run(ctx) {
      const first = ctx.spawn('agent');
      const result = drop(ctx, 'agent', first.lease);
      assert.deepEqual(drop(ctx, 'agent', first.lease), result);
      const next = ctx.spawn('agent');
      assert.notEqual(first.lease, next.lease);
      ctx.fail(['heartbeat', 'agent', '--lease', first.lease]);
      assert.deepEqual(drop(ctx, 'agent', first.lease, '--discard'), result);
      ctx.fail(['restore', 'agent', '--lease', first.lease]);
      assert.ok(existsSync(next.path));
      assert.equal(ctx.cli(['status', 'agent']).id, next.result.id);
      ctx.cli(['heartbeat', 'agent', '--lease', next.lease]);
      drop(ctx, 'agent', next.lease);
    },
  },
];
