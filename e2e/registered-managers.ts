/** Public registration and reuse contracts, using a local synthetic package manager. */
import assert from 'node:assert/strict';
import { copyFileSync, cpSync, existsSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
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
  return ['deps','manager','add', manager, '--cache-env', 'FIXTURE_CACHE',
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
function purgeFixture(ctx: Context) {
  writeFileSync(join(ctx.repo, 'fixture.lock'), 'locked\n');
  writeFileSync(join(ctx.repo, '.gitignore'), 'vendor/\n');
  ctx.git(['add', 'fixture.lock', '.gitignore']);
  ctx.git(['commit', '-qm', 'purge fixture']);
  mkdirSync(join(ctx.repo, 'vendor/bundle'), { recursive: true });
  writeFileSync(join(ctx.repo, 'vendor/bundle/package'), 'original package');
  const args = registration(ctx, ['--install-dir', 'vendor/bundle', '--input', 'fixture.lock', '--key', 'purge']);
  ctx.cli(args);
  const native = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
  assert.equal(typeof native, 'boolean');
  ctx.measure('native_clone', native);
  const state = join(ctx.repo, '.git', 'graft');
  const root = join(state, 'registered-installs');
  const generations = existsSync(root) ? readdirSync(root).filter(name => /^[a-f0-9]{64}$/.test(name)) : [];
  return { args, native, state, root, generations };
}

export const cases: Scenario[] = [{
  name: 'land commit refuses a Unicode case-renamed registered installation',
  run(ctx) {
    const probe = join(ctx.root, 'Ä-case-probe');
    mkdirSync(probe);
    const insensitive = existsSync(join(ctx.root, 'ä-case-probe'));
    rmSync(probe, { recursive: true });
    ctx.measure('unicode_case_insensitive', insensitive);
    const native = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
    assert.equal(typeof native, 'boolean');
    ctx.measure('native_clone', native);
    if (!insensitive || !native) return;
    const upper = 'Ä-Packages';
    const lower = 'ä-Packages';
    writeFileSync(join(ctx.repo, '.gitignore'), `/${upper}/\n`);
    writeFileSync(join(ctx.repo, 'fixture.lock'), 'locked\n');
    ctx.git(['add', '.gitignore', 'fixture.lock']); ctx.git(['commit', '-qm', 'Unicode install fixture']);
    mkdirSync(join(ctx.repo, upper));
    writeFileSync(join(ctx.repo, upper, 'package'), 'dependency payload');
    ctx.cli(registration(ctx, ['--install-dir', upper, '--input', 'fixture.lock', '--key', 'unicode']));
    const ws = ctx.spawn('unicode-case');
    assert.equal(registered(ws.result)?.status, 'attached');
    renameSync(join(ws.path, upper), join(ws.path, lower));
    assert(readdirSync(ws.path).some(name => name.normalize('NFC') === lower));
    writeFileSync(join(ws.path, '.gitignore'), '');
    writeFileSync(join(ws.path, 'tracked.txt'), 'uncommitted source\n');
    const primary = ctx.git(['rev-parse', 'HEAD']);
    const workspace = ctx.git(['rev-parse', 'HEAD'], ws.path);
    const staged = ctx.git(['diff', '--cached', '--binary'], ws.path);
    assert.equal(ctx.fail(['land', 'unicode-case', '--lease', ws.lease, '--commit', 'Land source']).code,
      'registered_install_exposed');
    assert.equal(ctx.git(['rev-parse', 'HEAD']), primary);
    assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), workspace);
    assert.equal(ctx.git(['diff', '--cached', '--binary'], ws.path), staged);
    assert.equal(readFileSync(join(ws.path, 'tracked.txt'), 'utf8'), 'uncommitted source\n');
    assert.equal(readFileSync(join(ws.path, lower, 'package'), 'utf8'), 'dependency payload');
    assert.equal(readFileSync(join(ctx.repo, upper, 'package'), 'utf8'), 'dependency payload');
  },
}, {
  name: 'land commit stages source beside protected top-level and nested installs',
  run(ctx) {
    const native = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
    assert.equal(typeof native, 'boolean');
    ctx.measure('native_clone', native);
    if (!native) return;
    writeFileSync(join(ctx.repo, '.gitignore'), '/node_modules/\n/vendor/bundle/\n');
    writeFileSync(join(ctx.repo, 'fixture.lock'), 'locked\n');
    mkdirSync(join(ctx.repo, 'vendor'), { recursive: true });
    writeFileSync(join(ctx.repo, 'node_modules_notes.txt'), 'original top source\n');
    writeFileSync(join(ctx.repo, 'vendor/bundle-notes.txt'), 'original nested source\n');
    ctx.git(['add', '.']); ctx.git(['commit', '-qm', 'fixture source']);
    const base = ctx.git(['rev-parse', 'HEAD']);
    for (const [directory, contents] of [
      ['node_modules', 'top-level dependency payload\n'],
      ['vendor/bundle', 'nested dependency payload\n'],
    ]) {
      mkdirSync(join(ctx.repo, directory), { recursive: true });
      writeFileSync(join(ctx.repo, directory, 'package'), contents);
    }
    const top = registration(ctx, ['--install-dir', 'node_modules', '--input', 'fixture.lock', '--key', 'top']);
    const nested = registration(ctx, ['--install-dir', 'vendor/bundle', '--input', 'fixture.lock', '--key', 'nested']);
    nested[3] = 'nested-fixture-manager';
    ctx.cli(top); ctx.cli(nested);
    const ws = ctx.spawn('protected-land');
    const receipts = objectResult(ws.result.storage).registered_dependencies;
    assert(Array.isArray(receipts));
    for (const name of [manager, 'nested-fixture-manager']) {
      assert.equal(receipts.map(objectResult).find(item => item.manager === name)?.status, 'attached');
    }
    const paths = ['node_modules/package', 'vendor/bundle/package'];
    const payloads = paths.map(path => readFileSync(join(ws.path, path), 'utf8'));
    const blobs = paths.map(path => ctx.git(['hash-object', path], ws.path));
    for (const [index, path] of paths.entries()) {
      assert.equal(readFileSync(join(ctx.repo, path), 'utf8'), payloads[index]);
      assert.equal(ctx.git(['ls-files', '--', path], ws.path), '');
      assert.notEqual(ctx.command('git', ['cat-file', '-e', blobs[index]]).status, 0);
    }
    assert.equal(ctx.git(['show', `${base}:node_modules_notes.txt`]), 'original top source');
    assert.equal(ctx.git(['show', `${base}:vendor/bundle-notes.txt`]), 'original nested source');
    writeFileSync(join(ws.path, 'node_modules_notes.txt'), 'updated top source\n');
    writeFileSync(join(ws.path, 'vendor/bundle-notes.txt'), 'updated nested source\n');
    const result = ctx.cli(['land', 'protected-land', '--lease', ws.lease,
      '--commit', 'Land source with installs', '--discard-ignored']);
    assert.equal(ctx.git(['rev-parse', 'HEAD']), result.commit);
    assert.equal(ctx.git(['show', 'HEAD:node_modules_notes.txt']), 'updated top source');
    assert.equal(ctx.git(['show', 'HEAD:vendor/bundle-notes.txt']), 'updated nested source');
    for (const [index, path] of paths.entries()) {
      assert.equal(ctx.git(['ls-tree', '-r', '--name-only', 'HEAD', '--', path]), '');
      assert.notEqual(ctx.command('git', ['cat-file', '-e', blobs[index]]).status, 0);
      assert.equal(readFileSync(join(ctx.repo, path), 'utf8'), payloads[index]);
    }
  },
}, {
  name: 'manager discovery does not create missing state or replace foreign state',
  run(ctx) {
    const state = join(ctx.repo, '.git', 'graft');
    assert.deepEqual(ctx.cli(['deps','manager','list']).managers, []);
    assert(!existsSync(state));
    mkdirSync(state);
    const foreign = join(state, 'foreign-note');
    writeFileSync(foreign, 'retain foreign state');
    ctx.fail(['deps','manager','list']);
    assert.equal(readFileSync(foreign, 'utf8'), 'retain foreign state');
    assert.deepEqual(readdirSync(state), ['foreign-note']);
  },
}, {
  name: 'manager discovery does not recreate removed cache directories',
  run(ctx) {
    ctx.cli(['deps','manager','add', 'cache-only']);
    const state = join(ctx.repo, '.git', 'graft');
    const cache = join(state, 'cache');
    const layers = join(state, 'layers');
    rmSync(cache, { recursive: true, force: true });
    rmSync(layers, { recursive: true, force: true });
    const list = ctx.cli(['deps','manager','list']);
    assert(Array.isArray(list.managers) && list.managers.some(item => objectResult(item).manager === 'cache-only'));
    assert(!existsSync(cache));
    assert(!existsSync(layers));
  },
}, {
  name: 'checkout-local donor excludes cannot introduce dependencies into a landed commit',
  run(ctx) {
    writeFileSync(join(ctx.repo, 'fixture.lock'), 'locked\n');
    ctx.git(['add', 'fixture.lock']); ctx.git(['commit', '-qm', 'fixture input']);
    mkdirSync(join(ctx.repo, '.git', 'info'), { recursive: true });
    writeFileSync(join(ctx.repo, '.git', 'info', 'exclude'), '/vendor/\n');
    mkdirSync(join(ctx.repo, 'vendor/bundle'), { recursive: true });
    writeFileSync(join(ctx.repo, 'vendor/bundle/package'), 'dependency payload');
    const args = registration(ctx, ['--install-dir', 'vendor/bundle', '--input', 'fixture.lock', '--key', 'exclude']);
    ctx.cli(args);
    const ws = ctx.spawn('local-exclude');
    const attachment = registered(ws.result);
    assert(attachment);
    assert.notEqual(attachment.status, 'attached');
    assert(!existsSync(join(ws.path, 'vendor/bundle/package')));
    writeFileSync(join(ws.path, 'tracked.txt'), 'landed source\n');
    ctx.cli(['land', 'local-exclude', '--lease', ws.lease, '--commit', 'Land source']);
    assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'landed source\n');
    assert.equal(ctx.git(['ls-tree', '-r', '--name-only', 'HEAD', '--', 'vendor']), '');
    assert.equal(readFileSync(join(ctx.repo, 'vendor/bundle/package'), 'utf8'), 'dependency payload');
  },
}, {
  name: 'land commit protects an attached install after workspace ignore rules change',
  run(ctx) {
    const fixture = purgeFixture(ctx);
    if (!fixture.native) return;
    const primary = ctx.git(['rev-parse', 'HEAD']);
    for (const [index, ignored] of ['', 'vendor/*\n!vendor/bundle/\n'].entries()) {
      const name = `ignore-change-${index}`;
      const ws = ctx.spawn(name);
      assert.equal(registered(ws.result)?.status, 'attached');
      const payload = join(ws.path, 'vendor/bundle/package');
      assert.equal(readFileSync(payload, 'utf8'), 'original package');
      writeFileSync(join(ws.path, '.gitignore'), ignored);
      writeFileSync(join(ws.path, 'tracked.txt'), 'workspace source\n');
      const workspaceHead = ctx.git(['rev-parse', 'HEAD'], ws.path);
      assert.equal(ctx.fail(['land', name, '--lease', ws.lease, '--commit', 'Land source']).code,
        'registered_install_exposed');
      assert.equal(ctx.git(['rev-parse', 'HEAD']), primary);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), workspaceHead);
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'initial\n');
      assert.equal(readFileSync(join(ws.path, 'tracked.txt'), 'utf8'), 'workspace source\n');
      assert.equal(readFileSync(payload, 'utf8'), 'original package');
      assert.equal(ctx.git(['ls-files', '--', 'vendor'], ws.path), '');
      // A user may still choose to stage and commit that payload explicitly.
      ctx.git(['add', '--', '.gitignore', 'tracked.txt', 'vendor/bundle/package'], ws.path);
      ctx.git(['commit', '-qm', 'Explicit user commit'], ws.path);
      assert.equal(ctx.git(['show', 'HEAD:vendor/bundle/package'], ws.path), 'original package');
      assert.equal(ctx.git(['rev-parse', 'HEAD']), primary);
    }
  },
}, {
  name: 'invalid foreign registration data cannot block creating a source graft or be overwritten',
  run(ctx) {
    ctx.cli(['doctor']);
    const registry = join(ctx.repo, '.git', 'graft', 'registered-managers-v1.json');
    for (const [index, data] of ['{not json\n', '{"version":999,"managers":{}}\n'].entries()) {
      writeFileSync(registry, data, { mode: 0o600 });
      const ws = ctx.spawn(`foreign-registration-${index}`);
      assert.equal(readFileSync(join(ws.path, 'tracked.txt'), 'utf8'), 'initial\n');
      assert.deepEqual(objectResult(ws.result.storage).registered_dependencies,
        [{ manager: null, status: 'unavailable', reason: 'invalid_registration' }]);
      const argv = [process.execPath, '-e', 'process.stdout.write("source execution")'];
      const exec = ctx.cli(['exec', `foreign-registration-${index}`, '--lease', ws.lease, '--', ...argv]);
      assert.equal(exec.success, true);
      assert.equal(exec.stdout, 'source execution');
      const warning = objectResult(exec.registered_manager);
      assert.equal(warning.status, 'unavailable');
      assert.equal(warning.reason, 'invalid_registration');
      ctx.fail(['exec', `foreign-registration-${index}`, '--lease', ws.lease,
        '--manager', 'foreign-fixture', '--', ...argv]);
      assert.equal(readFileSync(registry, 'utf8'), data);
      for (const args of [
        ['deps','manager','add', 'foreign-fixture', '--cache-env', 'FIXTURE_CACHE'],
        ['deps','manager','remove', 'foreign-fixture'],
      ]) {
        ctx.fail(args);
        assert.equal(readFileSync(registry, 'utf8'), data);
      }
    }
  },
}, {
  name: 'registered install purge previews and retains unverified data while allowing reseed',
  run(ctx) {
    const fixture = purgeFixture(ctx);
    if (!fixture.native) return;
    assert.equal(fixture.generations.length, 1);
    const generation = join(fixture.root, fixture.generations[0]);
    const active = ctx.spawn('purge-active');
    assert.equal(registered(active.result)?.status, 'attached');
    for (const flags of [[], ['--apply']]) {
      const live = objectResult(ctx.cli(['prune','--expired', '--purge-cache', ...flags]).cache);
      assert.equal(live.eligible, false);
      assert.equal(live.purged, false);
      assert(existsSync(generation));
    }
    ctx.cli(['prune', 'purge-active', '--lease', active.lease, '--discard']);
    const foreign = join(fixture.root, 'foreign-user-directory');
    mkdirSync(foreign);
    writeFileSync(join(foreign, 'keep'), 'user data');
    const secondArgs = [...fixture.args];
    secondArgs[3] = 'second-manager';
    secondArgs[secondArgs.indexOf('--key') + 1] = 'second-purge';
    ctx.cli(secondArgs);
    const changedName = readdirSync(fixture.root).find(name => /^[a-f0-9]{64}$/.test(name) && name !== fixture.generations[0]);
    assert(changedName);
    const changed = join(fixture.root, changedName);
    writeFileSync(join(changed, 'tree', 'package'), 'modified data');
    for (const flags of [[], ['--apply']]) {
      const receipt = objectResult(ctx.cli(['prune','--expired', '--purge-cache', ...flags]).cache);
      assert(Array.isArray(receipt.candidates) && receipt.candidates.includes(generation));
      assert(Array.isArray(receipt.blocked));
      assert(receipt.blocked.map(objectResult).some(item => item.path === foreign));
      assert(receipt.blocked.map(objectResult).some(item => item.path === changed));
      assert.equal(readFileSync(join(foreign, 'keep'), 'utf8'), 'user data');
      assert.equal(readFileSync(join(changed, 'tree', 'package'), 'utf8'), 'modified data');
      assert.equal(existsSync(generation), flags.length === 0);
      assert(Array.isArray(receipt.removed));
      assert.equal(receipt.removed.includes(generation), flags.length > 0);
    }
    ctx.cli(fixture.args);
    const reseeded = ctx.spawn('purge-reseeded');
    assert.equal(registered(reseeded.result)?.status, 'attached');
    assert.equal(readFileSync(join(reseeded.path, 'vendor/bundle/package'), 'utf8'), 'original package');
    assert.equal(readFileSync(join(foreign, 'keep'), 'utf8'), 'user data');
  },
}, {
  name: 'byte-identical replacement cannot replay original or copied install attestation',
  run(ctx) {
    const fixture = purgeFixture(ctx);
    if (!fixture.native) return;
    assert.equal(fixture.generations.length, 1);
    const digest = fixture.generations[0];
    const generation = join(fixture.root, digest);
    const binding = join(fixture.state, `registered-install-${digest}-v1.json`);
    const original = join(ctx.root, 'original generation');
    renameSync(generation, original);
    cpSync(original, generation, { recursive: true, preserveTimestamps: true });
    const check = (name: string) => {
      const ws = ctx.spawn(name);
      assert.notEqual(registered(ws.result)?.status, 'attached');
      assert(!existsSync(join(ws.path, 'vendor/bundle')));
      ctx.cli(['prune', name, '--lease', ws.lease]);
      for (const flags of [[], ['--apply']]) {
        const receipt = objectResult(ctx.cli(['prune','--expired', '--purge-cache', ...flags]).cache);
        assert(Array.isArray(receipt.blocked) && receipt.blocked.map(objectResult).some(item => item.path === generation));
        assert(existsSync(join(generation, 'tree', 'package')));
        assert.equal(readFileSync(join(original, 'tree', 'package'), 'utf8'), 'original package');
      }
    };
    check('original-attestation');
    const originalBinding = join(ctx.root, 'original binding');
    renameSync(binding, originalBinding);
    copyFileSync(originalBinding, binding);
    check('copied-attestation');
  },
}, {
  name: 'modified install trees are not attached',
  run(ctx) {
    const fixture = purgeFixture(ctx);
    if (!fixture.native) return;
    assert.equal(fixture.generations.length, 1);
    const generation = join(fixture.root, fixture.generations[0]);
    const cached = join(generation, 'tree', 'package');
    writeFileSync(cached, 'tampered package');
    const tampered = ctx.spawn('install-tampered');
    assert.notEqual(registered(tampered.result)?.status, 'attached');
    assert(!existsSync(join(tampered.path, 'vendor/bundle/package')));
  },
}, {
  name: 'forged install provenance is not attached',
  run(ctx) {
    const fixture = purgeFixture(ctx);
    if (!fixture.native) return;
    assert.equal(fixture.generations.length, 1);
    writeFileSync(join(fixture.root, fixture.generations[0], 'provenance.json'), '{"forged":true}');
    const forged = ctx.spawn('install-forged');
    assert.notEqual(registered(forged.result)?.status, 'attached');
    assert(!existsSync(join(forged.path, 'vendor/bundle/package')));
  },
}, {
  name: 'registered cache is project-scoped and shared by explicit and implicit execution',
  run(ctx) {
    const args = registration(ctx);
    const first = ctx.cli(args);
    assert.equal(first.manager, manager);
    assert(typeof first.cache_dir === 'string');
    const cache = first.cache_dir;
    assert.equal(ctx.cli(args).cache_dir, cache);
    const listed = ctx.cli(['deps','manager','list']);
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
    assert(!existsSync(join(other, '.git', 'graft')));
    assert.deepEqual(ctx.cli(['deps','manager','list'], other).managers, []);
    assert(!existsSync(join(other, '.git', 'graft')));
    assert.notEqual(ctx.cli(args, other).cache_dir, cache);
    assert.deepEqual(ctx.cli(['deps','manager','remove', manager]), { manager, registered: false, cache_retained: true });
    assert.equal(readFileSync(join(cache, 'download'), 'utf8'), 'local package');
    assert.equal(readFileSync(join(a.path, 'private-output'), 'utf8'), 'changed');
  },
}, {
  name: 'cache-only registration, unselected execution and unknown explicit managers',
  run(ctx) {
    const entry = ctx.cli(['deps','manager','add', 'cache-only']);
    const ws = ctx.spawn('cache-only-workspace');
    const argv = [process.execPath, '-e', 'process.stdout.write(JSON.stringify({cache:process.env.GRAFT_PACKAGE_CACHE ?? null}))'];
    const run = (selection: string[]) => objectResult(JSON.parse(String(ctx.cli([
      'exec', 'cache-only-workspace', '--lease', ws.lease, ...selection, '--', ...argv,
    ]).stdout)));
    assert.equal(run(['--manager', 'cache-only']).cache, entry.cache_dir);
    assert.equal(run([]).cache, null);
    ctx.fail(['exec', 'cache-only-workspace', '--lease', ws.lease, '--manager', 'not-registered', '--', ...argv]);
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
    ctx.fail([...base, '--graft', 'seed-source', '--lease', 'wrong']);
    ctx.fail([...base, '--graft', 'seed-source']);
    const receipt = ctx.cli([...base, '--graft', 'seed-source', '--lease', source.lease]);
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
    const originalCommit = ctx.git(['rev-parse', 'HEAD']);
    const tree = join(ctx.repo, 'vendor', 'bundle');
    mkdirSync(tree, { recursive: true });
    writeFileSync(join(tree, 'package'), 'original package');
    const args = registration(ctx, ['--install-dir', 'vendor/bundle', '--input', 'fixture.lock', '--key', 'fixture-v1']);
    const native = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
    assert.equal(typeof native, 'boolean');
    ctx.measure('native_clone', native);
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
      writeFileSync(join(tree, 'package'), 'second version');
      ctx.cli(args);
      const second = ctx.spawn('install-v2');
      assert.equal(registered(second.result)?.status, 'attached');
      assert.equal(readFileSync(join(second.path, 'vendor/bundle/package'), 'utf8'), 'second version');
      const old = ctx.spawn('install-v1-again', ['--base', originalCommit]);
      assert.equal(registered(old.result)?.status, 'attached');
      assert.equal(readFileSync(join(old.path, 'vendor/bundle/package'), 'utf8'), 'original package');
    } else {
      assert.notEqual(receipt.status, 'attached');
      assert(!existsSync(join(a.path, 'vendor/bundle')));
    }
    const cache = String(registeredManager.cache_dir);
    ctx.cli(['deps','manager','remove', manager]);
    assert(existsSync(cache));
    assert.equal(readFileSync(join(tree, 'package'), 'utf8'), native ? 'second version' : 'original package');
  },
}];
