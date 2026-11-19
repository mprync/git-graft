/** Public registration and reuse contracts, using a local synthetic package manager. */
import assert from 'node:assert/strict';
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, symlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { objectResult, type Scenario } from './harness.ts';

const manager = 'graft-fixture-manager';

type Context = Parameters<Scenario['run']>[0];
function registration(ctx: Context, extra: string[] = []) {
  const script = join(ctx.root, 'fixture manager.cjs');
  writeFileSync(script, `const fs = require('node:fs');
const path = require('node:path');
const [literal, cache, ...args] = process.argv.slice(2);
if (literal !== 'literal with spaces' || cache !== process.env.FIXTURE_CACHE || cache !== process.env.GRAFT_PACKAGE_CACHE) process.exit(9);
fs.mkdirSync(cache, {recursive:true});
const download = path.join(cache, 'download');
if (!fs.existsSync(download)) { fs.writeFileSync(download, 'local package'); fs.appendFileSync(path.join(cache, 'downloads'), 'download\\n'); }
fs.writeFileSync('private-output', args.join('|'));
process.stdout.write(JSON.stringify({cache, args}));
`);
  return ['register-manager', manager, '--cache-env', 'FIXTURE_CACHE',
    '--cache-arg', script, '--cache-arg', 'literal with spaces', '--cache-arg', '{cache}', ...extra];
}
function execute(ctx: Context, ws: ReturnType<Context['spawn']>, exe: string, explicit: boolean) {
  const result = ctx.cli(['exec', String(ws.result.name), '--lease', ws.lease,
    ...(explicit ? ['--manager', manager] : []), '--', exe, 'argument with spaces', '$literal']);
  assert.equal(result.success, true);
  return objectResult(JSON.parse(String(result.stdout)));
}
function registered(receipt: Record<string, unknown>) {
  const storage = objectResult(receipt.storage);
  assert(Array.isArray(storage.registered_dependencies));
  return storage.registered_dependencies.map(objectResult).find(item => item.manager === manager);
}

export const cases: Scenario[] = [{
  name: 'registered cache is project-scoped and shared by explicit and implicit execution',
  run(ctx) {
    const args = registration(ctx);
    const first = ctx.cli(args);
    assert.equal(first.manager, manager);
    assert(typeof first.cache_dir === 'string');
    const cache = first.cache_dir;
    assert.equal(ctx.cli(args).cache_dir, cache);
    const listed = ctx.cli(['list-managers']);
    assert(Array.isArray(listed.managers));
    assert(listed.managers.some(item => objectResult(item).manager === manager && objectResult(item).cache_dir === cache));
    const executable = join(ctx.root, manager + (process.platform === 'win32' ? '.exe' : ''));
    copyFileSync(process.execPath, executable);
    const a = ctx.spawn('cache-a');
    const b = ctx.spawn('cache-b');
    assert.deepEqual(execute(ctx, a, executable, true).args, ['argument with spaces', '$literal']);
    assert.deepEqual(execute(ctx, b, executable, false).args, ['argument with spaces', '$literal']);
    assert.equal(readFileSync(join(cache, 'downloads'), 'utf8'), 'download\n');
    assert.equal(readFileSync(join(a.path, 'private-output'), 'utf8'), 'argument with spaces|$literal');
    writeFileSync(join(a.path, 'private-output'), 'changed');
    assert.equal(readFileSync(join(b.path, 'private-output'), 'utf8'), 'argument with spaces|$literal');
    const other = join(ctx.root, 'other-project');
    mkdirSync(other);
    ctx.git(['init', '-q', '-b', 'main'], other);
    writeFileSync(join(other, 'tracked'), 'other');
    ctx.git(['add', '.'], other); ctx.git(['commit', '-qm', 'other'], other);
    assert.deepEqual(ctx.cli(['list-managers'], other).managers, []);
    assert.notEqual(ctx.cli(args, other).cache_dir, cache);
    assert.deepEqual(ctx.cli(['unregister-manager', manager]), { manager, registered: false, cache_retained: true });
    assert.equal(readFileSync(join(cache, 'download'), 'utf8'), 'local package');
    assert.equal(readFileSync(join(a.path, 'private-output'), 'utf8'), 'changed');
  },
}, {
  name: 'registration authenticates a leased seed and rejects foreign paths',
  run(ctx) {
    writeFileSync(join(ctx.repo, 'fixture.lock'), 'locked\n');
    writeFileSync(join(ctx.repo, '.gitignore'), 'vendor/\n');
    ctx.git(['add', 'fixture.lock', '.gitignore']); ctx.git(['commit', '-qm', 'seed inputs']);
    const source = ctx.spawn('seed-source', ['--no-deps']);
    mkdirSync(join(source.path, 'vendor/bundle'), { recursive: true });
    writeFileSync(join(source.path, 'vendor/bundle/package'), 'workspace seed');
    const base = registration(ctx, ['--install-dir', 'vendor/bundle', '--input', 'fixture.lock', '--key', 'seed']);
    ctx.fail([...base, '--workspace', 'seed-source', '--lease', 'wrong']);
    ctx.fail([...base, '--workspace', 'seed-source']);
    const receipt = ctx.cli([...base, '--workspace', 'seed-source', '--lease', source.lease]);
    assert.equal(receipt.manager, manager);
    const next = ctx.spawn('seed-consumer');
    const native = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
    if (native) {
      assert.equal(registered(next.result)?.status, 'attached');
      assert.equal(readFileSync(join(next.path, 'vendor/bundle/package'), 'utf8'), 'workspace seed');
    } else assert.notEqual(registered(next.result)?.status, 'attached');
    const outside = join(ctx.root, 'outside');
    mkdirSync(outside);
    writeFileSync(join(outside, 'keep'), 'safe');
    ctx.fail(registration(ctx, ['--install-dir', '../outside', '--input', 'fixture.lock', '--key', 'bad']));
    try { symlinkSync(outside, join(ctx.repo, 'vendor'), process.platform === 'win32' ? 'junction' : 'dir'); }
    catch (error) {
      if (process.platform === 'win32' && error instanceof Error && 'code' in error && ['EPERM', 'EACCES'].includes(String(error.code))) return;
      throw error;
    }
    ctx.fail(registration(ctx, ['--install-dir', 'vendor', '--input', 'fixture.lock', '--key', 'foreign']));
    assert.equal(readFileSync(join(outside, 'keep'), 'utf8'), 'safe');
  },
}, {
  name: 'registered install trees attach only when safe and inputs match',
  run(ctx) {
    writeFileSync(join(ctx.repo, 'fixture.lock'), 'version one\n');
    writeFileSync(join(ctx.repo, '.gitignore'), 'vendor/\n');
    ctx.git(['add', 'fixture.lock', '.gitignore']); ctx.git(['commit', '-qm', 'fixture inputs']);
    const tree = join(ctx.repo, 'vendor', 'bundle');
    mkdirSync(tree, { recursive: true });
    writeFileSync(join(tree, 'package'), 'original package');
    const args = registration(ctx, ['--install-dir', 'vendor/bundle', '--input', 'fixture.lock', '--key', 'fixture-v1']);
    const native = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
    assert.equal(typeof native, 'boolean');
    const registeredManager = ctx.cli(args);
    const a = ctx.spawn('install-a');
    const receipt = registered(a.result);
    assert(receipt, 'missing registered dependency receipt');
    if (native) {
      assert.equal(receipt.status, 'attached');
      const b = ctx.spawn('install-b');
      assert.equal(registered(b.result)?.status, 'attached');
      assert.equal(readFileSync(join(a.path, 'vendor/bundle/package'), 'utf8'), 'original package');
      writeFileSync(join(a.path, 'vendor/bundle/package'), 'private edit');
      assert.equal(readFileSync(join(b.path, 'vendor/bundle/package'), 'utf8'), 'original package');
      assert.equal(readFileSync(join(tree, 'package'), 'utf8'), 'original package');
      const opted = ctx.spawn('install-opted', ['--no-deps']);
      assert(!existsSync(join(opted.path, 'vendor/bundle')));
      const layers = join(ctx.repo, '.git', 'graft', 'registered-installs');
      const generations = readdirSync(layers).filter(name => /^[a-f0-9]{64}$/.test(name));
      assert.equal(generations.length, 1);
      const generation = join(layers, generations[0]);
      const cached = join(generation, 'tree', 'package');
      const provenance = join(generation, 'provenance.json');
      const contents = readFileSync(cached);
      const proof = readFileSync(provenance);
      writeFileSync(cached, 'tampered package');
      const tampered = ctx.spawn('install-tampered');
      assert.notEqual(registered(tampered.result)?.status, 'attached');
      assert(!existsSync(join(tampered.path, 'vendor/bundle/package')));
      writeFileSync(cached, contents);
      writeFileSync(provenance, '{"forged":true}');
      const forged = ctx.spawn('install-forged');
      assert.notEqual(registered(forged.result)?.status, 'attached');
      assert(!existsSync(join(forged.path, 'vendor/bundle/package')));
      writeFileSync(provenance, proof);
      writeFileSync(join(tree, 'package'), 'tracked collision');
      ctx.git(['add', '-f', 'vendor/bundle/package']);
      ctx.git(['commit', '-qm', 'tracked install collision']);
      const collision = ctx.spawn('install-collision');
      assert.notEqual(registered(collision.result)?.status, 'attached');
      assert.equal(readFileSync(join(collision.path, 'vendor/bundle/package'), 'utf8'), 'tracked collision');
      ctx.git(['rm', '--cached', '-q', 'vendor/bundle/package']);
      ctx.git(['commit', '-qm', 'remove tracked collision']);
      writeFileSync(join(tree, 'package'), 'original package');
      writeFileSync(join(ctx.repo, 'fixture.lock'), 'version two\n');
      ctx.git(['add', 'fixture.lock']); ctx.git(['commit', '-qm', 'change input']);
      const missed = ctx.spawn('install-miss');
      assert.notEqual(registered(missed.result)?.status, 'attached');
      assert(!existsSync(join(missed.path, 'vendor/bundle')));
    } else {
      assert.notEqual(receipt.status, 'attached');
      assert(!existsSync(join(a.path, 'vendor/bundle')));
    }
    const cache = String(registeredManager.cache_dir);
    ctx.cli(['unregister-manager', manager]);
    assert(existsSync(cache));
    assert.equal(readFileSync(join(tree, 'package'), 'utf8'), 'original package');
  },
}];
