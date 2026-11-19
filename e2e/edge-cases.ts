import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join } from 'node:path';
import type { Scenario } from './harness.ts';

type Context = Parameters<Scenario['run']>[0];
type Workspace = ReturnType<Context['spawn']>;
function layer(workspace: Workspace, name: string, action: string, key = 'node-v1'): string[] {
  return ['layer', name, '--lease', workspace.lease, action, '--key', key, ...(action === 'forget' ? [] : ['--copy'])];
}
function write(path: string, data: string): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, data);
}
function commit(ctx: Context, message: string): void {
  ctx.git(['add', '-A']);
  ctx.git(['commit', '-qm', message]);
}
function npmProject(ctx: Context, manifest: Record<string, unknown> = {}): void {
  write(join(ctx.repo, 'package.json'), JSON.stringify({ name: 'fixture', version: '1.0.0', ...manifest }) + '\n');
  write(join(ctx.repo, 'package-lock.json'), JSON.stringify({ name: 'fixture', version: '1.0.0', lockfileVersion: 3, requires: true, packages: { '': { name: 'fixture', version: '1.0.0' } } }) + '\n');
  commit(ctx, 'npm project');
}
function object(value: unknown): Record<string, unknown> {
  assert.ok(value && typeof value === 'object' && !Array.isArray(value));
  return Object.fromEntries(Object.entries(value));
}

export const cases: Scenario[] = [
  {
    name: 'Git discovers the subcommand from a project subdirectory and accepts a lease from the environment',
    run(ctx) {
      const nested = join(ctx.repo, 'subdirectory');
      mkdirSync(nested);
      const pathKey = Object.keys(ctx.env).find(key => key.toUpperCase() === 'PATH');
      assert(pathKey && ctx.env[pathKey]);
      ctx.env[pathKey] = dirname(ctx.bin) + delimiter + ctx.env[pathKey];
      const created = object(JSON.parse(ctx.git(['graft', 'spawn', 'subcommand'], nested)));
      assert(typeof created.lease === 'string' && typeof created.path === 'string');
      ctx.env.GIT_GRAFT_LEASE = created.lease;
      const heartbeat = object(JSON.parse(ctx.git(['graft', 'heartbeat', 'subcommand'], nested)));
      assert.equal(heartbeat.name, 'subcommand');
      assert.equal(heartbeat.phase, 'ready');
      assert.equal(ctx.git(['rev-parse', '--abbrev-ref', 'HEAD'], created.path), 'graft/subcommand');
      ctx.git(['graft', 'drop', 'subcommand'], nested);
      assert.equal(existsSync(created.path), false);
      assert.equal(ctx.git(['status', '--porcelain']), '');
    },
  },
  {
    name: 'spawn validates portable names and keeps same-named primary branches private',
    run(ctx) {
      const head = ctx.git(['rev-parse', 'HEAD']);
      for (const name of ['../escape', 'CAPS', 'nul', '1agent', 'a'.repeat(49)]) {
        assert.equal(ctx.fail(['spawn', name]).code, 'invalid_name');
      }
      assert.equal(existsSync(join(ctx.root, 'escape')), false);
      ctx.git(['branch', 'graft/occupied']);
      const workspace = ctx.spawn('occupied');
      const owned = ['occupied', '--lease', workspace.lease];
      assert.equal(ctx.git(['branch', '--show-current'], workspace.path), 'graft/occupied');
      write(join(workspace.path, 'tracked.txt'), 'private same-name branch\n');
      ctx.git(['commit', '-qam', 'private commit'], workspace.path);
      ctx.cli(['drop', ...owned, '--discard']);
      ctx.cli(['restore', ...owned]);
      assert.equal(ctx.git(['show', 'graft/occupied:tracked.txt'], workspace.path), 'private same-name branch');
      assert.equal(ctx.git(['rev-parse', 'graft/occupied']), head);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), head);
      assert.equal(ctx.git(['status', '--porcelain']), '');
    },
  },
  {
    name: 'pending creation blocks workspace operations until recovered',
    run(ctx) {
      const ws = ctx.spawn('pending');
      write(join(ws.path, 'untracked'), 'preserve\n');
      assert(typeof ws.result.id === 'string');
      const path = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', ws.result.id + '.json');
      const record = readFileSync(path, 'utf8');
      writeFileSync(path, JSON.stringify({ ...object(JSON.parse(record)), status: 'pending', initialized: false }));
      for (const command of [
        ['heartbeat', 'pending', '--lease', ws.lease],
        ['exec', 'pending', '--lease', ws.lease, '--', process.execPath, '-e', 'process.exit(0)'],
      ]) {
        const fault = ctx.fail(command);
        assert.match(String(fault.message), /pending operation/i);
        assert.equal(ctx.cli(['status', 'pending']).phase, 'creating');
      }
      assert.equal(readFileSync(join(ws.path, 'untracked'), 'utf8'), 'preserve\n');
      writeFileSync(path, record);
      ctx.cli(['heartbeat', 'pending', '--lease', ws.lease]);
      assert.equal(readFileSync(join(ws.path, 'untracked'), 'utf8'), 'preserve\n');
    },
  },
  {
    name: 'layer rejects empty key and tracked dirty source',
    run(ctx) {
      npmProject(ctx);
      const ws = ctx.spawn('dirty-layer');
      write(join(ws.path, 'node_modules', 'fixture', 'index.js'), 'module.exports = 1;\n');
      assert.match(String(ctx.fail(layer(ws, 'dirty-layer', 'save', ' ')).message), /nonempty/i);
      write(join(ws.path, 'tracked.txt'), 'user changes\n');
      assert.match(String(ctx.fail(layer(ws, 'dirty-layer', 'save')).message), /tracked source and index must be clean/i);
      assert.equal(readFileSync(join(ws.path, 'tracked.txt'), 'utf8'), 'user changes\n');
      assert.equal(readFileSync(join(ws.path, 'node_modules', 'fixture', 'index.js'), 'utf8'), 'module.exports = 1;\n');
      write(join(ws.path, 'tracked.txt'), 'initial\n');
      ctx.cli(layer(ws, 'dirty-layer', 'save'));
    },
  },
  {
    name: 'layer rejects multiple root lockfiles without touching installed files',
    run(ctx) {
      npmProject(ctx);
      write(join(ctx.repo, 'yarn.lock'), '# alternate lock\n');
      commit(ctx, 'second lock');
      const ws = ctx.spawn('multiple-locks');
      const module = join(ws.path, 'node_modules', 'fixture', 'index.js');
      write(module, 'keep\n');
      const fault = ctx.fail(layer(ws, 'multiple-locks', 'save'));
      assert.match(String(fault.message), /exactly one root lockfile/i);
      assert.equal(readFileSync(module, 'utf8'), 'keep\n');
      assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
    },
  },
  {
    name: 'layer rejects local dependencies before capturing cache',
    run(ctx) {
      npmProject(ctx, { dependencies: { local: 'file:../local' } });
      const ws = ctx.spawn('local-layer');
      const module = join(ws.path, 'node_modules', 'local', 'index.js');
      write(module, 'keep local\n');
      assert.match(String(ctx.fail(layer(ws, 'local-layer', 'save')).message), /local and workspace dependencies/i);
      assert.equal(readFileSync(module, 'utf8'), 'keep local\n');
    },
  },
  {
    name: 'layer rejects duplicate save and corrupt provenance on restore or forget',
    run(ctx) {
      npmProject(ctx);
      const producer = ctx.spawn('producer');
      const consumer = ctx.spawn('consumer');
      const installed = join(producer.path, 'node_modules', 'fixture', 'index.js');
      write(installed, 'original\n');
      const saved = ctx.cli(layer(producer, 'producer', 'save'));
      assert.equal(typeof saved.digest, 'string');
      assert.match(String(ctx.fail(layer(producer, 'producer', 'save')).message), /already exists/i);
      const env = ctx.cli(['env', 'producer']);
      assert.equal(typeof env.cache_dir, 'string');
      const provenance = join(dirname(String(env.cache_dir)), 'layers', String(saved.digest), 'provenance.json');
      const valid = readFileSync(provenance, 'utf8');
      const broken = object(JSON.parse(valid));
      broken.compatibility = 'wrong';
      writeFileSync(provenance, JSON.stringify(broken));
      for (const action of ['restore', 'forget']) {
        assert.match(String(ctx.fail(layer(consumer, 'consumer', action)).message), /invalid layer provenance/i);
      }
      assert.equal(existsSync(join(consumer.path, 'node_modules')), false);
      assert.equal(readFileSync(installed, 'utf8'), 'original\n');
      assert.equal(readFileSync(provenance, 'utf8'), JSON.stringify(broken));
      writeFileSync(provenance, valid);
      ctx.cli(layer(consumer, 'consumer', 'restore'));
      assert.equal(readFileSync(join(consumer.path, 'node_modules', 'fixture', 'index.js'), 'utf8'), 'original\n');
      ctx.cli(layer(producer, 'producer', 'forget'));
      assert.equal(existsSync(provenance), false);
      assert.equal(readFileSync(installed, 'utf8'), 'original\n');
      rmSync(join(consumer.path, 'node_modules'), { recursive: true });
    },
  },
];
