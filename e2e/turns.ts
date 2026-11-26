/**
 * Turns on a shared resource: holders take it in the order they asked, a turn
 * runs out or ends with the holder's process or grafts, one turn serves
 * grafts of several repositories, a waiting take returns when the holder
 * gives the turn up, and MCP serves the same three operations with the MCP
 * session as the holder.
 */
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, readlinkSync, realpathSync, renameSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { objectResult, type Context, type ObjectResult, type Scenario } from './harness.ts';
import { call, discover, initialize, invalidCall, withClient } from './mcp.ts';
import { Gate, background, graft, running, waitFor } from './process-faults.ts';

type Holder = { name: string; lease: string };

function holder(name: string): Holder {
  return { name, lease: randomUUID() };
}
function take(ctx: Context, who: Holder, resource: string, ...options: string[]): ObjectResult {
  return ctx.cli(['turn', 'take', resource, '--holder', who.name, '--lease', who.lease, ...options]);
}
function takeIn(ctx: Context, repo: string, who: Holder, resource: string, ...options: string[]): ObjectResult {
  return ctx.cli(['turn', 'take', resource, '--holder', who.name, '--lease', who.lease, ...options], repo);
}
function release(ctx: Context, who: Holder, resource: string, repo = ctx.repo): ObjectResult {
  return ctx.cli(['turn', 'release', resource, '--holder', who.name, '--lease', who.lease], repo);
}
/** Each listed resource as [resource, holder, queue]. */
function turns(ctx: Context, repo = ctx.repo): unknown[][] {
  const listed = ctx.cli(['turn', 'list'], repo).turns;
  assert(Array.isArray(listed));
  return listed.map(objectResult).map(turn => [turn.resource, turn.holder, turn.queue]);
}
function stateFile(repo: string, resource: string): string {
  return join(repo, '.git', 'graft', 'turns', `turn-${resource}.json`);
}
function number(value: unknown): number {
  assert(typeof value === 'number');
  return value;
}
/** A child process that runs until killed. */
function sleeper(): ReturnType<typeof spawn> {
  return spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], { stdio: 'ignore', windowsHide: true });
}
/** A second repository beside the primary, with one commit. */
function sibling(ctx: Context, name: string): string {
  const repo = join(ctx.root, name);
  mkdirSync(repo);
  ctx.git(['init', '-q', '-b', 'main'], repo);
  writeFileSync(join(repo, 'README.md'), `${name}\n`);
  ctx.git(['add', '-A'], repo);
  ctx.git(['commit', '-qm', name], repo);
  return repo;
}

export const cases: Scenario[] = [
  {
    name: 'turn list leaves fresh and foreign project state untouched',
    run(ctx) {
      const state = join(ctx.repo, '.git', 'graft');
      assert.deepEqual(ctx.cli(['turn', 'list']), { turns: [] });
      assert(!existsSync(state));
      mkdirSync(state);
      const foreign = join(state, 'foreign-note');
      writeFileSync(foreign, 'retain foreign state');
      ctx.fail(['turn', 'list']);
      assert.equal(readFileSync(foreign, 'utf8'), 'retain foreign state');
      assert.deepEqual(readdirSync(state), ['foreign-note']);
    },
  },
  {
    name: 'holders take a turn in the order they asked and hand it over on release',
    run(ctx) {
      const [a, b, c] = ['task-a', 'task-b', 'task-c'].map(holder);
      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: false, next: null });
      assert(!existsSync(join(ctx.repo, '.git', 'graft', 'turns')));

      const first = take(ctx, a, 'web');
      const since = number(first.since);
      assert.deepEqual(first,
        { resource: 'web', granted: true, holder: 'task-a', since, expires_at: since + 1800, grafts: [], queue: [] });
      const held = { resource: 'web', holder: 'task-a', since, expires_at: since + 1800, grafts: [] };
      assert.deepEqual(take(ctx, b, 'web'), { ...held, granted: false, position: 1, queue: ['task-b'] });
      assert.deepEqual(take(ctx, c, 'web'), { ...held, granted: false, position: 2, queue: ['task-b', 'task-c'] });
      // Asking again keeps the place, and another resource has its own turn.
      assert.equal(take(ctx, b, 'web').position, 1);
      const database = take(ctx, b, 'database');
      assert.equal(database.granted, true);
      const listing = ctx.cli(['turn', 'list']);
      assert.deepEqual(listing, { turns: [
        { resource: 'database', holder: 'task-b', since: database.since, expires_at: database.expires_at, grafts: [], queue: [] },
        { ...held, queue: ['task-b', 'task-c'] },
      ] });
      // No lease anywhere: not in what a command prints, nor in the state.
      for (const { lease } of [a, b, c]) {
        assert(!JSON.stringify(listing).includes(lease));
        assert(!readFileSync(stateFile(ctx.repo, 'web'), 'utf8').includes(lease));
      }

      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: true, next: 'task-b' });
      // The turn is free, but task-b asked first and gets it when it next asks.
      assert.deepEqual(take(ctx, c, 'web'), { resource: 'web', granted: false, holder: null, since: null,
        expires_at: null, grafts: [], position: 2, queue: ['task-b', 'task-c'] });
      const handed = take(ctx, b, 'web');
      assert.deepEqual([handed.granted, handed.holder, handed.queue], [true, 'task-b', ['task-c']]);
      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: false, next: 'task-c' });
      assert.deepEqual(release(ctx, b, 'web'), { resource: 'web', released: true, next: 'task-c' });
      assert.equal(take(ctx, c, 'web').granted, true);
      // A waiter can leave the queue without ever holding the turn.
      assert.equal(take(ctx, a, 'web').position, 1);
      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: true, next: null });
      assert.deepEqual(release(ctx, c, 'web'), { resource: 'web', released: true, next: null });
      assert.deepEqual(turns(ctx), [['database', 'task-b', []]]);
    },
  },
  {
    name: 'a wrong lease, an invalid holder or resource, or an out-of-range option takes no turn and changes nothing',
    run(ctx) {
      const [a, b] = ['task-a', 'task-b'].map(holder);
      assert.equal(take(ctx, a, 'web').granted, true);
      assert.equal(take(ctx, b, 'web').position, 1);
      const stored = readFileSync(stateFile(ctx.repo, 'web'), 'utf8');
      const listed = ctx.cli(['turn', 'list']);

      for (const [action, name, lease] of [
        ['take', 'task-a', randomUUID()], ['take', 'task-a', b.lease],
        ['release', 'task-a', b.lease], ['release', 'task-b', randomUUID()],
      ]) {
        const error = ctx.fail(['turn', action, 'web', '--holder', name, '--lease', lease]);
        assert.equal(error.code, 'lease_mismatch', action);
        assert.deepEqual(error.details, { holder: name });
        assert(!JSON.stringify(error).includes(a.lease) && !JSON.stringify(error).includes(b.lease));
      }
      for (const [name, lease] of [['two words', a.lease], ['-dash', a.lease], ['task-a', 'not-a-uuid'],
        ['task-a', '00000000-0000-0000-0000-000000000000']]) {
        for (const action of ['take', 'release']) {
          assert.equal(ctx.fail(['turn', action, 'web', `--holder=${name}`, `--lease=${lease}`]).code, 'invalid_holder', name);
        }
      }
      for (const resource of ['Web', 'web_server', 'web/api', 'web server', 'a'.repeat(65)]) {
        for (const action of ['take', 'release']) {
          assert.equal(ctx.fail(['turn', action, resource, '--holder', 'task-a', '--lease', a.lease]).code,
            'invalid_resource', resource);
        }
      }
      // A process that is not running, or a path that is no graft, cannot bound a holder.
      assert.equal(ctx.fail(['turn', 'take', 'web', '--holder', 'task-c', '--lease', randomUUID(), '--pid', '4294967295']).code,
        'invalid_holder');
      assert.equal(ctx.fail(['turn', 'take', 'web', '--holder', 'task-c', '--lease', randomUUID(), '--graft', ctx.repo]).code,
        'invalid_holder');
      // The command line refuses these before anything runs.
      for (const options of [['--wait', '3601'], ['--ttl', '0'], ['--ttl', '86401'], ['--wait', '-1']]) {
        const out = ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', 'turn', 'take', 'web', '--holder', 'task-a',
          '--lease', a.lease, ...options]);
        assert.equal(out.status, 2, options.join(' '));
      }
      for (const missing of [['--holder', 'task-a'], ['--lease', a.lease]]) {
        assert.equal(ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', 'turn', 'take', 'web', ...missing]).status, 2);
      }
      assert.equal(readFileSync(stateFile(ctx.repo, 'web'), 'utf8'), stored);
      assert.deepEqual(ctx.cli(['turn', 'list']), listed);
    },
  },
  {
    name: 'a turn runs out after its ttl unless it is taken again',
    async run(ctx) {
      const [a, b] = ['task-a', 'task-b'].map(holder);
      const short = take(ctx, a, 'web', '--ttl', '1');
      assert.equal(short.granted, true);
      assert.equal(short.expires_at, number(short.since) + 1);
      await delay(2_100);
      assert.deepEqual(turns(ctx), []);
      const next = take(ctx, b, 'web', '--ttl', '600');
      assert.deepEqual([next.granted, next.holder, next.expires_at], [true, 'task-b', number(next.since) + 600]);
      // The earlier holder lost the turn and queues like anyone else.
      assert.deepEqual([take(ctx, a, 'web').granted, take(ctx, a, 'web').position], [false, 1]);

      // Taking it again renews it from now and keeps its ttl; a new ttl replaces it.
      await delay(1_100);
      const renewed = take(ctx, b, 'web');
      assert.deepEqual([renewed.granted, renewed.since], [true, next.since]);
      assert(number(renewed.expires_at) > number(next.expires_at));
      const shorter = take(ctx, b, 'web', '--ttl', '5');
      assert.equal(shorter.since, next.since);
      assert(number(shorter.expires_at) < number(renewed.expires_at));
    },
  },
  {
    name: 'a holder loses its turn and its place when the process it named ends',
    async run(ctx) {
      const [a, b, c] = ['task-a', 'task-b', 'task-c'].map(holder);
      const first = sleeper();
      const second = sleeper();
      try {
        assert.equal(take(ctx, a, 'web', '--pid', String(first.pid)).granted, true);
        assert.equal(take(ctx, b, 'web', '--pid', String(second.pid)).position, 1);
        assert.equal(take(ctx, c, 'web').position, 2);
        // A renewal that names no process keeps the one named before.
        assert.equal(take(ctx, a, 'web').granted, true);
        first.kill();
        await waitFor(() => !running(first.pid!), 'the first process to exit');
        second.kill();
        await waitFor(() => !running(second.pid!), 'the second process to exit');
        assert.deepEqual(turns(ctx), [['web', null, ['task-c']]]);
        assert.equal(take(ctx, c, 'web').granted, true);
        // Its name is free again, for any lease.
        assert.equal(take(ctx, holder('task-a'), 'web').position, 1);
      } finally {
        first.kill();
        second.kill();
      }
    },
  },
  {
    name: 'one holder holds one turn for grafts of several repositories, until none of them is active',
    run(ctx) {
      // The primary and a second repository side by side in one directory,
      // which is itself a repository and shares the resource stack.
      const router = sibling(ctx, 'router');
      ctx.git(['init', '-q', '-b', 'main'], ctx.root);
      writeFileSync(join(ctx.root, '.graftconfig.local'), JSON.stringify({ turns: ['stack'] }));
      // A configuration of its own does not take a repository out of the queue.
      writeFileSync(join(router, '.graftconfig.local'), JSON.stringify({ include: [] }));
      const app = ctx.spawn('fix-parser');
      const api = ctx.spawn('fix-parser', [], router);
      const other = ctx.spawn('other', [], router);
      const agent = holder('agent-1');
      const [appPath, apiPath] = [String(app.path), String(api.path)].map(path => realpathSync(path));

      const granted = takeIn(ctx, ctx.repo, agent, 'stack', '--graft', appPath, '--graft', apiPath);
      assert.deepEqual([granted.granted, granted.holder, granted.grafts], [true, 'agent-1', [appPath, apiPath]]);
      assert(existsSync(stateFile(ctx.root, 'stack')));
      assert(!existsSync(stateFile(ctx.repo, 'stack')) && !existsSync(stateFile(router, 'stack')));
      // The same turn from the other repository: the same holder renews it,
      // anyone else queues behind it.
      assert.equal(takeIn(ctx, router, agent, 'stack').granted, true);
      const second = holder('agent-2');
      assert.equal(takeIn(ctx, router, second, 'stack', '--graft', String(other.path)).position, 1);
      assert.deepEqual(turns(ctx, router), [['stack', 'agent-1', ['agent-2']]]);
      assert.deepEqual(turns(ctx), turns(ctx, router));
      // A resource the directory does not share stays each repository's own.
      assert.equal(takeIn(ctx, router, second, 'web').granted, true);
      assert.deepEqual(turns(ctx), [['stack', 'agent-1', ['agent-2']]]);

      // Archiving one graft leaves the turn; archiving the last frees it.
      ctx.cli(['remove', 'fix-parser', '--lease', app.lease]);
      assert.deepEqual(turns(ctx), [['stack', 'agent-1', ['agent-2']]]);
      ctx.cli(['remove', 'fix-parser', '--lease', api.lease], router);
      assert.deepEqual(turns(ctx), [['stack', null, ['agent-2']]]);
      assert.equal(takeIn(ctx, router, second, 'stack').granted, true);
      assert.deepEqual(release(ctx, second, 'stack'), { resource: 'stack', released: true, next: null });
    },
  },
  {
    name: 'links follow the holder of a shared resource to its grafts, and back to the primary when a graft is archived',
    run: async ctx => {
      if (process.platform === 'win32') ctx.unavailable('creating a link on Windows needs Developer Mode or an elevated prompt');
      const router = sibling(ctx, 'router');
      ctx.git(['init', '-q', '-b', 'main'], ctx.root);
      writeFileSync(join(ctx.root, '.graftconfig.local'), JSON.stringify({
        turns: ['stack'],
        links: { stack: { project: '.graft/stack/project', router: '.graft/stack/router' } },
        repositories: { project: {}, router: {} },
      }));
      const [appLink, routerLink] = ['project', 'router'].map(name => join(ctx.root, '.graft', 'stack', name));
      const [primary, routerPrimary] = [ctx.repo, router].map(path => realpathSync(path));
      const app = ctx.spawn('fix');
      const appPath = realpathSync(String(app.path));
      const agent = holder('agent-1');

      // The holder has a graft of the primary only.
      assert.equal(takeIn(ctx, router, agent, 'stack', '--graft', appPath).granted, true);
      assert.deepEqual([readlinkSync(appLink), readlinkSync(routerLink)], [appPath, routerPrimary]);
      // A queued holder moves no link.
      const second = holder('agent-2');
      assert.equal(takeIn(ctx, router, second, 'stack').granted, false);
      assert.equal(readlinkSync(appLink), appPath);

      // Merging the graft archives it and points its link at the primary.
      writeFileSync(join(appPath, 'fix.txt'), 'fix\n');
      ctx.git(['add', 'fix.txt'], appPath);
      ctx.git(['commit', '-qm', 'fix'], appPath);
      assert.equal(ctx.cli(['merge', 'fix', '--lease', app.lease]).removed, true);
      assert.equal(readlinkSync(appLink), primary);

      // Release leaves the links; anything but a link at a link's path
      // refuses the take and changes nothing.
      release(ctx, agent, 'stack');
      renameSync(appLink, join(ctx.root, '.graft', 'stack', 'aside'));
      mkdirSync(appLink);
      const refused = ctx.fail(['turn', 'take', 'stack', '--holder', second.name, '--lease', second.lease], router);
      assert.equal(refused.code, 'invalid_config');
      assert.equal(objectResult(refused.details).path, appLink);
      assert.deepEqual(turns(ctx), [['stack', null, ['agent-2']]]);
      release(ctx, second, 'stack');

      // An MCP host without --allow-exec still points the links.
      renameSync(appLink, join(ctx.root, '.graft', 'stack', 'emptied'));
      const work = ctx.spawn('work');
      const workPath = realpathSync(String(work.path));
      await withClient(ctx, false, async client => {
        await initialize(client);
        const taken = await call(client, 'graft_turn_take', { holder: 'mcp', resource: 'stack', grafts: [workPath] });
        assert.equal(taken.granted, true);
      });
      assert.deepEqual([readlinkSync(appLink), readlinkSync(routerLink)], [workPath, routerPrimary]);
    },
  },
  {
    name: 'post-take hooks run when a turn is newly granted or its grafts change, and a failing one gives the turn up',
    run: async ctx => {
      const log = join(ctx.root, 'post-take.log');
      const script = join(ctx.root, 'post-take.cjs');
      writeFileSync(script, `
const fs = require('node:fs');
const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => key.startsWith('GIT_GRAFT_')));
fs.appendFileSync(process.argv[2], JSON.stringify({ cwd: process.cwd(), env }) + '\\n');
process.stdout.write('switched to ' + process.env.GIT_GRAFT_TURN_HOLDER);
if (process.env.GIT_GRAFT_TURN_HOLDER === 'broken') process.exit(4);
`);
      writeFileSync(join(ctx.repo, '.graftconfig.local'), JSON.stringify({
        hooks: { switch: { event: 'post-take', argv: [process.execPath, script, log] } },
      }));
      const runs = () => existsSync(log)
        ? readFileSync(log, 'utf8').trim().split('\n').map(line => objectResult(JSON.parse(line)))
        : [];
      const work = ctx.spawn('work');
      const path = realpathSync(String(work.path));
      const [a, b] = ['task-a', 'task-b'].map(holder);
      const ran = (who: string) => [{ name: 'switch', event: 'post-take', file: '.graftconfig.local', status: 'ok',
        exit_code: 0, stdout: `switched to ${who}`, stderr: '', stdout_truncated: false, stderr_truncated: false }];

      const granted = take(ctx, a, 'web', '--graft', path);
      assert.deepEqual(granted.hooks, ran('task-a'));
      const [first] = runs();
      assert.equal(realpathSync(String(first.cwd)), realpathSync(ctx.repo));
      const env = objectResult(first.env);
      assert.deepEqual([env.GIT_GRAFT_HOOK, env.GIT_GRAFT_HOOK_NAME, env.GIT_GRAFT_TURN_RESOURCE,
        env.GIT_GRAFT_TURN_HOLDER, env.GIT_GRAFT_TURN_GRAFTS], ['post-take', 'switch', 'web', 'task-a', path]);
      assert(!JSON.stringify(env).includes(a.lease), 'the hook saw the holder lease');
      // A renewal or a queued ask runs nothing; new grafts run it again.
      assert(!('hooks' in take(ctx, a, 'web')));
      assert(!('hooks' in take(ctx, b, 'web')));
      assert.equal(runs().length, 1);
      const other = ctx.spawn('other');
      assert.deepEqual(take(ctx, a, 'web', '--graft', String(other.path)).hooks, ran('task-a'));
      release(ctx, a, 'web');
      assert.deepEqual(take(ctx, b, 'web').hooks, ran('task-b'));
      release(ctx, b, 'web');
      assert.equal(runs().length, 3);

      // A failing hook refuses the take and leaves the turn free.
      const broken = holder('broken');
      const error = ctx.fail(['turn', 'take', 'web', '--holder', broken.name, '--lease', broken.lease]);
      assert.equal(error.code, 'hook_failed');
      const details = objectResult(error.details);
      assert.deepEqual([details.resource, details.holder], ['web', 'broken']);
      assert.equal(objectResult((details.hooks as unknown[])[0]).exit_code, 4);
      assert.deepEqual(turns(ctx), []);

      // Without --allow-exec an MCP host takes the turn and runs no hook.
      await withClient(ctx, false, async client => {
        await initialize(client);
        const quiet = await call(client, 'graft_turn_take', { holder: 'quiet', resource: 'web' });
        assert.equal(quiet.granted, true);
        assert.deepEqual((quiet.hooks as unknown[]).map(entry => objectResult(entry).status), ['skipped']);
      });
      assert.equal(runs().length, 4);
    },
  },
  {
    name: 'take --wait returns soon after the holder gives up the turn and holds no lock meanwhile',
    async run(ctx) {
      const [a, b] = ['task-a', 'task-b'].map(holder);
      assert.equal(take(ctx, a, 'web').granted, true);
      let started = performance.now();
      const brief = take(ctx, b, 'web', '--wait', '1');
      assert.deepEqual([brief.granted, brief.position], [false, 1]);
      assert(performance.now() - started >= 900, 'a wait that was not granted returned early');

      let settled = false;
      const waiting = graft(ctx, ['turn', 'take', 'web', '--holder', b.name, '--lease', b.lease, '--wait', '20'])
        .finally(() => { settled = true; });
      await delay(1_500);
      assert(!settled, 'the waiting take returned while the turn was held');
      // Other commands, the holder's renewal included, are not kept waiting.
      started = performance.now();
      ctx.cli(['list']);
      assert.equal(take(ctx, a, 'web').granted, true);
      assert.deepEqual(turns(ctx), [['web', 'task-a', ['task-b']]]);
      assert(performance.now() - started < 5_000, 'a waiting take held up other commands');

      started = performance.now();
      assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: true, next: 'task-b' });
      const outcome = await waiting;
      assert(outcome.ok, JSON.stringify(outcome.json));
      assert.deepEqual([outcome.json.granted, outcome.json.holder, outcome.json.queue], [true, 'task-b', []]);
      assert(performance.now() - started < 10_000, 'the waiting take returned long after the release');
    },
  },
  {
    name: 'a holder takes and gives up turns while a process its graft started with exec runs',
    async run(ctx) {
      const work = ctx.spawn('agent');
      const [a, b] = ['agent', 'task-b'].map(holder);
      const started = join(ctx.root, 'background-started');
      const stop = join(ctx.root, 'background-release');
      // The usual order: take the turn, then start the shared thing.
      assert.equal(take(ctx, a, 'web', '--graft', String(work.path)).granted, true);
      const exec = ctx.cli(['exec', 'agent', '--lease', work.lease, '--', ...background(ctx, started, stop)]);
      assert.equal(exec.success, true, JSON.stringify(exec));
      let pid = 0;
      try {
        await waitFor(() => existsSync(started) && readFileSync(started, 'utf8').length > 0, 'the background process');
        pid = Number(readFileSync(started, 'utf8'));
        assert(running(pid), 'the background process exited early');
        assert.equal(ctx.fail(['lock', 'agent', '--lease', work.lease]).code, 'workspace_busy');
        assert.equal(take(ctx, a, 'web').granted, true);
        assert.equal(take(ctx, b, 'web').position, 1);
        assert.deepEqual(release(ctx, a, 'web'), { resource: 'web', released: true, next: 'task-b' });
        assert.equal(take(ctx, a, 'web').position, 2);
      } finally {
        writeFileSync(stop, 'release\n');
        if (pid) await waitFor(() => !running(pid), 'the background process to exit');
      }
    },
  },
  {
    name: 'turn commands do not wait for a graft that is being built',
    async run(ctx) {
      if (process.platform === 'win32') ctx.unavailable('the fault hooks are POSIX shell scripts');
      const [a, b] = ['task-a', 'task-b'].map(holder);
      // A smudge filter holds the next add part way through its checkout.
      const gate = new Gate(ctx, 'build');
      const filter = join(ctx.root, 'hold-filter.sh');
      writeFileSync(filter, ['#!/bin/sh', gate.shell(), 'exec cat', ''].join('\n'), { mode: 0o755 });
      writeFileSync(ctx.env.GIT_CONFIG_GLOBAL!, `[filter "hold"]\n\tsmudge = '${filter}'\n\tclean = cat\n`, { flag: 'a' });
      writeFileSync(join(ctx.repo, '.gitattributes'), '*.dat filter=hold\n');
      writeFileSync(join(ctx.repo, 'payload.dat'), 'filtered payload\n');
      ctx.git(['add', '-A']);
      ctx.git(['commit', '-qm', 'A filtered file']);
      gate.arm();
      let built = false;
      const held = graft(ctx, ['add', 'held']).finally(() => { built = true; });
      try {
        await waitFor(() => existsSync(gate.reached) || built, 'the held checkout');
        assert(existsSync(gate.reached), 'the add finished before its checkout');
        // Run beside the add, so its exit is noticed if a command waited for it.
        const turn = async (...args: string[]) => {
          const outcome = await graft(ctx, ['turn', ...args]);
          assert(!built, `turn ${args[0]} waited for the build`);
          assert(outcome.ok, JSON.stringify(outcome.json));
          return outcome.json;
        };
        assert.equal((await turn('take', 'web', '--holder', a.name, '--lease', a.lease)).granted, true);
        assert.equal((await turn('take', 'web', '--holder', b.name, '--lease', b.lease)).position, 1);
        const listed = (await turn('list')).turns;
        assert(Array.isArray(listed) && listed.length === 1);
        assert.deepEqual(objectResult(listed[0]).queue, ['task-b']);
        assert.deepEqual(await turn('release', 'web', '--holder', a.name, '--lease', a.lease),
          { resource: 'web', released: true, next: 'task-b' });
      } finally {
        gate.open();
        assert((await held).ok);
      }
    },
  },
  {
    name: 'MCP serves the turn tools without --allow-exec, with the session as the holder',
    run: async ctx => {
      const outsider = holder('outsider');
      await withClient(ctx, false, async client => {
        await initialize(client);
        const catalog = await discover(client, false);
        ctx.measure('advertised_tools', [...catalog.keys()].sort());
        const schema = objectResult(catalog.get('graft_turn_take')?.inputSchema);
        assert(Array.isArray(schema.required));
        assert.deepEqual([...schema.required].sort(), ['holder', 'resource']);
        assert.deepEqual(Object.keys(objectResult(schema.properties)).sort(), ['grafts', 'holder', 'resource', 'ttl', 'wait']);
        const releaseSchema = objectResult(catalog.get('graft_turn_release')?.inputSchema);
        assert.deepEqual(Object.keys(objectResult(releaseSchema.properties)).sort(), ['holder', 'resource']);
        assert.equal(objectResult(catalog.get('graft_turn_list')?.annotations).readOnlyHint, true);
        for (const name of ['graft_turn_take', 'graft_turn_release']) {
          const annotations = objectResult(catalog.get(name)?.annotations);
          assert.deepEqual([annotations.readOnlyHint, annotations.destructiveHint], [false, false], name);
        }

        assert.deepEqual(await call(client, 'graft_turn_list'), { turns: [] });
        assert(!existsSync(join(ctx.repo, '.git', 'graft')));
        const work = ctx.spawn('task-a');
        const mine = (name: string) => ({ holder: name, resource: 'web' });
        // A graft named relative to the repository resolves there.
        const relative = join('..', 'project.graft', 'task-a');
        const first = await call(client, 'graft_turn_take', { ...mine('task-a'), grafts: [relative], ttl: 600 });
        const since = number(first.since);
        assert.deepEqual(first, { resource: 'web', granted: true, holder: 'task-a', since, expires_at: since + 600,
          grafts: [realpathSync(String(work.path))], queue: [] });
        const queued = await call(client, 'graft_turn_take', mine('task-b'));
        assert.deepEqual([queued.granted, queued.position, queued.queue], [false, 1, ['task-b']]);
        // Outside the session, the names are taken: the session's lease is its own.
        for (const action of ['take', 'release']) {
          const refused = ctx.fail(['turn', action, 'web', '--holder', 'task-a', '--lease', randomUUID()]);
          assert.equal(refused.code, 'lease_mismatch', action);
        }
        assert.equal(take(ctx, outsider, 'web').position, 2);
        const listed = await call(client, 'graft_turn_list');
        assert.deepEqual(listed, ctx.cli(['turn', 'list']));
        assert.deepEqual(turns(ctx), [['web', 'task-a', ['task-b', 'outsider']]]);
        ctx.coverTool('graft_turn_list');

        await invalidCall(client, 'graft_turn_take', { resource: 'web' });
        await invalidCall(client, 'graft_turn_take', { holder: 'task-a' });
        await invalidCall(client, 'graft_turn_take', { ...mine('task-a'), lease: randomUUID() });
        await invalidCall(client, 'graft_turn_take', { ...mine('task-a'), pid: 1 });
        await invalidCall(client, 'graft_turn_take', { ...mine('task-a'), wait: 3601 });
        await invalidCall(client, 'graft_turn_take', { ...mine('task-a'), ttl: 0 });
        await invalidCall(client, 'graft_turn_take', { ...mine('task-a'), ttl: 86401 });
        await invalidCall(client, 'graft_turn_release', { holder: 'task-a' });
        await invalidCall(client, 'graft_turn_list', { resource: 'web' });
        for (const tool of ['graft_turn_take', 'graft_turn_release']) {
          assert.equal((await call(client, tool, mine('outsider'), true)).code, 'lease_mismatch', tool);
          assert.equal((await call(client, tool, { ...mine('task-a'), resource: 'Web' }, true)).code, 'invalid_resource', tool);
          assert.equal((await call(client, tool, mine('two words'), true)).code, 'invalid_holder', tool);
        }
        assert.deepEqual(await call(client, 'graft_turn_list'), listed);
        assert.deepEqual(release(ctx, outsider, 'web'), { resource: 'web', released: true, next: 'task-b' });

        // A wait that is not granted returns when its time is up.
        let started = performance.now();
        const waited = await call(client, 'graft_turn_take', { ...mine('task-b'), wait: 2 });
        assert.deepEqual([waited.granted, waited.position], [false, 1]);
        assert(performance.now() - started >= 1_900, 'a wait that was not granted returned early');

        // A wait the client cancels stops asking, so it takes no turn that
        // nobody would use. The server sends no reply to a cancelled request.
        const id = 7_000;
        client.send(JSON.stringify({ jsonrpc: '2.0', id, method: 'tools/call',
          params: { name: 'graft_turn_take', arguments: { ...mine('task-b'), wait: 20 } } }));
        await delay(1_500);
        client.send(JSON.stringify({ jsonrpc: '2.0', method: 'notifications/cancelled', params: { requestId: id, reason: 'test' } }));
        await delay(1_200);
        assert.deepEqual(await call(client, 'graft_turn_release', mine('task-a')), { resource: 'web', released: true, next: 'task-b' });
        await delay(2_200);
        assert.deepEqual(turns(ctx), [['web', null, ['task-b']]], 'a cancelled wait kept asking');

        // A wait still in progress is granted soon after the holder releases.
        assert.equal((await call(client, 'graft_turn_take', mine('task-a'))).position, 2);
        const handed = await call(client, 'graft_turn_take', mine('task-b'));
        assert.deepEqual([handed.granted, handed.holder, handed.queue], [true, 'task-b', ['task-a']]);
        const waiting = call(client, 'graft_turn_take', { ...mine('task-a'), wait: 20 });
        await delay(300);
        started = performance.now();
        assert.deepEqual(await call(client, 'graft_turn_release', mine('task-b')), { resource: 'web', released: true, next: 'task-a' });
        const granted = await waiting;
        assert.deepEqual([granted.granted, granted.holder, granted.queue], [true, 'task-a', []]);
        assert(performance.now() - started < 10_000, 'the waiting call returned long after the release');
        ctx.coverTool('graft_turn_take');

        assert.deepEqual(await call(client, 'graft_turn_release', mine('task-b')), { resource: 'web', released: false, next: null });
        assert.equal(take(ctx, outsider, 'web').position, 1);
        ctx.coverTool('graft_turn_release');
      });
      // The session's turns end with it; the outsider's place does not.
      await waitFor(() => JSON.stringify(turns(ctx)) === JSON.stringify([['web', null, ['outsider']]]),
        'the closed session to lose its turns');
      assert.equal(take(ctx, outsider, 'web').granted, true);
    },
  },
];
