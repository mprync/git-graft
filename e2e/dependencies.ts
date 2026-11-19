import assert from 'node:assert/strict';
import { createHash, randomBytes } from 'node:crypto';
import { chmodSync, existsSync, linkSync, lstatSync, mkdirSync, readFileSync, readdirSync, readlinkSync, rmSync, statSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join } from 'node:path';
import { gzipSync } from 'node:zlib';
import type { Context, Scenario } from './harness.ts';

type Workspace = ReturnType<Context['spawn']>;
type Result = Record<string, unknown>;

function record(value: unknown): Result {
  assert(value !== null && typeof value === 'object' && !Array.isArray(value));
  return Object.fromEntries(Object.entries(value));
}
function required(ctx: Context, exe: string, args = ['--version']): void {
  let program=exe, arguments_=args;
  if (process.platform === 'win32' && exe === 'npm') {
    const pathValue=Object.entries(ctx.env).find(([key]) => key.toUpperCase()==='PATH')?.[1] ?? '';
    const location=pathValue.split(delimiter).find(dir =>
      existsSync(join(dir,'node.exe')) && existsSync(join(dir,'node_modules','npm','bin','npm-cli.js')));
    if (!location) ctx.unavailable('npm requires node.exe and its adjacent npm-cli.js');
    program=join(location,'node.exe'); arguments_=[join(location,'node_modules','npm','bin','npm-cli.js'),...args];
  } else if (process.platform === 'win32' && exe === 'pnpm') {
    program='pnpm.exe';
  }
  const result = ctx.command(program, arguments_);
  if (result.error && 'code' in result.error && result.error.code === 'ENOENT') ctx.unavailable(`${exe} is required`);
  if (exe === 'pnpm' && result.status !== 0 && /corepack/i.test(result.stderr))
    ctx.unavailable('pnpm Corepack shim has no provisioned local release; install pnpm before testing');
  assert.equal(result.status, 0, `${exe}: ${result.error?.message ?? result.stderr}`);
  ctx.measure(`${exe}_version`, result.stdout.trim());
}
function put(path: string, data: string | Buffer): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, data);
}
function commit(ctx: Context, cwd: string): string {
  ctx.git(['add', '-A'], cwd);
  ctx.git(['commit', '-qm', 'dependency fixture'], cwd);
  return ctx.git(['rev-parse', 'HEAD'], cwd);
}
function add(ctx: Context, ws: Workspace, manager: string, packages: string[] = [], offline = false): Result {
  return ctx.cli(['add-dep', workspaceName(ws), '--lease', ws.lease,
    '--manager', manager, ...(offline ? ['--offline'] : []), ...packages]);
}
function failedAdd(ctx: Context, ws: Workspace, manager: string): Result {
  return ctx.fail(['add-dep', workspaceName(ws), '--lease', ws.lease,
    '--manager', manager, '--offline']);
}
function runtime(ctx: Context, ws: Workspace): Result {
  return ctx.cli(['env', workspaceName(ws)]);
}
function execute(ctx: Context, ws: Workspace, argv: string[]): Result {
  return ctx.cli(['exec', workspaceName(ws), '--lease', ws.lease, '--', ...argv]);
}
function workspaceName(ws: Workspace): string {
  assert(typeof ws.result.name === 'string');
  return ws.result.name;
}
function bytes(path: string): number {
  if (!existsSync(path)) return 0;
  return readdirSync(path, { withFileTypes: true }).reduce((total, item) => {
    const entry = join(path, item.name);
    return total + (item.isDirectory() ? bytes(entry) : item.isFile() ? statSync(entry).size : 0);
  }, 0);
}

// Minimal deterministic ustar/gzip archives avoid external tar/npm tooling and
// keep fixture creation identical on Windows, macOS and Linux.
function archive(files: Record<string, string | Buffer>): Buffer {
  const blocks: Buffer[] = [];
  for (const [name, contents] of Object.entries(files)) {
    const data = Buffer.from(contents);
    const header = Buffer.alloc(512);
    const writeOctal = (offset: number, size: number, n: number) =>
      header.write(n.toString(8).padStart(size - 1, '0') + '\0', offset, size, 'ascii');
    header.write(name, 0, 100, 'utf8');
    writeOctal(100, 8, 0o644); writeOctal(108, 8, 0); writeOctal(116, 8, 0);
    writeOctal(124, 12, data.length); writeOctal(136, 12, 0);
    header.fill(32, 148, 156); header[156] = 48;
    header.write('ustar\0', 257, 6, 'ascii'); header.write('00', 263, 2, 'ascii');
    writeOctal(148, 8, header.reduce((sum, value) => sum + value, 0));
    blocks.push(header, data, Buffer.alloc((512 - data.length % 512) % 512));
  }
  return gzipSync(Buffer.concat([...blocks, Buffer.alloc(1024)]));
}
export function npmArchive(root: string, payloadBytes=0): { path: string; integrity: string; payloadDigest: string | null } {
  const payload = payloadBytes ? randomBytes(payloadBytes) : null;
  const files: Record<string, string | Buffer> = {'package/package.json': JSON.stringify({name:'graft-fixture',version:'1.0.0',main:'index.js'}),
    'package/index.js':'module.exports = 42;\n'};
  if (payload) files['package/payload.bin'] = payload;
  const data = archive(files);
  const path = join(root, 'graft-fixture-1.0.0.tgz');
  writeFileSync(path, data);
  return {path, integrity:`sha512-${createHash('sha512').update(data).digest('base64')}`,
    payloadDigest: payload ? createHash('sha256').update(payload).digest('hex') : null};
}

function npmProject(ctx: Context, archivePath: string): void {
  put(join(ctx.repo,'package.json'), JSON.stringify({name:'graft-consumer',version:'1.0.0',private:true}));
  // Disable npm's audit/funding network requests; fixtures use only a local tarball.
  put(join(ctx.repo,'.npmrc'), 'audit=false\nfund=false\nupdate-notifier=false\n');
  assert(existsSync(archivePath));
  ctx.git(['add','package.json','.npmrc']);
  ctx.git(['commit','-qm','package fixture']);
}
function independentFiles(a: string, b: string): void {
  assert.equal(readFileSync(a,'utf8'), 'module.exports = 42;\n');
  assert.equal(readFileSync(b,'utf8'), 'module.exports = 42;\n');
  writeFileSync(a, 'changed\n');
  assert.equal(readFileSync(b,'utf8'), 'module.exports = 42;\n');
}

const jsManagers: Scenario[] = ['npm','pnpm','bun'].map(manager => ({
  name: `${manager} add and offline locked installations are independent`,
  run(ctx) {
    required(ctx, manager);
    const fixture = npmArchive(ctx.root);
    npmProject(ctx, fixture.path);
    const first = ctx.spawn('first');
    assert(lstatSync(join(first.path, '.git')).isDirectory());
    add(ctx, first, manager, [`file:${fixture.path}`]);
    const revision = commit(ctx, first.path);
    assert.notEqual(ctx.command('git', ['cat-file', '-e', revision]).status, 0);
    const second = ctx.spawn('second', ['--base', revision], first.path);
    const third = ctx.spawn('third', ['--base', revision], first.path);
    add(ctx, second, manager, [], true);
    add(ctx, third, manager, [], true);
    const a = join(second.path,'node_modules/graft-fixture/index.js');
    const b = join(third.path,'node_modules/graft-fixture/index.js');
    independentFiles(a,b);
    const firstEnv = runtime(ctx,second);
    const secondEnv = runtime(ctx,third);
    assert.equal(firstEnv.cache_dir,secondEnv.cache_dir);
    assert.notEqual(firstEnv.runtime_dir,secondEnv.runtime_dir);
    assert(typeof firstEnv.cache_dir === 'string');
    ctx.measure('shared_cache_bytes', bytes(firstEnv.cache_dir));
    ctx.measure('per_workspace_node_modules_bytes', [bytes(join(second.path,'node_modules')),bytes(join(third.path,'node_modules'))]);
  },
}));

const cargoCase: Scenario = {
  name: 'cargo local registry add and locked offline fetch isolate build outputs',
  run(ctx) {
    required(ctx,'cargo');
    const registry = join(ctx.root,'cargo registry');
    const crate = archive({'graft-fixture-1.0.0/Cargo.toml':'[package]\nname="graft-fixture"\nversion="1.0.0"\nedition="2021"\n',
      'graft-fixture-1.0.0/src/lib.rs':'pub fn value() -> u32 { 42 }\n'});
    put(join(registry,'graft-fixture-1.0.0.crate'),crate);
    put(join(registry,'index','gr','af','graft-fixture'),JSON.stringify({name:'graft-fixture',vers:'1.0.0',deps:[],
      cksum:createHash('sha256').update(crate).digest('hex'),features:{},yanked:false})+'\n');
    put(join(ctx.repo,'.cargo','config.toml'), `[source.crates-io]\nreplace-with = "fixture"\n[source.fixture]\nlocal-registry = ${JSON.stringify(registry)}\n`);
    put(join(ctx.repo,'Cargo.toml'),'[package]\nname="consumer"\nversion="0.1.0"\nedition="2021"\n');
    put(join(ctx.repo,'src','lib.rs'),'pub fn answer() -> u32 { graft_fixture::value() }\n');
    ctx.git(['add','.']); ctx.git(['commit','-qm','cargo fixture']);
    const first = ctx.spawn('first');
    const inherited = runtime(ctx,first);
    const originalWrapper=ctx.env.RUSTC_WRAPPER;
    delete ctx.env.RUSTC_WRAPPER;
    const probe = JSON.parse(String(execute(ctx,first,[process.execPath,'-e',
      'process.stdout.write(JSON.stringify({wrapper:process.env.RUSTC_WRAPPER,incremental:process.env.CARGO_INCREMENTAL}))']).stdout));
    const sccache = ctx.command('sccache',['--version']);
    if (sccache.status === 0) { assert.equal(probe.wrapper,'sccache'); assert.equal(probe.incremental,'0'); }
    if (originalWrapper === undefined) delete ctx.env.RUSTC_WRAPPER; else ctx.env.RUSTC_WRAPPER=originalWrapper;
    // Some sandboxes cannot start sccache's daemon; validate Cargo separately.
    ctx.env.RUSTC_WRAPPER = '';
    add(ctx,first,'cargo',['graft-fixture@1.0.0']);
    const revision = commit(ctx,first.path);
    const second = ctx.spawn('second',['--base',revision],first.path);
    const third = ctx.spawn('third',['--base',revision],first.path);
    add(ctx,second,'cargo',[],true); add(ctx,third,'cargo',[],true);
    const a = runtime(ctx,second); const b = runtime(ctx,third);
    assert.equal(a.cache_dir,b.cache_dir); assert.notEqual(a.cargo_target_dir,b.cargo_target_dir);
    const code = 'process.stdout.write(JSON.stringify({target:process.env.CARGO_TARGET_DIR,build:process.env.CARGO_BUILD_BUILD_DIR,wrapper:process.env.RUSTC_WRAPPER,incremental:process.env.CARGO_INCREMENTAL,port:process.env.PORT}))';
    const envA = JSON.parse(String(execute(ctx,second,[process.execPath,'-e',code]).stdout));
    const envB = JSON.parse(String(execute(ctx,third,[process.execPath,'-e',code]).stdout));
    assert.equal(envA.wrapper,'');
    assert.equal(envA.target,a.cargo_target_dir); assert.notEqual(envA.build,envB.build); assert.notEqual(envA.port,envB.port);
    execute(ctx,second,['cargo','check','--offline']);
    execute(ctx,third,['cargo','check','--offline']);
    assert(existsSync(String(a.cargo_target_dir))); assert(existsSync(envA.build));
    assert(existsSync(String(b.cargo_target_dir))); assert(existsSync(envB.build));
    ctx.measure('sccache_available',sccache.status === 0);
    assert.equal(inherited.cache_dir,a.cache_dir);
    ctx.measure('cargo_shared_cache_bytes',bytes(String(a.cache_dir)));
    if (originalWrapper === undefined) delete ctx.env.RUSTC_WRAPPER; else ctx.env.RUSTC_WRAPPER=originalWrapper;
  },
};

// A stored ZIP wheel keeps the fixture fully offline and needs no Python build backend.
function wheel(path: string): void {
  const crc32 = (data: Buffer) => {
    let crc = 0xffffffff;
    for (const byte of data) { crc ^= byte; for (let i=0;i<8;i++) crc = (crc >>> 1) ^ (0xedb88320 & -(crc & 1)); }
    return (crc ^ 0xffffffff) >>> 0;
  };
  const files: Record<string,string> = {
    'graft_fixture/__init__.py':'VALUE = 42\n',
    'graft_fixture-1.0.0.dist-info/METADATA':'Metadata-Version: 2.1\nName: graft-fixture\nVersion: 1.0.0\n',
    'graft_fixture-1.0.0.dist-info/WHEEL':'Wheel-Version: 1.0\nGenerator: graft-fixture\nRoot-Is-Purelib: true\nTag: py3-none-any\n',
    'graft_fixture-1.0.0.dist-info/RECORD':'',
  };
  const local: Buffer[] = []; const central: Buffer[] = []; let offset = 0;
  for (const [name, contents] of Object.entries(files)) {
    const filename=Buffer.from(name), data=Buffer.from(contents), checksum=crc32(data);
    const header=Buffer.alloc(30); header.writeUInt32LE(0x04034b50); header.writeUInt16LE(20,4);
    header.writeUInt32LE(checksum,14); header.writeUInt32LE(data.length,18); header.writeUInt32LE(data.length,22); header.writeUInt16LE(filename.length,26);
    const directory=Buffer.alloc(46); directory.writeUInt32LE(0x02014b50); directory.writeUInt16LE(20,4); directory.writeUInt16LE(20,6);
    directory.writeUInt32LE(checksum,16); directory.writeUInt32LE(data.length,20); directory.writeUInt32LE(data.length,24);
    directory.writeUInt16LE(filename.length,28); directory.writeUInt32LE(offset,42);
    local.push(header,filename,data); central.push(directory,filename); offset += header.length+filename.length+data.length;
  }
  const directory=Buffer.concat(central), end=Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50); end.writeUInt16LE(Object.keys(files).length,8); end.writeUInt16LE(Object.keys(files).length,10);
  end.writeUInt32LE(directory.length,12); end.writeUInt32LE(offset,16);
  writeFileSync(path,Buffer.concat([...local,directory,end]));
}

const uvCase: Scenario = {
  name: 'uv local wheel add and locked offline sync isolate environments',
  run(ctx) {
    required(ctx,'uv');
    const python=ctx.command('uv',['python','find','--no-python-downloads']);
    if (python.status !== 0 && /no (?:compatible )?python (?:installation|interpreter|version)/i.test(python.stderr))
      ctx.unavailable(`uv requires an installed Python: ${python.stderr}`);
    assert.equal(python.status,0,`uv could not inspect Python: ${python.stderr}`);
    const wheelPath=join(ctx.root,'graft_fixture-1.0.0-py3-none-any.whl'); wheel(wheelPath);
    put(join(ctx.repo,'pyproject.toml'),'[project]\nname="consumer"\nversion="1.0.0"\nrequires-python=">=3.9"\ndependencies=[]\n');
    ctx.git(['add','pyproject.toml']); ctx.git(['commit','-qm','uv fixture']);
    const first=ctx.spawn('first'); add(ctx,first,'uv',[wheelPath]);
    const revision=commit(ctx,first.path);
    const second=ctx.spawn('second',['--base',revision],first.path);
    const third=ctx.spawn('third',['--base',revision],first.path);
    add(ctx,second,'uv',[],true); add(ctx,third,'uv',[],true);
    const a=runtime(ctx,second), b=runtime(ctx,third);
    assert.equal(a.cache_dir,b.cache_dir); assert.notEqual(a.python_environment,b.python_environment);
    assert(existsSync(String(a.python_environment))); assert(existsSync(String(b.python_environment)));
    const py=process.platform === 'win32' ? 'Scripts/python.exe' : 'bin/python';
    const result=execute(ctx,second,[join(String(a.python_environment),py),'-c','import graft_fixture; print(graft_fixture.VALUE)']);
    assert.equal(String(result.stdout).trim(),'42');
    const moduleCode='import graft_fixture; print(graft_fixture.__file__)';
    const fileA=String(execute(ctx,second,[join(String(a.python_environment),py),'-c',moduleCode]).stdout).trim();
    const fileB=String(execute(ctx,third,[join(String(b.python_environment),py),'-c',moduleCode]).stdout).trim();
    assert.notEqual(fileA,fileB);
    writeFileSync(fileA,'VALUE = 99\n');
    assert.equal(readFileSync(fileB,'utf8'),'VALUE = 42\n');
    ctx.measure('uv_shared_cache_bytes',bytes(String(a.cache_dir)));
  },
};

function lockedNpmFixture(ctx: Context): {first: Workspace; second: Workspace; third: Workspace; fixture: {path:string;integrity:string}} {
  required(ctx,'npm');
  const fixture=npmArchive(ctx.root);
  const manifest={name:'consumer',version:'1.0.0',dependencies:{'graft-fixture':'1.0.0'}};
  const lock={name:'consumer',version:'1.0.0',lockfileVersion:3,packages:{
    '':manifest, 'node_modules/graft-fixture':{version:'1.0.0',resolved:'https://127.0.0.1:1/graft-fixture.tgz',integrity:fixture.integrity},
  }};
  put(join(ctx.repo,'package.json'),JSON.stringify(manifest));
  put(join(ctx.repo,'package-lock.json'),JSON.stringify(lock));
  put(join(ctx.repo,'.npmrc'),'audit=false\nfund=false\nupdate-notifier=false\n');
  ctx.git(['add','.']); ctx.git(['commit','-qm','locked registry fixture']);
  const first=ctx.spawn('first'), second=ctx.spawn('second'), third=ctx.spawn('third');
  return {first,second,third,fixture};
}

const npmIntegrity: Scenario = {
  name: 'npm cold offline miss then shared integrity cache hit',
  run(ctx) {
    const {first,second,third,fixture}=lockedNpmFixture(ctx);
    const miss=failedAdd(ctx,first,'npm');
    assert.equal(miss.success,false); assert.notEqual(miss.exit_code,0);
    add(ctx,first,'npm',[`file:${fixture.path}`]);
    // Remove the source tarball: only the integrity-addressed cache can supply it.
    rmSync(fixture.path);
    add(ctx,second,'npm',[],true); add(ctx,third,'npm',[],true);
    independentFiles(join(second.path,'node_modules/graft-fixture/index.js'),join(third.path,'node_modules/graft-fixture/index.js'));
    ctx.measure('npm_warmed_cache_bytes',bytes(String(runtime(ctx,second).cache_dir)));
  },
};

const execCase: Scenario = {
  name:'scoped exec preserves literal argv and bounds child output',
  run(ctx) {
    const ws=ctx.spawn('first');
    const literal='literal ; $(not-a-command) & spaces "quotes"';
    const code='process.stdout.write(JSON.stringify({arg:process.argv[1],cwd:process.cwd(),port:process.env.PORT,lease:process.env.GIT_GRAFT_LEASE}))';
    const output=record(JSON.parse(String(execute(ctx,ws,[process.execPath,'-e',code,literal]).stdout)));
    assert.equal(output.arg,literal); assert.equal(output.cwd,ws.path); assert.equal(output.lease,undefined);
    assert.equal(String(output.port),String(runtime(ctx,ws).port));
    const result=ctx.fail(['exec','first','--lease',ws.lease,'--',process.execPath,'-e',
      'process.stdout.write("a".repeat(300000));process.stderr.write("b".repeat(300000));process.exitCode=17']);
    assert.equal(result.success,false); assert.equal(result.exit_code,17);
    assert.equal(result.stdout_truncated,true); assert.equal(result.stderr_truncated,true);
    assert.equal(String(result.stdout).length,256*1024); assert.equal(String(result.stderr).length,256*1024);
  },
};

const layerCase: Scenario = {
  name:'installed layer verifies safety compatibility integrity and filesystem support',
  run(ctx) {
    const {first,second,third,fixture}=lockedNpmFixture(ctx);
    add(ctx,first,'npm',[`file:${fixture.path}`]);
    add(ctx,second,'npm',[],true);
    const layer=(action: string, ws: Workspace, key='node-1', copy=false) =>
      ['layer',workspaceName(ws),'--lease',ws.lease,action,'--key',key,...(copy ? ['--copy'] : [])];
    const badLink=join(second.path,'node_modules','external');
    try { symlinkSync(ctx.root,badLink,process.platform === 'win32' ? 'junction' : 'dir'); }
    catch (error) {
      if (process.platform === 'win32' && error instanceof Error && 'code' in error && ['EPERM','EACCES'].includes(String(error.code)))
        ctx.unavailable('Windows symlink creation requires Developer Mode or suitable privileges');
      throw error;
    }
    ctx.fail(layer('save',second)); unlinkSync(badLink);
    const sourcePackage=join(second.path,'node_modules','graft-fixture','index.js');
    if (process.platform !== 'win32') chmodSync(sourcePackage,0o751);
    const relativeLink=join(second.path,'node_modules','safe-link');
    try { symlinkSync('graft-fixture/index.js',relativeLink,'file'); }
    catch (error) {
      if (process.platform === 'win32' && error instanceof Error && 'code' in error && ['EPERM','EACCES'].includes(String(error.code)))
        ctx.unavailable('Relative file symlink creation requires Windows Developer Mode or suitable privileges');
      throw error;
    }
    const attempt=ctx.command(ctx.bin,['--repo',ctx.repo,'--json',...layer('save',second)]);
    assert.ifError(attempt.error);
    if (attempt.status === 0) {
      const normal=record(JSON.parse(attempt.stdout));
      assert.equal(normal.backend,'copy-on-write');
      assert(Number(normal.logical_bytes)>0);
      assert.equal(normal.copied_bytes,0);
      assert.equal(normal.logical_bytes,normal.cloned_bytes);
      ctx.measure('layer_default_backend',normal.backend);
      ctx.cli(layer('forget',second));
    } else {
      const refusal=record(JSON.parse(attempt.stderr || attempt.stdout));
      assert.equal(refusal.code,'cow_unavailable');
      ctx.measure('layer_default_backend','refused: cow_unavailable');
    }
    const saved=ctx.cli(layer('save',second,'node-1',true));
    assert.equal(saved.backend,'copy'); assert(Number(saved.copied_bytes)>0); assert.equal(saved.cloned_bytes,0);
    assert.equal(typeof saved.digest,'string');
    ctx.fail(layer('restore',third,'wrong-key'));
    const state=dirname(String(runtime(ctx,third).cache_dir));
    const cached=join(state,'layers',String(saved.digest),'node_modules/graft-fixture/index.js');
    const original=readFileSync(cached); writeFileSync(cached,'tampered');
    ctx.fail(layer('restore',third,'node-1',true));
    assert(!existsSync(join(third.path,'node_modules')));
    ctx.fail(layer('forget',second));
    assert.equal(readFileSync(cached,'utf8'),'tampered');
    writeFileSync(cached,original);
    if (process.platform !== 'win32') {
      const outside=join(ctx.root,'shared-package-file');
      linkSync(cached,outside);
      ctx.fail(layer('restore',third,'node-1',true));
      ctx.fail(layer('forget',second));
      assert(!existsSync(join(third.path,'node_modules')));
      assert.deepEqual(readFileSync(cached),original);
      assert.deepEqual(readFileSync(outside),original);
      rmSync(outside);
      const mode=statSync(cached).mode & 0o777;
      chmodSync(cached,mode ^ 0o100);
      ctx.fail(layer('restore',third,'node-1',true));
      ctx.fail(layer('forget',second));
      assert.equal(statSync(cached).mode & 0o777,mode ^ 0o100);
      chmodSync(cached,mode);
    }
    const restored=ctx.cli(layer('restore',third,'node-1',true));
    assert.equal(restored.backend,'copy'); assert.equal(restored.copied_bytes,saved.copied_bytes);
    assert.equal(readlinkSync(join(third.path,'node_modules','safe-link')).replaceAll('\\','/'),'graft-fixture/index.js');
    ctx.measure('relative_symlink_supported',true);
    const restoredPackage=join(third.path,'node_modules','graft-fixture','index.js');
    if (process.platform !== 'win32') {
      assert.equal(statSync(restoredPackage).mode & 0o777,0o751);
      chmodSync(restoredPackage,0o644);
      assert.equal(statSync(sourcePackage).mode & 0o777,0o751);
      assert.equal(statSync(cached).mode & 0o777,0o751);
    }
    independentFiles(sourcePackage,restoredPackage);
    ctx.fail(layer('restore',third));
    const forgotten=ctx.cli(layer('forget',second));
    assert(!existsSync(join(state,'layers',String(saved.digest))));
    if (process.platform === 'win32') {
      assert.equal(forgotten.retained,true);
      const retained=join(state,'retained-layers');
      const entry=readdirSync(retained).find(name => name.startsWith(String(saved.digest) + '-'));
      assert(entry);
      assert.deepEqual(readFileSync(join(retained,entry,'node_modules/graft-fixture/index.js')),original);
    }
    ctx.measure('layer_forced_copy_bytes',saved.copied_bytes);
  },
};

const npmRoot: Scenario = {
  name: 'npm installs in the moved checkout with an empty root and an enclosing package',
  run(ctx) {
    required(ctx, 'npm');
    const fixture = npmArchive(ctx.root);
    const parent = join(ctx.root, 'enclosing-package');
    const parentManifest = JSON.stringify({name: 'enclosing', version: '1.0.0', private: true, workspaces: ['*']});
    put(join(parent, 'package.json'), parentManifest);
    put(join(parent, 'node_modules', 'sentinel'), 'retained');
    const names = process.platform === 'win32' ? ['empty', 'declared'] : ['empty', 'declared', 'trailing '];
    const outside = join(ctx.root, 'external-prefix');
    ctx.env.npm_config_global = 'true';
    ctx.env.npm_config_prefix = outside;
    for (const [index, name] of names.entries()) {
      const ws = ctx.spawn(`package-${index}`, ['--no-deps']);
      const path = join(parent, name);
      ctx.cli(['move', workspaceName(ws), path, '--lease', ws.lease]);
      if (name === 'declared') {
        put(join(path, 'package.json'), '{"name":"inside","version":"1.0.0"}');
      }
      if (name.endsWith(' ')) {
        put(join(path.trimEnd(), 'package.json'), '{"name":"sibling","version":"1.0.0"}');
      }
      add(ctx, ws, 'npm', [`file:${fixture.path}`], true);
      assert.equal(readFileSync(join(path, 'node_modules/graft-fixture/index.js'), 'utf8'), 'module.exports = 42;\n');
      assert(existsSync(join(path, 'package-lock.json')));
      assert.equal(readFileSync(join(parent, 'package.json'), 'utf8'), parentManifest);
      assert.deepEqual(readdirSync(join(parent, 'node_modules')), ['sentinel']);
      assert(!existsSync(join(parent, 'package-lock.json')));
      assert(!existsSync(outside));
      if (name.endsWith(' ')) {
        assert.deepEqual(readdirSync(path.trimEnd()), ['package.json']);
        assert.equal(readFileSync(join(path.trimEnd(), 'package.json'), 'utf8'), '{"name":"sibling","version":"1.0.0"}');
      }
    }
  },
};

export const cases: Scenario[] = [...jsManagers,cargoCase,uvCase,npmIntegrity,npmRoot,execCase,layerCase];
