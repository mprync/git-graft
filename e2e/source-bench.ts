/** Source-only release comparison. Run with node e2e/source-bench.ts
 * [--bin target/release/git-graft] [--output PATH] [--apfs-file-data].
 * Use --smoke for a tiny fixture. GIT_GRAFT_TEST_VOLUME selects its volume. */
import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, statfsSync, writeFileSync } from 'node:fs';
import { basename, delimiter, dirname, join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { Context, objectResult } from './harness.ts';
import { objectBootstrap } from './benchmark-receipts.ts';

const arms = ['native','graft'] as const;
type Arm = typeof arms[number];
const {values} = parseArgs({options:{
  bin:{type:'string'}, output:{type:'string'}, smoke:{type:'boolean'}, 'apfs-file-data':{type:'boolean'},
  repeats:{type:'string'}, 'small-files':{type:'string'}, 'large-mib':{type:'string'},
  'history-commits':{type:'string'}, 'history-kib':{type:'string'},
}});
function positive(key: 'repeats' | 'small-files' | 'large-mib' | 'history-commits' | 'history-kib', normal: number, smoke: number) {
  const value = Number(values[key] ?? (values.smoke ? smoke : normal));
  assert(Number.isSafeInteger(value) && value > 0,`--${key} must be a positive integer`);
  return value;
}
const repeats = positive('repeats',3,1);
const smallFiles = positive('small-files',4000,12);
const largeMiB = positive('large-mib',8,1);
const historyCommits = positive('history-commits',8,2);
const historyKiB = positive('history-kib',1024,16);
const bin = resolve(values.bin ?? process.env.GIT_GRAFT_BIN ?? join(import.meta.dirname,'../target/release',
  process.platform === 'win32' ? 'git-graft.exe' : 'git-graft'));
const output = resolve(values.output ?? 'target/e2e/source-bench.json');
assert(existsSync(bin) && /^git-graft(?:\.exe)?$/i.test(basename(bin)),`Expected release executable named git-graft at ${bin}`);

function put(root: string, path: string, data: string | Buffer) {
  const file = join(root,path);
  mkdirSync(dirname(file),{recursive:true});
  writeFileSync(file,data);
}
function free(root: string) {
  const fs = statfsSync(root,{bigint:true});
  return fs.bsize * fs.bfree;
}
function digest(root: string, files: string[]) {
  const hash = createHash('sha256');
  for (const file of files) {
    hash.update(file).update('\0');
    hash.update(createHash('sha256').update(readFileSync(join(root,file))).digest());
  }
  return hash.digest('hex');
}
function observeApfs(ctx: Context, calibrate = false) {
  if (process.platform !== 'darwin') return {reason:'unsupported-platform'};
  const helper = join(import.meta.dirname,'apfs-file-data.py');
  const start = performance.now();
  const result = ctx.command('python3',['-I','-B',helper,...(calibrate ? ['--calibrate',dirname(ctx.root)] : [ctx.root])]);
  const observerSeconds = (performance.now()-start)/1000;
  if (result.status === 0) {
    try {
      const data = objectResult(JSON.parse(result.stdout));
      if (data.available === true) {
        if (calibrate) return {observerSeconds};
        if (typeof data.bytes === 'string' && /^\d+$/.test(data.bytes)) return {bytes:data.bytes,observerSeconds};
      }
      if (data.available === false && typeof data.reason === 'string' && /^[a-z0-9-]{1,64}$/.test(data.reason)) {
        return {reason:data.reason,observerSeconds};
      }
    } catch { /* Fall back to statfs when the observer response is unusable. */ }
  }
  const reason = (result.error as NodeJS.ErrnoException | undefined)?.code === 'ENOENT'
    ? 'python3-unavailable' : 'observer-failed';
  return {reason,observerSeconds};
}
type Source = {repo:string; head:string; tree:string; files:string[]; sha256:string};
function run(arm: Arm, source: Source, measureApfs: boolean) {
  const ctx = new Context(bin);
  try {
    // Each arm gets the exact same packed history and checkout in a fresh root.
    rmSync(ctx.repo,{recursive:true});
    cpSync(source.repo,ctx.repo,{recursive:true});
    const gitExecPath = ctx.git(['--exec-path']);
    const pathKey = Object.keys(ctx.env).find(k => k.toUpperCase() === 'PATH') ?? 'PATH';
    ctx.env[pathKey] = [ctx.env[pathKey] ?? '',gitExecPath].join(delimiter);
    ctx.env.GIT_EXEC_PATH = dirname(bin);
    ctx.env.GIT_AUTHOR_DATE = ctx.env.GIT_COMMITTER_DATE = '2001-01-01T00:00:00Z';
    const status = (path: string) => ctx.git(['--no-optional-locks','status','--porcelain','--untracked-files=all'],path);
    ctx.git(['update-index','--refresh']);
    assert.equal(status(ctx.repo),'');
    assert.equal(ctx.git(['rev-parse','HEAD']),source.head);
    put(ctx.repo,'tracked.txt','primary staged\n');
    ctx.git(['add','tracked.txt']);
    put(ctx.repo,'tracked.txt','primary unstaged\n');
    put(ctx.repo,'local.txt','primary untracked\n');
    put(ctx.repo,'ignored/private.txt','primary ignored\n');
    const primaryStatus = status(ctx.repo);
    const indexPath = join(ctx.repo,'.git','index');
    const primaryIndex = readFileSync(indexPath);
    const baseline = free(ctx.root);
    const baselineApfs = measureApfs ? observeApfs(ctx) : undefined;
    const checkouts = ['agent-0','agent-1'].map(name => {
      const branch = `graft/${name}`;
      let path = join(ctx.root,name);
      const argv = arm === 'native' ? ['worktree','add','-b',branch,path,source.head] :
        ['graft','--repo',ctx.repo,'--json',
          'spawn',name,'--base',source.head,'--no-deps'];
      const start = performance.now();
      const result = ctx.command('git',argv);
      const seconds = (performance.now()-start)/1000;
      const bytesSincePopulatedCheckout = (baseline-free(ctx.root)).toString();
      assert.equal(result.status,0,`${arm} spawn failed: ${result.error?.message ?? result.stderr}`);
      let objects: Record<string, unknown> | undefined;
      if (arm !== 'native') {
        const receipt = objectResult(JSON.parse(result.stdout));
        objects = objectBootstrap(objectResult(receipt.storage).objects);
        const checkout = receipt.path;
        assert(typeof checkout === 'string');
        assert.equal(receipt.branch,branch);
        path = checkout;
      }
      return {path,branch,seconds,bytesSincePopulatedCheckout,objectBootstrap:objects};
    });
    const repeatApfs = baselineApfs?.bytes !== undefined ? observeApfs(ctx) : undefined;
    const first = checkouts[0];
    const repeat = checkouts[1];
    for (const checkout of checkouts) {
      assert.equal(status(checkout.path),'');
      assert.equal(ctx.git(['rev-parse','HEAD'],checkout.path),source.head);
      assert.equal(ctx.git(['rev-parse','HEAD^{tree}'],checkout.path),source.tree);
      assert.equal(ctx.git(['symbolic-ref','HEAD'],checkout.path),`refs/heads/${checkout.branch}`);
      assert.equal(digest(checkout.path,source.files),source.sha256);
      assert(!['local.txt','ignored/private.txt','node_modules'].some(file => existsSync(join(checkout.path,file))));
      const ref = ctx.command('git',['show-ref','--verify','--quiet',`refs/heads/${checkout.branch}`]);
      assert.equal(ref.status,arm === 'graft' ? 1 : 0,ref.stderr);
    }
    const listed = ctx.git(['worktree','list','--porcelain']).split('\n').filter(line => line.startsWith('worktree '));
    assert.equal(listed.length,arm === 'graft' ? 1 : 3);
    put(first.path,'tracked.txt','isolated workspace edit\n');
    assert.equal(status(first.path),' M tracked.txt');
    ctx.git(['add','tracked.txt'],first.path);
    ctx.git(['commit','-qm','isolated workspace edit'],first.path);
    assert.equal(status(first.path),'');
    assert.equal(ctx.git(['rev-parse','HEAD^'],first.path),source.head);
    const editedHead = ctx.git(['rev-parse','HEAD'],first.path);
    assert.equal(ctx.git(['rev-parse','HEAD'],repeat.path),source.head);
    assert.equal(status(repeat.path),'');
    assert.equal(readFileSync(join(repeat.path,'tracked.txt'),'utf8'),'initial\n');
    assert.equal(ctx.git(['rev-parse','HEAD']),source.head);
    assert.deepEqual(readFileSync(indexPath),primaryIndex,'primary index changed');
    assert.equal(status(ctx.repo),primaryStatus);
    for (const [path,text] of [['tracked.txt','primary unstaged\n'],['local.txt','primary untracked\n'],['ignored/private.txt','primary ignored\n']]) {
      assert.equal(readFileSync(join(ctx.repo,path),'utf8'),text);
    }
    const sample = ({seconds,bytesSincePopulatedCheckout,objectBootstrap}: typeof first) => ({seconds,bytesSincePopulatedCheckout,objectBootstrap});
    return {cold:sample(first),repeat:sample(repeat),editedHead,
      apfsFileData:baselineApfs ? (baselineApfs.bytes !== undefined && repeatApfs?.bytes !== undefined ? {
        baselineBytes:baselineApfs.bytes,repeatBytes:repeatApfs.bytes,
        repeatBytesSincePopulatedCheckout:(BigInt(repeatApfs.bytes)-BigInt(baselineApfs.bytes)).toString(),
        observerSeconds:(baselineApfs.observerSeconds ?? 0)+(repeatApfs.observerSeconds ?? 0),
      } : {unavailable:baselineApfs.reason ?? repeatApfs?.reason ?? 'observer-failed'}) : undefined};
  } finally { ctx.close(); }
}

function median(values: number[]) {
  const sorted = [...values].sort((a,b) => a-b);
  const middle = Math.floor(sorted.length/2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle-1]+sorted[middle])/2;
}
const fixture = new Context(bin);
try {
  for (let i=0; i<historyCommits; i++) {
    put(fixture.repo,'historical.bin',randomBytes(historyKiB*1024));
    fixture.git(['add','historical.bin']);
    fixture.git(['commit','-qm',`replace historical data ${i}`]);
  }
  rmSync(join(fixture.repo,'historical.bin'));
  for (let i=0; i<smallFiles; i++) {
    const unit = String(i).padStart(5,'0');
    const body = Array.from({length:8+(i%31)},(_,j) => `export const entry${j} = ${JSON.stringify(`module ${unit}, entry ${j}`)};\n`).join('');
    put(fixture.repo,`src/group-${Math.floor(i/100)}/unit-${unit}.ts`,body);
  }
  for (let i=0; i<3; i++) put(fixture.repo,`assets/large-${i}.bin`,randomBytes(largeMiB*1024*1024));
  fixture.git(['add','-A']);
  fixture.git(['commit','-qm','source benchmark fixture']);
  fixture.git(['repack','-adq']);
  const head = fixture.git(['rev-parse','HEAD']);
  const tree = fixture.git(['rev-parse','HEAD^{tree}']);
  const files = fixture.git(['ls-files','-z']).split('\0').filter(Boolean);
  const sha256 = digest(fixture.repo,files);
  const source = {repo:fixture.repo,head,tree,files,sha256};
  const calibration = values['apfs-file-data'] ? observeApfs(fixture,true) : undefined;
  const runs = Array.from({length:repeats},(_,i) => {
    const offset = i % arms.length;
    const order = [...arms.slice(offset),...arms.slice(0,offset)];
    const results = Object.fromEntries(order.map(arm => [arm,run(arm,source,Boolean(calibration && !calibration.reason))]));
    assert.equal(new Set(Object.values(results).map(result => result.editedHead)).size,1);
    return {order,results};
  });
  const summary = Object.fromEntries(arms.map(arm => {
    const samples = runs.map(run => run.results[arm]);
    const apfs = samples.flatMap(s => s.apfsFileData?.repeatBytesSincePopulatedCheckout ?? []);
    return [arm,{
      coldSeconds:median(samples.map(s => s.cold.seconds)),
      repeatSeconds:median(samples.map(s => s.repeat.seconds)),
      coldStatfsBytes:median(samples.map(s => Number(s.cold.bytesSincePopulatedCheckout))),
      repeatStatfsBytes:median(samples.map(s => Number(s.repeat.bytesSincePopulatedCheckout))),
      apfsRepeatBytes:apfs.length === samples.length ? median(apfs.map(Number)) : undefined,
    }];
  }));
  const binary = readFileSync(bin);
  const report = {
    schemaVersion:1, platform:process.platform, architecture:process.arch, node:process.version, git:fixture.git(['--version']),
    graftBinary:{name:basename(bin),bytes:binary.length,sha256:createHash('sha256').update(binary).digest('hex')},
    fixture:{head,tree,sha256,trackedFiles:files.length,smallFiles,largeFiles:3,largeMiB,historyCommits,historyKiB,repeats},
    apfsCalibration:calibration,
    scope:'Cold is the first spawn in a fresh primary with no workspace state. Repeat follows immediately in that primary. Wall time covers only the Git or Graft CLI process. Fixture creation, validation, mutation after spawn, and disk observations are excluded. Signed disk deltas use volume-wide statfs. Optional APFS deltas count mapped regular-file allocation blocks once and exclude metadata, xattrs, and forks. Omit APFS scans for timing-only runs because scans can warm metadata caches.',
    summary,runs,
  };
  mkdirSync(dirname(output),{recursive:true});
  writeFileSync(output,JSON.stringify(report,null,2)+'\n');
  process.stdout.write(`Source benchmark written to ${output}\n`);
} finally { fixture.close(); }
