/** Compare release CLI processes on disposable local clones of a real repository.
 * node e2e/real-source-bench.ts [--repo /opt/homebrew] [--bin PATH]
 *   [--output PATH] [--apfs-file-data] [--rounds N] [--advance-base] [--include-revisions]
 *   [--history local|isolated|tip] [--project-history local|isolated|tip] [--clean-primary] [--no-deps]
 *   [--global-attribute 'PATTERN ATTRIBUTES']
 * The source is read only. Local clones copy the original pack files without repacking;
 * all configuration, branches, and test commits are created in temporary repositories.
 */
import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import { appendFileSync, existsSync, lstatSync, mkdirSync, readFileSync, readdirSync, realpathSync, rmSync, statSync, statfsSync, writeFileSync } from 'node:fs';
import { basename, delimiter, dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { release, type as osType, version as osVersion } from 'node:os';
import { parseArgs } from 'node:util';
import { Context, objectResult } from './harness.ts';
import { objectBootstrap } from './benchmark-receipts.ts';

const {values} = parseArgs({options:{repo:{type:'string'},bin:{type:'string'},output:{type:'string'},history:{type:'string'},'project-history':{type:'string'},
  rounds:{type:'string'},'apfs-file-data':{type:'boolean'},'advance-base':{type:'boolean'},'include-revisions':{type:'boolean'},'clean-primary':{type:'boolean'},'no-deps':{type:'boolean'},'global-attribute':{type:'string'},autocrlf:{type:'string'}}});
const repo = resolve(values.repo ?? '/opt/homebrew');
const bin = resolve(values.bin ?? process.env.GIT_GRAFT_BIN ?? join(import.meta.dirname,'../target/release',
  process.platform === 'win32' ? 'git-graft.exe' : 'git-graft'));
const output = resolve(values.output ?? 'target/e2e/real-source-bench.json');
const rounds = Number(values.rounds ?? 4);
const history = values.history;
const projectHistory = values['project-history'];
const autocrlf = values.autocrlf;
assert(autocrlf === undefined || ['true', 'false', 'input'].includes(autocrlf), 'Expected true, false or input for --autocrlf');
for (const policy of [projectHistory, history]) {
  assert(policy === undefined || ['isolated', 'local', 'tip'].includes(policy), 'Expected isolated, local or tip history');
}
const historyArgs = history ? ['--history', history] : [];
const dependencyArgs = values['no-deps'] ? ['--no-deps'] : [];
assert(Number.isSafeInteger(rounds) && rounds > 0, '--rounds must be a positive integer');
assert(existsSync(bin) && /^git-graft(?:\.exe)?$/i.test(basename(bin)), 'Expected release git-graft executable');
assert(existsSync(repo), 'Source repository does not exist');

const fixture = new Context(bin);
try {
  if (autocrlf !== undefined) fixture.git(['config', '--global', 'core.autocrlf', autocrlf]);
  const globalAttribute = values['global-attribute'];
  if (globalAttribute !== undefined) {
    const configDir = join(fixture.root, 'home', '.config', 'git');
    mkdirSync(configDir, {recursive:true});
    writeFileSync(join(configDir, 'attributes'), globalAttribute + '\n');
  }
  const envPath = Object.keys(fixture.env).find(key => key.toUpperCase() === 'PATH') ?? 'PATH';
  fixture.env[envPath] = [dirname(bin),fixture.env[envPath] ?? ''].join(delimiter);
  fixture.env.GIT_NO_LAZY_FETCH = '1';
  fixture.env.GIT_AUTHOR_DATE = fixture.env.GIT_COMMITTER_DATE = '2001-01-01T00:00:00Z';
  const command = (args: string[], cwd = fixture.root) => {
    const result = fixture.command('git',args,cwd);
    assert.equal(result.status,0,result.error?.message ?? result.stderr);
    return result.stdout.trim();
  };
  const sourceRoot = realpathSync(command(['-C',repo,'rev-parse','--show-toplevel']));
  const sourceGitDir = realpathSync(command(['-C',repo,'rev-parse','--absolute-git-dir']));
  try { assert(!lstatSync(output).isSymbolicLink(), 'Output must not be a symlink'); }
  catch (error) { if ((error as NodeJS.ErrnoException).code !== 'ENOENT') throw error; }
  let ancestor = dirname(output);
  while (!existsSync(ancestor)) ancestor = dirname(ancestor);
  const resolvedOutput = existsSync(output) ? realpathSync(output)
    : resolve(realpathSync(ancestor),relative(ancestor,output));
  for (const protectedRoot of [sourceRoot,sourceGitDir]) {
    const path = relative(protectedRoot,resolvedOutput);
    assert(path === '..' || path.startsWith(`..${sep}`) || isAbsolute(path), 'Output must be outside the source repository');
  }
  const objects = command(['-C',repo,'rev-parse','--git-path','objects']);
  const sourceObjects = resolve(repo,objects);
  assert(!existsSync(join(sourceObjects,'info','alternates')), 'Source with alternate object stores is unsupported');
  assert.equal(command(['-C',repo,'rev-parse','--is-shallow-repository']),'false','Source must have complete history');
  const head = command(['-C',repo,'rev-parse','HEAD^{commit}']);
  const tree = command(['-C',repo,'rev-parse','HEAD^{tree}']);
  const existingUnrelated = command(['-C',repo,'rev-list','--all','--max-count=1','--not',head]);
  const packs = readdirSync(join(sourceObjects,'pack')).filter(name => name.endsWith('.pack')).sort();
  assert(packs.length > 0,'Source must contain packed history');
  const template = join(fixture.root,'template-repo');
  const prepStart = performance.now();
  command(['clone','--quiet','--no-hardlinks','--no-checkout','--',repo,template]);
  command(['remote','remove','origin'],template);
  command(['checkout','--quiet','--detach',head],template);
  command(['config','gc.auto','0'],template);
  const prepSeconds = (performance.now()-prepStart)/1000;
  const copiedPacks = readdirSync(join(template,'.git','objects','pack')).filter(name => name.endsWith('.pack')).sort();
  assert.deepEqual(copiedPacks,packs,'Local clone did not retain the primary pack layout');
  const free = () => { const fs = statfsSync(fixture.root,{bigint:true}); return fs.bfree*fs.bsize; };
  const observe = (root: string, calibrate = false, groups = false): {available?: unknown; bytes?: unknown; reason?: unknown; groups?: Record<string,string>; observerSeconds:number} => {
    const start = performance.now();
    const result = fixture.command('python3',['-I','-B',join(import.meta.dirname,'apfs-file-data.py'),
      ...(calibrate ? ['--calibrate'] : []), ...(groups ? ['--checkout-groups'] : []), root]);
    const observerSeconds = (performance.now()-start)/1000;
    if (result.status !== 0) return {available:false,reason:'observer-failed',observerSeconds};
    try { return {...objectResult(JSON.parse(result.stdout)),observerSeconds}; }
    catch { return {available:false,reason:'observer-failed',observerSeconds}; }
  };
  const calibration = values['apfs-file-data'] ? observe(fixture.root,true) : undefined;
  const fileDataDelta = (before: ReturnType<typeof observe> | undefined, after: ReturnType<typeof observe> | undefined) => {
    if (!before?.groups || !after?.groups || typeof before.bytes !== 'string' || typeof after.bytes !== 'string') return undefined;
    const delta: Record<string,string> = {};
    let total = 0n;
    for (const [group,bytes] of Object.entries(after.groups)) {
      const change = BigInt(bytes) - BigInt(before.groups[group]);
      delta[group === 'primary' ? 'primaryChange' : group] = change.toString();
      total += change;
    }
    assert.equal(total,BigInt(after.bytes)-BigInt(before.bytes),'APFS attribution must reconcile to the measured delta');
    return delta;
  };
  const probe = process.platform === 'darwin' ? 'bsd' : process.platform === 'linux' ? 'gnu' : null;
  const timeArgs = probe === 'bsd' ? ['-l','-p'] : ['-f','graft-bench %U %S %M'];
  const useResources = probe !== null && existsSync('/usr/bin/time') &&
    fixture.command('/usr/bin/time',[...timeArgs,'/usr/bin/true']).status === 0;
  const resources = (stderr: string) => {
    if (probe === 'bsd') {
      const user = stderr.match(/^user (\d+\.\d+)$/m);
      const system = stderr.match(/^sys (\d+\.\d+)$/m);
      const rss = stderr.match(/^\s*(\d+)\s+maximum resident set size$/m);
      return user && system && rss ? {userSeconds:Number(user[1]),systemSeconds:Number(system[1]),peakRssBytes:Number(rss[1])} : null;
    }
    const match = stderr.match(/^graft-bench ([\d.]+) ([\d.]+) (\d+)$/m);
    return match ? {userSeconds:Number(match[1]),systemSeconds:Number(match[2]),peakRssBytes:Number(match[3])*1024} : null;
  };
  const sourceReceipt = (storage: Record<string,unknown>) => {
    const safe: Record<string,string|number|boolean> = {};
    if (typeof storage.backend === 'string' && ['git','empty','clonefile','reflink','native','copy','mixed'].includes(storage.backend)) safe.backend = storage.backend;
    for (const key of ['cloned_files','cloned_bytes','private_files','private_bytes','primary_reused_files','primary_reused_bytes']) {
      const value = storage[key];
      if (typeof value === 'number' && Number.isSafeInteger(value) && value >= 0) safe[key] = value;
    }
    if (typeof storage.snapshot_reused === 'boolean') safe.snapshot_reused = storage.snapshot_reused;
    return safe;
  };
  const run = (arm: 'native'|'graft', round: number) => {
    const root = join(fixture.root,`round-${round}-${arm}`);
    const primary = join(root,'primary');
    mkdirSync(root);
    const cloneStart = performance.now();
    command(['clone','--quiet','--no-hardlinks','--no-checkout','--',template,primary]);
    command(['remote','remove','origin'],primary);
    command(['checkout','--quiet','--detach',head],primary);
    command(['config','gc.auto','0'],primary);
    if (projectHistory) command(['config','--local','graft.history',projectHistory],primary);
    const cloneSeconds = (performance.now()-cloneStart)/1000;
    const branch = command(['commit-tree',`${head}^{tree}`,'-p',head,'-m',`unrelated benchmark commit ${round}`],primary);
    command(['update-ref','refs/heads/unrelated-benchmark',branch],primary);
    if (existingUnrelated) command(['update-ref','refs/heads/unrelated-source',existingUnrelated],primary);
    // Deliberately dirty only this disposable primary; neither workspace may
    // receive the files or change the primary's index, HEAD, config or excludes.
    const prefix = `.graft-bench-${randomBytes(8).toString('hex')}`;
    const staged = `${prefix}-staged`, untracked = `${prefix}-untracked`, ignored = `${prefix}-ignored`;
    for (const name of [staged,untracked,ignored]) assert(!existsSync(join(primary,name)));
    const donorFiles = values['clean-primary'] ? [] :
      [[staged,'unstaged data\n'],[untracked,'untracked data\n'],[ignored,'ignored data\n']];
    if (donorFiles.length) {
      writeFileSync(join(primary,staged),'staged data\n');
      command(['add','-f','--',staged],primary);
      for (const [file,content] of donorFiles) writeFileSync(join(primary,file),content);
      mkdirSync(join(primary,'.git','info'),{recursive:true});
      appendFileSync(join(primary,'.git','info','exclude'),`\n/${ignored}\n`);
    }
    const primaryStatus = command(['--no-optional-locks','status','--porcelain','--untracked-files=all'],primary);
    if (values['clean-primary']) assert.equal(primaryStatus,'');
    const readMetadata = (name: string) => {
      const path = join(primary,'.git',name);
      return existsSync(path) ? readFileSync(path) : null;
    };
    const metadata = ['HEAD','index','config','info/exclude'].map(file => [file,readMetadata(file)] as const);
    const before = free();
    const beforeApfs = calibration?.available === true ? observe(root,false,arm === 'graft') : undefined;
    let firstApfs: ReturnType<typeof observe> | undefined;
    const workspacePaths: string[] = [];
    const firstStatus = (path: string) => {
      const start = performance.now();
      assert.equal(command(['status','--porcelain','--untracked-files=all'],path),'');
      return (performance.now()-start)/1000;
    };
    const checkHistory = (path: string, storage: Record<string,unknown>) => {
      const objects = objectResult(storage.objects);
      const local = objects.source === 'local-snapshot';
      const policy = history ?? projectHistory ?? 'local';
      if (policy !== 'local') assert(!local, 'Restricted history used the whole donor database');
      if (policy === 'tip') assert.equal(command(['rev-list','--count','HEAD'],path),'1');
      for (const oid of [branch,...(existingUnrelated ? [existingUnrelated] : [])]) {
        const read = fixture.command('git',['cat-file','-e',oid],path);
        if (local) assert.equal(read.status,0,'Local snapshot lost a primary Git object');
        else assert([1,128].includes(read.status ?? -1),'Isolated checkout exposed an unrelated object');
      }
      assert.equal(command(['for-each-ref','refs/heads/unrelated-benchmark'],path),'','Primary ref was copied');
    };
    const samples = [0,1].map(index => {
      const name = `agent-${index}`;
      const target = join(root,name);
      const args = arm === 'native' ? ['worktree','add','-b',`graft/${name}`,target,head]
        : ['graft','--repo',primary,'--json','spawn',name,...historyArgs,'--base',head,...dependencyArgs];
      const start = performance.now();
      const result = useResources ? fixture.command('/usr/bin/time',[...timeArgs,'git',...args],primary)
        : fixture.command('git',args,primary);
      const seconds = (performance.now()-start)/1000;
      assert.equal(result.status,0,result.error?.message ?? result.stderr);
      const receipt = arm === 'graft' ? objectResult(JSON.parse(result.stdout)) : undefined;
      const path = receipt ? receipt.path : target;
      assert(typeof path === 'string');
      workspacePaths.push(path);
      const usedBytes = (before-free()).toString();
      const firstStatusSeconds = firstStatus(path);
      assert.equal(command(['rev-parse','HEAD'],path),head);
      assert.equal(command(['rev-parse','HEAD^{tree}'],path),tree);
      for (const file of [staged,untracked,ignored]) assert(!existsSync(join(path,file)),`Private primary file appeared in workspace: ${file}`);
      const storage = receipt ? objectResult(receipt.storage) : undefined;
      if (storage) checkHistory(path, storage);
      const objectStorage = storage ? objectBootstrap(storage.objects,values['include-revisions'] === true) : undefined;
      if (index === 0 && beforeApfs?.available === true) firstApfs = observe(root,false,arm === 'graft');
      return {seconds,firstStatusSeconds,resources:useResources ? resources(result.stderr) : null,bytesSincePopulatedCheckout:usedBytes,
        sourceStorage:storage ? sourceReceipt(storage) : undefined,objectBootstrap:objectStorage};
    });
    assert.equal(command(['rev-parse','HEAD'],primary),head);
    assert.equal(command(['rev-parse','HEAD^{tree}'],primary),tree);
    assert.equal(command(['rev-parse','refs/heads/unrelated-benchmark'],primary),branch);
    if (existingUnrelated) assert.equal(command(['rev-parse','refs/heads/unrelated-source'],primary),existingUnrelated);
    assert.equal(command(['--no-optional-locks','status','--porcelain','--untracked-files=all'],primary),primaryStatus);
    for (const [file,bytes] of metadata) assert.deepEqual(readMetadata(file),bytes,`Primary Git metadata changed: ${file}`);
    for (const [file,expected] of donorFiles)
      assert.equal(readFileSync(join(primary,file),'utf8'),expected,`Primary data changed: ${file}`);
    const afterApfs = beforeApfs?.available === true ? observe(root,false,arm === 'graft') : undefined;
    const apfs = typeof beforeApfs?.bytes === 'string' && typeof firstApfs?.bytes === 'string' && typeof afterApfs?.bytes === 'string'
      ? {bytesSincePopulatedCheckout:(BigInt(afterApfs.bytes)-BigInt(beforeApfs.bytes)).toString(),
        firstCheckoutBytes:(BigInt(firstApfs.bytes)-BigInt(beforeApfs.bytes)).toString(),
        additionalSecondCheckoutBytes:(BigInt(afterApfs.bytes)-BigInt(firstApfs.bytes)).toString(),
        firstCheckoutFileDataByPurpose:fileDataDelta(beforeApfs,firstApfs),
        additionalSecondCheckoutFileDataByPurpose:fileDataDelta(firstApfs,afterApfs),
        fileDataByPurpose:fileDataDelta(beforeApfs,afterApfs),
        observerSeconds:beforeApfs.observerSeconds+firstApfs.observerSeconds+afterApfs.observerSeconds}
      : values['apfs-file-data'] ? {unavailable:calibration?.reason ?? beforeApfs?.reason ?? firstApfs?.reason ?? afterApfs?.reason ?? 'observer-failed'} : undefined;
    let advanceBase;
    if (values['advance-base']) {
      // Construct a real one-file commit with a temporary index, preserving the
      // primary's staged/unstaged changes. Preparation is outside CLI timing.
      const prepStart = performance.now();
      const file = `.graft-bench-advance-${head.slice(0,12)}`;
      assert(!existsSync(join(primary,file)), 'Advance fixture path already exists');
      const content = 'new committed source\n';
      const payload = join(root,'advance-payload');
      const temporaryIndex = join(root,'advance-index');
      writeFileSync(payload,content);
      const blob = command(['hash-object','-w',payload],primary);
      const previousIndex = fixture.env.GIT_INDEX_FILE;
      let advancedTree: string;
      try {
        fixture.env.GIT_INDEX_FILE = temporaryIndex;
        command(['read-tree',head],primary);
        command(['update-index','--add','--cacheinfo',`100644,${blob},${file}`],primary);
        advancedTree = command(['write-tree'],primary);
      } finally {
        if (previousIndex === undefined) delete fixture.env.GIT_INDEX_FILE;
        else fixture.env.GIT_INDEX_FILE = previousIndex;
        rmSync(temporaryIndex,{force:true});
      }
      const advanced = command(['commit-tree',advancedTree,'-p',head,'-m','benchmark one source change'],primary);
      command(['update-ref','refs/heads/benchmark-advanced',advanced],primary);
      command(['symbolic-ref','HEAD','refs/heads/benchmark-advanced'],primary);
      command(['update-index','--add','--cacheinfo',`100644,${blob},${file}`],primary);
      writeFileSync(join(primary,file),content);
      assert.equal(command(['--no-optional-locks','status','--porcelain','--untracked-files=all'],primary),primaryStatus);
      const advancedMetadata = metadata.map(([name]) => [name,readMetadata(name)] as const);
      const preparationSeconds = (performance.now()-prepStart)/1000;
      // Start after the primary already owns the new file, commit and index.
      const advanceApfsBefore = calibration?.available === true ? observe(root) : undefined;
      const beforeAdvance = free();
      const target = join(root,'agent-2');
      const args = arm === 'native' ? ['worktree','add','-b','graft/agent-2',target,advanced]
        : ['graft','--repo',primary,'--json','spawn','agent-2',...historyArgs,...dependencyArgs];
      const start = performance.now();
      const result = useResources ? fixture.command('/usr/bin/time',[...timeArgs,'git',...args],primary)
        : fixture.command('git',args,primary);
      const seconds = (performance.now()-start)/1000;
      const deltaBytes = (beforeAdvance-free()).toString();
      assert.equal(result.status,0,result.error?.message ?? result.stderr);
      const receipt = arm === 'graft' ? objectResult(JSON.parse(result.stdout)) : undefined;
      const path = receipt ? receipt.path : target;
      assert(typeof path === 'string');
      const firstStatusSeconds = firstStatus(path);
      assert.equal(command(['rev-parse','HEAD'],path),advanced);
      assert.equal(command(['rev-parse','HEAD^{tree}'],path),advancedTree);
      if (arm === 'graft' && (history ?? projectHistory) === 'tip') {
        assert.notEqual(fixture.command('git',['cat-file','-e',head],path).status,0);
      } else {
        assert.equal(command(['rev-parse','HEAD^'],path),head);
      }
      assert.equal(readFileSync(join(path,file),'utf8'),content);
      for (const original of workspacePaths) {
        assert.equal(command(['rev-parse','HEAD'],original),head);
        assert.equal(command(['rev-parse','HEAD^{tree}'],original),tree);
        assert(!existsSync(join(original,file)));
      }
      for (const privateFile of [staged,untracked,ignored]) assert(!existsSync(join(path,privateFile)));
      assert.equal(command(['symbolic-ref','HEAD'],primary),'refs/heads/benchmark-advanced');
      assert.equal(command(['rev-parse','HEAD'],primary),advanced);
      assert.equal(command(['--no-optional-locks','status','--porcelain','--untracked-files=all'],primary),primaryStatus);
      for (const [name,bytes] of advancedMetadata) assert.deepEqual(readMetadata(name),bytes,`Advanced primary metadata changed: ${name}`);
      for (const [name,expected] of [...donorFiles,[file,content]])
        assert.equal(readFileSync(join(primary,name),'utf8'),expected);
      const storage = receipt ? objectResult(receipt.storage) : undefined;
      if (storage) checkHistory(path, storage);
      const advanceApfsAfter = advanceApfsBefore?.available === true ? observe(root) : undefined;
      const apfs = typeof advanceApfsBefore?.bytes === 'string' && typeof advanceApfsAfter?.bytes === 'string'
        ? {additionalCheckoutBytes:(BigInt(advanceApfsAfter.bytes)-BigInt(advanceApfsBefore.bytes)).toString(),
          observerSeconds:advanceApfsBefore.observerSeconds+advanceApfsAfter.observerSeconds}
        : values['apfs-file-data'] ? {unavailable:calibration?.reason ?? advanceApfsBefore?.reason ?? advanceApfsAfter?.reason ?? 'observer-failed'} : undefined;
      advanceBase = {head:advanced,tree:advancedTree,preparationSeconds,seconds,firstStatusSeconds,resources:useResources ? resources(result.stderr) : null,deltaBytes,
        apfs,sourceStorage:storage ? sourceReceipt(storage) : undefined,objectBootstrap:storage ? objectBootstrap(storage.objects,values['include-revisions'] === true) : undefined};
    }
    rmSync(root,{recursive:true,force:true});
    return {cloneSeconds,cold:samples[0],repeat:samples[1],apfs,...(advanceBase ? {advanceBase} : {})};
  };
  const runs = Array.from({length:rounds},(_,round) => {
    const order: ('native'|'graft')[] = round % 2 ? ['graft','native'] : ['native','graft'];
    return {order,results:Object.fromEntries(order.map(arm => [arm,run(arm,round)]))};
  });
  if (values['advance-base']) {
    const commits = runs.flatMap(run => ['native','graft'].map(arm => run.results[arm].advanceBase!.head));
    assert.equal(new Set(commits).size,1,'Advanced commits differ between benchmark arms');
  }
  if (!values['include-revisions']) for (const run of runs) for (const arm of ['native','graft']) {
    const sample = run.results[arm].advanceBase;
    if (sample) { delete (sample as Partial<typeof sample>).head; delete (sample as Partial<typeof sample>).tree; }
  }
  const median = (values: number[]) => {const a = [...values].sort((x,y)=>x-y); const m = Math.floor(a.length/2); return a.length%2 ? a[m] : (a[m-1]+a[m])/2;};
  const summary = Object.fromEntries((['native','graft'] as const).map(arm => {
    const samples = runs.map(run => run.results[arm]);
    return [arm,{coldSeconds:median(samples.map(s=>s.cold.seconds)),repeatSeconds:median(samples.map(s=>s.repeat.seconds)),
      coldBytes:median(samples.map(s=>Number(s.cold.bytesSincePopulatedCheckout))),
      repeatBytes:median(samples.map(s=>Number(s.repeat.bytesSincePopulatedCheckout))),
      ...(values['advance-base'] ? {advanceBaseSeconds:median(samples.map(s=>s.advanceBase!.seconds)),
        advanceBaseDeltaBytes:median(samples.map(s=>Number(s.advanceBase!.deltaBytes)))} : {})}];
  }));
  const binary = readFileSync(bin);
  const report = {schemaVersion:1,platform:process.platform,architecture:process.arch,node:process.version,
    operatingSystem:{type:osType(),release:release(),version:osVersion()},
    resourceObserver:{tool:useResources ? '/usr/bin/time' : null,format:useResources ? probe : null,
      scope:'Per CLI command; user and system CPU seconds and peak RSS. Includes the time wrapper; child accounting is OS-specific and is not simultaneous process-tree memory.'},
    git:command(['--version']),binary:{bytes:binary.length,sha256:createHash('sha256').update(binary).digest('hex')},
    source:{...(values['include-revisions'] ? {head,tree} : {}),packCount:packs.length,packBytes:packs.reduce((n,p)=>n+statSync(join(sourceObjects,'pack',p)).size,0),
      commits:Number(command(['rev-list','--count',head],template)),hasUnrelatedSourceCommit:Boolean(existingUnrelated)},
    preparation:{templateCloneSeconds:prepSeconds,method:'local clone --no-hardlinks; source pack layout preserved; no source configuration copied'},
    requestedHistory:history ?? 'default',effectiveHistory:history ?? projectHistory ?? 'local',primaryCheckout:values['clean-primary'] ? 'clean' : 'dirty',apfsCalibration:calibration,summary,runs,
    projectHistory,globalAttribute,autocrlf,dependencyDiscovery:!values['no-deps'],
    scope:'Each round starts from a fresh populated clone. Spawn timings include the entire CLI process and cold object setup. The first git status is timed separately for each workspace. Clone preparation and advanced commit creation are outside both timers. Checks verify source contents, primary state and object visibility for the actual history mode. Signed statfs deltas cover the volume. Separately timed APFS scans can warm caches and exclude metadata, forks and xattrs.'};
  mkdirSync(dirname(output),{recursive:true});
  writeFileSync(output,JSON.stringify(report,null,2)+'\n');
  process.stdout.write(`Real-source benchmark written to ${output}\n`);
} finally { fixture.close(); }
