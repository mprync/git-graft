import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { chmodSync, copyFileSync, existsSync, lstatSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync, statfsSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join, relative } from 'node:path';
import { assertSeparateRegularFiles, objectResult, type Context, type Scenario } from './harness.ts';
import { npmArchive } from './dependencies.ts';

// Use a real npm install and an isolated cache containing only the fixture.
function npm(ctx: Context, args: string[], cwd = ctx.repo): string {
  let program = 'npm';
  if (process.platform === 'win32') {
    const pathValue = Object.entries(ctx.env).find(([key]) => key.toUpperCase() === 'PATH')?.[1] ?? '';
    const dir = pathValue.split(delimiter).find(part => existsSync(join(part, 'node.exe')) &&
      existsSync(join(part, 'node_modules', 'npm', 'bin', 'npm-cli.js')));
    if (!dir) ctx.unavailable('npm and node.exe are required');
    program = join(dir, 'node.exe');
    args = [join(dir, 'node_modules', 'npm', 'bin', 'npm-cli.js'), ...args];
  }
  const result = ctx.command(program, args, cwd);
  if (result.error && 'code' in result.error && result.error.code === 'ENOENT') ctx.unavailable('npm is required');
  assert.equal(result.status, 0, result.error?.message ?? result.stderr);
  return result.stdout.trim();
}

export const cases: Scenario[] = [{
  name: 'shared installed dependencies survive an offline additive npm install',
  run(ctx) {
    ctx.measure('npm_version',npm(ctx,['--version']));
    ctx.measure('base_payload_bytes',16 * 1024 * 1024);
    const fixture = npmArchive(ctx.root, 16 * 1024 * 1024);
    const manifest = {name:'consumer',version:'1.0.0',private:true,dependencies:{'graft-fixture':'1.0.0'}};
    const lock = {name:manifest.name,version:manifest.version,lockfileVersion:3,requires:true,packages:{
      '':manifest,'node_modules/graft-fixture':{version:'1.0.0',resolved:'https://127.0.0.1:1/graft-fixture.tgz',integrity:fixture.integrity},
    }};
    writeFileSync(join(ctx.repo,'package.json'),JSON.stringify(manifest));
    writeFileSync(join(ctx.repo,'package-lock.json'),JSON.stringify(lock));
    ctx.git(['add','package.json','package-lock.json']); ctx.git(['commit','-qm','locked fixture']);
    const primaryWorktrees = ctx.git(['worktree','list','--porcelain']);
    const cache = join(ctx.root,'npm-cache');
    npm(ctx,['cache','add',fixture.path,'--offline','--cache',cache]);
    npm(ctx,['ci','--offline','--ignore-scripts','--cache',cache]);
    const primaryManifest = readFileSync(join(ctx.repo,'package.json'));
    const primaryLock = readFileSync(join(ctx.repo,'package-lock.json'));
    const original = join(ctx.repo,'node_modules/graft-fixture/payload.bin');
    const digest = (path: string) => createHash('sha256').update(readFileSync(path)).digest('hex');
    const originalDigest = digest(original);
    assert.equal(originalDigest,fixture.payloadDigest);
    const incompatible = ctx.command(ctx.bin,['--repo',ctx.repo,'--json','seed','--shared','--copy','--key','bad']);
    assert.ifError(incompatible.error); assert.notEqual(incompatible.status,0);
    assert.equal(objectResult(JSON.parse(incompatible.stderr || incompatible.stdout)).code,'invalid_dependency_mode');
    const attempt = ctx.command(ctx.bin,['--repo',ctx.repo,'--json','seed','--shared','--key','fixture']);
    assert.ifError(attempt.error);
    if (attempt.status !== 0) {
      const failure = objectResult(JSON.parse(attempt.stderr || attempt.stdout));
      assert.equal(failure.code,'cow_unavailable');
      const absent = ctx.spawn('unsupported');
      assert(!existsSync(join(absent.path,'node_modules')));
      const storage = objectResult(absent.result.storage);
      assert(!('dependencies' in storage));
      const saved = ctx.cli(['seed','--copy','--key','portable']);
      const blocked = ctx.fail(['deps','layer','restore','unsupported','--lease',absent.lease,'--key','portable']);
      assert.equal(blocked.code,'cow_unavailable');
      assert(!existsSync(join(absent.path,'node_modules')));
      const restored = ctx.cli(['deps','layer','restore','unsupported','--lease',absent.lease,'--copy','--key','portable']);
      assert.equal(saved.backend,'copy'); assert(Number(saved.copied_bytes)>0);
      assert.equal(restored.backend,'copy'); assert(Number(restored.copied_bytes)>0);
      assert.equal(digest(join(absent.path,'node_modules/graft-fixture/payload.bin')),originalDigest);
      ctx.measure('shared_unavailable',true);
      return;
    }
    const saved = objectResult(JSON.parse(attempt.stdout));
    assert.equal(saved.copied_bytes,0);
    const id = randomUUID();
    const first = ctx.spawn('first',['--request-id',id]);
    assert(lstatSync(join(first.path,'.git')).isDirectory());
    assert.equal(ctx.git(['worktree','list','--porcelain']),primaryWorktrees);
    const attached = objectResult(objectResult(first.result.storage).dependencies);
    assert.equal(attached.status,'attached'); assert.equal(attached.action,'restore'); assert.equal(attached.copied_bytes,0);
    const retry = ctx.spawn('first',['--request-id',id]);
    assert.equal(retry.path,first.path); assert.equal(retry.lease,first.lease);
    assert.deepEqual(objectResult(retry.result.storage).dependencies,attached);
    const sibling = ctx.spawn('sibling');
    assert.equal(objectResult(objectResult(sibling.result.storage).dependencies).status,'attached');
    const opted = ctx.spawn('opted',['--no-deps']);
    assert(!('dependencies' in objectResult(opted.result.storage)));
    assert(!existsSync(join(opted.path,'node_modules')));
    const runtimeCache = String(ctx.cli(['status', 'first', '--env']).cache_dir);
    const cached = join(dirname(runtimeCache),'layers',String(saved.digest),'node_modules/graft-fixture/payload.bin');
    const cachedLock = join(dirname(dirname(cached)),'.package-lock.json');
    const cachedLockBytes = readFileSync(cachedLock);
    const provenancePath = join(dirname(dirname(dirname(cached))), 'provenance.json');
    const expectedProvenance = readFileSync(provenancePath);
    const tamperedProvenance = {...JSON.parse(expectedProvenance.toString()), generation: randomUUID()};
    writeFileSync(provenancePath,JSON.stringify(tamperedProvenance));
    const tampered = ctx.spawn('tampered-generation');
    assert.equal(objectResult(objectResult(tampered.result.storage).dependencies).reason,'layer_generation_unavailable');
    assert(!existsSync(join(tampered.path,'node_modules')));
    assert.deepEqual(JSON.parse(readFileSync(provenancePath,'utf8')),tamperedProvenance);
    writeFileSync(provenancePath,expectedProvenance);

    writeFileSync(cachedLock,'tampered metadata');
    const corrupt = ctx.spawn('corrupt-cache');
    assert.equal(objectResult(objectResult(corrupt.result.storage).dependencies).reason,'layer_restore_failed');
    assert(!existsSync(join(corrupt.path,'node_modules')));
    assert.equal(readFileSync(cachedLock,'utf8'),'tampered metadata');
    writeFileSync(cachedLock,cachedLockBytes);
    const firstLarge = join(first.path,'node_modules/graft-fixture/payload.bin');
    const siblingLarge = join(sibling.path,'node_modules/graft-fixture/payload.bin');
    for (const root of [ctx.repo, first.path, sibling.path]) {
      assert(lstatSync(join(root, 'node_modules')).isDirectory());
    }
    assertSeparateRegularFiles([original, cached, firstLarge, siblingLarge]);
    const before = statSync(firstLarge,{bigint:true});
    const fault = ctx.fail(['deps','add','first','--lease',first.lease,'--manager','npm','--offline']);
    assert.equal(fault.code,'dependencies_already_attached');
    assert.equal(statSync(firstLarge,{bigint:true}).ino,before.ino);
    const extra = join(ctx.root,'extra'); mkdirSync(extra);
    writeFileSync(join(extra,'package.json'),JSON.stringify({name:'graft-extra',version:'1.0.0',main:'index.js'}));
    writeFileSync(join(extra,'index.js'),'module.exports = 7;\n');
    npm(ctx,['pack',extra,'--offline','--pack-destination',ctx.root]);
    const tarball = join(ctx.root,'graft-extra-1.0.0.tgz');
    const modules = join(first.path,'node_modules');
    const kept = join(first.path,'kept-install');
    renameSync(modules,kept);
    const missing = ctx.fail(['deps','add','first','--lease',first.lease,'--manager','npm','--offline',tarball]);
    assert.match(String(missing.message), /attached node_modules is missing/);
    assert(!existsSync(modules), 'a missing shared install must not trigger reinstallation');
    renameSync(kept,modules);
    // Replace each entry after attachment. A fake npm executable makes the
    // refusal observable even if a subprocess would have made no changes.
    const probe = join(ctx.root,'symlink-probe');
    let symlinks = true;
    try { symlinkSync(join(ctx.repo,'package.json'),probe,'file'); rmSync(probe); }
    catch (error) {
      if (error instanceof Error && 'code' in error && ['EPERM','EACCES','ENOTSUP','EOPNOTSUPP'].includes(String(error.code))) symlinks = false;
      else throw error;
    }
    ctx.measure('add_symlink_supported',symlinks);
    if (symlinks) {
      const marker = join(ctx.root,'npm-started');
      const fakeBin = join(ctx.root,'fake-bin'); mkdirSync(fakeBin);
      if (process.platform === 'win32') {
        copyFileSync(process.execPath,join(fakeBin,'node.exe'));
        const cliDir = join(fakeBin,'node_modules','npm','bin'); mkdirSync(cliDir,{recursive:true});
        writeFileSync(join(cliDir,'npm-cli.js'),"require('fs').writeFileSync(process.env.GRAFT_TEST_NPM_MARKER,'started');\n");
      } else {
        const fakeNpm = join(fakeBin,'npm');
        writeFileSync(fakeNpm,'#!/bin/sh\ntouch "$GRAFT_TEST_NPM_MARKER"\n'); chmodSync(fakeNpm,0o755);
      }
      const pathKey = Object.keys(ctx.env).find(key => key.toUpperCase() === 'PATH') ?? 'PATH';
      const oldPath = ctx.env[pathKey];
      const sentinel = join(ctx.repo,'node_modules','primary-sentinel');
      writeFileSync(sentinel,'primary sentinel\n');
      try {
        for (const target of ['node_modules','package.json'] as const) {
          const name = target === 'node_modules' ? 'redirect-modules' : 'redirect-manifest';
          const workspace = ctx.spawn(name);
          assert.equal(objectResult(objectResult(workspace.result.storage).dependencies).status,'attached');
          const path = join(workspace.path,target);
          rmSync(path,{recursive:true,force:true});
          symlinkSync(join(ctx.repo,target),path,target === 'node_modules' ? (process.platform === 'win32' ? 'junction' : 'dir') : 'file');
          ctx.env[pathKey] = `${fakeBin}${delimiter}${oldPath ?? ''}`;
          ctx.env.GRAFT_TEST_NPM_MARKER = marker;
          ctx.fail(['deps','add',name,'--lease',workspace.lease,'--manager','npm','--offline',tarball]);
          assert(!existsSync(marker),'npm must not start for redirected install paths');
          assert.equal(readFileSync(sentinel,'utf8'),'primary sentinel\n');
          assert.deepEqual(readFileSync(join(ctx.repo,'package.json')),primaryManifest);
          assert.deepEqual(readFileSync(join(ctx.repo,'package-lock.json')),primaryLock);
          assert.equal(digest(original),originalDigest);
          assert(!existsSync(join(ctx.repo,'node_modules/graft-extra')));
        }
      } finally {
        if (oldPath === undefined) delete ctx.env[pathKey]; else ctx.env[pathKey] = oldPath;
        delete ctx.env.GRAFT_TEST_NPM_MARKER;
      }
    }
    const cacheDigest = Buffer.from(fixture.integrity.slice('sha512-'.length),'base64').toString('hex');
    const baseCacheBlob = join(runtimeCache,'npm','_cacache','content-v2','sha512',cacheDigest.slice(0,2),cacheDigest.slice(2,4),cacheDigest.slice(4));
    assert(!existsSync(baseCacheBlob),'base archive must not be prewarmed in the workspace cache');
    const free = () => { const fs = statfsSync(ctx.root,{bigint:true}); return fs.bfree * fs.bsize; };
    // This graph has no peers or transitives: other npm versions or graphs may
    // legitimately replace existing packages while resolving an added package.
    const outsidePrefix = join(ctx.root,'hostile-prefix'); mkdirSync(outsidePrefix);
    const outsideSentinel = join(outsidePrefix,'sentinel'); writeFileSync(outsideSentinel,'untouched\n');
    const workspaceName = relative(ctx.root,first.path).replaceAll('\\','/');
    assert(!workspaceName.startsWith('../') && workspaceName !== '');
    const ancestorManifest = JSON.stringify({name:'ancestor',version:'1.0.0',private:true,workspaces:[workspaceName]});
    const ancestorLock = JSON.stringify({name:'ancestor',version:'1.0.0',lockfileVersion:3,packages:{'':{name:'ancestor',version:'1.0.0'}}});
    writeFileSync(join(ctx.root,'package.json'),ancestorManifest);
    writeFileSync(join(ctx.root,'package-lock.json'),ancestorLock);
    const ancestorModules = join(ctx.root,'node_modules'); mkdirSync(ancestorModules);
    writeFileSync(join(ancestorModules,'sentinel'),'ancestor sentinel\n');
    assert.equal(npm(ctx,['prefix'],first.path),ctx.root,'fixture must select the ancestor npm workspace without isolation');
    ctx.env.npm_config_global = 'true'; ctx.env.npm_config_prefix = outsidePrefix;
    const freeBefore = free();
    const added = ctx.cli(['deps','add','first','--lease',first.lease,'--manager','npm','--offline',tarball]);
    delete ctx.env.npm_config_global; delete ctx.env.npm_config_prefix;
    assert.equal(added.success,true);
    assert.deepEqual(readdirSync(outsidePrefix),['sentinel']);
    assert.equal(readFileSync(outsideSentinel,'utf8'),'untouched\n');
    assert.equal(readFileSync(join(ctx.root,'package.json'),'utf8'),ancestorManifest);
    assert.equal(readFileSync(join(ctx.root,'package-lock.json'),'utf8'),ancestorLock);
    assert.deepEqual(readdirSync(ancestorModules),['sentinel']);
    assert.equal(readFileSync(join(ancestorModules,'sentinel'),'utf8'),'ancestor sentinel\n');
    const growth = freeBefore-free();
    const after = statSync(firstLarge,{bigint:true});
    assert.equal(after.dev,before.dev); assert.equal(after.ino,before.ino);
    assert.equal(after.mtimeNs,before.mtimeNs); assert.equal(after.ctimeNs,before.ctimeNs);
    assert(!existsSync(baseCacheBlob),'add must not populate another base archive');
    for (const path of [firstLarge,siblingLarge,original,cached]) assert.equal(digest(path),originalDigest);
    const extraDir = join(first.path,'node_modules/graft-extra');
    assert.equal(readFileSync(join(extraDir,'index.js'),'utf8'),'module.exports = 7;\n');
    assert('graft-extra' in JSON.parse(readFileSync(join(first.path,'package.json'),'utf8')).dependencies);
    assert('node_modules/graft-extra' in JSON.parse(readFileSync(join(first.path,'package-lock.json'),'utf8')).packages);
    assert.deepEqual(readFileSync(cachedLock),cachedLockBytes);
    const size = (path: string): number => statSync(path).isDirectory()
      ? readdirSync(path).reduce((sum,name) => sum+size(join(path,name)),0) : statSync(path).size;
    ctx.measure('add_signed_volume_growth_bytes',growth.toString());
    ctx.measure('added_package_logical_bytes',size(extraDir));
    for (const root of [ctx.repo,sibling.path]) {
      assert.deepEqual(readFileSync(join(root,'package.json')),primaryManifest);
      assert.deepEqual(readFileSync(join(root,'package-lock.json')),primaryLock);
      assert(!existsSync(join(root,'node_modules/graft-extra')));
    }
    if (symlinks) assert.equal(readFileSync(join(ctx.repo,'node_modules','primary-sentinel'),'utf8'),'primary sentinel\n');
    assert(!existsSync(join(dirname(cached),'../graft-extra')));
    writeFileSync(siblingLarge,'private change');
    assert.equal(readFileSync(siblingLarge,'utf8'),'private change');
    for (const path of [firstLarge,original,cached]) assert.equal(digest(path),originalDigest);
    writeFileSync(join(ctx.repo,'tracked.txt'),'source-only revision\n');
    ctx.git(['add','tracked.txt']); ctx.git(['commit','-qm','source only']);
    const compatible = ctx.spawn('compatible');
    assert.equal(objectResult(objectResult(compatible.result.storage).dependencies).status,'attached');
    assert.equal(digest(join(compatible.path,'node_modules/graft-fixture/payload.bin')),originalDigest);
    const bare = ctx.spawn('bare',['--base',ctx.git(['rev-list','--max-parents=0','HEAD'])]);
    assert(!existsSync(join(bare.path,'node_modules')));
    assert.notEqual(objectResult(objectResult(bare.result.storage).dependencies).status,'attached');
    const changedLock = {...lock,packages:{...lock.packages,'node_modules/graft-fixture':{...lock.packages['node_modules/graft-fixture'],integrity:'sha512-different'}}};
    writeFileSync(join(ctx.repo,'package-lock.json'),JSON.stringify(changedLock));
    ctx.git(['add','package-lock.json']); ctx.git(['commit','-qm','changed lock']);
    const mismatch = ctx.spawn('mismatch');
    assert.equal(objectResult(objectResult(mismatch.result.storage).dependencies).status,'inputs_mismatch');
    assert(!existsSync(join(mismatch.path,'node_modules')));
  },
}, {
  name: 'seed a primary install and restore a verified layer into a standalone Graft',
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
    ctx.fail(['seed', '--key', 'fixture']);
    writeFileSync(join(ctx.repo, 'tracked.txt'), 'initial\n');
    const saved = ctx.cli(['seed', '--copy', '--key', 'fixture']);
    assert.equal(typeof saved.digest, 'string');
    assert(Number(saved.logical_bytes) > 0);
    ctx.fail(['seed', '--key', 'fixture']);
    const ws = ctx.spawn('consumer');
    assert(lstatSync(join(ws.path,'.git')).isDirectory());
    const layer = (action: string, key = 'fixture') => ['deps', 'layer', action, 'consumer', '--lease', ws.lease, '--key', key, '--copy'];
    ctx.fail(layer('restore', 'different'));
    const state = dirname(String(ctx.cli(['status', 'consumer', '--env']).cache_dir));
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
    ctx.fail(['seed', '--key', 'fixture']);
    assert.equal(readFileSync(join(outside, 'keep'), 'utf8'), 'keep');
  },
}];
