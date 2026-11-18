import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join } from 'node:path';
import { createInterface } from 'node:readline';
import { clearTimeout, setTimeout } from 'node:timers';
import type { Scenario } from './harness.ts';

type Context = Parameters<Scenario['run']>[0];
type JsonObject = Record<string, unknown>;
type Pending = {
  timer: ReturnType<typeof setTimeout>;
  resolve: (response: JsonObject) => void;
  reject: (error: Error) => void;
};

const protocol = '2025-06-18';
const requestTimeoutMs = 30_000;
const executionTools = ['workspace_exec', 'workspace_add_dependency', 'workspace_layer', 'workspace_seed_layer'];
const defaultTools = [
  'workspace_doctor', 'workspace_list', 'workspace_spawn', 'workspace_status',
  'workspace_heartbeat', 'workspace_env', 'workspace_land_and_wipe', 'workspace_discard', 'workspace_gc',
  'workspace_restore', 'workspace_undo_land',
];

function isObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

function object(value: unknown, label: string): JsonObject {
  assert(isObject(value), `${label}: expected a JSON object, got ${JSON.stringify(value)}`);
  return value;
}

function string(value: unknown, label: string): string {
  assert(typeof value === 'string' && value.length > 0, `${label}: expected a nonempty string`);
  return value;
}

class McpClient {
  readonly child: ChildProcessWithoutNullStreams;
  readonly lines: ReturnType<typeof createInterface>;
  readonly exited: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;
  readonly pending = new Map<number, Pending>();
  stderr = '';
  nextId = 0;
  closing = false;
  failure?: Error;

  constructor(ctx: Context, allowExec: boolean) {
    const args = ['--repo', ctx.repo, 'mcp', ...(allowExec ? ['--allow-exec'] : [])];
    this.child = spawn(ctx.bin, args, { cwd: ctx.repo, env: ctx.env, stdio: 'pipe', windowsHide: true });
    this.exited = new Promise((resolve) => {
      this.child.once('close', (code, signal) => {
        resolve({ code, signal });
        if (!this.closing || this.pending.size > 0) {
          this.fail(new Error(`MCP server closed (exit ${code}, signal ${signal})`));
        }
      });
    });
    this.child.once('error', (error) => this.fail(error));
    this.child.stdin.on('error', (error) => {
      if (!this.closing) this.fail(error);
    });
    this.child.stderr.setEncoding('utf8').on('data', (chunk: string) => {
      this.stderr = (this.stderr + chunk).slice(-8_192);
    });
    this.lines = createInterface({ input: this.child.stdout, crlfDelay: Infinity });
    this.lines.on('error', (error) => this.fail(error));
    this.lines.on('line', (line) => {
      try {
        const response = object(JSON.parse(line), 'MCP stdout line');
        const id = response.id;
        assert(typeof id === 'number', `Unexpected MCP response: ${line}`);
        const pending = this.pending.get(id);
        assert(pending, `Unexpected MCP response ID: ${line}`);
        assert.equal(response.jsonrpc, '2.0');
        assert.notEqual('result' in response, 'error' in response, `Expected result or error: ${line}`);
        this.pending.delete(id);
        clearTimeout(pending.timer);
        pending.resolve(response);
      } catch (error) {
        this.fail(new Error(`Invalid MCP response: ${line}`, { cause: error }));
      }
    });
  }

  fail(cause: Error): void {
    this.failure ??= new Error(`${cause.message}${this.stderr ? `\n${this.stderr}` : ''}`, { cause });
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(this.failure);
    }
    this.pending.clear();
  }

  send(line: string): void {
    if (this.failure) throw this.failure;
    this.child.stdin.write(`${line}\n`);
  }

  request(method: string, params?: JsonObject): Promise<JsonObject> {
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => this.fail(new Error(`MCP ${method} (ID ${id}) timed out`)), requestTimeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      try {
        this.send(JSON.stringify({ jsonrpc: '2.0', id, method, ...(params === undefined ? {} : { params }) }));
      } catch (error) {
        this.fail(error instanceof Error ? error : new Error(String(error)));
      }
    });
  }

  async close(): Promise<void> {
    this.closing = true;
    this.child.stdin.end();
    try {
      let exit = await this.waitForExit();
      if (!exit) {
        this.child.kill('SIGTERM');
        exit = await this.waitForExit();
      }
      if (!exit) {
        this.child.kill('SIGKILL');
        exit = await this.waitForExit();
      }
      assert(exit, 'MCP process did not terminate after closing stdin and signalling it');
    } finally {
      this.lines.close();
      this.child.stdin.destroy();
      this.child.stdout.destroy();
      this.child.stderr.destroy();
      this.child.unref();
    }
  }

  async waitForExit(): Promise<Awaited<McpClient['exited']> | undefined> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([
        this.exited,
        new Promise<undefined>((resolve) => { timer = setTimeout(() => resolve(undefined), 2_000); }),
      ]);
    } finally {
      clearTimeout(timer);
    }
  }
}

async function withClient(
  ctx: Context,
  allowExec: boolean,
  run: (client: McpClient) => Promise<void>,
): Promise<void> {
  const client = new McpClient(ctx, allowExec);
  try {
    await run(client);
    if (client.failure) throw client.failure;
  } finally {
    await client.close();
  }
}

function result(reply: JsonObject): JsonObject {
  assert(!('error' in reply), `Unexpected JSON-RPC error: ${JSON.stringify(reply)}`);
  return object(reply.result, 'JSON-RPC result');
}

async function initialize(client: McpClient, version = protocol): Promise<JsonObject> {
  const info = result(await client.request('initialize', {
    protocolVersion: version,
    capabilities: {},
    clientInfo: { name: 'git-graft-typescript-e2e', version: '1.0.0' },
  }));
  assert.equal(info.protocolVersion, version);
  object(object(info.capabilities, 'capabilities').tools, 'tools capability');
  client.send(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }));
  return info;
}

async function discover(client: McpClient, allowExec: boolean): Promise<Map<string, JsonObject>> {
  const { tools } = result(await client.request('tools/list', {}));
  assert(Array.isArray(tools));
  const expected = allowExec ? [...defaultTools, ...executionTools] : [...defaultTools];
  const catalog = new Map<string, JsonObject>();
  for (const entry of tools) {
    const tool = object(entry, 'tool descriptor');
    const name = string(tool.name, 'tool name');
    assert(!catalog.has(name), `Duplicate MCP tool ${name}`);
    catalog.set(name, tool);
    const schema = object(tool.inputSchema, `${name} input schema`);
    assert.equal(schema.type, 'object');
    assert.equal(schema.additionalProperties, false);
    const description = string(tool.description, `${name} description`);
    if (executionTools.includes(name)) assert.match(description, /--allow-exec/);
  }
  assert.deepEqual([...catalog.keys()].sort(), expected.sort());
  return catalog;
}

async function call(client: McpClient, name: string, args: JsonObject = {}, isError = false): Promise<JsonObject> {
  const tool = result(await client.request('tools/call', { name, arguments: args }));
  assert.equal(tool.isError, isError, `${name}: ${JSON.stringify(tool)}`);
  const structured = object(tool.structuredContent, `${name} structured content`);
  assert(Array.isArray(tool.content) && tool.content.length > 0, `${name} content blocks`);
  const text = object(tool.content[0], `${name} text content`);
  assert.equal(text.type, 'text');
  assert.deepEqual(JSON.parse(string(text.text, `${name} text payload`)), structured);
  return structured;
}

async function invalidCall(client: McpClient, name: string, args: JsonObject): Promise<void> {
  const reply = await client.request('tools/call', { name, arguments: args });
  const error = object(reply.error, `${name} JSON-RPC error`);
  assert.equal(error.code, -32602);
  string(error.message, `${name} invalid-argument message`);
}

async function spawnWorkspace(client: McpClient, name: string, requestId = randomUUID()) {
  const result = await call(client, 'workspace_spawn', { name, request_id: requestId });
  const path = string(result.worktree_path, 'worktree path');
  const lease = string(result.lease, 'workspace lease');
  assert.equal(lease, requestId);
  assert.equal(result.name, name);
  assert.equal(result.branch, `graft/${name}`);
  assert.equal(result.phase, 'ready');
  assert.equal(readFileSync(join(path, 'tracked.txt'), 'utf8'), 'initial\n');
  return { path, lease, result };
}

function assertRefMissing(ctx: Context, name: string): void {
  assert.equal(ctx.git(['for-each-ref', '--format=%(refname)', `refs/heads/graft/${name}`]).trim(), '');
}

function writeJson(path: string, data: JsonObject): void {
  writeFileSync(path, `${JSON.stringify(data, null, 2)}\n`);
}

function npm(ctx: Context, args: string[], cwd: string): string {
  let executable = 'npm';
  let prefix: string[] = [];
  if (process.platform === 'win32') {
    const path = Object.entries(ctx.env).find(([key]) => key.toLowerCase() === 'path')?.[1] ?? '';
    const dir = path.split(delimiter).find((part) =>
      part && existsSync(join(part, 'node.exe')) && existsSync(join(part, 'node_modules', 'npm', 'bin', 'npm-cli.js')));
    if (!dir) ctx.unavailable('Windows requires node.exe and its npm CLI in the same PATH directory');
    executable = join(dir, 'node.exe');
    prefix = [join(dir, 'node_modules', 'npm', 'bin', 'npm-cli.js')];
  }
  const command = ctx.command(executable, [...prefix, ...args], cwd);
  if ((command.error as NodeJS.ErrnoException | undefined)?.code === 'ENOENT') {
    ctx.unavailable(`npm is required for the offline MCP fixture: ${command.error?.message}`);
  }
  assert.equal(command.status, 0, `npm ${args.join(' ')}: ${command.error ?? ''}\n${command.stderr}\n${command.stdout}`);
  return command.stdout;
}

const fixturePackage = 'graft-mcp-fixture';
const fixtureContents = 'module.exports = 42;\n';

function prepareNpmFixture(ctx: Context): string {
  const pkg = join(ctx.root, 'npm-package');
  mkdirSync(pkg);
  writeJson(join(pkg, 'package.json'), { name: fixturePackage, version: '1.0.0' });
  writeFileSync(join(pkg, 'index.js'), fixtureContents);
  const packed: unknown = JSON.parse(npm(ctx, [
    'pack', '--json', '--offline', '--ignore-scripts', '--cache', join(ctx.root, 'pack-cache'),
  ], pkg));
  assert(Array.isArray(packed) && packed.length === 1);
  const metadata = object(packed[0], 'npm pack metadata');
  const archive = join(pkg, string(metadata.filename, 'npm tarball'));
  const dependencies = { [fixturePackage]: '1.0.0' };
  const manifest = { name: 'mcp-consumer', version: '1.0.0', dependencies };
  writeJson(join(ctx.repo, 'package.json'), manifest);
  writeJson(join(ctx.repo, 'package-lock.json'), {
    name: manifest.name,
    version: manifest.version,
    lockfileVersion: 3,
    requires: true,
    packages: {
      '': manifest,
      [`node_modules/${fixturePackage}`]: {
        version: '1.0.0',
        resolved: 'http://127.0.0.1:1/graft-mcp-fixture.tgz',
        integrity: string(metadata.integrity, 'npm tarball integrity'),
      },
    },
  });
  ctx.git(['add', 'package.json', 'package-lock.json']);
  ctx.git(['commit', '-m', 'Commit offline npm fixture']);
  return archive;
}

export const cases: Scenario[] = [
  {
    name: 'MCP negotiates stdio, validates requests and gates execution',
    run: async (ctx) => withClient(ctx, false, async (client) => {
      const initialized = await initialize(client);
      const metadata = object(initialized.serverInfo, 'server metadata');
      assert.equal(metadata.name, 'git-graft');
      const version = ctx.command(ctx.bin, ['--version']);
      assert.equal(version.status, 0, version.stderr);
      assert.equal(version.stdout.trim(), `git-graft ${string(metadata.version, 'server version')}`);
      assert.match(string(initialized.instructions, 'instructions'), /disabled.*--allow-exec/);

      // An unparseable line is ignored by this stdio transport. The following
      // concurrent pings prove framing recovery and response ID correlation.
      client.send('{"jsonrpc":"2.0","id":9000,');
      const pings = await Promise.all([client.request('ping'), client.request('ping', {})]);
      for (const ping of pings) assert.deepEqual(result(ping), {});
      ctx.coverCli('mcp');
      const catalog = await discover(client, false);
      assert.equal(object(catalog.get('workspace_doctor')?.annotations, 'doctor annotations').readOnlyHint, true);
      assert.equal(object(catalog.get('workspace_land_and_wipe')?.annotations, 'land annotations').destructiveHint, true);
      const spawnSchema = object(catalog.get('workspace_spawn')?.inputSchema, 'spawn schema');
      assert.deepEqual(spawnSchema.required, ['name']);
      const properties = object(spawnSchema.properties, 'spawn schema properties');
      assert.equal(object(properties.name, 'spawn name schema').type, 'string');
      assert(properties.request_id);

      await invalidCall(client, 'workspace_status', { name: 7 });
      await invalidCall(client, 'workspace_status', {});
      await invalidCall(client, 'workspace_list', { unexpected: true });
      await invalidCall(client, 'workspace_unknown', {});
      assert.equal((await call(client, 'workspace_status', { name: 'missing' }, true)).code, 'not_found');

      const doctor = await call(client, 'workspace_doctor');
      const cliDoctor = ctx.cli(['doctor']);
      assert.equal(doctor.version, metadata.version);
      assert.equal(doctor.common_dir, cliDoctor.common_dir);
      assert.equal(realpathSync(string(doctor.common_dir, 'common Git directory')), realpathSync(join(ctx.repo, '.git')));
      assert.equal(doctor.git, cliDoctor.git);
      assert.deepEqual(doctor.root_lockfiles, []);
      ctx.coverTool('workspace_doctor');

      assert.deepEqual(await call(client, 'workspace_list'), { workspaces: [], recoverable: [] });
      const workspace = await spawnWorkspace(client, 'wire');
      const owned = { name: 'wire', lease: workspace.lease };
      const denied: [string, JsonObject][] = [
        ['workspace_exec', { ...owned, argv: [process.execPath, '-e', 'process.exit(7)'] }],
        ['workspace_add_dependency', { ...owned, manager: 'npm', offline: true, packages: [] }],
        ['workspace_layer', { ...owned, action: 'save', key: 'test-toolchain' }],
        ['workspace_seed_layer', { key: 'test-toolchain' }],
        ['workspace_land_and_wipe', { ...owned, into: 'main', reconcile: true }],
      ];
      for (const [name, args] of denied) {
        const refusal = await call(client, name, args, true);
        assert.match(string(refusal.message, 'execution permission error'), /--allow-exec/);
      }
      assert(existsSync(workspace.path));
      assert.equal(ctx.git(['symbolic-ref', 'HEAD'], workspace.path), 'refs/heads/graft/wire');
      assert.equal(ctx.git(['rev-parse', 'refs/heads/main']), workspace.result.base);
    }),
  },
  {
    name: 'MCP negotiates the 2024-11-05 protocol',
    run: async (ctx) => withClient(ctx, false, async (client) => {
      await initialize(client, '2024-11-05');
      assert.deepEqual(result(await client.request('ping')), {});
      await discover(client, false);
      assert.deepEqual(await call(client, 'workspace_list'), { workspaces: [], recoverable: [] });
    }),
  },
  {
    name: 'MCP ownership, spawn and discard receipts, and stale GC match CLI',
    run: async (ctx) => withClient(ctx, false, async (client) => {
      await initialize(client);
      const name = 'lifecycle';
      const first = await spawnWorkspace(client, name);
      const retry = await spawnWorkspace(client, name, first.lease);
      assert.deepEqual(retry, first);
      assert.equal((await call(client, 'workspace_spawn', { name, request_id: randomUUID() }, true)).code, 'already_exists');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], first.path), first.result.base);
      assert.equal(ctx.git(['symbolic-ref', 'HEAD'], first.path), `refs/heads/graft/${name}`);
      ctx.coverTool('workspace_spawn');

      const listed = await call(client, 'workspace_list');
      assert.deepEqual(listed, ctx.cli(['list']));
      assert(Array.isArray(listed.workspaces) && listed.workspaces.length === 1);
      const record = object(listed.workspaces[0], 'listed workspace');
      assert.equal(record.name, name);
      assert(!('lease' in record));
      ctx.coverTool('workspace_list');

      const status = await call(client, 'workspace_status', { name });
      assert.deepEqual(status, ctx.cli(['status', name]));
      assert.equal(status.registered, true);
      assert.equal(status.head, first.result.base);
      assert.deepEqual(status.changes, []);
      ctx.coverTool('workspace_status');

      const owned = { name, lease: first.lease };
      const wrongLease = { name, lease: randomUUID() };
      assert.equal((await call(client, 'workspace_heartbeat', wrongLease, true)).code, 'lease_mismatch');
      const renewal = await call(client, 'workspace_heartbeat', owned);
      const previousHeartbeat = first.result.last_heartbeat;
      assert(typeof renewal.last_heartbeat === 'number' && typeof previousHeartbeat === 'number');
      assert(renewal.last_heartbeat >= previousHeartbeat);
      const renewedStatus = await call(client, 'workspace_status', { name });
      assert.equal(renewedStatus.last_heartbeat, renewal.last_heartbeat);
      assert.equal(renewedStatus.expires_at, renewal.expires_at);
      ctx.coverTool('workspace_heartbeat');

      const discarded = await call(client, 'workspace_discard', owned);
      assert.deepEqual(discarded, { action: 'drop', name, removed: true, recovery_lease: first.lease });
      assert(!existsSync(first.path));
      assertRefMissing(ctx, name);
      assert.deepEqual(await call(client, 'workspace_discard', owned), discarded);
      assert.deepEqual(ctx.cli(['drop', name, '--lease', first.lease]), discarded);
      ctx.coverTool('workspace_discard');

      const reopened = await call(client, 'workspace_restore', owned);
      const second = {path: string(reopened.worktree_path, 'restored path'), lease: string(reopened.lease, 'new lease')};
      assert.deepEqual(await call(client, 'workspace_restore', owned), reopened);
      ctx.coverTool('workspace_restore');
      assert.notEqual(second.lease, first.lease);
      assert.equal((await call(client, 'workspace_discard', owned, true)).code, 'lease_mismatch');
      assert.equal((await call(client, 'workspace_land_and_wipe', owned, true)).code, 'lease_mismatch');
      assert.equal((await call(client, 'workspace_heartbeat', owned, true)).code, 'lease_mismatch');
      assert.equal(readFileSync(join(second.path, 'tracked.txt'), 'utf8'), 'initial\n');

      const dirty = await spawnWorkspace(client, 'dirty');
      writeFileSync(join(dirty.path, 'local.txt'), 'untracked\n');
      const stale = await spawnWorkspace(client, 'stale');
      // Age only the two fixture records. Preview and removal use the served MCP tools.
      for (const slug of ['stale', 'dirty']) {
        const file = join(ctx.repo, '.git', 'graft', 'records', `${slug}.json`);
        const record = object(JSON.parse(readFileSync(file, 'utf8')), 'fixture record');
        writeJson(file, { ...record, heartbeat: 0 });
      }
      const preview = await call(client, 'workspace_gc', { apply: false });
      assert.deepEqual(preview, ctx.cli(['gc']));
      assert.deepEqual(preview.candidates, ['stale']);
      assert.deepEqual(preview.removed, []);
      assert(Array.isArray(preview.retained) && preview.retained.length === 1);
      assert.equal(object(preview.retained[0], 'GC retained workspace').name, 'dirty');
      assert(existsSync(stale.path));

      const applied = await call(client, 'workspace_gc', { apply: true });
      assert.deepEqual(applied.removed, ['stale']);
      assert(!existsSync(stale.path));
      assertRefMissing(ctx, 'stale');
      assert(existsSync(second.path));
      assert.equal(readFileSync(join(dirty.path, 'local.txt'), 'utf8'), 'untracked\n');
      ctx.coverTool('workspace_gc');
      const parked = await call(client, 'workspace_discard', { name: 'dirty', lease: dirty.lease, discard: true });
      assert.equal(parked.recovery_lease, dirty.lease);
      for (const tool of ['workspace_doctor', 'workspace_list', 'workspace_gc']) {
        const published = await call(client, tool);
        assert(!JSON.stringify(published).includes(dirty.lease), tool + ' disclosed a retired workspace lease');
      }
      assert(!ctx.git(['for-each-ref', '--format=%(refname)']).includes(dirty.lease));
      const returned = await call(client, 'workspace_restore', { name: 'dirty', lease: dirty.lease });
      assert.notEqual(returned.lease, dirty.lease);
      assert.equal(readFileSync(join(dirty.path, 'local.txt'), 'utf8'), 'untracked\n');
    }),
  },
  {
    name: 'MCP reports merge conflicts and retries land without losing local work',
    run: async (ctx) => withClient(ctx, false, async (client) => {
      await initialize(client);
      const name = 'landing';
      const workspace = await spawnWorkspace(client, name);
      const owned = { name, lease: workspace.lease };
      const base = ctx.git(['rev-parse', 'HEAD']);
      writeFileSync(join(ctx.repo, 'tracked.txt'), 'destination\n');
      ctx.git(['commit', '-am', 'Destination edit']);
      const destination = ctx.git(['rev-parse', 'HEAD']);
      writeFileSync(join(workspace.path, 'tracked.txt'), 'workspace\n');
      ctx.git(['commit', '-am', 'Workspace edit'], workspace.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], workspace.path);

      const conflict = await call(client, 'workspace_land_and_wipe', owned, true);
      assert.equal(conflict.code, 'merge_conflict');
      const details = object(conflict.details, 'merge conflict details');
      assert.deepEqual(details.paths, ['tracked.txt']);
      assert.deepEqual(conflict, ctx.fail(['land', name, '--lease', workspace.lease]));
      assert.equal(ctx.git(['rev-parse', 'refs/heads/main']), destination);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), tip);
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'destination\n');
      assert.equal(readFileSync(join(workspace.path, 'tracked.txt'), 'utf8'), 'workspace\n');

      ctx.git(['branch', 'retry-target', base]);
      const target = join(ctx.root, 'landing target');
      ctx.git(['worktree', 'add', target, 'retry-target']);
      const landArgs = { ...owned, into: 'retry-target', squash: true };
      writeFileSync(join(target, 'keep.txt'), 'local work\n');
      const dirty = await call(client, 'workspace_land_and_wipe', landArgs, true);
      assert.match(string(dirty.message, 'destination refusal'), /local changes/);
      assert.equal(readFileSync(join(target, 'keep.txt'), 'utf8'), 'local work\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], target), base);
      assert(existsSync(workspace.path));
      ctx.git(['add', 'keep.txt'], target);
      ctx.git(['commit', '-m', 'Preserve local work'], target);
      const previous = ctx.git(['rev-parse', 'HEAD'], target);

      const landed = await call(client, 'workspace_land_and_wipe', landArgs);
      const commit = string(landed.commit, 'landed commit');
      assert.deepEqual(landed, {
        action: 'land', name, target: 'retry-target', previous, commit, workspace_tip: tip, removed: true, recovery_lease: workspace.lease,
      });
      assert.equal(ctx.git(['rev-parse', 'HEAD'], target), commit);
      assert.equal(ctx.git(['show', '-s', '--format=%P', commit]), previous);
      assert.equal(readFileSync(join(target, 'tracked.txt'), 'utf8'), 'workspace\n');
      assert.equal(readFileSync(join(target, 'keep.txt'), 'utf8'), 'local work\n');
      assert.equal(ctx.git(['status', '--porcelain'], target), '');
      assert.equal(ctx.git(['rev-parse', 'refs/heads/main']), destination);
      assert(!existsSync(workspace.path));
      assertRefMissing(ctx, name);
      assert.deepEqual(await call(client, 'workspace_land_and_wipe', landArgs), landed);
      assert.deepEqual(ctx.cli(['land', name, '--lease', workspace.lease, '--into', 'retry-target', '--squash']), landed);
      ctx.coverTool('workspace_land_and_wipe');
      const undone = await call(client, 'workspace_undo_land', owned);
      assert.equal(undone.target, 'retry-target');
      assert.equal(ctx.git(['rev-parse', 'HEAD^'], target), commit);
      assert.equal(ctx.git(['rev-parse', 'HEAD^{tree}'], target), ctx.git(['rev-parse', previous + '^{tree}']));
      assert.deepEqual(await call(client, 'workspace_undo_land', owned), undone);
      ctx.coverTool('workspace_undo_land');
    }),
  },
  {
    name: 'MCP executes scoped processes, installs from cache and verifies the layer backend',
    run: async (ctx) => withClient(ctx, true, async (client) => {
      const initialized = await initialize(client);
      assert.match(string(initialized.instructions, 'execution instructions'), /enabled by the host/);
      const catalog = await discover(client, true);
      ctx.measure('advertised_tools', [...catalog.keys()].sort());
      const layerSchema = object(catalog.get('workspace_layer')?.inputSchema, 'layer schema');
      const layerProperties = object(layerSchema.properties, 'layer schema properties');
      const copyOption = object(layerProperties.copy, 'layer copy option');
      assert.equal(copyOption.type, 'boolean');
      assert.equal(copyOption.default, false);
      assert(Array.isArray(layerSchema.required) && !layerSchema.required.includes('copy'));
      for (const name of executionTools) await invalidCall(client, name, {});
      const archive = prepareNpmFixture(ctx);
      const doctor = await call(client, 'workspace_doctor');
      assert.deepEqual(doctor.root_lockfiles, ['package-lock.json']);
      const storage = object(doctor.source_storage, 'native storage evidence');
      assert(typeof storage.native_clone === 'boolean');
      const sourceBackend = string(storage.backend, 'probe backend');
      const source = await spawnWorkspace(client, 'packages');
      const owned = { name: 'packages', lease: source.lease };

      const env = await call(client, 'workspace_env', { name: owned.name });
      assert.deepEqual(env, ctx.cli(['env', owned.name]));
      assert.equal(realpathSync(string(env.worktree, 'runtime worktree')), realpathSync(source.path));
      assert.equal(env.port, source.result.port);
      const cache = string(env.cache_dir, 'package cache path');
      const target = string(env.cargo_target_dir, 'private Cargo target path');
      assert.equal(dirname(target), env.runtime_dir);
      ctx.coverTool('workspace_env');

      const script = `const fs=require('node:fs');fs.writeFileSync('exec-proof.txt','wire\\n');process.stdout.write(JSON.stringify({cwd:process.cwd(),port:process.env.PORT,target:process.env.CARGO_TARGET_DIR}));`;
      const argv = [process.execPath, '-e', script];
      const executed = await call(client, 'workspace_exec', { ...owned, argv });
      assert.equal(executed.success, true);
      assert.equal(executed.exit_code, 0);
      const output = object(JSON.parse(string(executed.stdout, 'exec stdout')), 'runtime output');
      assert.equal(realpathSync(string(output.cwd, 'execution directory')), realpathSync(source.path));
      assert.equal(output.port, String(env.port));
      assert.equal(output.target, target);
      assert.equal(readFileSync(join(source.path, 'exec-proof.txt'), 'utf8'), 'wire\n');
      assert(!existsSync(join(ctx.repo, 'exec-proof.txt')));
      assert.deepEqual(executed, ctx.cli(['exec', owned.name, '--lease', source.lease, '--', ...argv]));

      const failingArgv = [process.execPath, '-e', "process.stderr.write('expected failure');process.exit(7)"];
      const failed = await call(client, 'workspace_exec', { ...owned, argv: failingArgv }, true);
      assert.equal(failed.success, false);
      assert.equal(failed.exit_code, 7);
      assert.equal(failed.stderr, 'expected failure');
      const failedCli = ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', 'exec', owned.name, '--lease', source.lease, '--', ...failingArgv]);
      assert.ifError(failedCli.error);
      assert.equal(failedCli.status, 7);
      assert.deepEqual(JSON.parse(failedCli.stdout), failed);
      ctx.coverTool('workspace_exec');

      const installArgs = { ...owned, manager: 'npm', offline: true, packages: [] };
      const cold = await call(client, 'workspace_add_dependency', installArgs, true);
      assert.equal(cold.success, false);
      assert.match(string(cold.stderr, 'cold offline cache error'), /ENOTCACHED|only-if-cached/i);
      npm(ctx, ['cache', 'add', archive, '--offline', '--ignore-scripts', '--cache', join(cache, 'npm')], ctx.repo);
      const installed = await call(client, 'workspace_add_dependency', installArgs);
      assert.equal(installed.success, true);
      assert.equal(installed.exit_code, 0);
      const moduleFile = join('node_modules', fixturePackage, 'index.js');
      const sourceModule = join(source.path, moduleFile);
      assert.equal(readFileSync(sourceModule, 'utf8'), fixtureContents);
      assert(!existsSync(join(ctx.repo, moduleFile)));
      assert.equal(ctx.git(['diff', 'HEAD', '--', 'package.json', 'package-lock.json'], source.path), '');
      ctx.coverTool('workspace_add_dependency');

      const layers = join(dirname(cache), 'layers');
      const keyPrefix = `node-${process.versions.node}-npm-ci-offline`;
      await invalidCall(client, 'workspace_layer', { ...owned, action: 'save', key: keyPrefix, copy: 'true' });
      assert(storage.native_clone ? ['clonefile', 'reflink'].includes(sourceBackend) : sourceBackend === 'git');
      const defaultBackend = storage.native_clone ? 'copy-on-write' : 'copy';
      const initialBytes = readdirSync(join(source.path, 'node_modules'), { recursive: true, withFileTypes: true })
        .filter((entry) => entry.isFile())
        .reduce((bytes, entry) => bytes + statSync(join(entry.parentPath, entry.name)).size, 0);
      assert(initialBytes >= Buffer.byteLength(fixtureContents));
      const transfers: JsonObject = {};
      for (const mode of [
        { name: 'automatic', options: {}, backend: defaultBackend },
        { name: 'forced', options: { copy: true }, backend: 'copy' },
      ]) {
        const key = `${keyPrefix}-${mode.name}`;
        writeFileSync(sourceModule, fixtureContents);
        const saveArgs = { ...owned, action: 'save', key, ...mode.options };
        const saved = await call(client, 'workspace_layer', saveArgs);
        const digest = string(saved.digest, 'layer digest');
        assert.match(digest, /^[a-f0-9]{64}$/);
        const transferReceipt = {
          digest, backend: mode.backend, logical_bytes: initialBytes,
          cloned_bytes: mode.backend === 'copy-on-write' ? initialBytes : 0,
          copied_bytes: mode.backend === 'copy' ? initialBytes : 0,
        };
        assert.deepEqual(saved, { action: 'save', ...transferReceipt });
        const savedTree = join(layers, digest);
        const savedModule = join(savedTree, moduleFile);
        assert.equal(readFileSync(savedModule, 'utf8'), fixtureContents);
        const producerEdit = 'module.exports = 99;\n';
        writeFileSync(sourceModule, producerEdit);
        assert.equal(readFileSync(savedModule, 'utf8'), fixtureContents);

        const name = `restore-${mode.name}`;
        const consumer = await spawnWorkspace(client, name);
        const restoreArgs = { name, lease: consumer.lease, action: 'restore', key, ...mode.options };
        assert(!existsSync(join(consumer.path, 'node_modules')));
        const restored = await call(client, 'workspace_layer', restoreArgs);
        assert.deepEqual(restored, { action: 'restore', ...transferReceipt });
        const restoredModule = join(consumer.path, moduleFile);
        assert.equal(readFileSync(restoredModule, 'utf8'), fixtureContents);
        assert.equal(statSync(restoredModule).nlink, 1);
        assert.match(string((await call(client, 'workspace_layer', restoreArgs, true)).message, 'restore collision'), /node_modules already exists/);
        const consumerEdit = 'module.exports = 101;\n';
        writeFileSync(restoredModule, consumerEdit);
        assert.equal(readFileSync(savedModule, 'utf8'), fixtureContents);
        assert.equal(readFileSync(sourceModule, 'utf8'), producerEdit);

        const forgotten = await call(client, 'workspace_layer', { name, lease: consumer.lease, action: 'forget', key });
        assert.deepEqual(forgotten, {
          action: 'forget', digest, backend: mode.backend, logical_bytes: 0, cloned_bytes: 0, copied_bytes: 0,
          ...(process.platform === 'win32' ? { retained: true } : {}),
        });
        assert.deepEqual(readdirSync(layers).filter(entry => entry !== '.git-graft-id'), []);
        assert.equal(readFileSync(restoredModule, 'utf8'), consumerEdit);
        transfers[mode.name] = { saved, restored, forgotten };
      }
      ctx.measure('mcp_layer', {
        platform: process.platform, sourceBackend, layerBackend: defaultBackend,
        nativeClone: storage.native_clone, transfers,
      });
      ctx.coverTool('workspace_layer');
      npm(ctx, ['ci', '--offline', '--ignore-scripts', '--cache', join(cache, 'npm')], ctx.repo);
      const key = keyPrefix + '-primary';
      const primarySeed = await call(client, 'workspace_seed_layer', { key });
      const clean = await spawnWorkspace(client, 'primary-consumer');
      const transfer = await call(client, 'workspace_layer', { name: 'primary-consumer', lease: clean.lease, action: 'restore', key });
      assert.equal(transfer.digest, primarySeed.digest);
      const file = join(clean.path, moduleFile);
      assert.equal(readFileSync(file, 'utf8'), fixtureContents);
      writeFileSync(file, 'private edit');
      assert.equal(readFileSync(join(ctx.repo, moduleFile), 'utf8'), fixtureContents);
      ctx.coverTool('workspace_seed_layer');
    }),
  },
];
