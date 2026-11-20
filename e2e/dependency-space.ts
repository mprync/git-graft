import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, statfsSync, writeFileSync } from 'node:fs';
import { delimiter, join } from 'node:path';
import { npmArchive } from './dependencies.ts';
import type { Context, Scenario } from './harness.ts';

function npmCommand(ctx: Context): [string, string[]] {
  if (process.platform === 'win32') {
    const path = Object.entries(ctx.env).find(([key]) => key.toUpperCase() === 'PATH')?.[1] ?? '';
    const dir = path.split(delimiter).find((entry) =>
      existsSync(join(entry, 'node.exe')) && existsSync(join(entry, 'node_modules/npm/bin/npm-cli.js')));
    if (!dir) ctx.unavailable('npm requires node.exe and adjacent npm-cli.js');
    return [join(dir, 'node.exe'), [join(dir, 'node_modules/npm/bin/npm-cli.js')]];
  }
  return ['npm', []];
}

function npm(ctx: Context, args: string[], cwd = ctx.repo): void {
  const [exe, prefix] = npmCommand(ctx);
  const result = ctx.command(exe, [...prefix,...args], cwd);
  if (result.error && 'code' in result.error && result.error.code === 'ENOENT')
    ctx.unavailable('npm is required for dependency space benchmark');
  assert.equal(result.status, 0, result.error?.message ?? result.stderr);
}

function disk(ctx: Context): { free: bigint; block: bigint; filesystem: string } {
  try {
    const stat = statfsSync(ctx.root, { bigint: true });
    return {free: stat.bfree * stat.bsize, block: stat.bsize, filesystem: stat.type.toString()};
  } catch (error) {
    ctx.unavailable(`statfs is required for dependency space benchmark: ${String(error)}`);
  }
}

function option(name: string, fallback: number): number {
  const value = process.env[name] === undefined ? fallback : Number(process.env[name]);
  assert(Number.isSafeInteger(value) && value > 0, `${name} must be a positive integer`);
  return value;
}

function hash(path: string): string {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

function cacheBlob(cache: string, integrity: string): string {
  const digest = Buffer.from(integrity.slice('sha512-'.length), 'base64').toString('hex');
  return join(cache, '_cacache', 'content-v2', 'sha512', digest.slice(0, 2), digest.slice(2, 4), digest.slice(4));
}

/** Compares two warmed content caches, including the first installed-layer capture. */
export const dependencySpace: Scenario = {
  name: 'native and graft dependency disk space comparison',
  run(ctx) {
    const count = option('GIT_GRAFT_DEP_WORKSPACES', 5);
    const mib = option('GIT_GRAFT_DEP_FIXTURE_MIB', 32);
    assert(count >= 2, 'at least two workspaces are needed for isolation');
    assert(mib >= 2, 'the dependency fixture must be at least 2 MiB');
    const fixture = npmArchive(ctx.root, mib * 1024 * 1024);
    assert(fixture.payloadDigest);
    const manifest = {name:'consumer', version:'1.0.0', dependencies:{'graft-fixture':'1.0.0'}};
    const lock = {name:'consumer', version:'1.0.0', lockfileVersion:3, packages:{
      '':manifest, 'node_modules/graft-fixture':{version:'1.0.0',
        resolved:'https://127.0.0.1:1/graft-fixture.tgz', integrity:fixture.integrity},
    }};
    writeFileSync(join(ctx.repo,'package.json'), JSON.stringify(manifest));
    writeFileSync(join(ctx.repo,'package-lock.json'), JSON.stringify(lock));
    writeFileSync(join(ctx.repo,'.npmrc'), 'audit=false\nfund=false\nupdate-notifier=false\n');
    ctx.git(['add','package.json','package-lock.json','.npmrc']);
    ctx.git(['commit','-qm','dependency benchmark fixture']);

    const doctor = ctx.cli(['doctor']);
    assert(typeof doctor.common_dir === 'string');
    const state = join(doctor.common_dir, 'graft');
    const layers = join(state, 'layers');
    assert.deepEqual(readdirSync(layers).filter(entry => entry !== '.git-graft-id'), [], 'benchmark requires no preexisting layer');
    const nativeCache = join(ctx.root, 'native-cache');
    const graftCache = join(state, 'cache', 'npm');
    mkdirSync(nativeCache, {recursive:true});
    // Native cache warming is outside the workspace measurement. Graft's cache
    // is initialized by its runtime after the first measured workspace exists.
    npm(ctx, ['cache','add',fixture.path,'--cache',nativeCache,'--offline']);
    const nativeBlob = cacheBlob(nativeCache, fixture.integrity);
    const graftBlob = cacheBlob(graftCache, fixture.integrity);
    assert(existsSync(nativeBlob), 'native cache must contain the locked tarball');

    const nativePaths: string[] = [];
    const nativeBefore = disk(ctx);
    const nativeClock = performance.now();
    for (let i=0; i<count; i++) {
      const path = join(ctx.root, `dependency-native-${i}`);
      ctx.git(['worktree','add','--detach',path,'HEAD']);
      npm(ctx, ['ci','--offline','--cache',nativeCache], path);
      nativePaths.push(path);
    }
    const nativeSeconds = (performance.now()-nativeClock)/1000;
    const nativeAfter = disk(ctx);

    const graftPaths: string[] = [];
    const receipts: Array<{action:string;backend:unknown;clonedBytes:unknown;copiedBytes:unknown;logicalBytes:unknown}> = [];
    const graftBefore = disk(ctx);
    const spawnClock = performance.now();
    const first = ctx.spawn('dependency-graft-0', ['--history', 'local']);
    const firstSpawnSeconds = (performance.now()-spawnClock)/1000;
    const beforePrime = disk(ctx);
    const primeClock = performance.now();
    const [npmExe,npmPrefix] = npmCommand(ctx);
    const prime = ctx.cli(['exec','dependency-graft-0','--lease',first.lease,'--',npmExe,...npmPrefix,
      'cache','add',fixture.path,'--offline']);
    assert.equal(prime.success,true);
    const cachePrimeSeconds = (performance.now()-primeClock)/1000;
    const afterPrime = disk(ctx);
    assert(existsSync(graftBlob), 'Graft cache must contain the locked tarball');
    assert(readFileSync(nativeBlob).equals(readFileSync(graftBlob)), 'warm cache contents differ');
    const graftClock = performance.now();
    let mode: 'copy-on-write' | 'copy' = 'copy-on-write';
    for (let i=0; i<count; i++) {
      const name = `dependency-graft-${i}`;
      const workspace = i === 0 ? first : ctx.spawn(name, ['--history', 'local']);
      graftPaths.push(workspace.path);
      if (i === 0) ctx.cli(['add-dep',name,'--lease',workspace.lease,'--manager','npm','--offline']);
      const action = i === 0 ? 'save' : 'restore';
      const args = ['layer',name,'--lease',workspace.lease,action,'--key','npm-space-fixture-v1'];
      let result: Record<string, unknown>;
      if (i === 0) {
        const attempt = ctx.command(ctx.bin, ['--repo',ctx.repo,'--json',...args]);
        assert.ifError(attempt.error);
        if (attempt.status === 0) {
          result = JSON.parse(attempt.stdout);
          ctx.coverCli('layer');
        } else {
          const refusal = JSON.parse(attempt.stderr || attempt.stdout);
          assert.equal(refusal.code,'cow_unavailable');
          mode = 'copy';
          result = ctx.cli([...args,'--copy']);
        }
      } else result = ctx.cli([...args,...(mode === 'copy' ? ['--copy'] : [])]);
      assert.equal(result.backend,mode);
      if (mode === 'copy-on-write') assert.equal(result.copied_bytes,0);
      else assert.equal(result.cloned_bytes,0);
      for (const key of ['cloned_bytes','copied_bytes','logical_bytes'])
        assert(typeof result[key] === 'number' && Number.isSafeInteger(result[key]) && result[key] >= 0, `invalid ${key}`);
      assert.equal(Number(result.logical_bytes),Number(result.cloned_bytes)+Number(result.copied_bytes));
      receipts.push({action,backend:result.backend,clonedBytes:result.cloned_bytes,copiedBytes:result.copied_bytes,logicalBytes:result.logical_bytes});
    }
    const graftSeconds = firstSpawnSeconds+(performance.now()-graftClock)/1000;
    const graftAfter = disk(ctx);
    assert.equal(nativeBefore.filesystem,nativeAfter.filesystem);
    assert.equal(nativeAfter.filesystem,graftAfter.filesystem);

    const payload = (root: string) => join(root,'node_modules','graft-fixture','payload.bin');
    for (const path of [...nativePaths,...graftPaths]) {
      assert.equal(hash(payload(path)),fixture.payloadDigest, `different dependency content in ${path}`);
      assert.equal(readFileSync(join(path,'node_modules/graft-fixture/index.js'),'utf8'), 'module.exports = 42;\n');
    }
    const original = readFileSync(payload(graftPaths[0]));
    const changed = Buffer.from(original); changed[0] ^= 0xff;
    writeFileSync(payload(graftPaths[0]),changed);
    assert.equal(hash(payload(graftPaths[0])),createHash('sha256').update(changed).digest('hex'));
    for (const path of [...nativePaths,...graftPaths.slice(1)])
      assert.equal(hash(payload(path)),fixture.payloadDigest,'workspace mutation leaked');

    const nativeDelta = nativeBefore.free-nativeAfter.free;
    const cachePrimeDelta = beforePrime.free-afterPrime.free;
    const graftDelta = graftBefore.free-graftAfter.free-cachePrimeDelta;
    const saved = nativeDelta-graftDelta;
    ctx.measure('dependency_space', {
      historyPolicy:'local',platform:process.platform, architecture:process.arch,
      filesystem:{typeCode:nativeBefore.filesystem,blockSize:nativeBefore.block.toString(),method:'statfs bfree * bsize'},
      sample:{workspaces:count,fixtureBytes:(mib*1024*1024).toString(),packageDigest:fixture.payloadDigest,
        caches:'same integrity-verified archive; native warmed before measurement, Graft warmed via exec after the charged first spawn; cache preparation time and signed disk delta are excluded from both totals',seedLayerIncluded:true},
      native:{freeBytesBefore:nativeBefore.free.toString(),freeBytesAfter:nativeAfter.free.toString(),
        freeBytesDelta:nativeDelta.toString(),totalSeconds:nativeSeconds},
      graft:{freeBytesBefore:graftBefore.free.toString(),freeBytesAfter:graftAfter.free.toString(),
        freeBytesDelta:graftDelta.toString(),rawFreeBytesDelta:(graftBefore.free-graftAfter.free).toString(),
        cachePreparation:{seconds:cachePrimeSeconds,freeBytesDelta:cachePrimeDelta.toString()},
        firstSpawnSeconds,totalSeconds:graftSeconds,selectedLayerMode:mode,layerTransfers:receipts},
      savedBytes: saved.toString(),
      savedPercent: nativeDelta > 0n && graftDelta >= 0n ? Number(saved) * 100 / Number(nativeDelta) : null,
      writeIsolation:true,
      caveat:'Signed volume-wide free-space deltas can include unrelated activity, filesystem metadata, snapshots and delayed allocation; no savings threshold is asserted.',
    });
  },
};
