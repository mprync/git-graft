import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, readdirSync, readFileSync, renameSync, rmSync, statSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { execute, objectResult, parseExact, type Context, type Scenario } from './harness.ts';

/** Hash every file below each root, so a test can prove nothing was written. */
function snapshot(...roots: string[]): Record<string, string> {
  const files: Record<string, string> = {};
  const walk = (path: string) => {
    const stat = statSync(path, { throwIfNoEntry: false });
    if (!stat) return;
    if (stat.isDirectory()) {
      for (const entry of readdirSync(path)) walk(join(path, entry));
    } else {
      files[path] = createHash('sha256').update(readFileSync(path)).digest('hex');
    }
  };
  for (const root of roots) walk(root);
  return files;
}

/** Run Graft against a primary at `repo` and return its JSON receipt or error. */
function graftAt(ctx: Context, repo: string, args: string[]) {
  const out = execute(ctx.bin, ['--repo', repo, '--json', ...args], repo, ctx.env);
  assert.ifError(out.error);
  return { ok: out.status === 0, json: objectResult(JSON.parse(out.status === 0 ? out.stdout : (out.stderr || out.stdout))) };
}

/** Two grafts with local work, one of them archived, for environment faults. */
function environment(ctx: Context) {
  const live = ctx.spawn('live');
  writeFileSync(join(live.path, 'tracked.txt'), 'committed graft work\n');
  ctx.git(['commit', '-qam', 'Graft work'], live.path);
  const tip = ctx.git(['rev-parse', 'HEAD'], live.path);
  writeFileSync(join(live.path, 'notes.txt'), 'uncommitted graft notes\n');
  const archived = ctx.spawn('archived');
  writeFileSync(join(archived.path, 'draft.txt'), 'archived draft\n');
  ctx.cli(['remove', 'archived', '--lease', archived.lease, '--discard']);
  return { live, tip, archived, root: dirname(live.path) };
}

function assertEnvironmentIntact(ctx: Context, env: ReturnType<typeof environment>, root = env.root) {
  const live = join(root, basename(env.live.path));
  assert.equal(readFileSync(join(live, 'notes.txt'), 'utf8'), 'uncommitted graft notes\n');
  assert.equal(ctx.git(['rev-parse', 'HEAD'], live), env.tip);
  ctx.git(['fsck', '--full'], live);
  const archives = readdirSync(join(root, '.retired')).filter(entry => /^[a-f0-9-]{36}$/.test(entry));
  assert.equal(archives.length, 1);
  assert.equal(readFileSync(join(root, '.retired', archives[0], 'draft.txt'), 'utf8'), 'archived draft\n');
}

/** Hold a file open on Windows without sharing, as an editor or build tool might. */
async function holdOpen(ctx: Context, file: string) {
  const ready = join(ctx.root, 'holder-ready-' + basename(file));
  const release = join(ctx.root, 'holder-release-' + basename(file));
  const script = join(ctx.root, 'holder.ps1');
  writeFileSync(script, [
    'param($Path, $Ready, $Release)',
    "$handle = [System.IO.File]::Open($Path, 'Open', 'ReadWrite', 'None')",
    "Set-Content -LiteralPath $Ready -Value 'ready'",
    'while (-not (Test-Path -LiteralPath $Release)) { Start-Sleep -Milliseconds 50 }',
    '$handle.Close()',
  ].join('\r\n'));
  const child = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', script, file, ready, release],
    { stdio: 'ignore', windowsHide: true });
  const exited = new Promise<number | null>((resolve, reject) => { child.once('error', reject); child.once('exit', resolve); });
  const deadline = Date.now() + 30_000;
  while (!existsSync(ready)) {
    assert(Date.now() < deadline && child.exitCode === null, 'the file holder did not start');
    await delay(25);
  }
  return async () => {
    writeFileSync(release, 'release\n');
    assert.equal(await exited, 0, 'the file holder failed');
  };
}

export const cases: Scenario[] = [
  {
    name: 'new private roots have owner-only permissions under a permissive umask',
    run(ctx) {
      const unix = process.platform !== 'win32';
      const previous = unix ? process.umask(0o022) : undefined;
      try {
        const workspace = ctx.spawn('private-roots');
        const state = join(ctx.repo, '.git', 'graft');
        for (const directory of ['cache', 'layers']) {
          assert(!existsSync(join(state, directory)), 'source-only project created a dependency cache during new');
        }
        const env = ctx.cli(['status', 'private-roots', '--env']);
        assert.equal(env.path, workspace.path);
        assert.equal(typeof env.runtime_dir, 'string');
        const runtime = String(env.runtime_dir);
        ctx.cli(['exec', 'private-roots', '--lease', workspace.lease, '--', process.execPath, '-e', '']);
        for (const path of [state, runtime, join(state, 'cache'), dirname(workspace.path)]) {
          const stat = statSync(path);
          assert(stat.isDirectory(), path);
          if (unix) assert.equal(stat.mode & 0o777, 0o700, path);
        }
      } finally {
        if (previous !== undefined) process.umask(previous);
      }
    },
  },
  ...(process.platform === 'win32' ? [] : [{
    name: 'permissive roots keep their modes and refuse mutations before remediation',
    run(ctx) {
      const workspace = ctx.spawn('legacy-mode');
      const state = join(ctx.repo, '.git', 'graft');
      const root = dirname(workspace.path);
      const file = join(workspace.path, 'tracked.txt');
      chmodSync(state, 0o755);
      assert.match(String(ctx.fail(['heartbeat', 'legacy-mode', '--lease', workspace.lease]).message), /chmod 700/i);
      assert.equal(statSync(state).mode & 0o777, 0o755);
      chmodSync(state, 0o700);

      chmodSync(root, 0o755);
      assert.match(String(ctx.fail(['add', 'new-workspace']).message), /chmod 700/i);
      assert.equal(statSync(root).mode & 0o777, 0o755);
      assert.equal(existsSync(join(root, 'new-workspace')), false);
      assert.equal(readFileSync(file, 'utf8'), 'initial\n');
      chmodSync(root, 0o700);
      assert.equal(ctx.cli(['status', 'legacy-mode']).path, workspace.path);
      ctx.cli(['heartbeat', 'legacy-mode', '--lease', workspace.lease]);
    },
  } satisfies Scenario]),
  {
    name: 'purge keeps the cache ownership marker and can retry',
    run(ctx) {
      ctx.cli(['doctor']);
      const cache = join(ctx.repo, '.git', 'graft', 'cache');
      const marker = join(cache, '.git-graft-id');
      const markerIdentity = statSync(marker, { bigint: true });
      const npm = join(cache, 'npm');
      const note = join(npm, 'cached.txt');
      const external = join(ctx.root, 'external');
      mkdirSync(external);
      const preserved = join(external, 'keep.txt');
      writeFileSync(preserved, 'keep linked content\n');
      for (let attempt = 0; attempt < 2; attempt++) {
        const ws = ctx.spawn('cache-writer');
        const owned = ['cache-writer', '--lease', ws.lease];
        ctx.cli(['exec', ...owned, '--', process.execPath, '-e', '']);
        if (process.platform !== 'win32') symlinkSync(external, join(npm, 'linked'), 'dir');
        writeFileSync(note, 'disposable cache data\n');
        ctx.cli(['remove', ...owned]);
        const result = objectResult(ctx.cli(['prune', '--purge-cache', '--apply']).cache);
        assert.equal(result.purged, true);
        assert(Array.isArray(result.removed) && result.removed.includes(npm));
        assert.equal(existsSync(note), false);
        assert.equal(statSync(marker, { bigint: true }).ino, markerIdentity.ino);
        assert.equal(readFileSync(preserved, 'utf8'), 'keep linked content\n');
      }
    },
  },
  {
    name: 'cache purge preserves a legacy cache without an ownership marker',
    run(ctx) {
      ctx.cli(['doctor']);
      const cache = join(ctx.repo, '.git', 'graft', 'cache');
      renameSync(cache, join(ctx.root, 'created-cache'));
      mkdirSync(cache);
      const note = join(cache, 'keep.txt');
      writeFileSync(note, 'data from the existing directory\n');
      const result = objectResult(ctx.cli(['prune', '--purge-cache', '--apply']).cache);
      assert.equal(result.eligible, false);
      assert.equal(result.purged, false);
      assert.match(JSON.stringify(result.blocked), /cache ownership is unverified/i);
      assert.equal(readFileSync(note, 'utf8'), 'data from the existing directory\n');
    },
  },
  {
    name: 'unrecognized registry data is left untouched',
    run(ctx) {
      const state = join(ctx.repo, '.git', 'graft');
      mkdirSync(state, { mode: 0o700 });
      const note = join(state, 'notes.txt');
      writeFileSync(note, 'keep this note\n');
      assert.match(String(ctx.fail(['doctor']).message), /unrecognized graft data/i);
      assert.equal(readFileSync(note, 'utf8'), 'keep this note\n');
      assert.equal(existsSync(join(state, 'project.lock')), false);
      assert.equal(existsSync(join(state, 'project-id')), false);
    },
  },
  {
    name: 'a linked state directory cannot be purged',
    run(ctx) {
      ctx.cli(['doctor']);
      const cache = join(ctx.repo, '.git', 'graft', 'cache');
      const other = join(ctx.root, 'separate-cache');
      renameSync(cache, other);
      const note = join(other, 'keep.txt');
      writeFileSync(note, 'keep this file\n');
      symlinkSync(other, cache, process.platform === 'win32' ? 'junction' : 'dir');
      assert.match(String(ctx.fail(['prune', '--purge-cache', '--apply']).message), /ordinary directory/i);
      assert.equal(readFileSync(note, 'utf8'), 'keep this file\n');
    },
  },
  {
    name: 'purge retains a cache containing a nested bare repository',
    run(ctx) {
      const ws = ctx.spawn('cache-writer');
      const owned = ['cache-writer', '--lease', ws.lease];
      ctx.cli(['exec', ...owned, '--', process.execPath, '-e', '']);
      const state = join(ctx.repo, '.git', 'graft');
      const npm = join(state, 'cache', 'npm');
      const nested = join(npm, 'nested.git');
      ctx.git(['clone', '--bare', '--no-hardlinks', ctx.repo, nested]);
      const head = ctx.git(['rev-parse', 'HEAD'], nested);
      const note = join(state, 'layers', 'keep.txt');
      writeFileSync(note, 'keep while purge is blocked\n');
      ctx.cli(['remove', ...owned]);
      for (const extra of [[], ['--apply']]) {
        const cache = objectResult(ctx.cli(['prune', '--purge-cache', ...extra]).cache);
        assert(Array.isArray(cache.blocked));
        const blocked = cache.blocked.map(objectResult).find(entry => entry.path === npm);
        assert(blocked);
        assert.match(String(objectResult(blocked.reason).message), /nested bare Git repository/i);
        assert(Array.isArray(cache.candidates) && !cache.candidates.includes(npm));
        assert(Array.isArray(cache.removed) && !cache.removed.includes(npm));
        assert.equal(ctx.git(['rev-parse', 'HEAD'], nested), head);
        assert.equal(readFileSync(note, 'utf8'), 'keep while purge is blocked\n');
      }
    },
  },
  {
    name: 'purge refuses a replacement package cache with a copied ownership marker',
    run(ctx) {
      const ws = ctx.spawn('cache-writer');
      const owned = ['cache-writer', '--lease', ws.lease];
      ctx.cli(['exec', ...owned, '--', process.execPath, '-e', '']);
      const npm = join(ctx.repo, '.git', 'graft', 'cache', 'npm');
      const marker = '.git-graft-cache-id';
      const original = join(ctx.root, 'original npm cache');
      const ownership = readFileSync(join(npm, marker));
      writeFileSync(join(npm, 'original.txt'), 'original cache data');
      renameSync(npm, original);
      mkdirSync(npm, { mode: 0o700 });
      writeFileSync(join(npm, marker), ownership, { mode: 0o600 });
      const replacement = join(npm, 'replacement.txt');
      writeFileSync(replacement, 'unrelated directory data');
      ctx.cli(['remove', ...owned]);

      for (const extra of [[], ['--apply']]) {
        const cache = objectResult(ctx.cli(['prune', '--purge-cache', ...extra]).cache);
        assert(Array.isArray(cache.blocked));
        const blocked = cache.blocked.map(objectResult).find(entry => entry.path === npm);
        assert(blocked);
        assert.match(String(objectResult(blocked.reason).message), /package cache ownership is unverified/i);
        assert(Array.isArray(cache.removed) && !cache.removed.includes(npm));
        assert.equal(readFileSync(replacement, 'utf8'), 'unrelated directory data');
        assert.deepEqual(readFileSync(join(npm, marker)), ownership);
        assert.equal(readFileSync(join(original, 'original.txt'), 'utf8'), 'original cache data');
      }
    },
  },
  {
    name: 'GC retains unrecognized source snapshot directories',
    run(ctx) {
      const workspace = ctx.spawn('snapshot-fixture');
      const unknown = join(ctx.repo, '.git', 'graft', 'bases', '0'.repeat(40));
      mkdirSync(unknown, { recursive: true });
      const note = join(unknown, 'keep.txt');
      writeFileSync(note, 'outside the source snapshot format\n');
      assert.equal(readFileSync(join(workspace.path, 'tracked.txt'), 'utf8'), 'initial\n');
      ctx.cli(['remove', 'snapshot-fixture', '--lease', workspace.lease]);
      for (const extra of [[], ['--apply']]) {
        const cache = objectResult(ctx.cli(['prune', '--purge-cache', ...extra]).cache);
        assert(Array.isArray(cache.blocked));
        assert(cache.blocked.map(objectResult).some(entry => entry.path === unknown));
        assert.equal(readFileSync(note, 'utf8'), 'outside the source snapshot format\n');
      }
    },
  },
  {
    name: 'a damaged workspace record cannot authorize operations',
    run(ctx) {
      const workspace = ctx.spawn('bad-record');
      assert(typeof workspace.result.id === 'string');
      const path = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', workspace.result.id + '.json');
      const record = objectResult(parseExact(readFileSync(path, 'utf8')));
      const head = ctx.git(['rev-parse', 'HEAD'], workspace.path);
      for (const [fields, reason] of [
        [{ id: 'different-id' }, /record ID mismatch/i],
        [{ base: 'invalid-object-id' }, /full Git object ID/i],
        [{ path: 'relative checkout' }, /invalid workspace path/i],
      ] as const) {
        const changed = JSON.stringify({ ...record, ...fields });
        writeFileSync(path, changed);
        assert.match(String(ctx.fail(['status', 'bad-record']).message), reason);
        assert.match(String(ctx.fail(['remove', 'bad-record', '--lease', workspace.lease, '--discard']).message), reason);
        assert.equal(readFileSync(path, 'utf8'), changed);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), head);
        assert.equal(readFileSync(join(workspace.path, 'tracked.txt'), 'utf8'), 'initial\n');
      }
    },
  },
  {
    name: 'workspace discovery ignores the calling Git command configuration',
    run(ctx) {
      const original = ctx.git(['rev-parse', 'HEAD']);
      Object.assign(ctx.env, {
        GIT_CONFIG_COUNT: '1',
        GIT_CONFIG_KEY_0: 'core.bare',
        GIT_CONFIG_VALUE_0: 'true',
        GIT_CONFIG_PARAMETERS: "'core.bare'='true'",
      });
      const workspace = ctx.spawn('hook-context');
      assert.equal(ctx.cli(['status', 'hook-context']).path, workspace.path);
      delete ctx.env.GIT_CONFIG_COUNT;
      delete ctx.env.GIT_CONFIG_PARAMETERS;
      assert.equal(ctx.git(['rev-parse', 'HEAD']), original);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), original);
      assert.equal(readFileSync(join(workspace.path, 'tracked.txt'), 'utf8'), 'initial\n');
    },
  },
  {
    name: 'landing refuses a destination with assume-unchanged edits',
    run(ctx) {
      const original = ctx.git(['rev-parse', 'HEAD']);
      const workspace = ctx.spawn('hidden');
      const source = join(workspace.path, 'tracked.txt');
      const destination = join(ctx.repo, 'tracked.txt');
      writeFileSync(source, 'incoming commitment\n');
      ctx.git(['commit', '-qam', 'Incoming changes'], workspace.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], workspace.path);
      ctx.git(['update-index', '--assume-unchanged', '--', 'tracked.txt']);
      writeFileSync(destination, 'local edit\n');
      const land = ['merge', 'hidden', '--lease', workspace.lease, '--into', 'main'];
      for (let attempt = 0; attempt < 2; attempt++) {
        assert.equal(ctx.fail(land).code, 'hidden_changes');
        assert.equal(ctx.git(['rev-parse', 'HEAD']), original);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), tip);
        assert.equal(readFileSync(destination, 'utf8'), 'local edit\n');
        assert.equal(readFileSync(source, 'utf8'), 'incoming commitment\n');
      }
    },
  },
  {
    name: 'landing preserves exact branches and refuses symbolic or differently cased destinations',
    run(ctx) {
      const original = ctx.git(['rev-parse', 'HEAD']);
      ctx.git(['branch', 'destination']);
      ctx.git(['symbolic-ref', 'refs/heads/destination-alias', 'refs/heads/destination']);
      const workspace = ctx.spawn('exact-target');
      const source = join(workspace.path, 'tracked.txt');
      writeFileSync(source, 'incoming changes\n');
      ctx.git(['commit', '-qam', 'Incoming changes'], workspace.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], workspace.path);
      for (const [target, reason] of [
        ['destination-alias', /direct local branch/i],
        ['MAIN', /destination local branch does not exist/i],
      ] as const) {
        const args = ['merge', 'exact-target', '--lease', workspace.lease, '--into', target];
        assert.match(String(ctx.fail(args).message), reason);
        assert.equal(ctx.git(['rev-parse', 'HEAD']), original);
        assert.equal(ctx.git(['rev-parse', 'refs/heads/destination']), original);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), tip);
        assert.equal(readFileSync(source, 'utf8'), 'incoming changes\n');
      }
      assert.equal(ctx.git(['symbolic-ref', 'refs/heads/destination-alias']), 'refs/heads/destination');
    },
  },
  {
    name: 'a checked-out destination ref lock leaves files and index intact before retry',
    run(ctx) {
      const sub = join(ctx.repo, 'sub');
      mkdirSync(sub);
      writeFileSync(join(sub, 'file'), 'tracked sparse directory\n');
      ctx.git(['add', 'sub']);
      ctx.git(['commit', '-qm', 'Sparse fixture']);
      const original = ctx.git(['rev-parse', 'HEAD']);
      const workspace = ctx.spawn('locked', ['--sparse', 'sub']);
      assert.equal(ctx.git(['sparse-checkout', 'list'], workspace.path), 'sub');
      writeFileSync(join(workspace.path, 'tracked.txt'), 'incoming edit\n');
      ctx.git(['commit', '-qam', 'Incoming changes'], workspace.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], workspace.path);
      const admin = ctx.git(['rev-parse', '--absolute-git-dir']);
      const index = join(admin, 'index');
      const before = readFileSync(index);
      const refLock = join(admin, 'refs', 'heads', 'main.lock');
      const land = ['merge', 'locked', '--lease', workspace.lease, '--into', 'main'];
      writeFileSync(refLock, 'fixture ref lock\n');
      try {
        assert.match(String(ctx.fail(land).message), /refs[/\\]heads[/\\]main\.lock|destination branch is locked/i);
        assert.equal(readFileSync(refLock, 'utf8'), 'fixture ref lock\n');
        assert.equal(ctx.git(['rev-parse', 'HEAD']), original);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), tip);
        assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'initial\n');
        assert.deepEqual(readFileSync(index), before);
      } finally {
        unlinkSync(refLock);
      }
      ctx.cli(land);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), tip);
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'incoming edit\n');
    },
  },
  {
    name: 'mutating commands and GC preserve foreign, corrupt and locked checkouts',
    run(ctx) {
      const owned = (command: string, workspace: { name: string; lease: string }, ...args: string[]) =>
        [command, workspace.name, '--lease', workspace.lease, ...args];
      const ws = { name: 'replaced', ...ctx.spawn('replaced') };
      const locked = { name: 'retained', ...ctx.spawn('retained', ['--lock', '--reason', 'keep this workspace']) };
      const expired = { name: 'expired', ...ctx.spawn('expired') };
      const dirty = { name: 'dirty', ...ctx.spawn('dirty') };
      writeFileSync(join(dirty.path, 'local.txt'), 'expired private checkout data');
      // Age these fixture records without waiting for a real lease to expire.
      const listing = ctx.cli(['list']);
      assert(Array.isArray(listing.workspaces));
      for (const workspace of listing.workspaces.map(objectResult)) {
        const id = workspace.id;
        assert(typeof id === 'string' && /^[a-f0-9-]{36}$/i.test(id));
        const path = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', `${id}.json`);
        const record = objectResult(parseExact(readFileSync(path, 'utf8')));
        writeFileSync(path, JSON.stringify({ ...record, heartbeat: 0 }));
      }
      const original = join(ctx.root, 'original workspace');
      renameSync(ws.path, original);
      ctx.git(['clone', '-q', '--no-local', ctx.repo, ws.path]);
      writeFileSync(join(ws.path, 'local.txt'), 'unrelated repository data');
      const foreignHead = ctx.git(['rev-parse', 'HEAD'], ws.path);
      const reject = () => {
        ctx.fail(owned('exec', ws, '--', process.execPath, '-e', 'process.exit(0)'));
        ctx.fail(owned('remove', ws, '--discard'));
        ctx.fail(owned('merge', ws));
        ctx.fail(['move', ws.name, join(ctx.root, 'unexpected move'), '--lease', ws.lease]);
      };
      reject();
      ctx.fail(['repair', ws.name, ws.path, '--lease', ws.lease]);
      const preview = ctx.cli(['prune']);
      assert.equal(preview.applied, false);
      assert.equal(readFileSync(join(expired.path, 'tracked.txt'), 'utf8'), 'initial\n');
      const applied = ctx.cli(['prune', '--apply']);
      assert.equal(applied.applied, true);
      for (const result of [preview, applied]) {
        assert(!JSON.stringify(result).includes(ws.lease));
        assert(!JSON.stringify(result).includes(locked.lease));
        assert(!JSON.stringify(result).includes(expired.lease));
        assert(!JSON.stringify(result).includes(dirty.lease));
        assert(Array.isArray(result.retained));
        const reasons = Object.fromEntries(result.retained.map(objectResult).map(workspace => [workspace.name, workspace.reason]));
        assert.deepEqual(reasons, {
          [ws.name]: 'missing_or_replaced', [locked.name]: 'locked',
          [expired.name]: result.applied ? 'archived' : 'eligible', [dirty.name]: 'dirty',
        });
      }
      assert(!existsSync(expired.path));
      ctx.cli(owned('restore', expired));
      assert.equal(readFileSync(join(expired.path, 'tracked.txt'), 'utf8'), 'initial\n');
      assert.equal(readFileSync(join(dirty.path, 'local.txt'), 'utf8'), 'expired private checkout data');
      assert.equal(readFileSync(join(ws.path, 'local.txt'), 'utf8'), 'unrelated repository data');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), foreignHead);
      assert.equal(ctx.cli(['status', locked.name]).locked, 'keep this workspace');
      ctx.git(['fsck', '--full'], locked.path);

      const foreign = join(ctx.root, 'foreign checkout');
      renameSync(ws.path, foreign);
      renameSync(original, ws.path);
      const head = join(ws.path, '.git', 'HEAD');
      const backup = readFileSync(head);
      writeFileSync(head, 'corrupt HEAD\n');
      reject();
      assert.equal(readFileSync(head, 'utf8'), 'corrupt HEAD\n');
      writeFileSync(head, backup);
      ctx.git(['fsck', '--full'], ws.path);
      assert.equal(readFileSync(join(foreign, 'local.txt'), 'utf8'), 'unrelated repository data');
    },
  },
  {
    name: 'retirement, move, replay and GC protect native linked checkouts of the workspace',
    run(ctx) {
      const ws = ctx.spawn('parent');
      const linked = join(ctx.root, 'native child');
      ctx.git(['worktree', 'add', '-qb', 'external', linked], ws.path);
      const base = ctx.git(['rev-parse', 'HEAD']);
      const gitfile = readFileSync(join(linked, '.git'));
      const local = join(linked, 'local.txt');
      writeFileSync(local, 'native checkout data\n');
      const owned = ['parent', '--lease', ws.lease];
      const destination = join(ctx.root, 'workspace moved after native checkout removal');
      const move = ['move', ...owned, destination];
      const assertPreserved = () => {
        assert.equal(ctx.git(['rev-parse', 'HEAD']), base);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], linked), base);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), base);
        assert.deepEqual(readFileSync(join(linked, '.git')), gitfile);
        assert.equal(readFileSync(local, 'utf8'), 'native checkout data\n');
        assert.equal(existsSync(destination), false);
      };
      for (const args of [
        ['remove', ...owned, '--discard'],
        ['merge', ...owned],
        move,
      ]) {
        assert.match(String(ctx.fail(args).message), /external Git linked checkouts/i);
        assertPreserved();
      }

      assert(typeof ws.result.id === 'string');
      const record = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', ws.result.id + '.json');
      const expired = { ...objectResult(parseExact(readFileSync(record, 'utf8'))), heartbeat: 0 };
      writeFileSync(record, JSON.stringify(expired));
      for (const options of [[], ['--apply']]) {
        const { retained } = ctx.cli(['prune', ...options]);
        assert(Array.isArray(retained));
        const row = retained.map(objectResult).find(entry => entry.name === 'parent');
        assert(row);
        assert.equal(row.reason, 'linked_checkouts');
        assert.equal(row.path, ws.path);
        assertPreserved();
      }

      // Simulate a native child appearing after a move is journaled but before the rename.
      writeFileSync(record, JSON.stringify({ ...expired, transition: { action: 'move', to: destination } }));
      assert.match(String(ctx.fail(move).message), /external Git linked checkouts/i);
      assertPreserved();

      unlinkSync(local);
      ctx.git(['worktree', 'remove', linked], ws.path);
      assert.equal(ctx.cli(move).path, destination);
      assert.equal(existsSync(ws.path), false);
      // The native checkout's branch stays behind; remove keeps it unless discarded.
      assert.match(String(ctx.fail(['remove', ...owned]).message), /branch external/);
      ctx.git(['branch', '-qD', 'external'], destination);
      ctx.cli(['remove', ...owned]);
      ctx.cli(['restore', ...owned]);
      ctx.git(['fsck', '--full'], destination);
    },
  },
  {
    name: 'relocation and GC protect an external Git checkout nested inside a workspace',
    run(ctx) {
      const ws = ctx.spawn('nested-native');
      const owned = ['nested-native', '--lease', ws.lease];
      const child = join(ws.path, 'nested');
      ctx.git(['worktree', 'add', '--detach', child, 'HEAD']);
      const base = ctx.git(['rev-parse', 'HEAD'], child);
      const backpointer = join(ctx.git(['rev-parse', '--absolute-git-dir'], child), 'gitdir');
      writeFileSync(join(child, 'tracked.txt'), 'staged child edit\n');
      ctx.git(['add', '--', 'tracked.txt'], child);
      writeFileSync(join(child, 'tracked.txt'), 'unstaged child edit\n');
      writeFileSync(join(child, 'local.txt'), 'untracked child data\n');
      const status = ctx.git(['status', '--porcelain'], child);
      assert.match(status, /^MM tracked\.txt$/m);
      assert.match(status, /^\?\? local\.txt$/m);
      const assertPreserved = (path: string, marker: Buffer, pointer: Buffer) => {
        assert.deepEqual(readFileSync(join(path, '.git')), marker);
        assert.deepEqual(readFileSync(backpointer), pointer);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], path), base);
        assert.equal(ctx.git(['status', '--porcelain'], path), status);
        assert.equal(ctx.git(['show', ':tracked.txt'], path), 'staged child edit');
        assert.equal(readFileSync(join(path, 'tracked.txt'), 'utf8'), 'unstaged child edit\n');
        assert.equal(readFileSync(join(path, 'local.txt'), 'utf8'), 'untracked child data\n');
      };
      const marker = readFileSync(join(child, '.git'));
      const pointer = readFileSync(backpointer);
      const relocated = join(ctx.root, 'manually moved parent');
      for (const args of [
        ['remove', ...owned, '--discard'],
        ['merge', ...owned],
        ['move', ...owned, relocated],
      ]) {
        const failure = ctx.fail(args);
        assert.equal(failure.code, 'nested_git_metadata');
        assert.equal(objectResult(failure.details).path, join(child, '.git'));
        assertPreserved(child, marker, pointer);
        assert.equal(existsSync(relocated), false);
      }

      assert(typeof ws.result.id === 'string');
      const record = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', ws.result.id + '.json');
      const expired = { ...objectResult(parseExact(readFileSync(record, 'utf8'))), heartbeat: 0 };
      writeFileSync(record, JSON.stringify(expired));
      for (const options of [[], ['--apply']]) {
        const { retained } = ctx.cli(['prune', ...options]);
        assert(Array.isArray(retained));
        assert.equal(retained.map(objectResult).find(entry => entry.name === 'nested-native')?.reason,
          'nested_git_metadata');
        assertPreserved(child, marker, pointer);
      }

      renameSync(ws.path, relocated);
      const movedChild = join(relocated, 'nested');
      ctx.git(['worktree', 'repair', movedChild]);
      const movedMarker = readFileSync(join(movedChild, '.git'));
      const movedPointer = readFileSync(backpointer);
      assert.notDeepEqual(movedPointer, pointer);
      const repair = ['repair', ...owned, relocated];
      const failure = ctx.fail(repair);
      assert.equal(failure.code, 'nested_git_metadata');
      assert.equal(objectResult(failure.details).path, join(movedChild, '.git'));
      assertPreserved(movedChild, movedMarker, movedPointer);

      const external = join(ctx.root, 'external child');
      ctx.git(['worktree', 'move', movedChild, external]);
      const externalMarker = readFileSync(join(external, '.git'));
      const externalPointer = readFileSync(backpointer);
      assert.equal(ctx.cli(repair).path, relocated);
      ctx.cli(['remove', ...owned]);
      assertPreserved(external, externalMarker, externalPointer);
      ctx.git(['fsck', '--full']);
    },
  },
  {
    name: 'a moved primary fails closed and its grafts and archives return when it is moved back',
    run(ctx) {
      const env = environment(ctx);
      const moved = join(ctx.root, 'moved project');
      renameSync(ctx.repo, moved);
      const before = snapshot(join(moved, '.git', 'graft'), env.root);
      for (const args of [
        ['list'], ['status', 'live'], ['merge', 'live', '--lease', env.live.lease],
        ['remove', 'live', '--lease', env.live.lease, '--discard'], ['prune', '--apply'],
        ['restore', 'archived', '--lease', env.archived.lease], ['add', 'another'],
        ['repair', 'live', env.live.path, '--lease', env.live.lease],
      ]) {
        const result = graftAt(ctx, moved, args);
        assert.equal(result.ok, false, `${args[0]} succeeded after the primary moved`);
        assert.match(String(result.json.message), /different primary/i, args[0]);
        assert.equal(result.json.code, 'primary_moved', args[0]);
      }
      assert.deepEqual(snapshot(join(moved, '.git', 'graft'), env.root), before);
      assert.equal(existsSync(moved + '.graft'), false, 'a graft root was created beside the moved primary');
      renameSync(moved, ctx.repo);
      assertEnvironmentIntact(ctx, env);
      assert.equal(readFileSync(join(String(ctx.cli(['restore', 'archived', '--lease', env.archived.lease]).path), 'draft.txt'), 'utf8'), 'archived draft\n');
      rmSync(join(env.live.path, 'notes.txt'));
      assert.equal(ctx.cli(['merge', 'live', '--lease', env.live.lease]).commit, env.tip);
    },
  },
  {
    name: 'deleting the Graft state leaves grafts and archives as plain recoverable repositories',
    run(ctx) {
      const env = environment(ctx);
      rmSync(join(ctx.repo, '.git', 'graft'), { recursive: true, force: true });
      const before = snapshot(env.root);
      assert.deepEqual(ctx.cli(['list']).workspaces, []);
      // The new state owns nothing, so the old root and names are never
      // adopted, replaced or pruned.
      for (const name of ['live', 'fresh']) {
        assert.match(String(ctx.fail(['add', name]).message), /graft root belongs to another repository/i, name);
      }
      assert.match(String(ctx.fail(['prune', '--apply']).message), /graft root belongs to another repository/i);
      assert.deepEqual(snapshot(env.root), before);
      assertEnvironmentIntact(ctx, env);
      // The work is still reachable with plain Git.
      ctx.git(['fetch', '-q', env.live.path, 'HEAD:refs/heads/recovered']);
      assert.equal(ctx.git(['show', 'recovered:tracked.txt']), 'committed graft work');
    },
  },
  {
    name: 'a renamed graft root fails closed and recovers when renamed back',
    run(ctx) {
      const env = environment(ctx);
      const aside = env.root + ' aside';
      renameSync(env.root, aside);
      const records = join(ctx.repo, '.git', 'graft', 'workspaces');
      const before = snapshot(records, aside);
      for (const args of [
        ['status', 'live'], ['merge', 'live', '--lease', env.live.lease],
        ['remove', 'live', '--lease', env.live.lease, '--discard'],
        ['restore', 'archived', '--lease', env.archived.lease],
      ]) {
        assert.match(String(ctx.fail(args).message), /graft root/i, args[0]);
      }
      const recordFiles = join(records, 'records');
      for (const file of readdirSync(recordFiles)) {
        const path = join(recordFiles, file);
        writeFileSync(path, JSON.stringify({ ...objectResult(parseExact(readFileSync(path, 'utf8'))), heartbeat: 0 }));
      }
      const aged = snapshot(records, aside);
      const pruned = ctx.cli(['prune', '--apply']);
      assert(Array.isArray(pruned.retained));
      assert(pruned.retained.map(objectResult).every(w => w.reason !== 'archived'), JSON.stringify(pruned));
      assert.deepEqual(snapshot(records, aside), aged);
      assert.notDeepEqual(aged, before);
      assert.equal(existsSync(env.root), false);
      renameSync(aside, env.root);
      assertEnvironmentIntact(ctx, env);
      assert.equal(readFileSync(join(String(ctx.cli(['restore', 'archived', '--lease', env.archived.lease]).path), 'draft.txt'), 'utf8'), 'archived draft\n');
      assert.equal(ctx.cli(['status', 'live']).name, 'live');
    },
  },
  {
    name: 'git gc --prune=now in the primary keeps every graft and archive intact',
    run(ctx) {
      const grafts = (['local', 'isolated'] as const).map(history => {
        const ws = ctx.spawn(history, ['--history', history]);
        writeFileSync(join(ws.path, 'tracked.txt'), `${history} committed\n`);
        ctx.git(['commit', '-qam', `${history} work`], ws.path);
        writeFileSync(join(ws.path, 'tracked.txt'), `${history} stashed\n`);
        ctx.git(['stash', '-q'], ws.path);
        writeFileSync(join(ws.path, 'tracked.txt'), `${history} uncommitted\n`);
        return { history, ws, tip: ctx.git(['rev-parse', 'HEAD'], ws.path) };
      });
      const archived = ctx.spawn('archived-gc');
      writeFileSync(join(archived.path, 'tracked.txt'), 'archived commit\n');
      ctx.git(['commit', '-qam', 'Archived work'], archived.path);
      const archivedTip = ctx.git(['rev-parse', 'HEAD'], archived.path);
      ctx.cli(['remove', 'archived-gc', '--lease', archived.lease, '--discard']);
      // Make primary objects unreachable, then prune them immediately.
      ctx.git(['checkout', '-qb', 'temporary']);
      writeFileSync(join(ctx.repo, 'temporary.txt'), 'temporary\n');
      ctx.git(['add', 'temporary.txt']);
      ctx.git(['commit', '-qm', 'Temporary']);
      ctx.git(['checkout', '-q', 'main']);
      ctx.git(['branch', '-qD', 'temporary']);
      ctx.git(['reflog', 'expire', '--expire=now', '--all']);
      ctx.git(['gc', '-q', '--prune=now']);
      for (const { history, ws, tip } of grafts) {
        ctx.git(['fsck', '--full'], ws.path);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip, history);
        assert.equal(readFileSync(join(ws.path, 'tracked.txt'), 'utf8'), `${history} uncommitted\n`);
        assert.equal(ctx.git(['show', 'stash@{0}:tracked.txt'], ws.path), `${history} stashed`);
        ctx.git(['checkout', '-q', '--', 'tracked.txt'], ws.path);
        ctx.git(['stash', 'drop', '-q'], ws.path);
      }
      const restored = String(ctx.cli(['restore', 'archived-gc', '--lease', archived.lease]).path);
      ctx.git(['fsck', '--full'], restored);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], restored), archivedTip);
      ctx.cli(['merge', 'local', '--lease', grafts[0].ws.lease]);
      assert.equal(ctx.git(['rev-parse', 'main']), grafts[0].tip);
      ctx.git(['fsck', '--full']);
    },
  },
  ...(process.platform === 'win32' ? [{
    name: 'Windows open file handles stop archive, move and restore without losing data',
    async run(ctx) {
      assert.equal(process.platform, 'win32');
      const ws = ctx.spawn('held');
      const file = join(ws.path, 'held.txt');
      writeFileSync(file, 'open in an editor\n');
      const attempt = async (args: string[], target: string) => {
        const release = await holdOpen(ctx, target);
        let refused: ReturnType<typeof graftAt>;
        try {
          refused = graftAt(ctx, ctx.repo, args);
        } finally {
          await release();
        }
        assert.equal(refused.ok, false, `${args[0]} succeeded while a file was held open`);
        assert.equal(readFileSync(target, 'utf8'), 'open in an editor\n');
        const retried = graftAt(ctx, ctx.repo, args);
        assert.equal(retried.ok, true, JSON.stringify(retried.json));
        return retried.json;
      };
      const moved = join(dirname(ws.path), 'held-moved');
      await attempt(['move', 'held', moved, '--lease', ws.lease], file);
      assert.equal(readFileSync(join(moved, 'held.txt'), 'utf8'), 'open in an editor\n');
      await attempt(['remove', 'held', '--lease', ws.lease, '--discard'], join(moved, 'held.txt'));
      assert.equal(existsSync(moved), false);
      const listing = ctx.cli(['list']);
      assert(Array.isArray(listing.recoverable));
      const archive = String(objectResult(listing.recoverable.map(objectResult).find(w => w.name === 'held')).archive_path);
      const restored = await attempt(['restore', 'held', '--lease', ws.lease], join(archive, 'held.txt'));
      assert.equal(readFileSync(join(String(restored.path), 'held.txt'), 'utf8'), 'open in an editor\n');
      // A merge archives after updating the destination; the retry completes it.
      const lease = String(restored.lease);
      const path = String(restored.path);
      ctx.git(['add', 'held.txt'], path);
      ctx.git(['commit', '-qm', 'Held file'], path);
      const tip = ctx.git(['rev-parse', 'HEAD'], path);
      const merged = await attempt(['merge', 'held', '--lease', lease], join(path, 'held.txt'));
      assert.equal(merged.commit, tip);
      assert.equal(ctx.git(['rev-parse', 'main']), tip);
      assert.equal(readFileSync(join(ctx.repo, 'held.txt'), 'utf8'), 'open in an editor\n');
    },
  } satisfies Scenario] : []),
];
