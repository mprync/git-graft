import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { type Context, type Scenario } from './harness.ts';
import { call, discover, initialize, withClient } from './mcp.ts';

type Graft = ReturnType<Context['spawn']>;

function object(value: unknown): Record<string, unknown> {
  assert.ok(value && typeof value === 'object' && !Array.isArray(value));
  return value as Record<string, unknown>;
}

/** A bare repository beside the primary, configured there as the remote `name`. */
function remote(ctx: Context, name = 'origin'): string {
  const bare = join(ctx.root, name + '.git');
  ctx.git(['init', '-q', '--bare', bare], ctx.root);
  ctx.git(['remote', 'add', name, bare]);
  return bare;
}

function commit(ctx: Context, path: string, file: string, content: string): string {
  writeFileSync(join(path, file), content);
  ctx.git(['add', '--', file], path);
  ctx.git(['commit', '-qm', file], path);
  return ctx.git(['rev-parse', 'HEAD'], path);
}

function push(name: string, graft: { lease: string }, ...options: string[]): string[] {
  return ['push', name, '--lease', graft.lease, ...options];
}

function refs(ctx: Context, path: string, ...patterns: string[]): string {
  return ctx.git(['for-each-ref', '--format=%(refname) %(objectname)', ...patterns], path);
}

/** HEAD, the index, every file status and every ref of a checkout except its remote-tracking refs. */
function state(ctx: Context, path: string): string {
  return [
    ctx.git(['symbolic-ref', '--quiet', 'HEAD'], path),
    ctx.git(['rev-parse', 'HEAD'], path),
    refs(ctx, path).split('\n').filter(line => !line.startsWith('refs/remotes/')).join('\n'),
    ctx.git(['ls-files', '--stage'], path),
    ctx.git(['status', '--porcelain=v1', '--ignored=matching'], path),
  ].join('\n--\n');
}

/** The private refs Graft keeps in the primary. A finished push leaves none. */
function privateRefs(ctx: Context): string {
  return ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft']);
}

/** Run a command and return its parsed JSON with everything it printed. */
function output(ctx: Context, args: string[]) {
  const out = ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', ...args]);
  assert.ifError(out.error);
  return { ok: out.status === 0, text: out.stdout + out.stderr, json: object(JSON.parse(out.stdout || out.stderr)) };
}

export const cases: Scenario[] = [
  {
    name: 'remotes in the configuration give a new graft the primary remote, its refs and plain fetch and push',
    run: async ctx => {
      const bare = remote(ctx);
      ctx.git(['push', '-q', 'origin', 'HEAD:refs/heads/main']);
      ctx.git(['fetch', '-q', 'origin']);
      ctx.git(['remote', 'set-head', 'origin', 'main']);
      ctx.git(['remote', 'add', 'secret', 'https://user:token@example.invalid/repo.git']);
      // A remote-tracking ref of upstream whose object the primary lacks
      // fails the copy of its refs after its keys are written.
      remote(ctx, 'upstream');
      const broken = join(ctx.repo, '.git', 'refs', 'remotes', 'upstream', 'broken');
      mkdirSync(join(broken, '..'), { recursive: true });
      writeFileSync(broken, '1'.repeat(40) + '\n');
      writeFileSync(join(ctx.repo, '.graftconfig.local'), JSON.stringify({ remotes: ['origin', 'upstream', 'secret', 'missing'] }));
      const added = ctx.spawn('remoted', ['--history', 'local']);
      const path = String(added.path);
      const entries = (added.result.remotes as unknown[]).map(object);
      assert.deepEqual(entries.map(entry => [entry.name, entry.status, entry.reason ?? entry.refs]).filter(([name]) => name !== 'upstream'),
        [['origin', 'configured', 2], ['secret', 'skipped', 'url_has_credentials'], ['missing', 'skipped', 'not_configured']]);
      assert.equal(entries[0].push, true);
      // The remote that failed is reported by name and leaves nothing behind.
      assert.equal(entries[1].name, 'upstream');
      assert.equal(entries[1].status, 'failed');
      assert.equal(typeof entries[1].reason, 'string');
      assert.equal(ctx.git(['remote'], path), 'origin');
      assert.equal(refs(ctx, path, 'refs/remotes/upstream'), '');
      assert.equal(ctx.git(['remote', 'get-url', 'origin'], path), bare);
      assert.equal(ctx.git(['rev-parse', 'origin/main'], path), ctx.git(['rev-parse', 'origin/main']));
      assert.equal(ctx.git(['symbolic-ref', 'refs/remotes/origin/HEAD'], path), 'refs/remotes/origin/main');
      // Plain Git works in the graft at once.
      const tip = commit(ctx, path, 'remoted.txt', 'remoted\n');
      ctx.git(['push', '-q', 'origin', 'HEAD:refs/heads/remoted'], path);
      assert.equal(ctx.git(['rev-parse', 'refs/heads/remoted'], bare), tip);
      ctx.git(['fetch', '-q', 'origin'], path);
      assert.equal(ctx.git(['rev-parse', 'origin/remoted'], path), tip);
      // When the upstream moves on, a fetch moves origin, not only origin/main.
      const other = join(ctx.root, 'other');
      ctx.git(['clone', '-q', '-b', 'main', bare, other], ctx.root);
      const moved = commit(ctx, other, 'moved.txt', 'moved\n');
      ctx.git(['push', '-q', 'origin', 'HEAD:refs/heads/main'], other);
      ctx.git(['fetch', '-q', 'origin'], path);
      assert.equal(ctx.git(['rev-parse', 'origin/main'], path), moved);
      assert.equal(ctx.git(['rev-parse', 'origin'], path), moved);

      // Restored, the graft keeps the remote it has and gets the one that failed.
      rmSync(broken);
      const archived = ctx.cli(['remove', 'remoted', '--lease', added.lease, '--discard']);
      const restored = ctx.cli(['restore', 'remoted', '--lease', String(archived.recovery_lease)]);
      assert.deepEqual((restored.remotes as unknown[]).slice(0, 2).map(object),
        [{ name: 'origin', status: 'kept', push: true }, { name: 'upstream', status: 'configured', refs: 0, push: true }]);
      writeFileSync(join(ctx.repo, '.graftconfig.local'), JSON.stringify({ remotes: ['origin'] }));

      // Isolated history gets the remote but no remote-tracking refs.
      const isolated = ctx.spawn('isolated', ['--history', 'isolated']);
      assert.deepEqual(object((isolated.result.remotes as unknown[])[0]), { name: 'origin', status: 'configured', refs: 0, push: true });
      assert.equal(ctx.git(['remote', 'get-url', 'origin'], String(isolated.path)), bare);

      // MCP adds get them too, without --allow-exec. Without --allow-push the
      // graft can fetch through them, but Git refuses to push.
      await withClient(ctx, false, async client => {
        await initialize(client);
        const viaMcp = await call(client, 'graft_add', { name: 'via-mcp' });
        const entry = object((viaMcp.remotes as unknown[])[0]);
        assert.equal(entry.status, 'configured');
        assert.equal(entry.push, false);
        const graft = String(viaMcp.path);
        assert.equal(ctx.git(['remote', 'get-url', 'origin'], graft), bare);
        ctx.git(['fetch', '-q', 'origin'], graft);
        assert.equal(ctx.git(['rev-parse', 'origin/main'], graft), moved);
        commit(ctx, graft, 'mcp.txt', 'mcp\n');
        for (const target of ['HEAD:refs/heads/via-mcp', 'HEAD:main']) {
          const pushed = ctx.command('git', ['push', 'origin', target], graft);
          assert.notEqual(pushed.status, 0, target);
          assert.match(pushed.stderr, /transport 'graft-push-disabled' not allowed/);
        }
        assert.equal(ctx.git(['for-each-ref', '--format=%(refname)', 'refs/heads'], bare), 'refs/heads/main\nrefs/heads/remoted');
        assert.equal(ctx.git(['rev-parse', 'refs/heads/main'], bare), moved);
      });
      // A host with --allow-push gives the graft a remote it can push through.
      await withClient(ctx, false, async client => {
        await initialize(client);
        const viaPush = await call(client, 'graft_add', { name: 'via-push' });
        assert.equal(object((viaPush.remotes as unknown[])[0]).push, true);
        const pushedTip = commit(ctx, String(viaPush.path), 'push.txt', 'push\n');
        ctx.git(['push', '-q', 'origin', 'HEAD:refs/heads/via-push'], String(viaPush.path));
        assert.equal(ctx.git(['rev-parse', 'refs/heads/via-push'], bare), pushedTip);
      }, true);
      // Without the setting, a graft has no remote, as before.
      writeFileSync(join(ctx.repo, '.graftconfig.local'), '{}');
      const plain = ctx.spawn('plain');
      assert(!('remotes' in plain.result));
      assert.equal(ctx.git(['remote'], String(plain.path)), '');
    },
  },
  {
    name: 'remotes a .graftconfig lists are skipped until trusted, apart from its hooks, and the person\'s own untracked file needs no trust',
    run: async ctx => {
      const bare = remote(ctx);
      ctx.git(['push', '-q', 'origin', 'HEAD:refs/heads/main']);
      remote(ctx, 'fork');
      const shared = (config: Record<string, unknown>) => writeFileSync(join(ctx.repo, '.graftconfig'), JSON.stringify(config));
      shared({ remotes: ['origin'] });
      ctx.git(['add', '.graftconfig']);
      ctx.git(['commit', '-qm', 'Name origin for grafts']);
      const skipped = (result: Record<string, unknown>) => (result.remotes as unknown[]).map(object)
        .filter(entry => entry.status === 'skipped').map(entry => {
          assert.equal(entry.reason, 'untrusted');
          assert.match(String(entry.hint), /git graft hook trust/);
          return entry.name;
        });
      // Untrusted, a remote is left out and the graft is made without it.
      const early = ctx.spawn('early');
      assert.deepEqual(skipped(early.result), ['origin']);
      assert.equal(ctx.git(['remote'], String(early.path)), '');
      await withClient(ctx, false, async client => {
        await initialize(client);
        const viaMcp = await call(client, 'graft_add', { name: 'agent' });
        assert.deepEqual(skipped(viaMcp), ['origin']);
      });
      assert.deepEqual(ctx.cli(['hook', 'list']).remotes, [{ name: 'origin', trusted: false }]);

      const trusted = ctx.cli(['hook', 'trust']);
      assert.deepEqual(trusted.trusted, []);
      assert.deepEqual(trusted.trusted_remotes, ['origin']);
      assert.match(String(trusted.remotes_sha256), /^[0-9a-f]{64}$/);
      assert.deepEqual(ctx.cli(['hook', 'list']).remotes, [{ name: 'origin', trusted: true }]);
      const one = ctx.spawn('one');
      assert.deepEqual(object((one.result.remotes as unknown[])[0]), { name: 'origin', status: 'configured', refs: 1, push: true });
      assert.equal(ctx.git(['remote', 'get-url', 'origin'], String(one.path)), bare);

      // A trusted hook and a changed remotes list: the hook still runs, and
      // every remote the list names is skipped until trusted again.
      const hooks = { check: { event: 'post-add', argv: ['git', '--version'] } };
      shared({ hooks, remotes: ['origin'] });
      const both = ctx.cli(['hook', 'trust']);
      assert.equal(object(both).remotes_sha256, trusted.remotes_sha256);
      const archived = ctx.cli(['remove', 'one', '--lease', one.lease, '--discard']);
      shared({ hooks, remotes: ['origin', 'fork'] });
      const two = ctx.spawn('two');
      assert.deepEqual((two.result.hooks as unknown[]).map(object).map(entry => entry.status), ['ok']);
      assert.deepEqual(skipped(two.result), ['origin', 'fork']);
      // A restore keeps the remote the graft has and leaves out the rest, and
      // a restore of the graft once active again changes nothing.
      const restored = ctx.cli(['restore', 'one', '--lease', String(archived.recovery_lease)]);
      assert.deepEqual(object((restored.remotes as unknown[])[0]), { name: 'origin', status: 'kept', push: true });
      assert.deepEqual(skipped(restored), ['fork']);
      const again = ctx.cli(['restore', 'one', '--lease', String(archived.recovery_lease)]);
      assert(!('remotes' in again));

      // Restored over MCP without --allow-push, a graft made on the command
      // line keeps its remote, but Git no longer pushes through it.
      shared({ hooks, remotes: ['origin'] });
      ctx.git(['push', '-q', 'origin', 'HEAD:refs/heads/from-cli'], String(one.path));
      const archivedAgain = ctx.cli(['remove', 'one', '--lease', String(restored.lease), '--discard']);
      await withClient(ctx, false, async client => {
        await initialize(client);
        const viaMcp = await call(client, 'graft_restore', { name: 'one', lease: String(archivedAgain.recovery_lease) });
        assert.deepEqual(object((viaMcp.remotes as unknown[])[0]), { name: 'origin', status: 'kept', push: false });
        const pushed = ctx.command('git', ['push', 'origin', 'HEAD:refs/heads/from-mcp'], String(viaMcp.path));
        assert.notEqual(pushed.status, 0);
        assert.match(pushed.stderr, /transport 'graft-push-disabled' not allowed/);
        ctx.git(['fetch', '-q', 'origin'], String(viaMcp.path));
      });
      assert.equal(ctx.git(['for-each-ref', '--format=%(refname)', 'refs/heads'], bare), 'refs/heads/from-cli\nrefs/heads/main');

      // What the person's own untracked .graftconfig.local lists needs no trust.
      writeFileSync(join(ctx.repo, '.graftconfig.local'), JSON.stringify({ remotes: ['fork'] }));
      const mine = ctx.spawn('mine');
      assert.deepEqual((mine.result.remotes as unknown[]).map(object).map(entry => [entry.name, entry.status]),
        [['origin', 'configured'], ['fork', 'configured']]);
    },
  },
  {
    name: 'a commit made in a graft is pushed and the primary keeps its branches, HEAD and files',
    run(ctx) {
      const bare = remote(ctx);
      // Work in progress in the primary must come through untouched.
      writeFileSync(join(ctx.repo, 'tracked.txt'), 'edited in the primary\n');
      writeFileSync(join(ctx.repo, 'staged.txt'), 'staged in the primary\n');
      ctx.git(['add', 'staged.txt']);
      writeFileSync(join(ctx.repo, 'untracked.txt'), 'untracked in the primary\n');
      const graft = ctx.spawn('task');
      const tip = commit(ctx, graft.path, 'feature.txt', 'work\n');
      const before = state(ctx, ctx.repo);

      const pushed = ctx.cli(push('task', graft));
      assert.deepEqual(pushed, {
        action: 'push', name: 'task', remote: 'origin', branch: 'graft/task', commit: tip, uncommitted_changes: 0,
      });
      assert.equal(refs(ctx, bare), 'refs/heads/graft/task ' + tip);
      assert.equal(state(ctx, ctx.repo), before);
      // Git records what it pushed, as it does for any push from the primary.
      assert.equal(refs(ctx, ctx.repo, 'refs/remotes'), 'refs/remotes/origin/graft/task ' + tip);
      assert.equal(privateRefs(ctx), '');
      const status = ctx.cli(['status', 'task']);
      assert.equal(status.phase, 'ready');
      assert.equal(status.head, tip);

      // Work continues in the same graft, and the next push fast-forwards.
      const next = commit(ctx, graft.path, 'more.txt', 'more work\n');
      assert.equal(ctx.cli(push('task', graft)).commit, next);
      assert.equal(refs(ctx, bare), 'refs/heads/graft/task ' + next);
      // Pushing a commit the remote already has succeeds and changes nothing.
      assert.equal(ctx.cli(push('task', graft)).commit, next);
      assert.equal(refs(ctx, bare), 'refs/heads/graft/task ' + next);
      assert.equal(state(ctx, ctx.repo), before);
      assert.equal(privateRefs(ctx), '');
    },
  },
  {
    name: '--to names the remote branch, and a detached or unborn graft is refused without it',
    run(ctx) {
      const bare = remote(ctx);
      const named = ctx.spawn('named');
      const tip = commit(ctx, named.path, 'feature.txt', 'work\n');
      const pushed = ctx.cli(push('named', named, '--to', 'review/parser'));
      assert.equal(pushed.branch, 'review/parser');
      assert.equal(pushed.commit, tip);
      assert.equal(refs(ctx, bare), 'refs/heads/review/parser ' + tip);
      for (const invalid of ['refs/heads/review', 'a..b', 'bad name', '@{-1}', 'HEAD', 'ends.lock']) {
        const refused = ctx.fail(push('named', named, '--to', invalid));
        assert.match(String(refused.message), /invalid branch name/, invalid);
      }

      const detached = ctx.spawn('detached', ['--detach']);
      const loose = commit(ctx, detached.path, 'loose.txt', 'detached work\n');
      const refused = ctx.fail(push('detached', detached));
      assert.equal(refused.code, 'detached_head', String(refused.message));
      assert.match(String(refused.message), /--to/);
      assert.deepEqual(refused.details, { name: 'detached', commit: loose });
      assert.equal(refs(ctx, bare), 'refs/heads/review/parser ' + tip);
      assert.equal(ctx.cli(push('detached', detached, '--to', 'from-detached')).branch, 'from-detached');
      assert.equal(ctx.git(['rev-parse', 'refs/heads/from-detached'], bare), loose);

      const empty = ctx.spawn('empty', ['--orphan', 'fresh']);
      const published = refs(ctx, bare);
      for (const options of [[], ['--to', 'fresh']]) {
        const unborn = ctx.fail(push('empty', empty, ...options));
        assert.equal(unborn.code, 'nothing_to_push', String(unborn.message));
      }
      assert.equal(refs(ctx, bare), published);
      assert.equal(privateRefs(ctx), '');
    },
  },
  {
    name: "a graft on the remote's default branch is refused unless --to names that branch",
    run(ctx) {
      const bare = remote(ctx);
      ctx.git(['push', '-q', 'origin', 'HEAD:refs/heads/main']);
      ctx.git(['remote', 'set-head', 'origin', 'main']);
      const base = ctx.git(['rev-parse', 'HEAD']);
      const graft = ctx.spawn('trunk', ['--branch', 'main']);
      const tip = commit(ctx, graft.path, 'trunk.txt', 'work\n');

      const refused = ctx.fail(push('trunk', graft));
      assert.equal(refused.code, 'default_branch', String(refused.message));
      assert.match(String(refused.message), /--to main/);
      assert.deepEqual(refused.details, { name: 'trunk', remote: 'origin', branch: 'main' });
      assert.equal(refs(ctx, bare), 'refs/heads/main ' + base);
      assert.equal(privateRefs(ctx), '');

      assert.equal(ctx.cli(push('trunk', graft, '--to', 'review/trunk')).branch, 'review/trunk');
      const named = ctx.cli(push('trunk', graft, '--to', 'main'));
      assert.equal(named.branch, 'main');
      assert.equal(ctx.git(['rev-parse', 'refs/heads/main'], bare), tip);
    },
  },
  {
    name: 'a graft that diverged from the remote branch is refused and pushes once it has merged it',
    run(ctx) {
      const bare = remote(ctx);
      const first = ctx.spawn('first');
      const second = ctx.spawn('second');
      const theirs = commit(ctx, first.path, 'first.txt', 'first\n');
      const ours = commit(ctx, second.path, 'second.txt', 'second\n');
      ctx.cli(push('first', first, '--to', 'shared'));
      const published = refs(ctx, bare);
      const before = refs(ctx, ctx.repo);
      const work = state(ctx, second.path);

      const rejected = ctx.fail(push('second', second, '--to', 'shared'));
      assert.equal(rejected.code, 'push_rejected', String(rejected.message));
      const details = object(rejected.details);
      assert.equal(details.remote, 'origin');
      assert.equal(details.branch, 'shared');
      assert.match(String(details.reason), /^(non-fast-forward|fetch first)$/);
      assert.equal(typeof details.stderr, 'string');
      assert(!JSON.stringify(rejected).includes(second.lease));
      assert.equal(refs(ctx, bare), published);
      assert.equal(refs(ctx, ctx.repo), before);
      assert.equal(privateRefs(ctx), '');
      assert.equal(state(ctx, second.path), work);
      assert.equal(ctx.cli(['status', 'second']).phase, 'ready');

      // The primary's remote-tracking ref holds what the first push sent.
      ctx.git(['fetch', '--no-tags', ctx.repo, 'refs/remotes/origin/shared'], second.path);
      ctx.git(['merge', '--no-edit', 'FETCH_HEAD'], second.path);
      const merged = ctx.git(['rev-parse', 'HEAD'], second.path);
      assert.equal(ctx.cli(push('second', second, '--to', 'shared')).commit, merged);
      assert.equal(refs(ctx, bare), 'refs/heads/shared ' + merged);
      assert.equal(ctx.git(['rev-list', '--parents', '-1', merged], bare), [merged, ours, theirs].join(' '));
    },
  },
  {
    name: 'an unknown remote is refused by name and a remote that cannot be reached fails',
    run(ctx) {
      const bare = remote(ctx);
      ctx.git(['remote', 'add', 'gone', join(ctx.root, 'missing.git')]);
      const graft = ctx.spawn('lost');
      commit(ctx, graft.path, 'feature.txt', 'work\n');
      const before = refs(ctx, ctx.repo);

      const unknown = ctx.fail(push('lost', graft, '--remote', 'upstream'));
      assert.equal(unknown.code, 'remote_not_found', String(unknown.message));
      assert.deepEqual(unknown.details, { remote: 'upstream', remotes: ['gone', 'origin'] });
      // Git would push to a path or URL. Graft takes only a configured name.
      const byPath = ctx.fail(push('lost', graft, '--remote', bare));
      assert.equal(byPath.code, 'remote_not_found', String(byPath.message));

      const failed = ctx.fail(push('lost', graft, '--remote', 'gone'));
      assert.equal(failed.code, 'push_failed', String(failed.message));
      const details = object(failed.details);
      assert.equal(details.remote, 'gone');
      assert.equal(details.branch, 'graft/lost');
      assert(typeof details.stderr === 'string' && details.stderr.length > 0);
      assert(!('reason' in details));

      const stranger = ctx.fail(push('lost', { lease: randomUUID() }));
      assert.notEqual(stranger.code, 'push_failed', String(stranger.message));

      assert.equal(refs(ctx, bare), '');
      assert.equal(refs(ctx, ctx.repo), before);
      assert.equal(privateRefs(ctx), '');
      assert.equal(ctx.cli(['status', 'lost']).phase, 'ready');
    },
  },
  {
    name: 'uncommitted and untracked files are counted and not pushed',
    run(ctx) {
      const bare = remote(ctx);
      const graft = ctx.spawn('dirty');
      const tip = commit(ctx, graft.path, 'done.txt', 'committed\n');
      writeFileSync(join(graft.path, 'tracked.txt'), 'unstaged edit\n');
      writeFileSync(join(graft.path, 'staged.txt'), 'staged\n');
      ctx.git(['add', 'staged.txt'], graft.path);
      mkdirSync(join(graft.path, 'notes'));
      writeFileSync(join(graft.path, 'notes', 'one.txt'), 'untracked\n');
      writeFileSync(join(graft.path, 'notes', 'two.txt'), 'untracked\n');
      // Ignored files are not work a push could have taken.
      mkdirSync(join(graft.path, 'ignored'));
      writeFileSync(join(graft.path, 'ignored', 'build.log'), 'ignored\n');
      const before = state(ctx, graft.path);

      const pushed = ctx.cli(push('dirty', graft));
      assert.equal(pushed.commit, tip);
      assert.equal(pushed.uncommitted_changes, 4);
      assert.deepEqual(ctx.git(['ls-tree', '-r', '--name-only', 'refs/heads/graft/dirty'], bare).split('\n'),
        ['.gitignore', 'done.txt', 'tracked.txt']);
      assert.equal(ctx.git(['show', 'refs/heads/graft/dirty:tracked.txt'], bare), 'initial');
      assert.equal(state(ctx, graft.path), before);
      assert.equal(readFileSync(join(graft.path, 'tracked.txt'), 'utf8'), 'unstaged edit\n');
    },
  },
  {
    name: 'tip and isolated grafts push with the history the primary holds',
    run(ctx) {
      const bare = remote(ctx);
      commit(ctx, ctx.repo, 'second.txt', 'second\n');
      for (const history of ['tip', 'isolated']) {
        const name = history + '-history';
        const graft = ctx.spawn(name, ['--history', history]);
        assert.equal(ctx.git(['rev-parse', '--is-shallow-repository'], graft.path), String(history === 'tip'));
        const tip = commit(ctx, graft.path, name + '.txt', 'work\n');
        assert.equal(ctx.cli(push(name, graft)).commit, tip);
        const branch = 'refs/heads/graft/' + name;
        assert.equal(ctx.git(['rev-parse', branch], bare), tip);
        // Both commits of the primary came along as the graft commit's ancestors.
        assert.equal(ctx.git(['rev-list', '--count', branch], bare), '3');
        ctx.git(['fsck', '--connectivity-only'], bare);
      }
      for (const path of [bare, ctx.repo]) {
        assert.equal(ctx.git(['rev-parse', '--is-shallow-repository'], path), 'false');
      }
      assert.equal(privateRefs(ctx), '');
    },
  },
  {
    name: 'a pushed graft still counts as unmerged work, and can be restored to push again',
    run(ctx) {
      const bare = remote(ctx);
      const graft = ctx.spawn('published');
      const tip = commit(ctx, graft.path, 'feature.txt', 'work\n');
      ctx.cli(push('published', graft));

      const refused = ctx.fail(['remove', 'published', '--lease', graft.lease]);
      assert.equal(refused.code, 'unmerged_work', String(refused.message));
      assert.equal(ctx.cli(['status', 'published']).phase, 'ready');
      const removed = ctx.cli(['remove', 'published', '--lease', graft.lease, '--discard']);
      assert.equal(removed.removed, true);
      assert(typeof removed.recovery_lease === 'string');
      assert(!existsSync(graft.path));
      assert.equal(refs(ctx, bare), 'refs/heads/graft/published ' + tip);

      const archived = ctx.fail(push('published', { lease: removed.recovery_lease }));
      assert.equal(archived.code, 'graft_archived', String(archived.message));
      const restored = ctx.cli(['restore', 'published', '--lease', removed.recovery_lease]);
      assert(typeof restored.lease === 'string');
      const next = commit(ctx, graft.path, 'review.txt', 'changes from review\n');
      assert.equal(ctx.cli(push('published', { lease: restored.lease })).commit, next);
      assert.equal(refs(ctx, bare), 'refs/heads/graft/published ' + next);
    },
  },
  {
    name: 'credentials in a remote URL never appear in a result or an error',
    run(ctx) {
      const bare = join(ctx.root, 'origin.git');
      ctx.git(['init', '-q', '--bare', bare], ctx.root);
      // Nothing listens on this port, so a push to the URL fails at once.
      const url = 'https://graft-user:graft-secret@127.0.0.1:1/team/project.git';
      ctx.git(['remote', 'add', 'origin', url]);
      ctx.git(['config', 'remote.origin.pushurl', bare]);
      ctx.git(['remote', 'add', 'web', url]);
      ctx.git(['config', 'http.proxy', '']);
      const graft = ctx.spawn('private');
      const tip = commit(ctx, graft.path, 'feature.txt', 'work\n');
      const clean = (text: string) => assert(!/graft-secret|graft-user/.test(text), text);

      // The primary's pushurl applies, and a result names the remote only.
      const pushed = output(ctx, push('private', graft));
      assert(pushed.ok, pushed.text);
      clean(pushed.text);
      assert(!pushed.text.includes('127.0.0.1') && !pushed.text.includes('origin.git'), pushed.text);
      assert.equal(pushed.json.remote, 'origin');
      assert.equal(refs(ctx, bare), 'refs/heads/graft/private ' + tip);

      const failed = output(ctx, push('private', graft, '--remote', 'web'));
      assert(!failed.ok);
      assert.equal(failed.json.code, 'push_failed', failed.text);
      clean(failed.text);
      assert(!failed.text.includes(graft.lease));

      const unknown = output(ctx, push('private', graft, '--remote', 'upstream'));
      assert.equal(unknown.json.code, 'remote_not_found', unknown.text);
      assert.deepEqual(object(unknown.json.details).remotes, ['origin', 'web']);
      clean(unknown.text);
      assert(!unknown.text.includes('127.0.0.1'), unknown.text);
      assert.equal(privateRefs(ctx), '');
    },
  },
  {
    name: 'SSH runs in batch mode unless the user chose an SSH command',
    run(ctx) {
      remote(ctx);
      const log = join(ctx.root, 'ssh.log');
      const hooks = join(ctx.git(['rev-parse', '--absolute-git-dir']), 'hooks');
      mkdirSync(hooks, { recursive: true });
      // Git passes the push's environment to the hook, so it sees the SSH command Git would run.
      writeFileSync(join(hooks, 'pre-push'),
        '#!/bin/sh\nprintf \'%s\\n\' "${GIT_SSH_COMMAND-unset}" >> "$GRAFT_E2E_SSH_LOG"\n');
      chmodSync(join(hooks, 'pre-push'), 0o755);
      ctx.env.GRAFT_E2E_SSH_LOG = log.replaceAll('\\', '/');
      const graft = ctx.spawn('ssh');
      const last = () => readFileSync(log, 'utf8').trim().split('\n').at(-1);
      try {
        commit(ctx, graft.path, 'one.txt', 'one\n');
        ctx.cli(push('ssh', graft));
        assert.equal(last(), 'ssh -o BatchMode=yes');

        ctx.git(['config', 'core.sshCommand', 'ssh -i chosen']);
        commit(ctx, graft.path, 'two.txt', 'two\n');
        ctx.cli(push('ssh', graft));
        assert.equal(last(), 'unset');
        ctx.git(['config', '--unset', 'core.sshCommand']);

        ctx.env.GIT_SSH_COMMAND = 'ssh -F chosen';
        commit(ctx, graft.path, 'three.txt', 'three\n');
        ctx.cli(push('ssh', graft));
        assert.equal(last(), 'ssh -F chosen');
      } finally {
        delete ctx.env.GIT_SSH_COMMAND;
        delete ctx.env.GRAFT_E2E_SSH_LOG;
      }
    },
  },
  {
    name: "the primary's pre-push hook runs without the lease, and the import runs no hook",
    run(ctx) {
      const bare = remote(ctx);
      const log = join(ctx.root, 'hooks.log');
      const refuse = join(ctx.root, 'refuse-push');
      const script = join(ctx.root, 'record hook.cjs');
      writeFileSync(script, [
        "const fs = require('node:fs');",
        "let input = '';",
        "try { input = fs.readFileSync(0, 'utf8'); } catch {}",
        'fs.appendFileSync(process.env.GRAFT_E2E_PUSH_LOG, JSON.stringify({ hook: process.argv[2],',
        "  args: process.argv.slice(3), input, lease: process.env.GIT_GRAFT_LEASE ?? null }) + '\\n');",
        "if (process.argv[2] === 'pre-push' && fs.existsSync(process.env.GRAFT_E2E_PUSH_REFUSE)) process.exit(1);",
      ].join('\n') + '\n');
      const hooks = join(ctx.git(['rev-parse', '--absolute-git-dir']), 'hooks');
      mkdirSync(hooks, { recursive: true });
      for (const hook of ['pre-push', 'reference-transaction']) {
        writeFileSync(join(hooks, hook),
          `#!/bin/sh\nexec "$GRAFT_E2E_PUSH_NODE" "$GRAFT_E2E_PUSH_HOOK" ${hook} "$@"\n`);
        chmodSync(join(hooks, hook), 0o755);
      }
      Object.assign(ctx.env, {
        GRAFT_E2E_PUSH_NODE: process.execPath.replaceAll('\\', '/'),
        GRAFT_E2E_PUSH_HOOK: script.replaceAll('\\', '/'),
        GRAFT_E2E_PUSH_LOG: log,
        GRAFT_E2E_PUSH_REFUSE: refuse,
      });
      const graft = ctx.spawn('hooked');
      const tip = commit(ctx, graft.path, 'feature.txt', 'work\n');
      const entries = () => readFileSync(log, 'utf8').trim().split('\n').map(line => object(JSON.parse(line)));

      // The lease comes from the environment here, where a hook could read it.
      writeFileSync(log, '');
      ctx.env.GIT_GRAFT_LEASE = graft.lease;
      let pushed;
      try {
        pushed = ctx.cli(['push', 'hooked']);
      } finally {
        delete ctx.env.GIT_GRAFT_LEASE;
      }
      assert.equal(pushed.commit, tip);
      assert.equal(refs(ctx, bare), 'refs/heads/graft/hooked ' + tip);
      const seen = entries();
      const prePush = seen.filter(entry => entry.hook === 'pre-push');
      assert.equal(prePush.length, 1);
      // Git names the remote to the hook; the lease is not in its environment.
      const hookArgs = prePush[0].args;
      assert(Array.isArray(hookArgs));
      assert.equal(hookArgs[0], 'origin');
      assert.match(String(prePush[0].input), new RegExp(`^\\S+ ${tip} refs/heads/graft/hooked [0-9a-f]+\\n$`));
      // Git's update of the remote-tracking ref is the user's own ref change.
      assert(seen.some(entry => entry.hook === 'reference-transaction'
        && String(entry.input).includes('refs/remotes/origin/graft/hooked')), JSON.stringify(seen));
      // The private ref is created and removed without the primary's hooks.
      for (const entry of seen) {
        assert.equal(entry.lease, null);
        assert(!String(entry.input).includes('refs/graft/'), JSON.stringify(entry));
      }

      // A hook that declines stops the push before the remote hears of it.
      const next = commit(ctx, graft.path, 'more.txt', 'more work\n');
      writeFileSync(refuse, '');
      const declined = ctx.fail(push('hooked', graft));
      assert.equal(declined.code, 'push_failed', String(declined.message));
      assert.equal(refs(ctx, bare), 'refs/heads/graft/hooked ' + tip);
      assert.equal(privateRefs(ctx), '');
      rmSync(refuse);
      assert.equal(ctx.cli(push('hooked', graft)).commit, next);
      assert.equal(refs(ctx, bare), 'refs/heads/graft/hooked ' + next);
    },
  },
  {
    name: 'MCP offers graft_push only to a host started with --allow-push',
    async run(ctx) {
      const bare = remote(ctx);
      const graft = ctx.spawn('agent');
      const tip = commit(ctx, graft.path, 'feature.txt', 'work\n');
      writeFileSync(join(graft.path, 'scratch.txt'), 'not committed\n');

      // Permission to run programs is not permission to publish.
      await withClient(ctx, true, async client => {
        const info = await initialize(client);
        assert.match(String(info.instructions), /disabled.*--allow-push/);
        const catalog = await discover(client, true);
        assert(!catalog.has('graft_push'));
        const refused = await call(client, 'graft_push', { name: 'agent', lease: graft.lease }, true);
        assert.match(String(refused.message), /graft_push requires the host to start with --allow-push/);
      });
      assert.equal(refs(ctx, bare), '');
      assert.equal(privateRefs(ctx), '');

      await withClient(ctx, false, async client => {
        const info = await initialize(client);
        assert.match(String(info.instructions), /graft_push is enabled by the host/);
        const catalog = await discover(client, false, true);
        ctx.measure('advertised_tools', [...catalog.keys()].sort());
        const tool = object(catalog.get('graft_push'));
        assert.equal(object(tool.annotations).readOnlyHint, false);
        assert.equal(object(tool.annotations).destructiveHint, true);
        const schema = object(tool.inputSchema);
        assert(Array.isArray(schema.required));
        assert.deepEqual([...schema.required].sort(), ['lease', 'name']);
        assert.deepEqual(Object.keys(object(schema.properties)).sort(), ['lease', 'name', 'remote', 'to']);

        const unknown = await call(client, 'graft_push', { name: 'agent', lease: graft.lease, remote: 'upstream' }, true);
        assert.equal(unknown.code, 'remote_not_found');
        assert.deepEqual(object(unknown.details).remotes, ['origin']);
        const pushed = await call(client, 'graft_push', { name: 'agent', lease: graft.lease });
        assert.deepEqual(pushed, {
          action: 'push', name: 'agent', remote: 'origin', branch: 'graft/agent', commit: tip, uncommitted_changes: 1,
        });
        const renamed = await call(client, 'graft_push', { name: 'agent', lease: graft.lease, remote: 'origin', to: 'review/agent' });
        assert.equal(renamed.branch, 'review/agent');
        ctx.coverTool('graft_push');
      }, true);
      assert.equal(refs(ctx, bare), ['refs/heads/graft/agent ' + tip, 'refs/heads/review/agent ' + tip].join('\n'));
      assert.equal(privateRefs(ctx), '');
      assert.equal(ctx.cli(['status', 'agent']).phase, 'ready');
    },
  },
];
