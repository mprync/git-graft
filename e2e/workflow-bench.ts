/** Whole-workflow comparison. Run with node e2e/workflow-bench.ts [--bin /path/to/git-graft].
 * Use --mode shared for automatic dependency views, seeded for manual layers,
 * or cold to include first-workspace dependency installation;
 * --copy forces layer copies. Set GIT_GRAFT_TEST_VOLUME to select the fixture volume.
 * Configure --repeats, --small-files, --large-mib, --dependency-mib and --output;
 * --add-package measures an offline additive install in a separate stage.
 * Samples are volume-wide and should be collected on an otherwise quiet volume. */
import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, rmSync, statfsSync, statSync, writeFileSync } from 'node:fs';
import { basename, delimiter, dirname, join, resolve } from 'node:path';
import { Context, objectResult } from './harness.ts';
import { npmArchive } from './dependencies.ts';
import { objectBootstrap } from './benchmark-receipts.ts';

type Sample = { seconds: number; deltaBytes: string; cumulativeBytes: string; readySeconds?: number; commands: { name: string; seconds: number }[]; receipts: unknown[] };
type Arm = { stages: Record<string, Sample>; totalSeconds: number; firstReadyIncludingSeedSeconds: number; dependencyMode: string; npmVersion: string; testOutput: string; validation: string[] };
const args = process.argv.slice(2);
function option(name: string, fallback?: string): string {
  const index = args.indexOf(`--${name}`);
  const value = index < 0 ? fallback : args[index + 1];
  assert(value && !value.startsWith('--'), `Missing --${name}`);
  return value;
}
function positive(name: string, fallback: string): number {
  const value = Number(option(name, fallback));
  assert(Number.isSafeInteger(value) && value > 0, `--${name} must be a positive integer`);
  return value;
}
const bin = resolve(args.includes('--bin') ? option('bin') : process.env.GIT_GRAFT_BIN ??
  join(import.meta.dirname, '../target/release', process.platform === 'win32' ? 'git-graft.exe' : 'git-graft'));
assert(existsSync(bin), `Binary does not exist: ${bin}`);
const mode = option('mode', 'seeded');
assert(mode === 'cold' || mode === 'seeded' || mode === 'shared', '--mode must be cold, seeded or shared');
const copy = args.includes('--copy');
const addPackage = args.includes('--add-package');
const repeats = positive('repeats', '3');
const smallFiles = positive('small-files', '400');
const largeMiB = positive('large-mib', '8');
const dependencyMiB = positive('dependency-mib', '8');
const output = resolve(option('output', 'target/e2e/workflow-bench.json'));
const key = 'workflow-bench-npm-offline-v1';

function write(root: string, path: string, data: string | Buffer) {
  const target = join(root, path);
  mkdirSync(dirname(target), {recursive:true});
  writeFileSync(target, data);
}
function free(path: string): bigint {
  const s = statfsSync(path, {bigint:true});
  return s.bfree * s.bsize;
}
function npmCommand(ctx: Context): [string, string[]] {
  if (process.platform !== 'win32') return ['npm', []];
  const path = Object.entries(ctx.env).find(([k]) => k.toUpperCase() === 'PATH')?.[1] ?? '';
  const dir = path.split(delimiter).find(p => existsSync(join(p, 'node.exe')) && existsSync(join(p, 'node_modules/npm/bin/npm-cli.js')));
  assert(dir, 'npm and node must be adjacent on PATH');
  return [join(dir, 'node.exe'), [join(dir, 'node_modules/npm/bin/npm-cli.js')]];
}
function run(arm: 'graft' | 'native', payload: Buffer[], archive: {path:string; integrity:string}): Arm {
  const ctx = new Context(bin);
  try {
    const [npmExe, npmPrefix] = npmCommand(ctx);
    const command = (exe: string, argv: string[], cwd = ctx.repo): string => {
      const r = ctx.command(exe, argv, cwd);
      const leaseIndex = argv.indexOf('--lease');
      const lease = leaseIndex >= 0 ? argv[leaseIndex+1] : undefined;
      const scrub = (s: string) => lease ? s.replaceAll(lease,'[redacted]') : s;
      assert.equal(r.status, 0, `${exe} ${scrub(argv.join(' '))}: ${scrub(r.error?.message ?? '')}\n${scrub(r.stderr)}\n${scrub(r.stdout)}`);
      return r.stdout.trim();
    };
    const git = (argv: string[], cwd = ctx.repo) => command('git', argv, cwd);
    const npm = (argv: string[], cwd = ctx.repo) => command(npmExe, [...npmPrefix, ...argv], cwd);
    const npmVersion = npm(['--version']);
    // Git discovers the release executable as its git-graft subcommand.
    const pathKey = Object.keys(ctx.env).find(k => k.toUpperCase() === 'PATH') ?? 'PATH';
    ctx.env[pathKey] = `${dirname(bin)}${delimiter}${ctx.env[pathKey] ?? ''}`;
    assert(/^git-graft(?:\.exe)?$/i.test(bin.split(/[\\/]/).at(-1)!), '--bin must be named git-graft (or git-graft.exe)');
    const graft = (argv: string[]) => objectResult(JSON.parse(git(['graft', '--repo', ctx.repo, '--json', ...argv])));
    // A filesystem without CoW can still measure the same two-workspace workflow
    // using explicit layer copies. Only the documented CoW refusal triggers this.
    let useCopy = copy;
    let shared = mode === 'shared' && !useCopy;
    const layerWithFallback = (argv: string[], allowSharedFallback = false) => {
      const invoke = (args: string[]) => ctx.command('git',['graft','--repo',ctx.repo,'--json',...args],ctx.repo);
      const withCopy = (args: string[]) => args[1] === 'share' ? [...args.filter(arg => arg !== '--snapshot'),'--snapshot','--copy'] : [...args,'--copy'];
      const attempt = invoke(useCopy ? withCopy(argv) : argv);
      assert.ifError(attempt.error);
      if (attempt.status === 0) return objectResult(JSON.parse(attempt.stdout));
      const refusal = objectResult(JSON.parse(attempt.stderr || attempt.stdout));
      assert.equal(refusal.code,'cow_unavailable',attempt.stderr);
      useCopy = true;
      if (allowSharedFallback) shared = false;
      const fallback = invoke(withCopy(argv));
      assert.equal(fallback.status,0,fallback.error?.message ?? fallback.stderr);
      return objectResult(JSON.parse(fallback.stdout));
    };
    const manifest = {name:'workflow-fixture',version:'1.0.0',private:true,dependencies:{'graft-fixture':'1.0.0'},scripts:{build:'node build.cjs',test:'node test.cjs'}};
    const lock = {name:manifest.name,version:manifest.version,lockfileVersion:3,requires:true,packages:{'':manifest,'node_modules/graft-fixture':{version:'1.0.0',resolved:'https://127.0.0.1:1/graft-fixture.tgz',integrity:archive.integrity}}};
    write(ctx.repo, 'package.json', JSON.stringify(manifest));
    write(ctx.repo, 'package-lock.json', JSON.stringify(lock));
    write(ctx.repo, 'build.cjs', "const fs=require('fs');const n=require('graft-fixture');fs.mkdirSync('dist',{recursive:true});fs.writeFileSync('dist/result.txt',String(n)+':'+fs.readFileSync('source.txt','utf8'));\n");
    write(ctx.repo, 'test.cjs', "const fs=require('fs');if(!fs.readFileSync('dist/result.txt','utf8').startsWith('42:'))process.exit(1);console.log('WORKFLOW_TEST_PASSED');\n");
    write(ctx.repo, 'source.txt', 'initial\n');
    write(ctx.repo, '.gitignore', 'ignored/\nnode_modules/\ndist/\n');
    for (let i=0;i<smallFiles;i++) write(ctx.repo, `src/file-${String(i).padStart(5,'0')}.txt`, `small source ${i}\n`);
    for (let i=0;i<payload.length;i++) write(ctx.repo, `assets/large-${i}.bin`, payload[i]);
    git(['add','-A']); git(['commit','-qm','whole workflow fixture']);
    git(['branch','integration']);
    const originalHead = git(['rev-parse','HEAD']);
    const cache = join(ctx.root,'npm-cache');
    mkdirSync(cache,{recursive:true});
    npm(['cache','add',archive.path,'--offline','--cache',cache]);
    npm(['ci','--offline','--cache',cache]);
    // Cold mode primes a separate workspace cache after the first checkout for
    // each arm. The primary's existing install/cache remain untouched.
    const workspaceCache = mode === 'cold' ? join(ctx.root,'workspace-npm-cache') : cache;
    const extra = join(ctx.root,'extra');
    if (addPackage) {
      write(extra,'package.json',JSON.stringify({name:'graft-bench-extra',version:'1.0.0',main:'index.js'}));
      write(extra,'index.js','module.exports = 7;\n');
      npm(['pack',extra,'--offline','--pack-destination',ctx.root]);
    }
    const digest = Buffer.from(archive.integrity.slice('sha512-'.length),'base64').toString('hex');
    const cacheBlob = join(cache,'_cacache','content-v2','sha512',digest.slice(0,2),digest.slice(2,4),digest.slice(4));
    const assertOriginalCache = () => assert.equal(createHash('sha512').update(readFileSync(cacheBlob)).digest('hex'),digest,'original npm cache changed');
    assertOriginalCache();
    assert.equal(readFileSync(join(ctx.repo,'node_modules/graft-fixture/index.js'),'utf8'),'module.exports = 42;\n');
    const baseline = free(ctx.root);
    let previous = baseline;
    const stages: Record<string, Sample> = {};
    let commands: Sample['commands'] = [];
    let receipts: unknown[] = [];
    let readySeconds: number | undefined;
    let testOutput = '';
    const timed = <T>(name: string, fn: () => T): T => {
      const start = performance.now();
      const result = fn();
      commands.push({name,seconds:(performance.now()-start)/1000});
      return result;
    };
    const stage = (name: string, fn: () => void) => {
      commands = []; receipts = []; readySeconds = undefined;
      const start = performance.now(); fn(); const seconds = (performance.now()-start)/1000;
      const after = free(ctx.root);
      // Never publish arbitrary CLI output: land/drop receipts contain leases.
      const numericKeys = ['cloned_bytes','copied_bytes','logical_bytes','cloned_files','parent_reused_bytes','parent_reused_files','primary_reused_files','primary_reused_bytes','private_files','private_bytes','exit_code'];
      const safeReceipts = receipts.map(value => {
        const item = objectResult(value);
        const safe: Record<string, unknown> = {};
        for (const key of numericKeys) if (typeof item[key] === 'number' || item[key] === null) safe[key] = item[key] as number | null;
        for (const key of ['snapshot_reused','success','removed']) if (typeof item[key] === 'boolean') safe[key] = item[key] as boolean;
        if (typeof item.backend === 'string' && ['copy','copy-on-write','mixed','git','native','reflink','clonefile'].includes(item.backend)) safe.backend = item.backend;
        if (typeof item.action === 'string' && ['save','restore','merge','remove'].includes(item.action)) safe.action = item.action;
        if (item.objects && typeof item.objects === 'object' && !Array.isArray(item.objects)) safe.objects = objectBootstrap(item.objects);
        return safe;
      }).filter(item => Object.keys(item).length > 0);
      stages[name] = {seconds,deltaBytes:(previous-after).toString(),cumulativeBytes:(baseline-after).toString(),readySeconds,commands,receipts:safeReceipts};
      previous = after;
    };
    stage('cache-setup', () => {
      // In cold mode, each arm primes its workspace cache in the first stage,
      // after the first checkout has initialized the appropriate runtime.
    });
    stage('seed', () => {
      if (arm === 'graft' && mode !== 'cold') receipts.push(timed('git graft deps share', () => layerWithFallback(['deps','share','--key',key,...(shared ? [] : ['--snapshot'])],true)));
      // Dirty local data is present before either workspace is created.
      write(ctx.repo,'tracked.txt','private tracked edit\n');
      write(ctx.repo,'local.txt','private untracked\n');
      write(ctx.repo,'ignored/private.txt','private ignored\n');
    });
    const paths: string[] = [], leases: string[] = [];
    for (let i=0;i<2;i++) stage(i ? 'additional' : 'first', () => {
      const name = `agent-${i}`;
      const readyStart = performance.now();
      let path: string;
      if (arm === 'graft') {
        const result = timed('git graft add', () => graft(['add',name,'--history','local']));
        assert(typeof result.path === 'string' && typeof result.lease === 'string');
        path = result.path; leases.push(result.lease); receipts.push(result.storage);
        if (mode === 'cold' && i === 0) {
          const primed = timed('git graft exec npm cache add', () => graft(['exec',name,'--lease',result.lease as string,
            '--',npmExe,...npmPrefix,'cache','add',archive.path,'--offline']));
          assert.equal(primed.success,true);
          receipts.push(primed);
          receipts.push(timed('git graft deps add', () => graft(['deps','add',name,'--lease',result.lease as string,'--manager','npm','--offline'])));
          receipts.push(timed('git graft deps layer save', () => layerWithFallback(['deps','layer','save',name,'--lease',result.lease as string,'--key',key])));
        } else if (shared) {
          const dependency = objectResult(objectResult(result.storage).dependencies);
          assert.equal(dependency.status,'attached');
          assert.equal(dependency.action,'restore');
          assert.equal(dependency.copied_bytes,0);
          receipts.push(dependency);
        } else receipts.push(timed('git graft deps layer restore', () => layerWithFallback(['deps','layer','restore',name,'--lease',result.lease as string,'--key',key])));
      } else {
        path = join(ctx.root,name);
        timed('git worktree add', () => git(['worktree','add','-b',name,path,'main']));
        if (mode === 'cold' && i === 0)
          timed('npm cache add', () => npm(['cache','add',archive.path,'--offline','--cache',workspaceCache],path));
        timed('npm ci --offline', () => npm(['ci','--offline','--cache',workspaceCache],path));
      }
      readySeconds = (performance.now()-readyStart)/1000;
      paths.push(path);
      assert.equal(readFileSync(join(path,'tracked.txt'),'utf8'),'initial\n');
      assert(!existsSync(join(path,'local.txt')) && !existsSync(join(path,'ignored/private.txt')));
      assert.equal(readFileSync(join(path,'node_modules/graft-fixture/index.js'),'utf8'),'module.exports = 42;\n');
    });
    if (addPackage) {
      const manifestBytes = readFileSync(join(paths[0],'package.json'));
      const lockBytes = readFileSync(join(paths[0],'package-lock.json'));
      const tarball = join(ctx.root,'graft-bench-extra-1.0.0.tgz');
      stage('add-package', () => {
        const result = arm === 'graft'
          ? timed('git graft deps add package', () => graft(['deps','add','agent-0','--lease',leases[0],'--manager','npm','--offline',tarball]))
          : (timed('npm install package --offline', () => npm(['install','--offline','--cache',cache,tarball],paths[0])), undefined);
        if (result) { assert.equal(result.success,true); receipts.push(result); }
        assert.equal(readFileSync(join(paths[0],'node_modules/graft-bench-extra/index.js'),'utf8'),'module.exports = 7;\n');
        assert(!existsSync(join(paths[1],'node_modules/graft-bench-extra')));
        assert(!existsSync(join(ctx.repo,'node_modules/graft-bench-extra')));
        const dir = join(paths[0],'node_modules/graft-bench-extra');
        assert(statSync(join(dir,'index.js')).size+statSync(join(dir,'package.json')).size > 0);
      });
      stage('add-package-reset', () => {
        writeFileSync(join(paths[0],'package.json'),manifestBytes);
        writeFileSync(join(paths[0],'package-lock.json'),lockBytes);
        rmSync(join(paths[0],'node_modules/graft-bench-extra'),{recursive:true,force:true});
      });
    }
    stage('edit-build-test', () => {
      write(paths[0],'node_modules/graft-fixture/index.js','module.exports = 99;\n');
      assert.equal(readFileSync(join(paths[1],'node_modules/graft-fixture/index.js'),'utf8'),'module.exports = 42;\n');
      assert.equal(readFileSync(join(ctx.repo,'node_modules/graft-fixture/index.js'),'utf8'),'module.exports = 42;\n');
      write(paths[0],'node_modules/graft-fixture/index.js','module.exports = 42;\n');
      write(paths[0],'source.txt','agent change\n');
      assert.equal(readFileSync(join(paths[1],'source.txt'),'utf8'),'initial\n');
      assert.equal(readFileSync(join(ctx.repo,'source.txt'),'utf8'),'initial\n');
      if (arm === 'graft') {
        receipts.push(timed('git graft exec npm run build', () => graft(['exec','agent-0','--lease',leases[0],'--',npmExe,...npmPrefix,'run','build'])));
        const result = timed('git graft exec npm test', () => graft(['exec','agent-0','--lease',leases[0],'--',npmExe,...npmPrefix,'test']));
        receipts.push(result);
        assert(typeof result.stdout === 'string');
        testOutput = result.stdout;
      } else {
        timed('npm run build', () => npm(['run','build'],paths[0]));
        testOutput = timed('npm test', () => npm(['test'],paths[0]));
      }
      assert(testOutput.split(/\r?\n/).includes('WORKFLOW_TEST_PASSED'), 'test output must confirm the executed fixture test');
      // The fixture emits one known line; keep it verbatim without publishing
      // arbitrary process output or environment-dependent paths.
      testOutput = 'WORKFLOW_TEST_PASSED';
      assert.equal(readFileSync(join(paths[0],'dist/result.txt'),'utf8'),'42:agent change\n');
      assert(!existsSync(join(paths[1],'dist/result.txt')));
    });
    stage('land-drop', () => {
      if (arm === 'graft') {
        receipts.push(timed('git graft merge', () => graft(['merge','agent-0','--lease',leases[0],'--into','integration','--commit','agent change','--discard-ignored'])));
        receipts.push(timed('git graft remove', () => graft(['remove','agent-1','--lease',leases[1],'--discard'])));
      } else {
        timed('git add', () => git(['add','source.txt'],paths[0]));
        timed('git commit', () => git(['commit','-qm','agent change'],paths[0]));
        const tip = git(['rev-parse','agent-0']);
        assert.equal(git(['merge-base','--is-ancestor','integration',tip]),'');
        timed('git update-ref integration', () => git(['update-ref','refs/heads/integration',tip,originalHead]));
        for (let i=0;i<2;i++) {
          timed('git worktree remove', () => git(['worktree','remove','--force',paths[i]]));
          const branch = `refs/heads/agent-${i}`;
          const branchTip = git(['rev-parse',branch]);
          timed('git update-ref delete branch', () => git(['update-ref','-d',branch,branchTip]));
        }
      }
      assert.equal(git(['show','integration:source.txt']),'agent change');
      assert.equal(git(['rev-parse','HEAD']),originalHead);
      assert.equal(readFileSync(join(ctx.repo,'source.txt'),'utf8'),'initial\n');
      assert.equal(readFileSync(join(ctx.repo,'tracked.txt'),'utf8'),'private tracked edit\n');
      assert.equal(readFileSync(join(ctx.repo,'local.txt'),'utf8'),'private untracked\n');
      assert(paths.every(p => !existsSync(p)));
    });
    stage('cleanup', () => {
      if (arm === 'graft') receipts.push(timed('git graft prune --purge-cache --apply', () => graft(['prune','--purge-cache','--apply'])));
      assert.equal(readFileSync(join(ctx.repo,'tracked.txt'),'utf8'),'private tracked edit\n');
      assert.equal(readFileSync(join(ctx.repo,'local.txt'),'utf8'),'private untracked\n');
      assert.equal(readFileSync(join(ctx.repo,'ignored/private.txt'),'utf8'),'private ignored\n');
      assert(existsSync(join(ctx.repo,'node_modules/graft-fixture/index.js')));
      assertOriginalCache();
    });
    return {stages,totalSeconds:Object.values(stages).reduce((sum,s) => sum+s.seconds,0),
      firstReadyIncludingSeedSeconds:stages['cache-setup'].seconds+stages.seed.seconds+stages.first.readySeconds!,
      dependencyMode:arm === 'native' ? 'npm-install' : shared ? 'shared-layer' : useCopy ? 'copied-layer' : 'manual-layer',npmVersion,testOutput,
      validation:['private data excluded','source and build isolation','build and test passed','landed change','workspace retirement','original local data and install preserved']};
  } finally { ctx.close(); }
}

function median(values: number[]): {sorted:number[]; median:number} {
  const sorted = [...values].sort((a,b) => a-b), mid = Math.floor(sorted.length/2);
  return {sorted,median:sorted.length % 2 ? sorted[mid] : (sorted[mid-1]+sorted[mid])/2};
}
// The archive and source payload are shared inputs, not shared repositories,
// workspaces, npm caches, or Graft state. Alternate order to reduce order bias.
const fixture = new Context(bin);
try {
  const archive = npmArchive(fixture.root,dependencyMiB*1024*1024);
  const payload = Array.from({length:3}, () => randomBytes(largeMiB*1024*1024));
  const pairs: {graft:Arm; native:Arm}[] = [];
  for (let i=0;i<repeats;i++) {
    const order: ('graft'|'native')[] = i % 2 ? ['native','graft'] : ['graft','native'];
    const results: Partial<Record<'graft'|'native',Arm>> = {};
    for (const arm of order) results[arm] = run(arm,payload,archive);
    pairs.push({graft:results.graft!,native:results.native!});
  }
  const stageNames = Object.keys(pairs[0].graft.stages);
  const summary = Object.fromEntries(stageNames.map(name => [name,{
    graftSeconds:median(pairs.map(p => p.graft.stages[name].seconds)),
    nativeSeconds:median(pairs.map(p => p.native.stages[name].seconds)),
    pairedSecondsNativeMinusGraft:median(pairs.map(p => p.native.stages[name].seconds-p.graft.stages[name].seconds)),
    graftBytes:median(pairs.map(p => Number(p.graft.stages[name].deltaBytes))),
    nativeBytes:median(pairs.map(p => Number(p.native.stages[name].deltaBytes))),
    pairedBytesNativeMinusGraft:median(pairs.map(p => Number(BigInt(p.native.stages[name].deltaBytes)-BigInt(p.graft.stages[name].deltaBytes)))),
    cumulativeBytesSincePopulatedCheckout:{
      graft:median(pairs.map(p=>Number(p.graft.stages[name].cumulativeBytes))),
      native:median(pairs.map(p=>Number(p.native.stages[name].cumulativeBytes))),
      pairedNativeMinusGraft:median(pairs.map(p=>Number(BigInt(p.native.stages[name].cumulativeBytes)-BigInt(p.graft.stages[name].cumulativeBytes)))),
    },
    ...(['first','additional'].includes(name) ? {
      readinessSeconds:{graft:median(pairs.map(p=>p.graft.stages[name].readySeconds!)),native:median(pairs.map(p=>p.native.stages[name].readySeconds!))},
      spawnSeconds:{graft:median(pairs.map(p=>p.graft.stages[name].commands.find(c=>c.name==='git graft add')!.seconds)),native:median(pairs.map(p=>p.native.stages[name].commands.find(c=>c.name==='git worktree add')!.seconds))},
    } : {}),
  }]));
  const footprints = Object.fromEntries(Object.entries({firstActive:'first',additionalActive:'additional',afterEditBuild:'edit-build-test',afterRetirement:'land-drop',afterCleanup:'cleanup'}).map(([label,stage]) => [label,{
    graft:median(pairs.map(p=>Number(p.graft.stages[stage].cumulativeBytes))),
    native:median(pairs.map(p=>Number(p.native.stages[stage].cumulativeBytes))),
    pairedNativeMinusGraft:median(pairs.map(p=>Number(BigInt(p.native.stages[stage].cumulativeBytes)-BigInt(p.graft.stages[stage].cumulativeBytes)))),
  }]));
  const fs = statfsSync(fixture.root,{bigint:true});
  const executable = readFileSync(bin);
  const graftBinary = {
    name: basename(bin),
    bytes: executable.length,
    sha256: createHash('sha256').update(executable).digest('hex'),
  };
  const report = {binary:basename(bin),graftBinary,mode,copy,addPackage,repeats,platform:process.platform,architecture:process.arch,node:process.version,
    historyPolicy:'local',fixture:{smallFiles,largeFiles:3,largeMiB,dependencyMiB,workspaces:2,...(addPackage ? {addedPackageLogicalBytes:Buffer.byteLength('module.exports = 7;\n')+Buffer.byteLength(JSON.stringify({name:'graft-bench-extra',version:'1.0.0',main:'index.js'})),addPackageCaveat:'This fixture has no peers or transitives; npm version and dependency graph can change which existing packages are replaced.'} : {})},
    filesystem:{typeCode:fs.type.toString(),blockSize:fs.bsize.toString(),method:'statfs bfree * bsize'},
    methodology:'Independent bootstrapped repositories and npm caches, same source and archive; large files contain independent incompressible data. Baseline follows installation in one checkout and cache warmup. Shared mode charges one registration and attaches each spawn automatically; seeded mode charges primary seed and manual restores. Cold mode charges each arm for priming a fresh workspace cache after the first checkout; Graft primes its managed cache through exec, then installs and saves a layer. The original warmed cache is preserved. Native installs both workspaces offline from the corresponding cache. Optional add-package measures the same local tarball in each arm without additional base cache priming; a reset stage restores tracked files for the common landing workload. Installed package logical bytes exclude npm cache and metadata. Source snapshot is cold in all modes. Both land into an unchecked integration branch; native fast-forwards the ref with an expected-old-value check. Dirty primary data remains in place. Readiness includes spawn and dependency preparation; first readiness excludes separately charged setup and seed, while firstReadyIncludingSeedSeconds includes both. Land-drop cumulative bytes show retained storage after retirement, cleanup cumulative bytes show retained storage after cache purge. Volume deltas are signed and include unrelated activity, metadata and delayed allocation. CPU/RSS unavailable portably; command wall time is measured. No savings threshold.',
    summary,footprints,firstReadyIncludingSeedSeconds:{graft:median(pairs.map(p=>p.graft.firstReadyIncludingSeedSeconds)),native:median(pairs.map(p=>p.native.firstReadyIncludingSeedSeconds)),pairedNativeMinusGraft:median(pairs.map(p=>p.native.firstReadyIncludingSeedSeconds-p.graft.firstReadyIncludingSeedSeconds))},
    totalSeconds:{graft:median(pairs.map(p=>p.graft.totalSeconds)),native:median(pairs.map(p=>p.native.totalSeconds)),pairedNativeMinusGraft:median(pairs.map(p=>p.native.totalSeconds-p.graft.totalSeconds))},
    totalBytesAfterCleanup:{graft:median(pairs.map(p=>Number(p.graft.stages.cleanup.cumulativeBytes))),native:median(pairs.map(p=>Number(p.native.stages.cleanup.cumulativeBytes))),pairedNativeMinusGraft:median(pairs.map(p=>Number(BigInt(p.native.stages.cleanup.cumulativeBytes)-BigInt(p.graft.stages.cleanup.cumulativeBytes))))},pairs};
  mkdirSync(dirname(output),{recursive:true}); writeFileSync(output,JSON.stringify(report,null,2)+'\n');
  process.stdout.write(`Workflow benchmark written to ${output}\n`);
} finally { fixture.close(); }
