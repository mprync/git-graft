/** Matched release comparison after building with cargo build --release --locked.
 * node e2e/agents-bench.ts [--bin target/release/git-graft] [--output PATH]
 * Use --smoke for a small fixture.
 * Use --apfs-file-data for optional fixture-scoped extent measurements on Darwin.
 * Use --mixed-history to pack one additional primary branch tip outside the selected history.
 * Set GIT_GRAFT_TEST_VOLUME to use a quiet volume. Each result uses a fresh primary. */
import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import {
  accessSync, constants, existsSync, mkdirSync, readFileSync, readdirSync, renameSync,
  rmSync, statfsSync, statSync, writeFileSync,
} from 'node:fs';
import { basename, delimiter, dirname, join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { npmArchive } from './dependencies.ts';
import { Context, objectResult, type ObjectResult } from './harness.ts';
import { objectBootstrap } from './benchmark-receipts.ts';

const arms = ['native', 'graft'] as const;
type Arm = typeof arms[number];
const { values } = parseArgs({ options: {
  bin: {type:'string'}, output: {type:'string'}, arms: {type:'string'}, smoke: {type:'boolean'},
  repeats: {type:'string'}, 'small-files': {type:'string'}, 'large-mib': {type:'string'},
  'dependency-mib': {type:'string'}, 'history-commits': {type:'string'}, 'history-kib': {type:'string'},
  'apfs-file-data': {type:'boolean'}, 'mixed-history': {type:'boolean'},
} });
function size(value: string | undefined, fallback: number, name: string): number {
  const result = Number(value ?? fallback);
  assert(Number.isSafeInteger(result) && result > 0, `--${name} must be a positive integer`);
  return result;
}
const repeats = size(values.repeats, values.smoke ? 1 : 3, 'repeats');
const smallFiles = size(values['small-files'], values.smoke ? 4 : 400, 'small-files');
const largeMiB = size(values['large-mib'], values.smoke ? 1 : 8, 'large-mib');
const dependencyMiB = size(values['dependency-mib'], values.smoke ? 1 : 8, 'dependency-mib');
const historyCommits = size(values['history-commits'], values.smoke ? 1 : 8, 'history-commits');
const historyKiB = size(values['history-kib'], values.smoke ? 16 : 1024, 'history-kib');
const bin = resolve(values.bin ?? process.env.GIT_GRAFT_BIN ?? join(import.meta.dirname, '../target/release', process.platform === 'win32' ? 'git-graft.exe' : 'git-graft'));
const output = resolve(values.output ?? 'target/e2e/agents-bench.json');
assert(existsSync(bin) && /^git-graft(?:\.exe)?$/i.test(basename(bin)), `Expected release executable named git-graft at ${bin}`);
const selectedArms = (values.arms?.split(',') ?? [...arms]).map(arm => {
  const match = arms.find(candidate => candidate === arm);
  assert(match, `Unknown arm ${arm}`);
  return match;
});
assert.equal(new Set(selectedArms).size, selectedArms.length, 'Arms must be unique');

const dependencyFile = 'node_modules/graft-fixture/index.js';
const dependencyPayload = 'node_modules/graft-fixture/payload.bin';
const tarballName = 'graft-bench-extra-1.0.0.tgz';
const packageArg = './vendor/' + tarballName;
const discardedFiles = ['node_modules', 'scratch.txt'];
type Fixture = {
  archive: ReturnType<typeof npmArchive>; source: Buffer[]; history: Buffer[]; extra: Buffer;
};
type Resources = {userSeconds: number; systemSeconds: number; peakRssBytes: number};
type Command = {name: string; seconds: number; resources: Resources | null};
type Stage = {
  seconds: number; commands: Command[]; deltaBytes: string; bytesSincePopulatedCheckout: string;
  resources: Resources | null; receipts: Record<string, unknown>;
};
type ApfsCheckpoint = 'baseline' | 'repeat' | 'addDependency' | 'gc';
type ApfsObservation =
  | {available:true; bytes:string; observerSeconds:number}
  | {available:false; reason:string; observerSeconds:number};
type ApfsFailure = Extract<ApfsObservation,{available:false}> & {checkpoint:ApfsCheckpoint | 'calibration'};
const resourceProbe = process.platform === 'darwin' ? 'bsd' : process.platform === 'linux' ? 'gnu' : null;
const resourceArgs = resourceProbe === 'bsd' ? ['-l', '-p']
  : ['-f', 'graft-bench %U %S %M'];

function put(root: string, file: string, contents: string | Buffer) {
  const path = join(root, file);
  mkdirSync(dirname(path), {recursive:true});
  writeFileSync(path, contents);
}
function read(root: string, file: string) { return readFileSync(join(root, file), 'utf8'); }
function free(path: string) {
  const fs = statfsSync(path, {bigint:true});
  return fs.bfree * fs.bsize;
}
function logicalBytes(root: string): number {
  let bytes = 0;
  for (const entry of readdirSync(root, {withFileTypes:true})) {
    const path = join(root,entry.name);
    if (entry.isDirectory()) bytes += logicalBytes(path);
    else if (entry.isFile()) bytes += statSync(path).size;
  }
  return bytes;
}
function observeApfs(ctx: Context, calibrate = false): ApfsObservation {
  if (process.platform !== 'darwin') return {available:false,reason:'unsupported-platform',observerSeconds:0};
  const helper = join(import.meta.dirname,'apfs-file-data.py');
  const start = performance.now();
  const result = ctx.command('python3',['-I','-B',helper,
    ...(calibrate ? ['--calibrate',dirname(ctx.root)] : [ctx.root])]);
  const observerSeconds = (performance.now()-start)/1000;
  if (result.status === 0) {
    try {
      const fields = objectResult(JSON.parse(result.stdout));
      if (fields.available === true) {
        const bytes = calibrate ? '0' : fields.bytes;
        if (typeof bytes === 'string' && /^\d+$/.test(bytes)) return {available:true,bytes,observerSeconds};
      }
      if (fields.available === false && typeof fields.reason === 'string' && /^[a-z0-9-]{1,64}$/.test(fields.reason)) {
        return {available:false,reason:fields.reason,observerSeconds};
      }
    } catch { /* A malformed observer response is unavailable. */ }
  }
  const reason = (result.error as NodeJS.ErrnoException | undefined)?.code === 'ENOENT'
    ? 'python3-unavailable' : 'observer-failed';
  return {available:false,reason,observerSeconds};
}
function executable(env: NodeJS.ProcessEnv, name: string) {
  const path = Object.entries(env).find(([key]) => key.toUpperCase() === 'PATH')?.[1] ?? '';
  return path.split(delimiter).filter(Boolean).map(dir => join(dir, name)).find(file => {
    try { accessSync(file, constants.X_OK); return statSync(file).isFile(); } catch { return false; }
  });
}
function npmCommand(ctx: Context): [string, string[]] {
  if (process.platform !== 'win32') return ['npm', []];
  const node = executable(ctx.env, 'node.exe');
  assert(node, 'node.exe must be on PATH');
  const npm = join(dirname(node), 'node_modules/npm/bin/npm-cli.js');
  assert(existsSync(npm), 'npm must be installed adjacent to node.exe');
  return [node, [npm]];
}
function resources(stderr: string): Resources | null {
  if (resourceProbe === 'bsd') {
    const user = stderr.match(/^user (\d+\.\d+)$/m);
    const system = stderr.match(/^sys (\d+\.\d+)$/m);
    const rss = stderr.match(/^\s*(\d+)\s+maximum resident set size$/m);
    if (user && system && rss) return {userSeconds:Number(user[1]), systemSeconds:Number(system[1]), peakRssBytes:Number(rss[1])};
  } else if (resourceProbe === 'gnu') {
    const match = stderr.match(/^graft-bench ([\d.]+) ([\d.]+) (\d+)$/m);
    if (match) return {userSeconds:Number(match[1]), systemSeconds:Number(match[2]), peakRssBytes:Number(match[3])*1024};
  }
  return null;
}
function usage(commands: Command[]): Resources | null {
  const samples = commands.flatMap(command => command.resources ? [command.resources] : []);
  if (!samples.length || samples.length !== commands.length) return null;
  return {
    userSeconds:samples.reduce((sum, sample) => sum+sample.userSeconds, 0),
    systemSeconds:samples.reduce((sum, sample) => sum+sample.systemSeconds, 0),
    peakRssBytes:Math.max(...samples.map(sample => sample.peakRssBytes)),
  };
}
// Publish numeric counters and short enums only. Raw receipts include leases,
// machine-local paths, and process output.
function counters(value: unknown): ObjectResult {
  const fields = [
    'files', 'copied_bytes', 'cloned_bytes', 'logical_bytes',
    'copied_files', 'cloned_files', 'private_files', 'private_bytes', 'snapshot_reused',
    'parent_reused_files', 'parent_reused_bytes', 'primary_reused_files', 'primary_reused_bytes',
    'success', 'exit_code', 'removed', 'applied', 'expired_receipts', 'purged', 'eligible',
    'bytes', 'pack_bytes', 'index_bytes', 'shared_bytes', 'cache_bytes',
  ];
  const enums = {
    backend:['git','native','independent','empty','copy','copy-on-write','mixed','reflink','clonefile'],
    method:['clonefile','reflink','local-pack','none'],
    source:['primary-pack','filtered-primary','cache','ancestor-cache','local-export','none'],
    action:['save','restore','land','drop'],
    status:['attached','registered'],
    object_format:['sha1','sha256'],
  };
  const receipt = objectResult(value);
  const allowed = Object.entries(enums).flatMap<[string,string | number | boolean | null]>(([key,labels]) => {
    const field = receipt[key];
    return typeof field === 'string' && labels.includes(field) ? [[key,field]] : [];
  });
  for (const key of fields) {
    const field = receipt[key];
    if (field === null || typeof field === 'number' || typeof field === 'boolean') allowed.push([key,field]);
  }
  return Object.fromEntries(allowed);
}

function run(arm: Arm | 'nativeConventional', fixture: Fixture, apfsCalibration?: ApfsObservation) {
  const ctx = new Context(bin);
  try {
    const isNative = arm !== 'graft';
    const conventional = arm === 'nativeConventional';
    const gitExe = executable(ctx.env, process.platform === 'win32' ? 'git.exe' : 'git') ?? 'git';
    const gitExecPath = ctx.git(['--exec-path']);
    const pathKey = Object.keys(ctx.env).find(key => key.toUpperCase() === 'PATH') ?? 'PATH';
    ctx.env[pathKey] = [dirname(bin), gitExecPath, ctx.env[pathKey] ?? ''].join(delimiter);
    ctx.env.GIT_EXEC_PATH = dirname(bin);
    ctx.env.LC_ALL = 'C';
    ctx.env.GIT_AUTHOR_DATE = ctx.env.GIT_COMMITTER_DATE = '2001-01-01T00:00:00Z';
    ctx.env.npm_config_offline = 'true';
    const [npmExe, npmPrefix] = npmCommand(ctx);
    const npmCache = join(ctx.root, 'npm-cache');
    const useResources = resourceProbe !== null && existsSync('/usr/bin/time')
      && ctx.command('/usr/bin/time', [...resourceArgs, '/usr/bin/true']).status === 0;
    let commands: Command[] = [];
    let receipts: Record<string, unknown> = {};
    const observedCommand = (name: string, exe: string, args: string[], cwd = ctx.repo, measure = true) => {
      const wrapped = useResources && measure;
      const start = performance.now();
      const result = wrapped ? ctx.command('/usr/bin/time', [...resourceArgs, exe, ...args], cwd) : ctx.command(exe, args, cwd);
      if (measure) commands.push({name, seconds:(performance.now()-start)/1000, resources:wrapped ? resources(result.stderr) : null});
      return result;
    };
    const command = (name: string, exe: string, args: string[], cwd = ctx.repo, measure = true) => {
      const result = observedCommand(name,exe,args,cwd,measure);
      const leaseIndex = args.indexOf('--lease');
      const lease = leaseIndex < 0 ? undefined : args[leaseIndex+1];
      const scrub = (s: string) => lease ? s.replaceAll(lease, '[redacted]') : s;
      assert.equal(result.status, 0, `${arm} ${name}: ${scrub(result.error?.message ?? '')}\n${scrub(result.stderr)}\n${scrub(result.stdout)}`);
      return result.stdout.trim();
    };
    const git = (args: string[], cwd = ctx.repo, measure = true) => command(`git ${args[0]}`, gitExe, args, cwd, measure);
    const npm = (args: string[], cwd = ctx.repo) => command(`npm ${args[0]}`, npmExe, [...npmPrefix, '--cache', npmCache, ...args], cwd);
    const graft = (args: string[], label = args[0]) => {
      const result = objectResult(JSON.parse(command(`git graft ${label}`, gitExe,
        ['graft','--repo',ctx.repo,'--json',...args])));
      receipts[label] = counters(result);
      return result;
    };
    const gitVersion = git(['--version'], ctx.repo, false);
    const npmVersion = command('npm --version', npmExe, [...npmPrefix,'--version'], ctx.repo, false);
    const commandStart = performance.now();
    // Context's initial commit predates the fixed dates. Amend only this
    // disposable primary so all arms share exactly the same commit graph.
    git(['commit','--amend','--reset-author','--no-edit','-q']);
    git(['reflog','expire','--expire=all','--all']);
    for (const [index, bytes] of fixture.history.entries()) {
      put(ctx.repo, 'historical.bin', bytes);
      git(['add','historical.bin']);
      git(['commit','-qm',`history ${index}`]);
    }
    rmSync(join(ctx.repo, 'historical.bin'));
    const manifest = {name:'workflow-fixture',version:'1.0.0',private:true,dependencies:{'graft-fixture':'1.0.0'},scripts:{build:'node build.cjs',test:'node test.cjs'}};
    put(ctx.repo, 'package.json', JSON.stringify(manifest));
    put(ctx.repo, 'package-lock.json', JSON.stringify({
      name:manifest.name, version:manifest.version, lockfileVersion:3, requires:true,
      packages:{'':manifest,'node_modules/graft-fixture':{version:'1.0.0',
        resolved:'https://127.0.0.1:1/graft-fixture.tgz',integrity:fixture.archive.integrity}},
    }));
    put(ctx.repo, 'build.cjs', "const fs=require('fs');const n=require('graft-fixture');fs.mkdirSync('dist',{recursive:true});fs.writeFileSync('dist/result.txt',String(n)+':'+fs.readFileSync('source.txt','utf8'));\n");
    put(ctx.repo, 'test.cjs', "const fs=require('fs');if(!fs.readFileSync('dist/result.txt','utf8').startsWith('42:'))process.exit(1);console.log('WORKFLOW_TEST_PASSED');\n");
    put(ctx.repo, 'source.txt', 'initial\n');
    put(ctx.repo, '.gitignore', 'ignored/\nnode_modules/\ndist/\n');
    put(ctx.repo, packageArg, fixture.extra);
    for (let i=0; i<smallFiles; i++) put(ctx.repo, `src/file-${String(i).padStart(5,'0')}.txt`, `small source ${i}\n`);
    for (const [index, bytes] of fixture.source.entries()) put(ctx.repo, `assets/large-${index}.bin`, bytes);
    git(['add','-A']);
    git(['commit','-qm','whole workflow fixture']);
    git(['branch','integration']);
    let privateTip: string | undefined;
    if (values['mixed-history']) {
      privateTip = git(['commit-tree','HEAD^{tree}','-p','HEAD','-m','Unmerged private branch']);
      git(['branch','primary-only',privateTip]);
    }
    git(['repack','-ad','-q']);
    const historySetupSeconds = (performance.now()-commandStart)/1000;
    const packDir = join(ctx.repo,'.git','objects','pack');
    const packFiles = readdirSync(packDir);
    const bytesByExtension = (extension: string) => packFiles.filter(name => name.endsWith(extension))
      .reduce((sum,name) => sum+statSync(join(packDir,name)).size,0);
    const packedHistoryBytes = bytesByExtension('.pack');
    const head = git(['rev-parse','HEAD'], ctx.repo, false);
    const historyGraph = {
      commitCount:Number(git(['rev-list','--count',head],ctx.repo,false)),
      reachableObjectCount:git(['rev-list','--objects','--no-object-names',head],ctx.repo,false).split('\n').length,
      packCount:packFiles.filter(name => name.endsWith('.pack')).length,
      packBytes:packedHistoryBytes,
      indexBytes:bytesByExtension('.idx'),
    };
    const historyCommands = commands;
    commands = [];
    const populateStart = performance.now();
    npm(['cache','add',fixture.archive.path,'--offline']);
    npm(['ci','--offline']);
    const populateSeconds = (performance.now()-populateStart)/1000;
    const populateCommands = commands;
    const digest = Buffer.from(fixture.archive.integrity.slice('sha512-'.length),'base64').toString('hex');
    const cacheBlob = join(npmCache,'_cacache','content-v2','sha512',digest.slice(0,2),digest.slice(2,4),digest.slice(4));
    const baselineLogicalBytes = logicalBytes(ctx.root);
    const baseline = free(ctx.root);
    let apfsFailure: ApfsFailure | undefined = apfsCalibration?.available === false
      ? {...apfsCalibration,checkpoint:'calibration'} : undefined;
    const apfsCheckpoints: Partial<Record<ApfsCheckpoint,{
      bytes:string; deltaBytes:string; bytesSincePopulatedCheckout:string; observerSeconds:number;
    }>> = {};
    let previousApfs: bigint | undefined;
    const sampleApfs = (checkpoint: ApfsCheckpoint) => {
      if (!apfsCalibration || apfsFailure) return;
      const observation = observeApfs(ctx);
      if (!observation.available) {
        apfsFailure = {...observation,checkpoint};
        return;
      }
      const bytes = BigInt(observation.bytes);
      const first = BigInt(apfsCheckpoints.baseline?.bytes ?? observation.bytes);
      apfsCheckpoints[checkpoint] = {
        bytes:observation.bytes, deltaBytes:(bytes-(previousApfs ?? bytes)).toString(),
        bytesSincePopulatedCheckout:(bytes-first).toString(), observerSeconds:observation.observerSeconds,
      };
      previousApfs = bytes;
    };
    sampleApfs('baseline');
    let previous = baseline;
    const stages: Record<string, Stage> = {};
    const stage = (name: string, fn: () => void) => {
      commands = [];
      receipts = {};
      const start = performance.now();
      fn();
      const seconds = (performance.now()-start)/1000;
      const after = free(ctx.root);
      stages[name] = {seconds,commands,receipts,resources:usage(commands),
        deltaBytes:(previous-after).toString(),bytesSincePopulatedCheckout:(baseline-after).toString()};
      previous = after;
    };
    let copiedLayer = false;
    if (!isNative) stage('seed', () => {
      const key = 'workflow-bench-npm-offline-v1';
      const result = observedCommand('git graft seed-layer',gitExe,
        ['graft','--repo',ctx.repo,'--json','seed-layer','--shared','--key',key]);
      assert.ifError(result.error);
      if (result.status === 0) {
        const receipt = objectResult(JSON.parse(result.stdout));
        assert.equal(receipt.status,'registered');
        receipts.seed = counters(receipt);
      } else {
        // Git emits compact JSON on one line; /usr/bin/time appends resource
        // statistics (and on GNU time, its own exit-status line) to stderr.
        const refusal = (result.stderr+'\n'+result.stdout).split(/\r?\n/).flatMap(line => {
          try {
            const value = JSON.parse(line);
            return value && typeof value === 'object' && !Array.isArray(value) ? [objectResult(value)] : [];
          } catch { return []; }
        }).find(value => typeof value.code === 'string');
        assert.equal(refusal?.code,'cow_unavailable',result.stderr);
        copiedLayer = true;
        assert.equal(graft(['seed-layer','--key',key,'--copy']).action,'save');
      }
    });
    stage('primaryDirt', () => {
      put(ctx.repo, 'tracked.txt', 'private staged\n');
      git(['add','tracked.txt']);
      put(ctx.repo, 'tracked.txt', 'private unstaged\n');
      put(ctx.repo, 'local.txt', 'primary untracked\n');
      put(ctx.repo, 'ignored/private.txt', 'primary ignored\n');
    });
    const status = () => command('git status',gitExe,['--no-optional-locks','status','--porcelain','--ignored'],ctx.repo,false);
    const primaryStatus = status();
    const primaryIndex = readFileSync(join(ctx.repo,'.git','index'));
    const validatePrimary = () => {
      assert.equal(git(['rev-parse','HEAD'],ctx.repo,false), head);
      const indexFile = join(ctx.repo,'.git','index');
      assert.deepEqual(readFileSync(indexFile), primaryIndex, 'primary index changed');
      assert.equal(status(), primaryStatus);
      assert.equal(read(ctx.repo,'tracked.txt'), 'private unstaged\n');
      assert.equal(read(ctx.repo,'local.txt'), 'primary untracked\n');
      assert.equal(read(ctx.repo,'ignored/private.txt'), 'primary ignored\n');
      assert.equal(read(ctx.repo,'source.txt'), 'initial\n');
      assert.equal(read(ctx.repo,dependencyFile), 'module.exports = 42;\n');
      assert.equal(createHash('sha512').update(readFileSync(cacheBlob)).digest('hex'),digest);
    };
    const ready: Record<string,{spawnSeconds:number;readySeconds:number}> = {};
    const workspaces = ['agent-0','agent-1'].map((name, index) => {
      const label = index ? 'repeat' : 'first';
      let path = join(ctx.root,name);
      let lease = '';
      let localSnapshot = false;
      stage(label, () => {
        if (isNative) {
          git(['worktree','add','-b',name,path,'main']);
          npm(['ci','--offline'],path);
        } else {
          const result = graft(['spawn',name,'--history','local']);
          const checkout = result.path;
          assert(typeof checkout === 'string' && typeof result.lease === 'string');
          path = checkout;
          lease = result.lease;
          const storage = objectResult(result.storage);
          const dependencies = copiedLayer
            ? graft(['layer',name,'--lease',lease,'restore','--key','workflow-bench-npm-offline-v1','--copy'])
            : objectResult(storage.dependencies);
          if (copiedLayer) assert.equal(dependencies.action,'restore');
          else assert.equal(dependencies.status,'attached');
          const objects = objectResult(storage.objects);
          localSnapshot = objects.source === 'local-snapshot';
          if (objects) assert(typeof objects.pack_bytes === 'number' && objects.pack_bytes > 0);
          receipts.spawn = {...counters(storage),dependencies:counters(dependencies),
            ...(objects ? {objects:objectBootstrap(objects)} : {})};
        }
      });
      ready[label] = {spawnSeconds:stages[label].commands[0].seconds, readySeconds:stages[label].seconds};
      if (label === 'repeat') sampleApfs('repeat');
      assert.equal(read(path,'tracked.txt'),'initial\n');
      assert(!existsSync(join(path,'local.txt')) && !existsSync(join(path,'ignored/private.txt')));
      assert.equal(read(path,dependencyFile),'module.exports = 42;\n');
      assert.equal(git(['rev-parse','HEAD'],path,false),head);
      if (!isNative) assert(statSync(join(path,'.git')).isDirectory());
      if (privateTip) {
        const visibility = ctx.command(gitExe,['cat-file','-e',privateTip],path);
        assert.ifError(visibility.error);
        if (isNative || localSnapshot) assert.equal(visibility.status,0,visibility.stderr);
        else assert([1,128].includes(visibility.status ?? -1),'Fallback export exposed an unrelated primary commit');
      }
      return {name,path,lease};
    });
    if (!isNative) {
      const listed = git(['worktree','list','--porcelain'],ctx.repo,false);
      assert.equal(listed.split('\n').filter(line => line.startsWith('worktree ')).length,1);
    }
    const [landed, dropped] = workspaces;
    const cli = (operation: string, workspace: typeof landed, args: string[] = [], label = operation) =>
      graft([operation,workspace.name,'--lease',workspace.lease,...args],label);
    stage('addDependency', () => {
      if (isNative) npm(['install','--offline',packageArg],landed.path);
      else assert.equal(cli('add-dep',landed,['--manager','npm','--offline',packageArg]).success,true);
    });
    assert.equal(read(landed.path,'node_modules/graft-bench-extra/index.js'),'module.exports = 7;\n');
    assert(![ctx.repo,dropped.path].some(path => existsSync(join(path,'node_modules/graft-bench-extra'))));
    sampleApfs('addDependency');
    let testOutput = '';
    stage('editBuildTest', () => {
      put(landed.path,'source.txt','agent change\n');
      put(dropped.path,'source.txt','unmerged change\n');
      put(dropped.path,'scratch.txt','private scratch\n');
      if (isNative) {
        npm(['run','build'],landed.path);
        testOutput = npm(['test'],landed.path);
      } else {
        cli('exec',landed,['--',npmExe,...npmPrefix,'run','build'],'exec build');
        testOutput = String(cli('exec',landed,['--',npmExe,...npmPrefix,'test'],'exec test').stdout);
      }
    });
    assert(testOutput.split(/\r?\n/).includes('WORKFLOW_TEST_PASSED'));
    assert.equal(read(landed.path,'dist/result.txt'),'42:agent change\n');
    assert(!existsSync(join(dropped.path,'dist/result.txt')));
    let landedTip = '', droppedTip = '';
    stage('commit', () => {
      for (const workspace of workspaces) {
        git(['add','source.txt','package.json','package-lock.json'],workspace.path);
        git(['commit','-qm',`${workspace.name} change`],workspace.path);
      }
      landedTip = git(['rev-parse','HEAD'],landed.path);
      droppedTip = git(['rev-parse','HEAD'],dropped.path);
    });
    // Keep the native recovery pin in a separate repository, like Graft's
    // private retired checkout, rather than adding private refs to the primary.
    const recovery = (workspace: typeof landed) => join(ctx.root,`recovery-${workspace.name}`);
    const recoveryRepo = (workspace: typeof landed) => join(ctx.root,`recovery-git-${workspace.name}.git`);
    let recoveryPreparationSeconds = 0;
    const nativeRetire = (workspace: typeof landed, tip: string, files: string[], pin: boolean) => {
      if (conventional) {
        git(['worktree','remove','--force',workspace.path]);
        git(['update-ref','-d',`refs/heads/${workspace.name}`,tip]);
        return;
      }
      const recoveryStart = performance.now();
      if (pin) {
        git(['init','--bare','-q',recoveryRepo(workspace)]);
        git(['--git-dir',recoveryRepo(workspace),'fetch','--no-tags','--no-write-fetch-head',workspace.path,'HEAD:refs/heads/recovered']);
      }
      mkdirSync(recovery(workspace),{recursive:true});
      for (const file of files) renameSync(join(workspace.path,file),join(recovery(workspace),file));
      recoveryPreparationSeconds += (performance.now()-recoveryStart)/1000;
      git(['worktree','remove','--force',workspace.path]);
      git(['update-ref','-d',`refs/heads/${workspace.name}`,tip]);
    };
    stage('land', () => {
      if (isNative) {
        git(['merge-base','--is-ancestor',head,landedTip]);
        git(['update-ref','refs/heads/integration',landedTip,head]);
        nativeRetire(landed,landedTip,['node_modules','dist'],false);
      } else {
        assert.equal(cli('land',landed,['--into','integration','--discard-ignored']).removed,true);
      }
    });
    stage('drop', () => {
      if (isNative) nativeRetire(dropped,droppedTip,discardedFiles,true);
      else assert.equal(cli('drop',dropped,['--discard']).removed,true);
    });
    assert(workspaces.every(workspace => !existsSync(workspace.path)));
    assert.equal(git(['rev-parse','integration'],ctx.repo,false),landedTip);
    const primaryRefs = git(['for-each-ref','--format=%(objectname)'],ctx.repo,false).split('\n');
    assert(!primaryRefs.includes(droppedTip),'private recovery tip must not be pinned in primary refs');
    if (!isNative) {
      const visibility = ctx.command(gitExe,['cat-file','-e',droppedTip],ctx.repo);
      assert([1,128].includes(visibility.status ?? -1),'private dropped commit is visible in primary');
    }
    validatePrimary();
    if (!conventional) {
    stage('restore', () => {
      if (isNative) {
        git(['fetch','--no-tags','--no-write-fetch-head',recoveryRepo(dropped),'refs/heads/recovered']);
        git(['worktree','add','-b',dropped.name,dropped.path,droppedTip]);
        for (const file of discardedFiles) renameSync(join(recovery(dropped),file),join(dropped.path,file));
      } else {
        const result = cli('restore',dropped);
        assert.equal(result.path,dropped.path);
        assert(typeof result.lease === 'string' && result.lease !== dropped.lease);
        dropped.lease = result.lease;
      }
    });
    assert.equal(git(['rev-parse','HEAD'],dropped.path,false),droppedTip);
    assert.equal(read(dropped.path,'source.txt'),'unmerged change\n');
    assert.equal(read(dropped.path,'scratch.txt'),'private scratch\n');
    assert.equal(read(dropped.path,dependencyFile),'module.exports = 42;\n');
    assert.equal(createHash('sha256').update(readFileSync(join(dropped.path,dependencyPayload))).digest('hex'),fixture.archive.payloadDigest);
    stage('dropRestored', () => {
      if (isNative) nativeRetire(dropped,droppedTip,discardedFiles,false);
      else assert.equal(cli('drop',dropped,['--discard']).removed,true);
    });
    }
    stage('gc', () => {
      if (isNative) git(['worktree','prune']);
      else {
        const result = graft(['gc','--purge-cache','--apply']);
        assert.equal(result.applied,true);
        if (result.cache) receipts.cache = counters(result.cache);
      }
    });
    sampleApfs('gc');
    assert(!existsSync(dropped.path));
    validatePrimary();
    const totalSeconds = Object.values(stages).reduce((sum,s) => sum+s.seconds,0);
    return {head,landedTip,droppedTip,comparison:conventional ? 'native-conventional' : isNative ? 'native-private-recovery' : 'graft-private-recovery',gitVersion,npmVersion,gitBinaryBytes:gitExe === 'git' ? null : statSync(gitExe).size,
      baselineFreeBytes:baseline.toString(),baselineLogicalBytes,retainedLogicalBytesAfterGc:logicalBytes(ctx.root)-baselineLogicalBytes,
      apfsFileData:apfsCalibration ? apfsFailure ?? {available:true as const,checkpoints:apfsCheckpoints} : undefined,
      primarySetup:{historySetupSeconds,packedHistoryBytes,historyGraph,populateSeconds,historyCommands,populateCommands,
        resources:usage([...historyCommands,...populateCommands])},
      ready,dependencyMode:copiedLayer ? 'copied-layer' : isNative ? 'npm-install' : 'shared-layer',firstReadyIncludingSeedSeconds:(stages.seed?.seconds ?? 0)+ready.first.readySeconds,
      stages,totalSeconds,recoveryPreparationSeconds: isNative && !conventional ? recoveryPreparationSeconds : undefined,
      includingPrimarySetupSeconds:historySetupSeconds+populateSeconds+totalSeconds,
      resources:usage(Object.values(stages).flatMap(s => s.commands))};
  } finally { ctx.close(); }
}

function median(values: number[]) {
  const sorted = [...values].sort((a,b) => a-b), middle = Math.floor(values.length/2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle-1]+sorted[middle])/2;
}
const ctx = new Context(bin);
try {
  const versionResult = ctx.command(bin,['--version']);
  assert.equal(versionResult.status,0,versionResult.error?.message ?? versionResult.stderr);
  const version = versionResult.stdout.trim().match(/^git-graft (\d+\.\d+\.\d+(?:[-+][\w.+-]+)?)$/)?.[1];
  assert(version,'Expected a git-graft version');
  const binary = readFileSync(bin);
  const graftBinary = {name:basename(bin),version,bytes:binary.length,sha256:createHash('sha256').update(binary).digest('hex')};
  const archive = npmArchive(ctx.root,dependencyMiB*1024*1024);
  const extra = join(ctx.root,'extra');
  put(extra,'package.json',JSON.stringify({name:'graft-bench-extra',version:'1.0.0',main:'index.js'}));
  put(extra,'index.js','module.exports = 7;\n');
  const [npmExe,npmPrefix] = npmCommand(ctx);
  const pack = ctx.command(npmExe,[...npmPrefix,'pack',extra,'--offline','--cache',join(ctx.root,'npm-cache'),'--pack-destination',ctx.root]);
  assert.equal(pack.status,0,pack.error?.message ?? pack.stderr);
  const fixture = {archive,extra:readFileSync(join(ctx.root,tarballName)),
    source:Array.from({length:3},() => randomBytes(largeMiB*1024*1024)),
    history:Array.from({length:historyCommits},() => randomBytes(historyKiB*1024))};
  const apfsCalibration = values['apfs-file-data'] ? observeApfs(ctx,true) : undefined;
  const measuredArms: (Arm | 'nativeConventional')[] = selectedArms.flatMap(arm =>
    arm === 'native' ? ['native','nativeConventional'] : ['graft']);
  const runs = Array.from({length:repeats}, (_,index) => {
    const offset = index % measuredArms.length;
    const order = [...measuredArms.slice(offset),...measuredArms.slice(0,offset)];
    const results = Object.fromEntries(order.map(arm => [arm,run(arm,fixture,apfsCalibration)]));
    for (const ref of ['head','landedTip','droppedTip'] as const) {
      assert.equal(new Set(Object.values(results).map(result => result[ref])).size,1,`Matched revision differs: ${ref}`);
    }
    return {order,results};
  });
  const summary = Object.fromEntries(measuredArms.map(arm => {
    const samples = runs.map(run => run.results[arm]);
    const apfsSamples = samples.flatMap(s => s.apfsFileData?.available ? [s.apfsFileData.checkpoints] : []);
    return [arm,{
      firstSpawnSeconds:median(samples.map(s => s.ready.first.spawnSeconds)),
      repeatSpawnSeconds:median(samples.map(s => s.ready.repeat.spawnSeconds)),
      firstReadySeconds:median(samples.map(s => s.ready.first.readySeconds)),
      repeatReadySeconds:median(samples.map(s => s.ready.repeat.readySeconds)),
      firstReadyIncludingSeedSeconds:median(samples.map(s => s.firstReadyIncludingSeedSeconds)),
      comparison:samples[0].comparison,
      totalSeconds:median(samples.map(s => s.totalSeconds)),
      ...(arm === 'native' ? {recoveryPreparationSeconds:median(samples.map(s => s.recoveryPreparationSeconds!))} : {}),
      includingPrimarySetupSeconds:median(samples.map(s => s.includingPrimarySetupSeconds)),
      retainedBytesAfterGc:median(samples.map(s => Number(s.stages.gc.bytesSincePopulatedCheckout))),
      retainedLogicalBytesAfterGc:median(samples.map(s => s.retainedLogicalBytesAfterGc)),
      primaryPackBytes:median(samples.map(s => s.primarySetup.historyGraph.packBytes)),
      primaryPackIndexBytes:median(samples.map(s => s.primarySetup.historyGraph.indexBytes)),
      apfsFileData:apfsSamples.length === samples.length ? {
        repeatBytesSincePopulatedCheckout:median(apfsSamples.map(s => Number(s.repeat!.bytesSincePopulatedCheckout))),
        addDependencyDeltaBytes:median(apfsSamples.map(s => Number(s.addDependency!.deltaBytes))),
        addDependencyBytesSincePopulatedCheckout:median(apfsSamples.map(s => Number(s.addDependency!.bytesSincePopulatedCheckout))),
        gcBytesSincePopulatedCheckout:median(apfsSamples.map(s => Number(s.gc!.bytesSincePopulatedCheckout))),
      } : undefined,
      resources:samples.every(s => s.resources !== null) ? {
        userSeconds:median(samples.map(s => s.resources!.userSeconds)),
        systemSeconds:median(samples.map(s => s.resources!.systemSeconds)),
        peakRssBytes:median(samples.map(s => s.resources!.peakRssBytes)),
      } : null,
      stages:Object.fromEntries(Object.keys(samples[0].stages).map(name => [name,{
        seconds:median(samples.map(s => s.stages[name].seconds)),
        deltaBytes:median(samples.map(s => Number(s.stages[name].deltaBytes))),
        bytesSincePopulatedCheckout:median(samples.map(s => Number(s.stages[name].bytesSincePopulatedCheckout))),
      }])),
    }];
  }));
  const fs = statfsSync(ctx.root,{bigint:true});
  const report = {
    schemaVersion:1, platform:process.platform, architecture:process.arch, node:process.version,
    graftBinary,
    apfsFileData:apfsCalibration ? {
      calibrated:apfsCalibration.available, observerSeconds:apfsCalibration.observerSeconds,
      ...(!apfsCalibration.available ? {reason:apfsCalibration.reason} : {}),
    } : undefined,
    historyPolicy:'local',fixture:{repeats,smallFiles,largeFiles:fixture.source.length,largeMiB,dependencyMiB,historyCommits,historyKiB,workspaces:2,
      additionalPackedBranchTips:values['mixed-history'] ? 1 : 0},
    filesystem:{typeCode:fs.type.toString(),blockSize:fs.bsize.toString()},
    scope:{
      timing:'Stage wall time includes commands and in-stage filesystem work. Validation, statfs, and temporary-directory cleanup are outside stage timings. The independently measured nativeConventional arm does not export private data or perform restore and second drop. The native arm includes private export and recovery for a stronger-semantics comparison with Graft; do not label it ordinary Git performance. Primary setup starts after initialization and includes creating and packing history, the checkout, and the initial offline install. Any history duplication in a workspace is charged where it occurs.',
      disk:'Signed volume-wide statfs bfree * bsize, relative to the populated primary with its own packed history and warm npm cache. Tracked changes and private files are added after seed because shared seed requires a clean index and tracked checkout. Background writes and delayed allocation can affect samples. The gc checkpoint includes retained recovery bytes. The logical comparison sums regular-file lengths within the fixture root, including every CoW copy at its apparent size.',
      ...(apfsCalibration ? {apfsFileData:'Union of APFS allocation blocks mapped to regular-file data within EOF, by device, within each arm root. Shared blocks and hardlinks count once. Directory and inode metadata, xattrs and resource forks are outside scope. A failed mapping invalidates all extent deltas for that arm. Observer time is excluded and reported separately. Scans can warm filesystem metadata caches, so omit --apfs-file-data for timing-only runs.'} : {}),
      recovery:'The native fixture retains a dropped commit in a separate bare repository and moves known private files before removal. Its restore fetches and checks out that commit and returns the saved dependency tree and scratch file. Native retains the landed build and dependency files and uses integration to retain the landed commit. These steps cover this fixture. Graft uses its public recovery and may retain more state, including the full retired private repository.',
      resources:'Optional per-command /usr/bin/time CPU and maximum RSS, with OS-specific child accounting. Stage and run CPU sum their command samples; peak RSS is their maximum, not simultaneous process-tree memory. Node harness filesystem work and validation are outside resource accounting. Executable sizes exclude shared libraries.',
    },
    nativeRecoveryComparison:selectedArms.includes('native') ? {
      conventionalTotalSeconds:median(runs.map(run => run.results.nativeConventional.totalSeconds)),
      privateRecoveryTotalSeconds:median(runs.map(run => run.results.native.totalSeconds)),
      pairedDifferenceSeconds:median(runs.map(run => run.results.native.totalSeconds-run.results.nativeConventional.totalSeconds)),
      scope:'Independent native runs with and without private retention, restore and second drop. The paired difference includes normal run-to-run noise; preparation time and recovery stages are also reported separately.',
    } : undefined,
    summary,runs,
  };
  mkdirSync(dirname(output),{recursive:true});
  writeFileSync(output,JSON.stringify(report,null,2)+'\n');
  process.stdout.write(`Matched workspace benchmark written to ${output}\n`);
} finally { ctx.close(); }
