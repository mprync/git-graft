import assert from 'node:assert/strict';
import { execFile, spawn as spawnProcess } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, renameSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { createInterface } from 'node:readline';
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
/** Expire a graft's lease so prune considers it, as 24 idle hours would. */
function expire(ctx: Context, id: unknown): void {
  assert(typeof id === 'string');
  const file = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', `${id}.json`);
  writeFileSync(file, JSON.stringify({ ...object(JSON.parse(readFileSync(file, 'utf8'))), heartbeat: 0 }));
}
/** Every ref, the stash, the index and every file status of a checkout. */
function work(ctx: Context, path: string): string {
  return [
    ctx.git(['for-each-ref', '--format=%(refname) %(objectname)'], path),
    ctx.git(['rev-parse', 'HEAD'], path),
    ctx.git(['ls-files', '--stage'], path),
    ctx.git(['status', '--porcelain=v1', '--ignored=matching'], path),
  ].join('\n--\n');
}
function reasons(result: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(array(result.retained).map(object).map((item) => [item.name, item.reason]));
}
/** One MCP tool call through the shipped server: initialize, then tools/call. */
async function mcpTool(ctx: Context, name: string, args: Record<string, unknown>): Promise<{ isError: boolean; content: Record<string, unknown> }> {
  const child = spawnProcess(ctx.bin, ['--repo', ctx.repo, 'mcp'], { cwd: ctx.repo, env: ctx.env, stdio: ['pipe', 'pipe', 'pipe'] });
  const closed = new Promise<void>((resolve) => child.once('close', () => resolve()));
  const lines = createInterface({ input: child.stdout, crlfDelay: Infinity })[Symbol.asyncIterator]();
  const reply = async (id: number): Promise<Record<string, unknown>> => {
    const timeout = delay(30_000).then(() => { throw new Error(`MCP request ${id} timed out`); });
    for (;;) {
      const next = await Promise.race([lines.next(), timeout]);
      assert(!next.done, 'MCP server closed early');
      const message = object(JSON.parse(next.value));
      if (message.id === id) return object(message.result);
    }
  };
  try {
    const send = (message: Record<string, unknown>) => child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', ...message })}\n`);
    send({ id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'lifecycle-e2e', version: '1.0.0' } } });
    await reply(1);
    send({ method: 'notifications/initialized' });
    send({ id: 2, method: 'tools/call', params: { name, arguments: args } });
    const result = await reply(2);
    ctx.coverTool(name);
    return { isError: result.isError === true, content: object(result.structuredContent) };
  } finally {
    child.stdin.end();
    await Promise.race([closed, delay(10_000).then(() => child.kill())]);
  }
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
  {
    name: 'remove keeps a stash, branches, tags and detached commits unless discard is given',
    async run(ctx) {
      const cases: [string, (path: string) => void, RegExp][] = [
        ['stash', (path) => {
          writeFileSync(join(path, 'tracked.txt'), 'stashed edit\n');
          ctx.git(['stash', 'push', '-q'], path);
        }, /a stash/],
        ['branch', (path) => ctx.git(['branch', 'extra'], path), /branch extra/],
        ['tag', (path) => ctx.git(['tag', 'v1'], path), /tag v1/],
        ['detached', (path) => {
          ctx.git(['checkout', '-q', '--detach'], path);
          writeFileSync(join(path, 'tracked.txt'), 'detached commit\n');
          commit(ctx, path, 'detached');
        }, /HEAD is detached, commits on the detached HEAD/],
        ['committed', (path) => {
          writeFileSync(join(path, 'tracked.txt'), 'unmerged commit\n');
          commit(ctx, path, 'unmerged');
        }, /commits on branch graft\/committed/],
      ];
      for (const [name, setup, found] of cases) {
        const w = ctx.spawn(name);
        setup(w.path);
        const before = work(ctx, w.path);
        const refused = ctx.fail(['remove', name, '--lease', w.lease]);
        assert.equal(refused.code, 'unmerged_work', name);
        assert.match(String(refused.message), found);
        assert.equal(String(refused.message).includes(w.lease), false);
        const tool = await mcpTool(ctx, 'graft_remove', { name, lease: w.lease });
        assert.equal(tool.isError, true, name);
        assert.equal(tool.content.code, 'unmerged_work');
        assert.match(String(tool.content.message), found);
        assert.equal(work(ctx, w.path), before, `remove changed ${name}`);
        const receipt = name === 'stash'
          ? (await mcpTool(ctx, 'graft_remove', { name, lease: w.lease, discard: true })).content
          : drop(ctx, name, w.lease, '--discard');
        assert.equal(receipt.action, 'remove');
        assert.equal(existsSync(w.path), false);
        const restored = ctx.cli(['restore', name, '--lease', w.lease]);
        assert.equal(work(ctx, String(restored.path)), before, `the archive lost work of ${name}`);
      }
    },
  },
  {
    name: 'prune previews and applies only expired grafts with nothing unmerged or running',
    async run(ctx) {
      const clean = ctx.spawn('clean');
      const ignored = ctx.spawn('ignored');
      mkdirSync(join(ignored.path, '.git', 'info'), { recursive: true });
      writeFileSync(join(ignored.path, '.git', 'info', 'exclude'), 'build/\n');
      mkdirSync(join(ignored.path, 'build'));
      writeFileSync(join(ignored.path, 'build', 'output'), 'build output');
      const tagged = ctx.spawn('tagged');
      ctx.git(['tag', 'keep'], tagged.path);
      const locked = ctx.spawn('locked');
      ctx.cli(['lock', 'locked', '--lease', locked.lease, '--reason', 'keep']);
      const running = ctx.spawn('running');
      const started = join(ctx.root, 'started');
      const release = join(ctx.root, 'release');
      const script = `const fs=require('node:fs'); fs.writeFileSync(process.argv[1],'started'); const end=Date.now()+30000; const timer=setInterval(()=>{if(fs.existsSync(process.argv[2])){clearInterval(timer);process.exit(0)} if(Date.now()>end){clearInterval(timer);process.exit(2)}},25)`;
      const child = spawnProcess(ctx.bin, ['--repo', ctx.repo, '--json', 'exec', 'running', '--lease', running.lease, '--', process.execPath, '-e', script, started, release], { env: ctx.env, stdio: ['ignore', 'pipe', 'pipe'] });
      let stderr = '';
      child.stderr.setEncoding('utf8').on('data', (data: string) => { stderr += data; });
      child.stdout.resume();
      const exit = new Promise<number | null>((resolve, reject) => {
        child.once('error', reject);
        child.once('close', resolve);
      });
      try {
        const deadline = Date.now() + 10000;
        while (!existsSync(started) && Date.now() < deadline) await delay(25);
        assert.ok(existsSync(started), `exec did not start: ${stderr}`);
        const grafts = [clean, ignored, tagged, locked, running];
        for (const w of grafts) expire(ctx, w.result.id);
        const kept = [ignored, tagged, locked, running].map((w) => [w, work(ctx, w.path)] as const);
        const expected = { clean: 'eligible', ignored: 'dirty', locked: 'locked', running: 'busy', tagged: 'changed' };
        const preview = ctx.cli(['prune']);
        assert.equal(preview.applied, false);
        assert.deepEqual(reasons(preview), expected);
        assert.equal('damaged_records' in preview, false);
        assert.ok(existsSync(clean.path));
        const applied = ctx.cli(['prune', '--apply']);
        assert.deepEqual(reasons(applied), { ...expected, clean: 'archived' });
        assert.equal(JSON.stringify([preview, applied]).includes(clean.lease), false, 'prune disclosed a lease');
        assert.equal(existsSync(clean.path), false);
        for (const [w, before] of kept) assert.equal(work(ctx, w.path), before, `prune changed ${w.path}`);
        assert.equal(readFileSync(join(ignored.path, 'build', 'output'), 'utf8'), 'build output');
        writeFileSync(release, 'release');
        assert.equal(await exit, 0, stderr);
      } finally {
        if (!existsSync(release)) writeFileSync(release, 'release');
        if (child.exitCode === null && child.signalCode === null) child.kill();
        await exit.catch(() => undefined);
      }
      ctx.cli(['restore', 'clean', '--lease', clean.lease]);
    },
  },
  {
    name: 'an agent whose graft was pruned is told it was archived and can restore it',
    async run(ctx) {
      const w = ctx.spawn('idle');
      const head = ctx.git(['rev-parse', 'HEAD'], w.path);
      expire(ctx, w.result.id);
      assert.equal(reasons(ctx.cli(['prune', '--apply'])).idle, 'archived');
      for (const args of [
        ['heartbeat', 'idle', '--lease', w.lease],
        ['exec', 'idle', '--lease', w.lease, '--', process.execPath, '-e', ''],
        ['merge', 'idle', '--lease', w.lease],
        ['lock', 'idle', '--lease', w.lease],
      ]) {
        const error = ctx.fail(args);
        assert.equal(error.code, 'graft_archived', args[0]);
        assert.match(String(error.message), /archived by remove, or by prune/);
        assert.match(String(error.message), /git graft restore idle --lease/);
        assert.equal(object(error.details).archived_by, 'remove_or_prune');
        assert.equal(JSON.stringify(error).includes(w.lease), false, `${args[0]} disclosed the lease`);
      }
      const tool = await mcpTool(ctx, 'graft_renew', { name: 'idle', lease: w.lease });
      assert.equal(tool.isError, true);
      assert.equal(tool.content.code, 'graft_archived');
      const restored = ctx.cli(['restore', 'idle', '--lease', w.lease]);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], String(restored.path)), head);
      ctx.cli(['heartbeat', 'idle', '--lease', String(restored.lease)]);
    },
  },
  {
    name: 'remove discard after a merge landed reports the later commits as unmerged',
    run(ctx) {
      const w = ctx.spawn('advanced');
      writeFileSync(join(w.path, 'feature.txt'), 'merged\n');
      commit(ctx, w.path, 'merged feature');
      const tip = ctx.git(['rev-parse', 'HEAD'], w.path);
      // A post-merge hook commits in the graft after the destination updates.
      const hook = join(ctx.repo, '.git', 'hooks', 'post-merge');
      mkdirSync(join(ctx.repo, '.git', 'hooks'), { recursive: true });
      writeFileSync(hook, '#!/bin/sh\nset -eu\nunset GIT_DIR GIT_INDEX_FILE GIT_WORK_TREE GIT_PREFIX\ncd "$GRAFT_E2E_GRAFT"\ngit commit -q --allow-empty -m "later work"\n', { mode: 0o700 });
      ctx.env.GRAFT_E2E_GRAFT = process.platform === 'win32' ? w.path.replaceAll('\\', '/') : w.path;
      try {
        ctx.fail(['merge', 'advanced', '--lease', w.lease]);
      } finally {
        delete ctx.env.GRAFT_E2E_GRAFT;
        rmSync(hook);
      }
      assert.equal(ctx.git(['rev-parse', 'main']), tip);
      const later = ctx.git(['rev-parse', 'HEAD'], w.path);
      assert.notEqual(later, tip, 'the hook did not commit in the graft');
      const receipt = drop(ctx, 'advanced', w.lease, '--discard');
      assert.equal(receipt.action, 'remove');
      assert.equal(receipt.unmerged_commits, true);
      assert.equal(receipt.head, later);
      assert.equal(receipt.graft_tip, tip);
      assert.equal(receipt.commit, tip);
      assert.deepEqual(drop(ctx, 'advanced', w.lease, '--discard'), receipt);
      assert.equal(ctx.fail(['heartbeat', 'advanced', '--lease', w.lease]).code, 'graft_archived');
      const restored = ctx.cli(['restore', 'advanced', '--lease', w.lease]);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], String(restored.path)), later);
    },
  },
  {
    name: 'a torn record is reported and kept while other grafts keep working',
    run(ctx) {
      const torn = ctx.spawn('torn');
      const other = ctx.spawn('other');
      assert(typeof torn.result.id === 'string');
      const file = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', `${torn.result.id}.json`);
      const bytes = readFileSync(file);
      writeFileSync(file, bytes.subarray(0, bytes.length >> 1));
      const damaged = readFileSync(file);
      const listed = ctx.cli(['list']);
      assert.deepEqual(names(listed.workspaces), ['other']);
      const report = array(listed.damaged_records).map(object);
      assert.equal(report.length, 1);
      assert.equal(report[0].file, file);
      assert.equal(JSON.stringify(listed).includes(torn.lease), false, 'list disclosed a damaged record lease');
      assert.equal(ctx.fail(['heartbeat', 'torn', '--lease', torn.lease]).code, 'record_damaged');
      expire(ctx, other.result.id);
      const pruned = ctx.cli(['prune', '--apply']);
      assert.deepEqual(reasons(pruned), { other: 'archived' });
      assert.equal(array(pruned.damaged_records).length, 1);
      ctx.cli(['restore', 'other', '--lease', other.lease]);
      assert.deepEqual(readFileSync(file), damaged);
      assert.equal(readFileSync(join(torn.path, 'tracked.txt'), 'utf8'), 'initial\n');
    },
  },
];
