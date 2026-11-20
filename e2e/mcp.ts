import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, realpathSync, renameSync, statSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join } from 'node:path';
import { createInterface } from 'node:readline';
import { clearTimeout, setTimeout } from 'node:timers';
import { parseExact, type Scenario } from './harness.ts';

type Context = Parameters<Scenario['run']>[0];
type JsonObject = Record<string, unknown>;
type Pending = {
  timer: ReturnType<typeof setTimeout>;
  resolve: (response: JsonObject) => void;
  reject: (error: Error) => void;
};

const protocol = '2025-06-18';
const requestTimeoutMs = 30_000;
const maxRequestBytes = 256 * 1024;
const executionTools = ['graft_exec', 'graft_add_dependency'];
const defaultTools = ['graft_list', 'graft_new', 'graft_status', 'graft_renew', 'graft_land', 'graft_prune',
  'graft_restore'];
const spawnOptions = ['base', 'branch', 'detach', 'history', 'lock', 'name', 'no_checkout', 'no_deps', 'orphan',
  'reason', 'request_id', 'sparse'];
const leaseTools = new Set(['graft_add_dependency', 'graft_prune', 'graft_exec', 'graft_renew', 'graft_land',
  'graft_restore']);

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

function workspacePath(result: JsonObject): string {
  assert(!('worktree_path' in result), 'Workspace responses use the path field');
  return string(result.path, 'workspace path');
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

  request(method: string, params?: JsonObject, padToBytes?: number): Promise<JsonObject> {
    const id = ++this.nextId;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => this.fail(new Error(`MCP ${method} (ID ${id}) timed out`)), requestTimeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      try {
        const line = JSON.stringify({ jsonrpc: '2.0', id, method, ...(params === undefined ? {} : { params }) });
        this.send(padToBytes === undefined ? line : line.padEnd(padToBytes));
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
  const expected = [...defaultTools];
  if (allowExec) expected.push(...executionTools);
  const catalog = new Map<string, JsonObject>();
  for (const entry of tools) {
    const tool = object(entry, 'tool descriptor');
    const name = string(tool.name, 'tool name');
    assert(!catalog.has(name), `Duplicate MCP tool ${name}`);
    catalog.set(name, tool);
    const schema = object(tool.inputSchema, `${name} input schema`);
    assert.equal(schema.type, 'object');
    assert.equal(schema.additionalProperties, false);
    const required = Array.isArray(schema.required) ? schema.required : [];
    assert.equal(required.includes('lease'), leaseTools.has(name), name + ' lease requirement');
    const description = string(tool.description, `${name} description`);
    if (executionTools.includes(name)) assert.match(description, /--allow-exec/);
  }
  assert.deepEqual([...catalog.keys()].sort(), expected.sort());
  const spawnDescription = string(catalog.get('graft_new')?.description, 'spawn description');
  assert.match(spawnDescription, /omit history/i);
  assert.match(spawnDescription, /defaults to local/i);
  assert.match(spawnDescription, /isolated.*(?:unrelated|excluded)/i);
  const spawnSchema = object(catalog.get('graft_new')?.inputSchema, 'spawn schema');
  assert.deepEqual(spawnSchema.required, ['name']);
  assert.deepEqual(Object.keys(object(spawnSchema.properties, 'spawn properties')).sort(), spawnOptions);
  const landSchema = object(catalog.get('graft_land')?.inputSchema, 'land schema');
  assert(Array.isArray(landSchema.required));
  assert.deepEqual([...landSchema.required].sort(), ['lease', 'name']);
  assert('undo' in object(landSchema.properties, 'land properties'));
  const pruneSchema = object(catalog.get('graft_prune')?.inputSchema, 'prune schema');
  assert(Array.isArray(pruneSchema.required));
  assert.deepEqual([...pruneSchema.required].sort(), ['lease', 'name']);
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
  const result = await call(client, 'graft_new', { name, request_id: requestId });
  const path = workspacePath(result);
  const lease = string(result.lease, 'workspace lease');
  assert.equal(lease, requestId);
  assert.equal(result.name, name);
  assert.equal(result.branch, `graft/${name}`);
  assert.equal(result.phase, 'ready');
  assert.equal(readFileSync(join(path, 'tracked.txt'), 'utf8'), 'initial\n');
  return { path, lease, result };
}

/** Arguments for `deps layer save|restore|forget` on the CLI; layers are CLI-only. */
function layerArgs(action: string, name: string, lease: string, key: string, copy = false): string[] {
  return ['deps', 'layer', action, name, '--lease', lease, '--key', key, ...(copy ? ['--copy'] : [])];
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
        resolved: 'https://127.0.0.1:1/graft-mcp-fixture.tgz',
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
    name: 'manager listing leaves fresh and foreign project state untouched',
    run(ctx) {
      const state = join(ctx.repo, '.git', 'graft');
      assert.deepEqual(ctx.cli(['deps', 'manager', 'list']), { managers: [] });
      assert(!existsSync(state));
      mkdirSync(state);
      const foreign = join(state, 'foreign-note');
      writeFileSync(foreign, 'retain foreign state');
      ctx.fail(['deps', 'manager', 'list']);
      assert.equal(readFileSync(foreign, 'utf8'), 'retain foreign state');
      assert.deepEqual(readdirSync(state), ['foreign-note']);
    },
  },
  {
    name: 'registered managers expose a shared cache to MCP execution and keep it on removal',
    run: async ctx => withClient(ctx, true, async client => {
      await initialize(client);
      const catalog = await discover(client, true);
      ctx.measure('advertised_tools', [...catalog.keys()]);
      const scriptPath = join(ctx.root, 'fixture script.cjs');
      writeFileSync(scriptPath, 'process.stdout.write(JSON.stringify({cache:process.env.FIXTURE_CACHE,graft:process.env.GRAFT_PACKAGE_CACHE,argv:process.argv.slice(2)}))');
      const registration = { manager: 'fixture-manager', cache_env: 'FIXTURE_CACHE',
        cache_args: [scriptPath, 'literal with spaces', '{cache}'] };
      const added = ctx.cli(['deps', 'manager', 'add', registration.manager, '--cache-env', registration.cache_env,
        ...registration.cache_args.flatMap(arg => ['--cache-arg', arg])]);
      assert.equal(added.manager, registration.manager);
      const cache = string(added.cache_dir, 'registered cache');
      const listed = ctx.cli(['deps', 'manager', 'list']);
      assert(Array.isArray(listed.managers));
      assert(listed.managers.some(item => object(item, 'manager').manager === registration.manager));
      const workspace = await spawnWorkspace(client, 'registered-mcp');
      const empty = await call(client, 'graft_exec', { name: 'registered-mcp', lease: workspace.lease, argv: [] }, true);
      assert.match(string(empty.message, 'empty exec error'), /empty|required/i);
      const executed = await call(client, 'graft_exec', { name: 'registered-mcp', lease: workspace.lease,
        manager: registration.manager, argv: [process.execPath, 'argument with spaces'] });
      const output = object(JSON.parse(string(executed.stdout, 'registered exec output')), 'registered output');
      assert.equal(output.cache, cache);
      assert.equal(output.graft, cache);
      assert.deepEqual(output.argv, ['literal with spaces', cache, 'argument with spaces']);
      const removed = ctx.cli(['deps', 'manager', 'remove', registration.manager]);
      assert.deepEqual(removed, { manager: registration.manager, registered: false, cache_retained: true });
      assert(existsSync(cache));
    }),
  },
  {
    name: 'restricted MCP does not attach registered installations',
    async run(ctx) {
      writeFileSync(join(ctx.repo, 'fixture.lock'), 'locked\n');
      writeFileSync(join(ctx.repo, '.gitignore'), 'vendor/\n');
      ctx.git(['add', 'fixture.lock', '.gitignore']);
      ctx.git(['commit', '-qm', 'registered input']);
      mkdirSync(join(ctx.repo, 'vendor/bundle'), { recursive: true });
      writeFileSync(join(ctx.repo, 'vendor/bundle/package'), 'seed');
      const registration = ctx.cli(['deps', 'manager', 'add', 'restricted-fixture', '--cache-env', 'FIXTURE_CACHE',
        '--install-dir', 'vendor/bundle', '--input', 'fixture.lock', '--key', 'restricted']);
      await withClient(ctx, false, async client => {
        await initialize(client);
        await discover(client, false);
        const spawned = await call(client, 'graft_new', { name: 'restricted-seed', no_deps: false });
        const storage = object(spawned.storage, 'restricted spawn storage');
        assert(!('registered_dependencies' in storage) ||
          (Array.isArray(storage.registered_dependencies) && storage.registered_dependencies.every(item => object(item, 'receipt').status !== 'attached')));
        assert(!existsSync(join(workspacePath(spawned), 'vendor/bundle')));
      });
      assert.equal(readFileSync(join(ctx.repo, 'vendor/bundle/package'), 'utf8'), 'seed');
      assert(existsSync(string(registration.cache_dir, 'registered cache')));
    },
  },
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
      assert.equal(object(catalog.get('graft_list')?.annotations, 'list annotations').readOnlyHint, true);
      assert.equal(object(catalog.get('graft_land')?.annotations, 'land annotations').destructiveHint, true);
      const spawnSchema = object(catalog.get('graft_new')?.inputSchema, 'spawn schema');
      assert.deepEqual(spawnSchema.required, ['name']);
      const properties = object(spawnSchema.properties, 'spawn schema properties');
      assert.equal(object(properties.name, 'spawn name schema').type, 'string');
      assert(properties.request_id);
      assert.equal(object(properties.no_deps, 'spawn no_deps schema').type, 'boolean');
      assert.match(string(initialized.instructions, 'instructions'), new RegExp(String(maxRequestBytes)));

      // A complete mutation JSON followed by extra bytes must be discarded as
      // one frame, even when it is read in several chunks.
      const oversizedMutation = JSON.stringify({
        jsonrpc: '2.0', id: 9001, method: 'tools/call',
        params: { name: 'graft_new', arguments: { name: 'oversized' } },
      });
      client.send(oversizedMutation.padEnd(maxRequestBytes + 1));
      assert.deepEqual(result(await client.request('ping', {}, maxRequestBytes)), {});

      await invalidCall(client, 'graft_status', { name: 7 });
      await invalidCall(client, 'graft_status', {});
      await invalidCall(client, 'graft_new', { name: 'unknown-history', history: 'remote' });
      await invalidCall(client, 'graft_list', { unexpected: true });
      await invalidCall(client, 'graft_unknown', {});
      assert.equal((await call(client, 'graft_status', { name: 'missing' }, true)).code, 'not_found');

      const doctor = ctx.cli(['doctor']);
      assert.equal(realpathSync(string(doctor.common_dir, 'common Git directory')), realpathSync(join(ctx.repo, '.git')));
      // Maintenance commands stay on the CLI and are not served over MCP.
      for (const removed of ['graft_doctor', 'graft_env', 'graft_lock', 'graft_unlock', 'graft_move', 'graft_repair',
        'graft_gc', 'graft_layer', 'graft_seed_layer', 'graft_list_managers']) {
        await invalidCall(client, removed, {});
      }

      assert.deepEqual(await call(client, 'graft_list'), { workspaces: [], recoverable: [] });
      const workspace = await spawnWorkspace(client, 'wire');
      const owned = { name: 'wire', lease: workspace.lease };
      const marker = join(workspace.path, 'execution-marker');
      const denied: [string, JsonObject][] = [
        ['graft_exec', { ...owned, argv: [process.execPath, '-e',
          'require("fs").writeFileSync("execution-marker", "ran")'] }],
        ['graft_add_dependency', { ...owned, manager: 'npm', offline: true, packages: [] }],
        ['graft_land', { ...owned, into: 'main', reconcile: true }],
      ];
      for (const [name, args] of denied) {
        const refusal = await call(client, name, args, true);
        assert.match(string(refusal.message, 'execution permission error'), /--allow-exec/);
      }
      assert(!existsSync(marker));
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
      assert.deepEqual(await call(client, 'graft_list'), { workspaces: [], recoverable: [] });
    }),
  },
  {
    name: 'MCP ownership, new and prune receipts, and expired pruning match CLI',
    run: async (ctx) => withClient(ctx, false, async (client) => {
      await initialize(client);
      const name = 'lifecycle';
      const first = await spawnWorkspace(client, name);
      ctx.git(['commit', '--allow-empty', '-m', 'Advance primary after spawn']);
      const primaryHead = ctx.git(['rev-parse', 'HEAD']);
      assert.notEqual(primaryHead, first.result.base);
      await withClient(ctx, false, async replay => {
        await initialize(replay);
        assert.deepEqual(await spawnWorkspace(replay, name, first.lease), first);
        const requestId = first.lease.toUpperCase().replaceAll('-', '');
        assert.deepEqual(await call(replay, 'graft_new', { name, request_id: requestId }), first.result);
      });
      const differentOptions = await call(client, 'graft_new', {
        name, request_id: first.lease, detach: true,
      }, true);
      assert.match(string(differentOptions.message, 'spawn replay error'), /different new options/);
      assert.equal((await call(client, 'graft_new', { name, request_id: randomUUID() }, true)).code, 'already_exists');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], first.path), first.result.base);
      assert.equal(ctx.git(['symbolic-ref', 'HEAD'], first.path), `refs/heads/graft/${name}`);
      ctx.coverTool('graft_new');

      const listed = await call(client, 'graft_list');
      assert.deepEqual(listed, ctx.cli(['list']));
      assert(Array.isArray(listed.workspaces) && listed.workspaces.length === 1);
      const record = object(listed.workspaces[0], 'listed workspace');
      assert.equal(record.name, name);
      assert.equal(workspacePath(record), first.path);
      assert(!JSON.stringify(listed).includes(first.lease));
      ctx.coverTool('graft_list');

      const status = await call(client, 'graft_status', { name });
      assert.deepEqual(status, ctx.cli(['status', name]));
      assert.equal(workspacePath(status), first.path);
      assert.equal(status.head, first.result.base);
      assert.deepEqual(status.changes, []);
      assert(!JSON.stringify(status).includes(first.lease));
      ctx.coverTool('graft_status');

      const owned = { name, lease: first.lease };
      const wrongLease = { name, lease: randomUUID() };
      for (const [tool, extra] of [['graft_renew', {}], ['graft_prune', {}], ['graft_land', {}],
        ['graft_land', { undo: true }], ['graft_restore', {}]] as const) {
        await invalidCall(client, tool, { name, ...extra });
        const error = await call(client, tool, { ...wrongLease, ...extra }, true);
        assert.equal(error.code, 'lease_mismatch');
        assert(!JSON.stringify(error).includes(first.lease));
      }
      await invalidCall(client, 'graft_land', { ...owned, undo: true, into: 'main' });
      await invalidCall(client, 'graft_land', { ...owned, undo: true, squash: true });
      assert.equal(ctx.git(['rev-parse', 'HEAD']), primaryHead);
      assert.deepEqual(await call(client, 'graft_status', { name }), status);
      const renewal = await call(client, 'graft_renew', owned);
      const previousHeartbeat = first.result.last_heartbeat;
      assert(typeof renewal.last_heartbeat === 'number' && typeof previousHeartbeat === 'number');
      assert(renewal.last_heartbeat >= previousHeartbeat);
      const renewedStatus = await call(client, 'graft_status', { name });
      assert.equal(renewedStatus.last_heartbeat, renewal.last_heartbeat);
      assert(!JSON.stringify(renewal).includes(first.lease));
      ctx.coverTool('graft_renew');

      const discarded = await call(client, 'graft_prune', owned);
      assert.deepEqual(discarded, { action: 'drop', name, removed: true, recovery_lease: first.lease });
      assert(!existsSync(first.path));
      assertRefMissing(ctx, name);
      assert.deepEqual(await call(client, 'graft_prune', owned), discarded);
      assert.deepEqual(ctx.cli(['prune', name, '--lease', first.lease]), discarded);
      ctx.coverTool('graft_prune');

      mkdirSync(first.path);
      writeFileSync(join(first.path, 'keep.txt'), 'foreign data\n');
      const blocked = await call(client, 'graft_restore', owned, true);
      assert.equal(blocked.code, 'already_exists');
      assert.equal(readFileSync(join(first.path, 'keep.txt'), 'utf8'), 'foreign data\n');
      renameSync(first.path, join(ctx.root, 'preserved foreign data'));
      const reopened = await call(client, 'graft_restore', owned);
      const second = { path: workspacePath(reopened), lease: string(reopened.lease, 'new lease') };
      assert.deepEqual(await call(client, 'graft_restore', owned), reopened);
      assert.deepEqual(ctx.cli(['restore', name, '--lease', first.lease]), reopened);
      ctx.coverTool('graft_restore');
      assert.notEqual(second.lease, first.lease);
      assert.deepEqual(await call(client, 'graft_prune', owned), discarded);
      await call(client, 'graft_land', owned, true);
      assert.equal((await call(client, 'graft_renew', owned, true)).code, 'lease_mismatch');
      assert.equal(readFileSync(join(second.path, 'tracked.txt'), 'utf8'), 'initial\n');

      const dirty = await spawnWorkspace(client, 'dirty');
      writeFileSync(join(dirty.path, 'local.txt'), 'untracked\n');
      const stale = await spawnWorkspace(client, 'stale');
      // Age only the two fixture records. Expired pruning is CLI-only.
      for (const workspace of [stale, dirty]) {
        const id = string(workspace.result.id, 'fixture workspace id');
        const file = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', `${id}.json`);
        const saved = object(parseExact(readFileSync(file, 'utf8')), 'fixture record');
        writeJson(file, { ...saved, heartbeat: 0 });
      }
      const preview = ctx.cli(['prune', '--expired']);
      assert.equal(preview.applied, false);
      assert(Array.isArray(preview.retained) && preview.retained.length === 2);
      const eligibility = Object.fromEntries(preview.retained.map(value => {
        const item = object(value, 'GC workspace');
        return [item.name, item.reason];
      }));
      assert.deepEqual(eligibility, { dirty: 'dirty', stale: 'eligible' });
      assert(!JSON.stringify(preview).includes(stale.lease));
      assert(existsSync(stale.path));

      const applied = ctx.cli(['prune', '--expired', '--apply']);
      assert.equal(applied.applied, true);
      assert(Array.isArray(applied.retained));
      const archived = applied.retained.map(value => object(value, 'GC workspace'))
        .find(item => item.reason === 'archived');
      assert(archived);
      assert.equal(archived.name, 'stale');
      assert.equal(workspacePath(archived), stale.path);
      assert(!JSON.stringify(applied).includes(stale.lease));
      assert(!existsSync(stale.path));
      assertRefMissing(ctx, 'stale');
      assert(existsSync(second.path));
      const restoredStale = await call(client, 'graft_restore', { name: 'stale', lease: stale.lease });
      assert.notEqual(restoredStale.lease, stale.lease);
      assert.equal(readFileSync(join(workspacePath(restoredStale), 'tracked.txt'), 'utf8'), 'initial\n');
      assert.equal(readFileSync(join(dirty.path, 'local.txt'), 'utf8'), 'untracked\n');
      const parked = await call(client, 'graft_prune', { name: 'dirty', lease: dirty.lease, discard: true });
      assert.equal(parked.recovery_lease, dirty.lease);
      for (const [label, published] of [['doctor', ctx.cli(['doctor'])], ['graft_list', await call(client, 'graft_list')],
        ['prune --expired', ctx.cli(['prune', '--expired'])]] as const) {
        assert(!JSON.stringify(published).includes(dirty.lease), label + ' disclosed a retired graft lease');
      }
      assert(!ctx.git(['for-each-ref', '--format=%(refname)']).includes(dirty.lease));
      const returned = await call(client, 'graft_restore', { name: 'dirty', lease: dirty.lease });
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

      const conflict = await call(client, 'graft_land', owned, true);
      assert.equal(conflict.code, 'merge_conflict');
      const details = object(conflict.details, 'merge conflict details');
      assert.deepEqual(details.paths, ['tracked.txt']);
      assert.deepEqual(conflict, ctx.fail(['land', name, '--lease', workspace.lease]));
      assert.equal(ctx.git(['rev-parse', 'refs/heads/main']), destination);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), tip);
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'destination\n');
      assert.equal(readFileSync(join(workspace.path, 'tracked.txt'), 'utf8'), 'workspace\n');

      ctx.git(['branch', 'retry-target', base]);
      const target = ctx.repo;
      ctx.git(['switch', 'retry-target']);
      const landArgs = { ...owned, into: 'retry-target', squash: true };
      writeFileSync(join(target, 'keep.txt'), 'local work\n');
      const dirty = await call(client, 'graft_land', landArgs, true);
      assert.match(string(dirty.message, 'destination refusal'), /local changes/);
      assert.equal(readFileSync(join(target, 'keep.txt'), 'utf8'), 'local work\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], target), base);
      assert(existsSync(workspace.path));
      ctx.git(['add', 'keep.txt'], target);
      ctx.git(['commit', '-m', 'Preserve local work'], target);
      const previous = ctx.git(['rev-parse', 'HEAD'], target);

      const landed = await call(client, 'graft_land', landArgs);
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
      assert.deepEqual(await call(client, 'graft_land', landArgs), landed);
      assert.deepEqual(ctx.cli(['land', name, '--lease', workspace.lease, '--into', 'retry-target', '--squash']), landed);
      ctx.coverTool('graft_land');
      const undone = await call(client, 'graft_land', { ...owned, undo: true });
      assert.equal(undone.target, 'retry-target');
      assert.equal(ctx.git(['rev-parse', 'HEAD^'], target), commit);
      assert.equal(ctx.git(['rev-parse', 'HEAD^{tree}'], target), ctx.git(['rev-parse', previous + '^{tree}']));
      assert.deepEqual(await call(client, 'graft_land', { ...owned, undo: true }), undone);
      assert.deepEqual(ctx.cli(['land', name, '--undo', '--lease', workspace.lease]), undone);
    }),
  },
  {
    name: 'locks, moves, repairs and restores a standalone graft across the CLI and MCP',
    run: async (ctx) => withClient(ctx, false, async (client) => {
      await initialize(client);
      assert.deepEqual(await call(client, 'graft_list'), { workspaces: [], recoverable: [] });
      const base = ctx.git(['rev-parse', 'HEAD']);
      const nativePaths = () => ctx.git(['worktree', 'list', '--porcelain']).split(/\r?\n/)
        .filter(line => line.startsWith('worktree '));
      const inventory = nativePaths();
      const name = 'mcp-private';
      const workspace = await spawnWorkspace(client, name);
      assert(statSync(join(workspace.path, '.git')).isDirectory());
      assertRefMissing(ctx, name);
      const owned = { name, lease: workspace.lease };
      const wrong = { name, lease: randomUUID() };
      const moved = join(ctx.root, 'moved checkout');
      const repaired = join(ctx.root, 'externally moved checkout');

      const leaseOf = (graft: { lease: string }) => ['--lease', graft.lease];
      assert.equal(ctx.fail(['lock', name, ...leaseOf(wrong), '--reason', 'wrong lease']).code, 'lease_mismatch');
      assert.equal(ctx.cli(['lock', name, ...leaseOf(owned), '--reason', 'review']).locked, 'review');
      ctx.fail(['move', name, moved, ...leaseOf(owned)]);
      await call(client, 'graft_prune', { ...owned, discard: true }, true);
      assert.equal(ctx.fail(['unlock', name, ...leaseOf(wrong)]).code, 'lease_mismatch');
      assert.equal(ctx.cli(['unlock', name, ...leaseOf(owned)]).locked, null);
      assert.equal(ctx.fail(['move', name, moved, ...leaseOf(wrong)]).code, 'lease_mismatch');
      assert(!existsSync(moved));
      ctx.fail(['move', name, join(ctx.repo, 'invalid'), ...leaseOf(owned)]);
      const foreign = join(ctx.root, 'foreign checkout');
      mkdirSync(foreign);
      writeFileSync(join(foreign, 'keep'), 'foreign data\n');
      ctx.fail(['move', name, foreign, ...leaseOf(owned)]);
      ctx.fail(['repair', name, foreign, ...leaseOf(owned)]);
      assert.equal(readFileSync(join(foreign, 'keep'), 'utf8'), 'foreign data\n');
      const movedResult = ctx.cli(['move', name, moved, ...leaseOf(owned)]);
      assert.equal(workspacePath(movedResult), moved);
      assert.deepEqual(ctx.cli(['move', name, moved, ...leaseOf(owned)]), movedResult);
      assert(!existsSync(workspace.path));
      renameSync(moved, repaired);
      assert.equal(ctx.fail(['repair', name, repaired, ...leaseOf(wrong)]).code, 'lease_mismatch');
      await call(client, 'graft_status', { name }, true);
      const repairedResult = ctx.cli(['repair', name, repaired, ...leaseOf(owned)]);
      assert.equal(workspacePath(repairedResult), repaired);
      assert.deepEqual(ctx.cli(['repair', name, repaired, ...leaseOf(owned)]), repairedResult);

      writeFileSync(join(repaired, 'feature.txt'), 'feature\n');
      ctx.git(['add', 'feature.txt'], repaired);
      ctx.git(['commit', '-qm', 'Feature from independent workspace'], repaired);
      const tip = ctx.git(['rev-parse', 'HEAD'], repaired);
      ctx.git(['tag', 'private-tag'], repaired);
      const status = await call(client, 'graft_status', { name });
      assert.equal(workspacePath(status), repaired);
      assert.equal(status.head, tip);
      assert.equal(ctx.git(['tag', '--list', 'private-tag']), '');
      assert.equal(ctx.git(['rev-parse', 'HEAD']), base);

      const receipt = await call(client, 'graft_land', owned);
      assert.deepEqual(receipt, {
        action: 'land', name, target: 'main', previous: base, commit: tip,
        workspace_tip: tip, removed: true, recovery_lease: workspace.lease,
      });
      assert(!existsSync(repaired));
      assert.equal(readFileSync(join(ctx.repo, 'feature.txt'), 'utf8'), 'feature\n');
      const retired = await call(client, 'graft_list');
      assert.deepEqual(retired.workspaces, []);
      assert(Array.isArray(retired.recoverable) && retired.recoverable.length === 1);
      const recoverable = object(retired.recoverable[0], 'recoverable workspace');
      assert.equal(recoverable.name, name);
      assert.equal(workspacePath(recoverable), repaired);
      assert(!JSON.stringify(retired).includes(workspace.lease));
      assert.deepEqual(await call(client, 'graft_land', owned), receipt);

      const reversed = await call(client, 'graft_land', { ...owned, undo: true });
      assert.equal(ctx.git(['rev-parse', 'HEAD']), reversed.commit);
      assert.equal(ctx.git(['rev-parse', 'HEAD^']), tip);
      assert.equal(ctx.git(['rev-parse', 'HEAD^{tree}']), ctx.git(['rev-parse', `${base}^{tree}`]));
      const restored = await call(client, 'graft_restore', owned);
      assert.equal(workspacePath(restored), repaired);
      assert.deepEqual(await call(client, 'graft_restore', owned), restored);
      const lease = string(restored.lease, 'restored lease');
      assert.notEqual(lease, workspace.lease);
      ctx.fail(['lock', name, '--lease', workspace.lease]);
      assert.equal(ctx.git(['rev-parse', 'refs/tags/private-tag'], repaired), tip);
      assert.equal(readFileSync(join(repaired, 'feature.txt'), 'utf8'), 'feature\n');
      writeFileSync(join(repaired, 'staged.txt'), 'staged\n');
      ctx.git(['add', 'staged.txt'], repaired);
      writeFileSync(join(repaired, 'untracked.txt'), 'untracked\n');
      mkdirSync(join(repaired, 'ignored'));
      writeFileSync(join(repaired, 'ignored', 'output'), 'ignored build output\n');
      const discarded = await call(client, 'graft_prune', { name, lease, discard: true });
      assert.equal(discarded.recovery_lease, lease);
      assert(!existsSync(repaired));
      await call(client, 'graft_restore', owned, true);
      const reopened = await call(client, 'graft_restore', { name, lease });
      assert.equal(workspacePath(reopened), repaired);
      assert.notEqual(reopened.lease, lease);
      assert.equal(ctx.git(['show', ':staged.txt'], repaired), 'staged');
      assert.equal(readFileSync(join(repaired, 'untracked.txt'), 'utf8'), 'untracked\n');
      assert.equal(readFileSync(join(repaired, 'ignored', 'output'), 'utf8'), 'ignored build output\n');
      assert.equal(ctx.git(['rev-parse', 'refs/tags/private-tag'], repaired), tip);
      assertRefMissing(ctx, name);
      assert.deepEqual(nativePaths(), inventory);
    }),
  },
  {
    name: 'MCP advertises local history and honors overrides and retries',
    run: async (ctx) => withClient(ctx, true, async (client) => {
      const initialized = await initialize(client);
      assert.match(string(initialized.instructions, 'instructions'), /enabled by the host/);
      const guidance = string(initialized.instructions, 'history instructions');
      assert.match(guidance, /omit history.*project setting.*default: local/i);
      assert.match(guidance, /isolated.*selected commit.s reachable history/i);

      ctx.command('git', ['config', '--local', '--unset-all', 'graft.history'], ctx.repo);
      const unrelated = ctx.git(['commit-tree', 'HEAD^{tree}', '-p', 'HEAD', '-m', 'unrelated']);
      ctx.git(['update-ref', 'refs/heads/unrelated', unrelated]);
      const probe = object(ctx.cli(['doctor']).source_storage, 'source filesystem capability');
      const defaultArgs = { name: 'mcp-default-history', request_id: randomUUID() };
      const defaultWorkspace = await call(client, 'graft_new', defaultArgs);
      const visibility = (workspace: JsonObject) => ctx.command('git', ['cat-file', '-e', unrelated], workspacePath(workspace)).status;
      assert.equal(visibility(defaultWorkspace) === 0, probe.native_clone, 'unconfigured local history visibility');
      ctx.git(['config', '--local', 'graft.history', 'isolated']);
      assert.deepEqual(await call(client, 'graft_new', defaultArgs), defaultWorkspace, 'retry must preserve original policy');
      const configured = await call(client, 'graft_new', { name: 'mcp-configured-history' });
      assert.notEqual(visibility(configured), 0, 'configured isolated policy must exclude unrelated objects');
      const override = await call(client, 'graft_new', { name: 'mcp-override-history', history: 'local' });
      assert.equal(visibility(override) === 0, probe.native_clone, 'explicit local policy must override project setting');
      ctx.git(['config', '--local', 'graft.history', 'local']);
      const isolated = await call(client, 'graft_new', { name: 'mcp-isolated-history', history: 'isolated' });
      assert.notEqual(visibility(isolated), 0, 'explicit isolated policy must exclude unrelated objects');
    }),
  },
  {
    name: 'MCP supports the CLI graft creation modes',
    run: async (ctx) => withClient(ctx, true, async (client) => {
      await initialize(client);
      const probe = object(ctx.cli(['doctor']).source_storage, 'source filesystem capability');
      for (const options of [
        { branch: 'chosen', detach: true },
        { branch: 'chosen', orphan: 'root' },
        { detach: true, orphan: 'root' },
        { orphan: 'root', base: 'HEAD' },
        { orphan: 'root', sparse: ['src'] },
        { no_checkout: true, sparse: ['src'] },
        { reason: 'requires lock' },
        { detach: 'true' },
        { unexpected: true },
      ]) {
        await invalidCall(client, 'graft_new', { name: 'bad-mode', ...options });
      }
      assert.deepEqual(await call(client, 'graft_list'), { workspaces: [], recoverable: [] });
      const base = ctx.git(['rev-parse', 'HEAD']);
      const args = {
        name: 'mcp-branch', request_id: randomUUID(), branch: 'custom/branch',
        base, no_checkout: true, lock: true, reason: 'manual population',
      };
      ctx.git(['config', '--local', 'graft.history', 'local']);
      const locked = await call(client, 'graft_new', args);
      if (probe.native_clone) {
        assert.equal(object(object(locked.storage, 'workspace storage').objects, 'workspace history').source, 'local-snapshot');
      }
      ctx.git(['config', '--local', '--unset', 'graft.history']);
      assert.equal(locked.lease, args.request_id);
      assert.equal(locked.locked, 'manual population');
      assert.deepEqual(await call(client, 'graft_new', args), locked);
      const empty = workspacePath(locked);
      assert.equal(ctx.git(['symbolic-ref', '--short', 'HEAD'], empty), 'custom/branch');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], empty), base);
      assert.equal(ctx.git(['ls-files'], empty), '');
      assert(!existsSync(join(empty, 'tracked.txt')));
      ctx.git(['restore', '--source=HEAD', '--staged', '--worktree', '--', '.'], empty);
      assert.equal(readFileSync(join(empty, 'tracked.txt'), 'utf8'), 'initial\n');

      const detached = await call(client, 'graft_new', { name: 'mcp-detached', detach: true, history: 'tip' });
      const detachedPath = workspacePath(detached);
      assert.equal(ctx.git(['rev-parse', '--is-shallow-repository'], detachedPath), 'true');
      assert.equal(detached.branch, null);
      assert.equal(ctx.git(['branch', '--show-current'], detachedPath), '');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], detachedPath), base);
      const orphan = await call(client, 'graft_new', { name: 'mcp-orphan', orphan: 'private-root' });
      const orphanPath = workspacePath(orphan);
      assert.equal(ctx.git(['symbolic-ref', '--short', 'HEAD'], orphanPath), 'private-root');
      assert.notEqual(ctx.command('git', ['rev-parse', '--verify', 'HEAD'], orphanPath).status, 0);
      assert(!existsSync(join(orphanPath, 'tracked.txt')));
    }),
  },
  {
    name: 'shared seed and new respect CLI and MCP execution boundaries',
    async run(ctx) {
      const archive = prepareNpmFixture(ctx);
      const cache = join(ctx.root, 'shared-npm-cache');
      npm(ctx, ['cache', 'add', archive, '--offline', '--ignore-scripts', '--cache', cache], ctx.repo);
      npm(ctx, ['ci', '--offline', '--ignore-scripts', '--cache', cache], ctx.repo);
      const file = join('node_modules', fixturePackage, 'index.js');
      const doctor = ctx.cli(['doctor']);
      const nativeClone = object(doctor.source_storage, 'source storage').native_clone;
      assert.equal(typeof nativeClone, 'boolean');
      const key = 'mcp-shared-fixture';
      const conflict = ctx.fail(['seed', '--shared', '--copy', '--key', key]);
      assert.equal(conflict.code, 'invalid_dependency_mode');
      assert.match(String(conflict.message), /omit --copy/i);

      // Seeding is CLI-only; MCP grafts inherit the registered shared install.
      const seed = nativeClone ? ctx.cli(['seed', '--shared', '--key', key]) : ctx.fail(['seed', '--shared', '--key', key]);
      if (!nativeClone) assert.equal(seed.code, 'cow_unavailable');
      await withClient(ctx, true, async client => {
        await initialize(client);
        if (nativeClone) {
          const attached = await spawnWorkspace(client, 'mcp-shared');
          const dependencies = object(object(attached.result.storage, 'storage').dependencies, 'dependency receipt');
          assert.equal(dependencies.status, 'attached');
          assert.equal(dependencies.copied_bytes, 0);
          assert.equal(readFileSync(join(attached.path, file), 'utf8'), fixtureContents);
          const optedOut = await call(client, 'graft_new', { name: 'mcp-no-deps', no_deps: true });
          assert(!('dependencies' in object(optedOut.storage, 'opted-out storage')));
          assert(!existsSync(join(workspacePath(optedOut), file)));
        }
      });
      await withClient(ctx, false, async client => {
        await initialize(client);
        const sourceOnly = await call(client, 'graft_new', { name: 'mcp-disabled', no_deps: false });
        assert(!('dependencies' in object(sourceOnly.storage, 'disabled storage')));
        assert(!existsSync(join(workspacePath(sourceOnly), file)));
      });
      if (nativeClone) {
        const cli = ctx.spawn('cli-shared');
        const dependencies = object(object(cli.result.storage, 'CLI storage').dependencies, 'CLI dependency receipt');
        assert.equal(dependencies.status, 'attached');
        assert.equal(dependencies.copied_bytes, 0);
        assert.equal(readFileSync(join(cli.path, file), 'utf8'), fixtureContents);
        const noDeps = ctx.spawn('cli-no-deps', ['--no-deps']);
        assert(!('dependencies' in object(noDeps.result.storage, 'CLI opted-out storage')));
        assert(!existsSync(join(noDeps.path, file)));
      } else {
        assert.equal(ctx.fail(['seed', '--shared', '--key', 'cli-shared-fixture']).code, 'cow_unavailable');
      }
    },
  },
  {
    name: 'MCP executes scoped processes and installs from cache; CLI layers use the verified backend',
    run: async (ctx) => withClient(ctx, true, async (client) => {
      const initialized = await initialize(client);
      assert.match(string(initialized.instructions, 'execution instructions'), /enabled by the host/);
      const catalog = await discover(client, true);
      ctx.measure('advertised_tools', [...catalog.keys()].sort());
      for (const name of executionTools) await invalidCall(client, name, {});
      const archive = prepareNpmFixture(ctx);
      const doctor = ctx.cli(['doctor']);
      const storage = object(doctor.source_storage, 'native storage evidence');
      assert(typeof storage.native_clone === 'boolean');
      const sourceBackend = string(storage.backend, 'probe backend');
      const source = await spawnWorkspace(client, 'packages');
      const owned = { name: 'packages', lease: source.lease };

      const env = ctx.cli(['status', owned.name, '--env']);
      assert.equal(realpathSync(workspacePath(env)), realpathSync(source.path));
      assert(!('worktree' in env));
      assert(!JSON.stringify(env).includes(source.lease));
      assert.equal(env.port, source.result.port);
      const cache = string(env.cache_dir, 'package cache path');
      const target = string(env.cargo_target_dir, 'private Cargo target path');
      assert.equal(dirname(target), env.runtime_dir);

      const script = `const fs=require('node:fs');fs.writeFileSync('exec-proof.txt','wire\\n');process.stdout.write(JSON.stringify({cwd:process.cwd(),port:process.env.PORT,target:process.env.CARGO_TARGET_DIR}));`;
      const argv = [process.execPath, '-e', script];
      const wrong = { ...owned, lease: randomUUID() };
      for (const [tool, args] of [
        ['graft_exec', { ...wrong, argv }],
        ['graft_add_dependency', { ...wrong, manager: 'npm', offline: true, packages: [] }],
      ] as const) {
        const refusal = await call(client, tool, args, true);
        assert.equal(refusal.code, 'lease_mismatch');
        assert(!JSON.stringify(refusal).includes(source.lease));
      }
      assert(!existsSync(join(source.path, 'exec-proof.txt')));
      assert(!existsSync(join(source.path, 'node_modules')));
      const executed = await call(client, 'graft_exec', { ...owned, argv });
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
      const failed = await call(client, 'graft_exec', { ...owned, argv: failingArgv }, true);
      assert.equal(failed.success, false);
      assert.equal(failed.exit_code, 7);
      assert.equal(failed.stderr, 'expected failure');
      const failedCli = ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', 'exec', owned.name, '--lease', source.lease, '--', ...failingArgv]);
      assert.ifError(failedCli.error);
      assert.equal(failedCli.status, 7);
      assert.deepEqual(JSON.parse(failedCli.stdout), failed);
      ctx.coverTool('graft_exec');

      const installArgs = { ...owned, manager: 'npm', offline: true, packages: [] };
      const cold = await call(client, 'graft_add_dependency', installArgs, true);
      assert.equal(cold.success, false);
      assert.match(string(cold.stderr, 'cold offline cache error'), /ENOTCACHED|only-if-cached/i);
      npm(ctx, ['cache', 'add', archive, '--offline', '--ignore-scripts', '--cache', join(cache, 'npm')], ctx.repo);
      const installed = await call(client, 'graft_add_dependency', installArgs);
      assert.equal(installed.success, true);
      assert.equal(installed.exit_code, 0);
      const moduleFile = join('node_modules', fixturePackage, 'index.js');
      const sourceModule = join(source.path, moduleFile);
      assert.equal(readFileSync(sourceModule, 'utf8'), fixtureContents);
      assert(!existsSync(join(ctx.repo, moduleFile)));
      assert.equal(ctx.git(['diff', 'HEAD', '--', 'package.json', 'package-lock.json'], source.path), '');
      ctx.coverTool('graft_add_dependency');

      const layers = join(dirname(cache), 'layers');
      const keyPrefix = `node-${process.versions.node}-npm-ci-offline`;
      assert(storage.native_clone ? ['clonefile', 'reflink'].includes(sourceBackend) : sourceBackend === 'git');
      const defaultBackend = storage.native_clone ? 'copy-on-write' : 'refused';
      const initialBytes = readdirSync(join(source.path, 'node_modules'), { recursive: true, withFileTypes: true })
        .filter((entry) => entry.isFile())
        .reduce((bytes, entry) => bytes + statSync(join(entry.parentPath, entry.name)).size, 0);
      assert(initialBytes >= Buffer.byteLength(fixtureContents));
      const transfers: JsonObject = {};
      if (!storage.native_clone) {
        const refusal = ctx.fail(layerArgs('save', owned.name, owned.lease, `${keyPrefix}-automatic`));
        assert.equal(refusal.code, 'cow_unavailable');
        transfers.automatic = refusal;
      }
      for (const mode of [
        ...(storage.native_clone ? [{ name: 'automatic', options: {}, backend: 'copy-on-write' }] : []),
        { name: 'forced', options: { copy: true }, backend: 'copy' },
      ]) {
        const key = `${keyPrefix}-${mode.name}`;
        writeFileSync(sourceModule, fixtureContents);
        const copy = 'copy' in mode.options;
        const saved = ctx.cli(layerArgs('save', owned.name, owned.lease, key, copy));
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
        const restoreArgs = layerArgs('restore', name, consumer.lease, key, copy);
        assert(!existsSync(join(consumer.path, 'node_modules')));
        const restored = ctx.cli(restoreArgs);
        assert.deepEqual(restored, { action: 'restore', ...transferReceipt });
        const restoredModule = join(consumer.path, moduleFile);
        assert.equal(readFileSync(restoredModule, 'utf8'), fixtureContents);
        assert.equal(statSync(restoredModule).nlink, 1);
        assert.match(string(ctx.fail(restoreArgs).message, 'restore collision'), /node_modules already exists/);
        const consumerEdit = 'module.exports = 101;\n';
        writeFileSync(restoredModule, consumerEdit);
        assert.equal(readFileSync(savedModule, 'utf8'), fixtureContents);
        assert.equal(readFileSync(sourceModule, 'utf8'), producerEdit);

        const forgotten = ctx.cli(layerArgs('forget', name, consumer.lease, key));
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
      npm(ctx, ['ci', '--offline', '--ignore-scripts', '--cache', join(cache, 'npm')], ctx.repo);
      const key = keyPrefix + '-primary';
      const primarySeed = ctx.cli(['seed', '--key', key, '--copy']);
      const clean = await spawnWorkspace(client, 'primary-consumer');
      const transfer = ctx.cli(layerArgs('restore', 'primary-consumer', clean.lease, key, true));
      assert.equal(transfer.digest, primarySeed.digest);
      const file = join(clean.path, moduleFile);
      assert.equal(readFileSync(file, 'utf8'), fixtureContents);
      writeFileSync(file, 'private edit');
      assert.equal(readFileSync(join(ctx.repo, moduleFile), 'utf8'), fixtureContents);
    }),
  },
];
