import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join } from 'node:path';
import type { Context, Scenario } from './harness.ts';
import { npmArchive } from './dependencies.ts';

// Use a real npm install and an isolated cache containing only the fixture.
function npm(ctx: Context, args: string[]): void {
  let program = 'npm';
  if (process.platform === 'win32') {
    const pathValue = Object.entries(ctx.env).find(([key]) => key.toUpperCase() === 'PATH')?.[1] ?? '';
    const dir = pathValue.split(delimiter).find(part => existsSync(join(part, 'node.exe')) &&
      existsSync(join(part, 'node_modules', 'npm', 'bin', 'npm-cli.js')));
    if (!dir) ctx.unavailable('npm and node.exe are required');
    program = join(dir, 'node.exe');
    args = [join(dir, 'node_modules', 'npm', 'bin', 'npm-cli.js'), ...args];
  }
  const result = ctx.command(program, args);
  if (result.error && 'code' in result.error && result.error.code === 'ENOENT') ctx.unavailable('npm is required');
  assert.equal(result.status, 0, result.error?.message ?? result.stderr);
}

export const cases: Scenario[] = [{
  name: 'seed a real primary install and restore an independent verified layer',
  run(ctx) {
    const fixture = npmArchive(ctx.root);
    const manifest = { name: 'consumer', version: '1.0.0', dependencies: { 'graft-fixture': '1.0.0' } };
    const lock = { name: 'consumer', version: '1.0.0', lockfileVersion: 3, packages: {
      '': manifest, 'node_modules/graft-fixture': { version: '1.0.0',
        resolved: 'https://127.0.0.1:1/graft-fixture.tgz', integrity: fixture.integrity },
    } };
    writeFileSync(join(ctx.repo, 'package.json'), JSON.stringify(manifest));
    writeFileSync(join(ctx.repo, 'package-lock.json'), JSON.stringify(lock));
    ctx.git(['add', 'package.json', 'package-lock.json']);
    ctx.git(['commit', '-qm', 'locked fixture']);
    const cache = join(ctx.root, 'npm-cache');
    npm(ctx, ['cache', 'add', fixture.path, '--cache', cache]);
    npm(ctx, ['ci', '--offline', '--ignore-scripts', '--cache', cache]);
    const original = join(ctx.repo, 'node_modules', 'graft-fixture', 'index.js');
    assert.equal(readFileSync(original, 'utf8'), 'module.exports = 42;\n');

    writeFileSync(join(ctx.repo, 'tracked.txt'), 'dirty');
    ctx.fail(['seed-layer', '--key', 'fixture']);
    writeFileSync(join(ctx.repo, 'tracked.txt'), 'initial\n');
    const saved = ctx.cli(['seed-layer', '--key', 'fixture']);
    assert.equal(typeof saved.digest, 'string');
    assert(Number(saved.logical_bytes) > 0);
    ctx.fail(['seed-layer', '--key', 'fixture']);
    const ws = ctx.spawn('consumer');
    const layer = (action: string, key = 'fixture') => ['layer', 'consumer', '--lease', ws.lease, action, '--key', key];
    ctx.fail(layer('restore', 'different'));
    const state = dirname(String(ctx.cli(['env', 'consumer']).cache_dir));
    const cached = join(state, 'layers', String(saved.digest), 'node_modules', 'graft-fixture', 'index.js');
    const bytes = readFileSync(cached);
    writeFileSync(cached, 'tampered');
    ctx.fail(layer('restore'));
    assert(!existsSync(join(ws.path, 'node_modules')));
    writeFileSync(cached, bytes);
    mkdirSync(join(ws.path, 'node_modules'));
    writeFileSync(join(ws.path, 'node_modules', 'keep'), 'keep');
    ctx.fail(layer('restore'));
    assert.equal(readFileSync(join(ws.path, 'node_modules', 'keep'), 'utf8'), 'keep');
    rmSync(join(ws.path, 'node_modules'), { recursive: true });
    const restored = ctx.cli(layer('restore'));
    assert.equal(restored.digest, saved.digest);
    const installed = join(ws.path, 'node_modules', 'graft-fixture', 'index.js');
    writeFileSync(installed, 'changed');
    assert.equal(readFileSync(original, 'utf8'), 'module.exports = 42;\n');
    assert.deepEqual(readFileSync(cached), bytes);
    ctx.measure('seed_backend', saved.backend);
    ctx.measure('restore_backend', restored.backend);
  },
}, {
  name: 'seed refuses a replaced dependency tree without touching its target',
  run(ctx) {
    writeFileSync(join(ctx.repo, 'package.json'), '{"name":"fixture","version":"1.0.0"}');
    writeFileSync(join(ctx.repo, 'package-lock.json'), '{"lockfileVersion":3}');
    ctx.git(['add', 'package.json', 'package-lock.json']);
    ctx.git(['commit', '-qm', 'manifest']);
    const outside = join(ctx.root, 'outside');
    mkdirSync(outside);
    writeFileSync(join(outside, 'keep'), 'keep');
    try { symlinkSync(outside, join(ctx.repo, 'node_modules'), process.platform === 'win32' ? 'junction' : 'dir'); }
    catch (error) {
      if (process.platform === 'win32' && error instanceof Error && 'code' in error && ['EPERM', 'EACCES'].includes(String(error.code)))
        ctx.unavailable('Creating a directory junction requires permission');
      throw error;
    }
    ctx.fail(['seed-layer', '--key', 'fixture']);
    assert.equal(readFileSync(join(outside, 'keep'), 'utf8'), 'keep');
  },
}];
