/**
 * Concurrency and process-lifetime scenarios. Each race runs real Graft
 * processes in parallel. Where an outcome depends on ordering, a Git hook in
 * the primary holds the first process at a known point until the test opens a
 * file gate, so the second process always starts inside the race window.
 */
import assert from 'node:assert/strict';
import { execFile, spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { createInterface } from 'node:readline';
import { setTimeout as delay } from 'node:timers/promises';
import { objectResult, parseExact, type Context, type ObjectResult, type Scenario } from './harness.ts';

/** The result of one Graft process, kept whether it succeeded or not. */
export type Outcome = { ok: boolean; code: number | null; json: ObjectResult; stderr: string };

/** Run Graft without throwing on failure, and parse its JSON receipt or error. */
export function graft(ctx: Context, args: string[], env: NodeJS.ProcessEnv = ctx.env): Promise<Outcome> {
  return new Promise((resolve, reject) => {
    execFile(ctx.bin, ['--repo', ctx.repo, '--json', ...args],
      { cwd: ctx.repo, env, timeout: 120_000, maxBuffer: 16 * 1024 * 1024, windowsHide: true },
      (error, stdout, stderr) => {
        const code = error ? (typeof error.code === 'number' ? error.code : null) : 0;
        if (error && code === null) return reject(error);
        const text = code === 0 ? stdout : (stderr.trim().startsWith('{') ? stderr : stdout);
        try {
          resolve({ ok: code === 0, code, json: objectResult(JSON.parse(text)), stderr });
        } catch (cause) {
          reject(new Error(`graft ${args[0]} printed no JSON (exit ${code}): ${stderr}`, { cause }));
        }
      });
  });
}

/** Poll for a condition the fixture itself sets, failing after a deadline. */
export async function waitFor(condition: () => boolean, label: string, timeout = 30_000): Promise<void> {
  const deadline = Date.now() + timeout;
  while (!condition()) {
    assert(Date.now() < deadline, `timed out waiting for ${label}`);
    await delay(25);
  }
}

/** Whether a process ID still names a running process. */
export function running(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === 'EPERM';
  }
}

/**
 * A file gate. `hold` arms it; the first holder to `pass` it records that it
 * arrived and waits until `open` is called. Later holders pass straight through.
 */
export class Gate {
  readonly dir: string;
  constructor(ctx: Context, name: string) {
    this.dir = join(ctx.root, 'gate-' + name);
    mkdirSync(this.dir);
  }
  get reached(): string { return join(this.dir, 'reached'); }
  arm(): void { writeFileSync(join(this.dir, 'armed'), 'armed\n'); }
  open(): void { writeFileSync(join(this.dir, 'open'), 'open\n'); }
  /** POSIX shell that holds the first caller. Git for Windows runs hooks with sh. */
  shell(): string {
    const dir = this.dir.replaceAll('\\', '/');
    return [
      `gate='${dir}'`,
      // Renaming the arming file is atomic, so exactly one caller holds.
      'if mv "$gate/armed" "$gate/taken" 2>/dev/null; then',
      '  printf reached > "$gate/reached"',
      '  i=0',
      '  while [ ! -f "$gate/open" ] && [ "$i" -lt 1200 ]; do sleep 0.05; i=$((i+1)); done',
      'fi',
    ].join('\n');
  }
}

/** Hold the first merge into `branch` after Git has locked the ref, before it commits. */
export function holdReferenceUpdate(ctx: Context, gate: Gate, branch = 'main'): () => void {
  const hook = join(ctx.repo, '.git', 'hooks', 'reference-transaction');
  mkdirSync(dirname(hook), { recursive: true });
  writeFileSync(hook, [
    '#!/bin/sh',
    '[ "$1" = prepared ] || exit 0',
    'held=',
    'while read -r old new reference; do',
    `  [ "$reference" = refs/heads/${branch} ] && held=1`,
    'done',
    '[ -n "$held" ] || exit 0',
    gate.shell(),
    'exit 0',
  ].join('\n') + '\n');
  chmodSync(hook, 0o700);
  gate.arm();
  return () => rmSync(hook, { force: true });
}

function write(root: string, path: string, content: string): void {
  const file = join(root, path);
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, content);
}

function commit(ctx: Context, root: string, message: string): string {
  ctx.git(['add', '-A'], root);
  ctx.git(['commit', '-qm', message], root);
  return ctx.git(['rev-parse', 'HEAD'], root);
}

function contains(ctx: Context, ancestor: string, rev = 'main'): boolean {
  // A graft's private commits reach the primary's objects only when merged.
  if (ctx.command('git', ['cat-file', '-e', ancestor + '^{commit}']).status !== 0) return false;
  const out = ctx.command('git', ['merge-base', '--is-ancestor', ancestor, rev]);
  assert.ifError(out.error);
  assert(out.status === 0 || out.status === 1, out.stderr);
  return out.status === 0;
}

/** Make every graft's lease look expired, so prune considers it. */
export function age(ctx: Context): void {
  const records = join(ctx.repo, '.git', 'graft', 'workspaces', 'records');
  const listing = ctx.cli(['list']);
  assert(Array.isArray(listing.workspaces));
  for (const workspace of listing.workspaces.map(objectResult)) {
    const path = join(records, `${String(workspace.id)}.json`);
    const record = objectResult(parseExact(readFileSync(path, 'utf8')));
    writeFileSync(path, JSON.stringify({ ...record, heartbeat: 0 }));
  }
}

function graftStatus(ctx: Context, name: string): ObjectResult | undefined {
  const listing = ctx.cli(['list']);
  assert(Array.isArray(listing.workspaces));
  return listing.workspaces.map(objectResult).find(workspace => workspace.name === name);
}

/** Restore a graft archived by an earlier operation and return its checkout. */
function restore(ctx: Context, name: string, lease: string): string {
  const restored = ctx.cli(['restore', name, '--lease', lease]);
  assert.equal(typeof restored.path, 'string');
  return String(restored.path);
}

/** A minimal MCP client: initialize, then call tools over stdio. */
class Mcp {
  readonly child: ChildProcessWithoutNullStreams;
  readonly waiting = new Map<number, (value: ObjectResult) => void>();
  next = 0;
  stderr = '';
  constructor(ctx: Context) {
    this.child = spawn(ctx.bin, ['--repo', ctx.repo, 'mcp'], { cwd: ctx.repo, env: ctx.env, stdio: 'pipe', windowsHide: true });
    this.child.stderr.setEncoding('utf8').on('data', (chunk: string) => { this.stderr = (this.stderr + chunk).slice(-8192); });
    createInterface({ input: this.child.stdout, crlfDelay: Infinity }).on('line', line => {
      const message = objectResult(JSON.parse(line));
      const resolve = this.waiting.get(Number(message.id));
      assert(resolve, 'unexpected MCP message: ' + line);
      this.waiting.delete(Number(message.id));
      resolve(message);
    });
  }
  request(method: string, params: ObjectResult): Promise<ObjectResult> {
    const id = ++this.next;
    const reply = new Promise<ObjectResult>(resolve => this.waiting.set(id, resolve));
    this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
    return reply;
  }
  async initialize(): Promise<void> {
    const reply = await this.request('initialize', {
      protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'process-faults', version: '1' },
    });
    assert(reply.result, 'MCP initialize failed: ' + JSON.stringify(reply) + this.stderr);
    this.child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' }) + '\n');
  }
  /** Call a tool and return its structured result, with `isError` alongside. */
  async call(name: string, args: ObjectResult): Promise<{ isError: boolean; value: ObjectResult }> {
    const reply = await this.request('tools/call', { name, arguments: args });
    const result = objectResult(reply.result);
    return { isError: result.isError === true, value: objectResult(result.structuredContent) };
  }
  async close(): Promise<void> {
    const exited = new Promise(resolve => this.child.once('close', resolve));
    this.child.stdin.end();
    await exited;
  }
}

/** The body of a background process: it records its PID, waits for the
 * release file, then writes into its working directory, the graft. */
const backgroundBody = [
  "const fs = require('node:fs');",
  'const [started, release] = process.argv.slice(2);',
  'fs.writeFileSync(started, String(process.pid));',
  'const end = Date.now() + 60000;',
  'const timer = setInterval(() => {',
  '  if (fs.existsSync(release) || Date.now() > end) {',
  "    clearInterval(timer); fs.writeFileSync('late.txt', 'written after exec returned\\n');",
  '  }',
  '}, 25);',
].join('\n');

/**
 * The command `exec` runs to leave a process behind and return at once. On
 * Unix it is the common agent pattern, a shell job started with nohup and
 * detached from every output pipe. Windows has no such shell, so Node starts
 * a detached process instead.
 */
function background(ctx: Context, started: string, release: string): string[] {
  const script = join(ctx.root, 'background.js');
  writeFileSync(script, backgroundBody);
  if (process.platform !== 'win32') {
    return ['/bin/sh', '-c', 'nohup "$0" "$1" "$2" "$3" >/dev/null 2>&1 </dev/null &', process.execPath, script, started, release];
  }
  return [process.execPath, '-e', [
    "const { spawn } = require('node:child_process');",
    'spawn(process.execPath, process.argv.slice(1), { detached: true, stdio: "ignore", windowsHide: true }).unref();',
  ].join('\n'), script, started, release];
}

export const cases: Scenario[] = [
  {
    name: 'a background process started by exec keeps the graft busy until it exits',
    async run(ctx) {
      const ws = ctx.spawn('agent');
      const started = join(ctx.root, 'background-started');
      const release = join(ctx.root, 'background-release');
      const exec = ctx.cli(['exec', 'agent', '--lease', ws.lease, '--', ...background(ctx, started, release)]);
      assert.equal(exec.success, true, JSON.stringify(exec));
      let pid = 0;
      try {
        await waitFor(() => existsSync(started) && readFileSync(started, 'utf8').length > 0, 'the background process');
        pid = Number(readFileSync(started, 'utf8'));
        assert(running(pid), 'the background process exited early');
        // Other processes may still run in the graft, and the agent can renew its lease.
        assert.equal(ctx.cli(['exec', 'agent', '--lease', ws.lease, '--', process.execPath, '-e', '']).success, true);
        ctx.cli(['heartbeat', 'agent', '--lease', ws.lease]);
        // Lifecycle operations refuse while it runs.
        for (const args of [
          ['remove', 'agent', '--lease', ws.lease, '--discard'],
          ['remove', 'agent', '--lease', ws.lease],
          ['merge', 'agent', '--lease', ws.lease],
          ['move', 'agent', join(ctx.root, 'moved agent'), '--lease', ws.lease],
          ['lock', 'agent', '--lease', ws.lease],
        ]) {
          const out = ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', ...args]);
          assert.notEqual(out.status, 0, `${args[0]} succeeded under a running process`);
          assert.equal(objectResult(JSON.parse(out.stderr || out.stdout)).code, 'workspace_busy', args[0]);
        }
        age(ctx);
        const prune = ctx.cli(['prune', '--apply']);
        assert(Array.isArray(prune.retained));
        assert.deepEqual(prune.retained.map(objectResult).map(w => [w.name, w.reason]), [['agent', 'busy']]);
        assert(existsSync(join(ws.path, 'tracked.txt')), 'the graft was archived under a running process');
        assert.equal(graftStatus(ctx, 'agent')?.name, 'agent');
      } finally {
        writeFileSync(release, 'release\n');
        if (pid) await waitFor(() => !running(pid), 'the background process to exit');
      }
      assert.equal(readFileSync(join(ws.path, 'late.txt'), 'utf8'), 'written after exec returned\n');
      ctx.cli(['remove', 'agent', '--lease', ws.lease, '--discard']);
      assert.equal(existsSync(ws.path), false);
      assert.equal(readFileSync(join(restore(ctx, 'agent', ws.lease), 'late.txt'), 'utf8'), 'written after exec returned\n');
    },
  },
  ...[false, true].map((overlap): Scenario => ({
    name: overlap ? 'concurrent conflicting merges into one checked-out branch keep the losing graft intact'
      : 'concurrent merges from two grafts into one checked-out branch keep both',
    async run(ctx) {
      const base = ctx.git(['rev-parse', 'HEAD']);
      const tasks = ['alpha', 'beta'].map(name => {
        const ws = ctx.spawn(name);
        write(ws.path, overlap ? 'tracked.txt' : name + '.txt', name + '\n');
        return { name, ws, tip: commit(ctx, ws.path, name) };
      });
      const gate = new Gate(ctx, 'merge');
      const unhook = holdReferenceUpdate(ctx, gate);
      let outcomes: Outcome[];
      try {
        const first = graft(ctx, ['merge', 'alpha', '--lease', tasks[0].ws.lease]);
        await waitFor(() => existsSync(gate.reached), 'the first merge to reach the ref update');
        const second = graft(ctx, ['merge', 'beta', '--lease', tasks[1].ws.lease]);
        gate.open();
        outcomes = await Promise.all([first, second]);
      } finally {
        gate.open();
        unhook();
      }
      assert.equal(outcomes[0].ok, true, JSON.stringify(outcomes[0].json));
      assert.equal(outcomes[0].json.action, 'merge');
      assert(contains(ctx, tasks[0].tip), 'the first merge is missing from main');
      const loser = outcomes[1];
      if (overlap) {
        assert.equal(loser.ok, false, 'a conflicting merge succeeded');
        assert(!contains(ctx, tasks[1].tip), 'a refused merge reached main');
        // The losing graft is untouched and still active.
        assert.equal(graftStatus(ctx, 'beta')?.name, 'beta');
        assert.equal(readFileSync(join(tasks[1].ws.path, 'tracked.txt'), 'utf8'), 'beta\n');
        assert.equal(ctx.git(['rev-parse', 'HEAD'], tasks[1].ws.path), tasks[1].tip);
        assert.equal(ctx.git(['status', '--porcelain'], tasks[1].ws.path), '');
        assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'alpha\n');
      } else {
        assert.equal(loser.ok, true, JSON.stringify(loser.json));
        assert(contains(ctx, tasks[1].tip), 'the second merge is missing from main');
        for (const { name } of tasks) assert.equal(readFileSync(join(ctx.repo, name + '.txt'), 'utf8'), name + '\n');
      }
      assert(contains(ctx, base));
      assert.equal(ctx.git(['status', '--porcelain']), '');
      assert.equal(existsSync(join(ctx.repo, '.git', 'graft', 'checkout-update')), false);
      const restored = restore(ctx, 'alpha', tasks[0].ws.lease);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], restored), tasks[0].tip);
      ctx.git(['fsck', '--full']);
    },
  })),
  {
    name: 'the same merge issued twice at once lands once and both receipts agree',
    async run(ctx) {
      const ws = ctx.spawn('twice');
      write(ws.path, 'feature.txt', 'feature\n');
      const tip = commit(ctx, ws.path, 'feature');
      const gate = new Gate(ctx, 'twice');
      const unhook = holdReferenceUpdate(ctx, gate);
      let outcomes: Outcome[];
      try {
        const first = graft(ctx, ['merge', 'twice', '--lease', ws.lease]);
        await waitFor(() => existsSync(gate.reached), 'the first merge to reach the ref update');
        const second = graft(ctx, ['merge', 'twice', '--lease', ws.lease]);
        gate.open();
        outcomes = await Promise.all([first, second]);
      } finally {
        gate.open();
        unhook();
      }
      assert.equal(outcomes[0].ok, true, JSON.stringify(outcomes[0].json));
      assert.equal(outcomes[0].json.commit, tip);
      const second = outcomes[1];
      if (second.ok) {
        assert.deepEqual(second.json, outcomes[0].json);
      } else {
        assert.equal(second.json.code, 'workspace_busy', JSON.stringify(second.json));
        assert.deepEqual(ctx.cli(['merge', 'twice', '--lease', ws.lease]), outcomes[0].json);
      }
      assert.equal(ctx.git(['rev-parse', 'main']), tip);
      assert.equal(ctx.git(['rev-list', '--count', 'main']), '2');
      assert.equal(readFileSync(join(restore(ctx, 'twice', ws.lease), 'feature.txt'), 'utf8'), 'feature\n');
    },
  },
  ...(['remove', 'prune', 'mcp-remove'] as const).map((rival): Scenario => ({
    name: `a merge racing ${rival === 'mcp-remove' ? 'an MCP remove' : rival} of the same graft keeps one consistent outcome`,
    async run(ctx) {
      const ws = ctx.spawn('raced');
      write(ws.path, 'feature.txt', 'raced work\n');
      const tip = commit(ctx, ws.path, 'raced work');
      write(ws.path, 'ignored/build.log', 'ignored output\n');
      if (rival === 'prune') age(ctx);
      const mcp = rival === 'mcp-remove' ? new Mcp(ctx) : undefined;
      await mcp?.initialize();
      const gate = new Gate(ctx, rival);
      const unhook = holdReferenceUpdate(ctx, gate);
      try {
        const merge = graft(ctx, ['merge', 'raced', '--lease', ws.lease, '--discard-ignored']);
        await waitFor(() => existsSync(gate.reached), 'the merge to reach the ref update');
        const other = rival === 'remove' ? graft(ctx, ['remove', 'raced', '--lease', ws.lease, '--discard'])
          : rival === 'prune' ? graft(ctx, ['prune', '--apply'])
          : mcp!.call('graft_remove', { name: 'raced', lease: ws.lease, discard: true });
        gate.open();
        const [merged, raced] = await Promise.all([merge, other]);
        assert.equal(merged.ok, true, JSON.stringify(merged.json));
        assert.equal(merged.json.action, 'merge');
        assert.equal(merged.json.commit, tip);
        if (rival === 'prune') {
          const result = raced as Outcome;
          assert.equal(result.ok, true, JSON.stringify(result.json));
          assert(Array.isArray(result.json.retained));
          const reasons = result.json.retained.map(objectResult).filter(w => w.name === 'raced').map(w => w.reason);
          assert(reasons.every(reason => reason !== 'archived'), 'prune archived a graft that was merging');
        } else {
          const { ok, json } = 'ok' in raced ? raced : { ok: !raced.isError, json: raced.value };
          if (ok) assert.equal(json.action, 'merge', 'remove reported a different archive: ' + JSON.stringify(json));
          else assert.match(String(json.code), /workspace_busy|operation_failed/, JSON.stringify(json));
        }
      } finally {
        gate.open();
        unhook();
        await mcp?.close();
      }
      assert.equal(ctx.git(['rev-parse', 'main']), tip);
      assert.equal(readFileSync(join(ctx.repo, 'feature.txt'), 'utf8'), 'raced work\n');
      assert.equal(existsSync(ws.path), false, 'the merged graft should be archived');
      const restored = restore(ctx, 'raced', ws.lease);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], restored), tip);
      assert.equal(readFileSync(join(restored, 'ignored', 'build.log'), 'utf8'), 'ignored output\n');
    },
  })),
  {
    name: 'a restore racing a merge with the old lease restores the archive intact',
    async run(ctx) {
      const ws = ctx.spawn('revived');
      write(ws.path, 'feature.txt', 'unmerged work\n');
      const tip = commit(ctx, ws.path, 'unmerged work');
      write(ws.path, 'notes.txt', 'uncommitted notes\n');
      ctx.cli(['remove', 'revived', '--lease', ws.lease, '--discard']);
      const base = ctx.git(['rev-parse', 'main']);
      const [restored, merged] = await Promise.all([
        graft(ctx, ['restore', 'revived', '--lease', ws.lease]),
        graft(ctx, ['merge', 'revived', '--lease', ws.lease]),
      ]);
      assert.equal(restored.ok, true, JSON.stringify(restored.json));
      // The archive was a remove, so a merge with its lease must never land it.
      assert.equal(merged.ok, false, JSON.stringify(merged.json));
      assert.equal(ctx.git(['rev-parse', 'main']), base);
      const path = String(restored.json.path);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], path), tip);
      assert.equal(readFileSync(join(path, 'notes.txt'), 'utf8'), 'uncommitted notes\n');
      // The restored graft merges normally with its fresh lease.
      ctx.git(['add', '-A'], path);
      ctx.git(['commit', '-qm', 'notes'], path);
      assert.equal(ctx.cli(['merge', 'revived', '--lease', String(restored.json.lease)]).action, 'merge');
      assert(contains(ctx, tip));
    },
  },
  {
    name: 'two adds with the same name at once create exactly one graft',
    async run(ctx) {
      const ids: string[] = [randomUUID(), randomUUID()];
      const outcomes = await Promise.all(ids.map(id => graft(ctx, ['add', 'twin', '--request-id', id])));
      const winners = outcomes.filter(outcome => outcome.ok);
      assert.equal(winners.length, 1, JSON.stringify(outcomes.map(outcome => outcome.json)));
      const loser = outcomes.find(outcome => !outcome.ok)!;
      assert.match(String(loser.json.code), /already_exists|workspace_busy/, JSON.stringify(loser.json));
      const winner = winners[0].json;
      assert(ids.includes(String(winner.lease)));
      const listing = ctx.cli(['list']);
      assert(Array.isArray(listing.workspaces));
      assert.deepEqual(listing.workspaces.map(objectResult).map(w => w.name), ['twin']);
      const path = String(winner.path);
      write(path, 'work.txt', 'winner work\n');
      // The losing request can neither adopt nor replace the winner.
      const loserId = ids.find(id => id !== winner.lease)!;
      assert.equal(ctx.fail(['add', 'twin', '--request-id', loserId]).code, 'already_exists');
      assert.equal(readFileSync(join(path, 'work.txt'), 'utf8'), 'winner work\n');
      ctx.cli(['heartbeat', 'twin', '--lease', String(winner.lease)]);
    },
  },
  {
    name: 'MCP and CLI operating on one graft at once never lose its work',
    async run(ctx) {
      const ws = ctx.spawn('shared');
      write(ws.path, 'feature.txt', 'shared work\n');
      const tip = commit(ctx, ws.path, 'shared work');
      const mcp = new Mcp(ctx);
      try {
        await mcp.initialize();
        const gate = new Gate(ctx, 'mcp');
        const unhook = holdReferenceUpdate(ctx, gate);
        try {
          const merge = mcp.call('graft_merge', { name: 'shared', lease: ws.lease });
          await waitFor(() => existsSync(gate.reached), 'the MCP merge to reach the ref update');
          const cli = graft(ctx, ['merge', 'shared', '--lease', ws.lease]);
          gate.open();
          const [viaMcp, viaCli] = await Promise.all([merge, cli]);
          assert.equal(viaMcp.isError, false, JSON.stringify(viaMcp.value));
          assert.equal(viaMcp.value.commit, tip);
          if (viaCli.ok) assert.deepEqual(viaCli.json, viaMcp.value);
          else assert.equal(viaCli.json.code, 'workspace_busy', JSON.stringify(viaCli.json));
        } finally {
          gate.open();
          unhook();
        }
        const status = await mcp.call('graft_status', { name: 'shared' });
        assert.equal(status.isError, true, 'a merged graft should no longer be live');
      } finally {
        await mcp.close();
      }
      assert.equal(ctx.git(['rev-parse', 'main']), tip);
      const restored = restore(ctx, 'shared', ws.lease);
      assert.equal(readFileSync(join(restored, 'feature.txt'), 'utf8'), 'shared work\n');
      ctx.coverTool('graft_merge');
    },
  },
  {
    name: 'removal refuses while native Git runs in the graft and restore never adopts a recreated path',
    async run(ctx) {
      const ws = ctx.spawn('native');
      write(ws.path, 'notes.txt', 'uncommitted\n');
      // A native Git command in the graft holds its index lock.
      const lock = join(ws.path, '.git', 'index.lock');
      writeFileSync(lock, '');
      const refused = await graft(ctx, ['remove', 'native', '--lease', ws.lease]);
      assert.equal(refused.ok, false, JSON.stringify(refused.json));
      assert.equal(readFileSync(join(ws.path, 'notes.txt'), 'utf8'), 'uncommitted\n');
      assert.equal(graftStatus(ctx, 'native')?.name, 'native');
      rmSync(lock);
      const removed = await graft(ctx, ['remove', 'native', '--lease', ws.lease, '--discard']);
      assert.equal(removed.ok, true, JSON.stringify(removed.json));
      const moved = join(ctx.root, 'elsewhere');
      // An editor recreates the old path after the archive. Restore keeps it.
      mkdirSync(ws.path);
      writeFileSync(join(ws.path, 'stray.txt'), 'editor recreated this\n');
      assert.equal(ctx.fail(['restore', 'native', '--lease', ws.lease]).code, 'already_exists');
      assert.equal(readFileSync(join(ws.path, 'stray.txt'), 'utf8'), 'editor recreated this\n');
      renameSync(ws.path, moved);
      assert.equal(readFileSync(join(restore(ctx, 'native', ws.lease), 'notes.txt'), 'utf8'), 'uncommitted\n');
      assert.equal(readFileSync(join(moved, 'stray.txt'), 'utf8'), 'editor recreated this\n');
    },
  },
  ...(['local', 'isolated'] as const).map((history): Scenario => ({
    name: `an add killed during its ${history}-history checkout keeps the primary intact and the graft recoverable`,
    async run(ctx) {
      // A global smudge filter holds the checkout that add starts.
      const gate = new Gate(ctx, 'add-' + history);
      const filter = join(ctx.root, 'hold-filter.sh');
      const finished = join(gate.dir, 'finished');
      writeFileSync(filter, ['#!/bin/sh', gate.shell(), 'cat', `printf done > '${finished.replaceAll('\\', '/')}'`, ''].join('\n'));
      chmodSync(filter, 0o755);
      writeFileSync(join(ctx.env.HOME!, 'gitconfig'), `[core]\n\tautocrlf = false\n[filter "hold"]\n\tsmudge = '${filter.replaceAll('\\', '/')}'\n\tclean = cat\n`);
      write(ctx.repo, '.gitattributes', '*.dat filter=hold\n');
      write(ctx.repo, 'payload.dat', 'filtered payload\n');
      const base = commit(ctx, ctx.repo, 'Filtered file');
      const index = readFileSync(join(ctx.repo, '.git', 'index'));
      const id = randomUUID();
      const args = ['--repo', ctx.repo, '--json', 'add', 'interrupted', '--history', history, '--request-id', id];
      gate.arm();
      const child = spawn(ctx.bin, args, { cwd: ctx.repo, env: ctx.env, stdio: 'ignore', windowsHide: true });
      const exited = new Promise(resolve => child.once('exit', resolve));
      try {
        await waitFor(() => existsSync(gate.reached) || child.exitCode !== null, 'the checkout filter');
        assert(existsSync(gate.reached), 'add finished before its checkout ran the filter');
        assert(child.kill('SIGKILL'), 'could not kill add');
        await exited;
      } finally {
        gate.open();
        if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
        await exited;
      }
      // The primary is untouched by the interrupted add.
      assert.equal(ctx.git(['rev-parse', 'HEAD']), base);
      assert.deepEqual(readFileSync(join(ctx.repo, '.git', 'index')), index);
      assert.equal(ctx.git(['status', '--porcelain']), '');
      assert.equal(readFileSync(join(ctx.repo, 'payload.dat'), 'utf8'), 'filtered payload\n');
      // The filter outlives the killed add; let it finish before retrying.
      await waitFor(() => existsSync(finished), 'the orphaned checkout filter');
      // Retrying the same request completes the graft; another request cannot take the name.
      assert.equal(ctx.fail(['add', 'interrupted', '--request-id', randomUUID()]).code, 'already_exists');
      const retried = await graft(ctx, ['add', 'interrupted', '--history', history, '--request-id', id]);
      let path: string;
      if (retried.ok) {
        path = String(retried.json.path);
      } else {
        // An unfinishable creation can be archived and restored with its lease.
        assert.equal(retried.json.code, 'spawn_incomplete', JSON.stringify(retried.json));
        ctx.cli(['remove', 'interrupted', '--lease', id, '--discard']);
        path = restore(ctx, 'interrupted', id);
      }
      assert.equal(ctx.git(['rev-parse', 'HEAD'], path), base);
      assert.equal(readFileSync(join(path, 'payload.dat'), 'utf8'), 'filtered payload\n');
      ctx.git(['fsck', '--full'], path);
      // Known gap: a local-history add reserves the graft's index with an
      // empty index.lock, and nothing reclaims it after a kill. It holds no
      // data, so deleting it, as Git's own message suggests, makes the graft
      // usable. Until recovery reclaims it, pin that it is Graft's empty file.
      const reservation = join(path, '.git', 'index.lock');
      if (existsSync(reservation)) {
        assert.equal(history, 'local', 'an isolated add left an index lock');
        assert.equal(readFileSync(reservation).length, 0);
        rmSync(reservation);
      }
      // The recovered graft is usable: Git can write its index and commit.
      write(path, 'after.txt', 'work after recovery\n');
      ctx.git(['add', 'after.txt'], path);
      ctx.git(['commit', '-qm', 'Work after recovery'], path);
    },
  })),
];
