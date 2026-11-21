import assert from 'node:assert/strict';
import { execFile, spawn as spawnProcess } from 'node:child_process';
import { appendFileSync, chmodSync, existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { promisify } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';
import { Running, waitFor } from './checkout-faults.ts';
import { leaveIndexedCheckoutJournal } from './checkout-journal.ts';
import { objectResult, type Scenario } from './harness.ts';

type Context = Parameters<Scenario['run']>[0];
type Workspace = ReturnType<Context['spawn']>;

function write(root: string, path: string, content: string): void {
  const file = join(root, path);
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, content);
}
function read(root: string, path: string): string {
  return readFileSync(join(root, path), 'utf8');
}
function commit(ctx: Context, root: string, message: string): string {
  ctx.git(['add', '-A'], root);
  ctx.git(['commit', '-qm', message], root);
  return ctx.git(['rev-parse', 'HEAD'], root).trim();
}
function landArgs(workspace: Workspace, name: string, ...extra: string[]): string[] {
  return ['merge', name, '--lease', workspace.lease, ...extra];
}
function field(value: unknown): Record<string, unknown> {
  assert.ok(value && typeof value === 'object' && !Array.isArray(value));
  return value as Record<string, unknown>;
}
function parents(ctx: Context, rev = 'HEAD', root = ctx.repo): string[] {
  return ctx.git(['show', '-s', '--format=%P', rev], root).trim().split(/\s+/).filter(Boolean);
}
function requireNpm(ctx: Context): void {
  const probe = process.platform === 'win32'
    ? ctx.command('where.exe', ['npm.cmd'])
    : ctx.command('npm', ['--version']);
  if (probe.status !== 0) ctx.unavailable(`npm is required for offline reconciliation: ${probe.stderr || probe.error?.message || 'not found'}`);
}
function packageFiles(root: string, item?: { name: string; version: string; section?: string }): void {
  const manifest: Record<string, unknown> = { name: 'fixture', version: '1.0.0' };
  const packageRoot: Record<string, unknown> = { name: 'fixture', version: '1.0.0' };
  const packages: Record<string, unknown> = { '': packageRoot };
  if (item) {
    const section = item.section ?? 'dependencies';
    manifest[section] = { [item.name]: item.version };
    packageRoot[section] = { [item.name]: item.version };
    packages[`node_modules/${item.name}`] = {
      version: item.version,
      resolved: `https://registry.npmjs.org/${item.name}/-/${item.name}-${item.version}.tgz`,
      integrity: 'sha512-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==',
      ...(section === 'devDependencies' ? { dev: true } : {}),
    };
  }
  write(root, 'package.json', JSON.stringify(manifest, null, 2) + '\n');
  write(root, 'package-lock.json', JSON.stringify({ name: 'fixture', version: '1.0.0', lockfileVersion: 3, requires: true, packages }, null, 2) + '\n');
}

export const cases: Scenario[] = [
  {
    name: 'a merge --commit killed while staging releases its graft index lock on retry',
    async run(ctx) {
      const name = 'staging';
      const workspace = ctx.spawn(name);
      const old = ctx.git(['rev-parse', 'HEAD']);
      const lock = join(workspace.path, '.git', 'index.lock');
      // A Git lock that Graft did not create is reported and kept.
      writeFileSync(lock, 'held by another Git command\n');
      const busy = ctx.fail(landArgs(workspace, name, '--commit', 'staged work'));
      assert.equal(busy.code, 'git_busy');
      assert.equal(readFileSync(lock, 'utf8'), 'held by another Git command\n');
      rmSync(lock);

      // A global clean filter blocks git add while Graft holds the lock.
      const fault = join(ctx.root, 'staging fault');
      mkdirSync(fault);
      const dir = fault.replaceAll('\\', '/');
      const script = join(fault, 'clean.sh');
      writeFileSync(script, `#!/bin/sh\nif [ -f '${dir}/armed' ]; then : > '${dir}/ready'; while [ ! -f '${dir}/release' ]; do sleep 0.1; done; fi\nexec cat\n`, { mode: 0o755 });
      const config = ctx.env.GIT_CONFIG_GLOBAL;
      assert(typeof config === 'string');
      appendFileSync(config, `[filter "slow"]\n\tclean = sh '${script.replaceAll('\\', '/')}'\n\tsmudge = cat\n`);
      write(workspace.path, '.git/info/attributes', '*.slow filter=slow\n');
      write(workspace.path, 'feature.slow', 'work staged by merge --commit\n');
      write(workspace.path, 'notes.txt', 'more uncommitted work\n');
      writeFileSync(join(fault, 'armed'), '');
      const merge = new Running(ctx, landArgs(workspace, name, '--commit', 'staged work'));
      try {
        await waitFor(() => existsSync(join(fault, 'ready')) || !merge.running(), 'the staging filter');
        assert(existsSync(join(fault, 'ready')), 'the merge exited before staging');
        merge.killCli();
        await merge.exit();
      } finally {
        rmSync(join(fault, 'armed'));
        writeFileSync(join(fault, 'release'), '');
      }
      assert.match(readFileSync(lock, 'utf8'), /^git-graft staging /);
      assert.equal(read(workspace.path, 'feature.slow'), 'work staged by merge --commit\n');
      assert.equal(read(workspace.path, 'notes.txt'), 'more uncommitted work\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
      assert.equal(ctx.cli(['status', name]).phase, 'ready');

      const receipt = ctx.cli(landArgs(workspace, name, '--commit', 'staged work'));
      assert.equal(receipt.removed, true);
      assert.equal(existsSync(lock), false);
      assert.equal(read(ctx.repo, 'feature.slow'), 'work staged by merge --commit\n');
      assert.equal(read(ctx.repo, 'notes.txt'), 'more uncommitted work\n');
      assert.equal(ctx.git(['show', '-s', '--format=%s', 'HEAD']), 'staged work');
    },
  },
  {
    name: 'interrupted first landing preserves work and retries with the same lease',
    async run(ctx) {
      const name = 'interrupted-process';
      const workspace = ctx.spawn(name);
      const old = ctx.git(['rev-parse', 'HEAD']);
      const workspaceHead = ctx.git(['rev-parse', 'HEAD'], workspace.path);
      write(workspace.path, 'feature.txt', 'private work survives interruption\n');
      const hooks = join(ctx.root, 'interrupt-hooks');
      mkdirSync(hooks);
      const ready = join(ctx.root, 'hook-ready');
      const release = join(ctx.root, 'hook-release');
      const done = join(ctx.root, 'hook-done');
      const hook = join(hooks, 'pre-commit');
      writeFileSync(hook, '#!/bin/sh\nset -eu\nprintf ready > "$GRAFT_TEST_READY"\ni=0\nwhile [ ! -f "$GRAFT_TEST_RELEASE" ] && [ "$i" -lt 30 ]; do sleep 1; i=$((i+1)); done\nprintf done > "$GRAFT_TEST_DONE"\nexit 1\n');
      chmodSync(hook, 0o755);
      ctx.git(['config', '--local', 'core.hooksPath', hooks], workspace.path);
      const args = landArgs(workspace, name, '--commit', 'commit interrupted work');
      const env = { ...ctx.env, GRAFT_TEST_READY: ready.replaceAll('\\', '/'),
        GRAFT_TEST_RELEASE: release.replaceAll('\\', '/'), GRAFT_TEST_DONE: done.replaceAll('\\', '/') };
      const child = spawnProcess(ctx.bin, ['--repo', ctx.repo, '--json', ...args],
        { cwd: ctx.repo, env, stdio: 'ignore', windowsHide: true });
      const exited = new Promise<{code: number | null; signal: NodeJS.Signals | null}>((resolve, reject) => {
        child.once('error', reject);
        child.once('exit', (code, signal) => resolve({code, signal}));
      });
      const waitForExit = async () => {
        let timer: ReturnType<typeof setTimeout> | undefined;
        try {
          return await Promise.race([exited, new Promise<never>((_, reject) => {
            timer = setTimeout(() => reject(new Error('landing child did not exit')), 10_000);
          })]);
        } finally {
          if (timer) clearTimeout(timer);
        }
      };
      const waitFor = async (condition: () => boolean, label: string, timeout = 20_000) => {
        const deadline = Date.now() + timeout;
        while (!condition()) {
          assert(Date.now() < deadline, `timed out waiting for ${label}`);
          await delay(50);
        }
      };
      try {
        await waitFor(() => existsSync(ready) || child.exitCode !== null || child.signalCode !== null, 'pre-commit hook');
        assert(existsSync(ready), 'landing exited before the pre-commit hook');
        assert(child.pid, 'landing child has no PID');
        assert(child.kill('SIGKILL'), 'could not terminate the landing child');
        const result = await waitForExit();
        assert.notEqual(result.code, 0, 'interrupted landing unexpectedly succeeded');
      } finally {
        writeFileSync(release, 'release\n');
        if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
        await waitForExit();
        await waitFor(() => !existsSync(join(workspace.path, '.git', 'index.lock')), 'Git commit to release its index lock');
        if (existsSync(ready)) {
          await waitFor(() => existsSync(done), 'hook completion');
        }
      }
      assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
      assert.equal(ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft']), '');
      assert.equal(read(ctx.repo, 'tracked.txt'), 'initial\n');
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), workspaceHead);
      assert.equal(read(workspace.path, 'feature.txt'), 'private work survives interruption\n');
      assert.equal(ctx.git(['show', ':feature.txt'], workspace.path), 'private work survives interruption');
      const plans = join(ctx.git(['rev-parse', '--absolute-git-dir']), 'graft', 'land-plans');
      const planning = readdirSync(plans).filter(entry => entry.startsWith('.planning-'));
      assert.equal(planning.length, 1, 'interrupted planning scratch should remain');
      ctx.git(['config', '--local', '--unset', 'core.hooksPath'], workspace.path);
      const retry = ctx.command(ctx.bin, ['--repo', ctx.repo, '--json', ...args]);
      assert.equal(retry.status, 0, retry.error?.message ?? (retry.stderr || retry.stdout));
      assert(retry.stderr.includes(planning[0]), 'retry should report retained planning scratch');
      assert(retry.stderr.length < 64 * 1024, 'scratch diagnostic should remain bounded');
      assert(existsSync(join(plans, planning[0])), 'ambiguous planning scratch must be preserved');
      const landed = field(JSON.parse(retry.stdout));
      assert.equal(landed.recovery_lease, workspace.lease);
      assert.equal(landed.previous, old);
      assert.equal(read(ctx.repo, 'feature.txt'), 'private work survives interruption\n');
      ctx.git(['fsck', '--full']);
      const restored = ctx.cli(['restore', name, '--lease', workspace.lease]);
      assert.equal(restored.path, workspace.path);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), landed.graft_tip);
      assert.equal(read(workspace.path, 'feature.txt'), 'private work survives interruption\n');
      ctx.git(['fsck', '--full'], workspace.path);
    },
  },
  {
    name: 'landing fast-forward preserves workspace commit',
    run(ctx) {
      const workspace = ctx.spawn('ff');
      const old = ctx.git(['rev-parse', 'HEAD']).trim();
      write(workspace.path, 'feature.txt', 'feature\n');
      const tip = commit(ctx, workspace.path, 'feature');
      const args = landArgs(workspace, 'ff');
      const result = ctx.cli(args);
      assert.equal(result.recovery_lease, workspace.lease);
      assert.equal(result.commit, tip);
      assert.equal(result.previous, old);
      assert.deepEqual(ctx.cli(args), result);
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), tip);
      assert.deepEqual(parents(ctx), [old]);
      assert.equal(read(ctx.repo, 'feature.txt'), 'feature\n');
      assert.equal(existsSync(workspace.path), false);
      ctx.git(['fsck', '--full']);
    },
  },
  ...[false, true].map((squash): Scenario => ({
    name: squash ? 'landing squash creates one parent' : 'landing divergence creates two parents',
    run(ctx) {
      const workspace = ctx.spawn(squash ? 'squash' : 'merge');
      write(ctx.repo, 'ours.txt', 'ours\n');
      const old = commit(ctx, ctx.repo, 'ours');
      let privateBlob: string | undefined;
      if (squash) {
        write(workspace.path, 'scratch.txt', 'private intermediate work\n');
        commit(ctx, workspace.path, 'private intermediate work');
        privateBlob = ctx.git(['rev-parse', 'HEAD:scratch.txt'], workspace.path);
        ctx.git(['rm', '--', 'scratch.txt'], workspace.path);
      }
      write(workspace.path, 'theirs.txt', 'theirs\n');
      const tip = commit(ctx, workspace.path, 'theirs');
      const name = squash ? 'squash' : 'merge';
      const args = landArgs(workspace, name, ...(squash ? ['--squash'] : []));
      const result = ctx.cli(args);
      assert.equal(result.graft_tip, tip);
      assert.equal(result.previous, old);
      assert.deepEqual(ctx.cli(args), result);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), result.commit);
      assert.deepEqual(parents(ctx), squash ? [old] : [old, tip]);
      assert.equal(read(ctx.repo, 'ours.txt'), 'ours\n');
      assert.equal(read(ctx.repo, 'theirs.txt'), 'theirs\n');
      assert.equal(ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft']), '');
      if (privateBlob) {
        for (const oid of [tip, privateBlob]) {
          const check = ctx.command('git', ['cat-file', '-e', oid]);
          assert.ifError(check.error);
          assert.equal(check.status, 1, `primary contains private Git object ${oid}`);
        }
      }
      assert.equal(ctx.git(['status', '--porcelain']), '');
      const replacement = ctx.spawn(name);
      write(replacement.path, 'local.txt', 'replacement data\n');
      assert.deepEqual(ctx.cli(args), result);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), result.commit);
      assert.equal(ctx.cli(['status', name]).id, replacement.result.id);
      assert.equal(read(replacement.path, 'local.txt'), 'replacement data\n');
      ctx.cli(['remove', name, '--lease', replacement.lease, '--discard']);
      ctx.git(['gc', '--prune=now']);
      ctx.git(['fsck', '--full']);
      const restored = ctx.cli(['restore', name, '--lease', workspace.lease]);
      assert.equal(restored.path, workspace.path);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), tip);
      if (privateBlob) assert.equal(ctx.git(['cat-file', 'blob', privateBlob], workspace.path), 'private intermediate work');
      ctx.git(['fsck', '--full'], workspace.path);
    },
  })),
  {
    name: 'landing protects dirty destination and ignored collision',
    run(ctx) {
      const workspace = ctx.spawn('dirty');
      write(workspace.path, 'collision.txt', 'landed\n');
      const tip = commit(ctx, workspace.path, 'feature');
      const old = ctx.git(['rev-parse', 'HEAD']).trim();
      write(ctx.repo, 'tracked.txt', 'staged user edit\n');
      ctx.git(['add', 'tracked.txt']);
      write(ctx.repo, 'tracked.txt', 'local edit\n');
      assert.match(String(ctx.fail(landArgs(workspace, 'dirty')).message), /local changes/i);
      assert.equal(read(ctx.repo, 'tracked.txt'), 'local edit\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), old);
      assert.equal(ctx.git(['show', ':tracked.txt']), 'staged user edit');
      ctx.git(['restore', '--source=HEAD', '--staged', '--worktree', '--', 'tracked.txt']);
      const gitDir = ctx.git(['rev-parse', '--absolute-git-dir']).trim();
      write(gitDir, 'info/exclude', 'collision.txt\n');
      write(ctx.repo, 'collision.txt', 'ignored user data\n');
      assert.match(String(ctx.fail(landArgs(workspace, 'dirty')).message), /collision\.txt/i);
      assert.equal(read(ctx.repo, 'collision.txt'), 'ignored user data\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), old);
      rmSync(join(ctx.repo, 'collision.txt'));
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), tip);
      assert.equal(ctx.cli(landArgs(workspace, 'dirty')).commit, tip);
      assert.equal(read(ctx.repo, 'collision.txt'), 'landed\n');
    },
  },
  {
    name: 'landing protects a native linked destination and retries after it is removed',
    run(ctx) {
      const workspace = ctx.spawn('linked');
      const dest = join(ctx.root, 'destination with spaces');
      ctx.git(['worktree', 'add', '-b', 'target', dest]);
      const main = ctx.git(['rev-parse', 'HEAD']);
      write(workspace.path, 'feature.txt', 'feature\n');
      const tip = commit(ctx, workspace.path, 'feature');
      const args = landArgs(workspace, 'linked', '--into', 'target');
      ctx.git(['worktree', 'lock', dest]);
      assert.match(String(ctx.fail(args).message), /locked/i);
      ctx.git(['worktree', 'unlock', dest]);
      assert.match(String(ctx.fail(args).message), /external linked worktree/i);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], dest), main);
      assert.equal(ctx.git(['status', '--porcelain'], dest), '');
      assert.equal(read(dest, 'tracked.txt'), 'initial\n');
      assert.equal(existsSync(join(dest, 'feature.txt')), false);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), tip);

      ctx.git(['worktree', 'remove', dest]);
      const landed = ctx.cli(args);
      assert.equal(landed.commit, tip);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), main);
      assert.equal(ctx.git(['show', 'target:feature.txt']), 'feature');
      ctx.git(['fsck', '--full']);
    },
  },
  {
    name: 'landing an unchecked branch leaves an unrelated native worktree intact',
    run(ctx) {
      ctx.git(['branch', 'target']);
      const dest = join(ctx.root, 'unrelated native worktree');
      ctx.git(['worktree', 'add', '-b', 'other', dest]);
      write(dest, 'tracked.txt', 'native staged edit\n');
      ctx.git(['add', 'tracked.txt'], dest);
      write(dest, 'tracked.txt', 'native local edit\n');
      const index = ctx.git(['rev-parse', '--path-format=absolute', '--git-path', 'index'], dest);
      const indexBefore = readFileSync(index);
      const workspace = ctx.spawn('native-other');
      write(workspace.path, 'feature.txt', 'workspace feature\n');
      const tip = commit(ctx, workspace.path, 'feature');
      const old = ctx.git(['rev-parse', 'HEAD']);
      assert.equal(ctx.cli(landArgs(workspace, 'native-other', '--into', 'target')).commit, tip);
      assert.equal(ctx.git(['rev-parse', 'target']), tip);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], dest), old);
      assert.deepEqual(readFileSync(index), indexBefore);
      assert.equal(ctx.git(['show', ':tracked.txt'], dest), 'native staged edit');
      assert.equal(read(dest, 'tracked.txt'), 'native local edit\n');
      assert.equal(existsSync(join(dest, 'feature.txt')), false);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
    },
  },
  {
    name: 'unchecked branch ref lock rejects landing and retry replans',
    run(ctx) {
      ctx.git(['branch', 'target']);
      const workspace = ctx.spawn('cas');
      write(workspace.path, 'feature.txt', 'feature\n');
      const tip = commit(ctx, workspace.path, 'feature');
      const original = ctx.git(['rev-parse', 'target']).trim();
      const common = ctx.git(['rev-parse', '--path-format=absolute', '--git-common-dir']).trim();
      const refLock = join(common, 'refs', 'heads', 'target.lock');
      writeFileSync(refLock, 'held\n');
      try {
        assert.match(String(ctx.fail(landArgs(workspace, 'cas', '--into', 'target')).message), /lock|reference|update-ref/i);
        assert.equal(ctx.git(['rev-parse', 'target']).trim(), original);
        assert.equal(read(workspace.path, 'feature.txt'), 'feature\n');
      } finally { rmSync(refLock, { force: true }); }
      write(ctx.repo, 'other.txt', 'other\n');
      const advanced = commit(ctx, ctx.repo, 'advance');
      ctx.git(['update-ref', 'refs/heads/target', advanced, original]);
      ctx.cli(landArgs(workspace, 'cas', '--into', 'target'));
      assert.deepEqual(parents(ctx, 'target'), [advanced, tip]);
      assert.equal(ctx.git(['show', 'target:feature.txt']).trim(), 'feature');
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), advanced);
    },
  },
  {
    name: 'concurrent first landings preserve both private commits on distinct destination branches',
    async run(ctx) {
      const primary = ctx.git(['rev-parse', 'HEAD']);
      const tasks = ['alpha', 'beta'].map(name => {
        ctx.git(['branch', name, primary]);
        const ws = ctx.spawn(name);
        write(ws.path, name, name + '\n');
        return { name, ws, tip: commit(ctx, ws.path, name) };
      });
      const run = promisify(execFile);
      // Finish both processes before inspecting refs or releasing the fixture.
      const attempts = await Promise.allSettled(tasks.map(({ name, ws }) =>
        run(ctx.bin, ['--repo', ctx.repo, '--json', ...landArgs(ws, name, '--into', name)],
          { cwd: ctx.repo, env: ctx.env, timeout: 60_000 })));
      for (const [i, result] of attempts.entries()) {
        // The public command arguments contain the lease. Keep diagnostics to stderr.
        assert.equal(result.status, 'fulfilled', result.status === 'rejected' ? String(result.reason?.stderr ?? 'landing failed') : undefined);
        const { name, ws, tip } = tasks[i];
        assert.equal(field(JSON.parse(result.value.stdout)).commit, tip);
        assert.equal(ctx.git(['rev-parse', name]), tip);
        assert.deepEqual(parents(ctx, name), [primary]);
        const restored = ctx.cli(['restore', name, '--lease', ws.lease]);
        assert.equal(read(String(restored.path), name), name + '\n');
      }
      assert.equal(ctx.git(['rev-parse', 'HEAD']), primary);
      ctx.git(['fsck', '--full']);
    },
  },
  ...[false, true].map((overlap): Scenario => ({
    name: overlap ? 'two grafts can resolve and retry competing edits to one line'
      : 'two grafts can land independent edits to the same tracked file',
    run(ctx) {
      const file = 'settings.txt';
      const base = ['first=1', ...Array.from({length: 10}, (_, i) => 'setting' + i), 'last=1', ''].join('\n');
      write(ctx.repo, file, base);
      commit(ctx, ctx.repo, 'base settings');
      const a = ctx.spawn('settings-a');
      const b = ctx.spawn('settings-b');
      const aText = base.replace('first=1', 'first=2');
      const bText = overlap ? base.replace('first=1', 'first=3') : base.replace('last=1', 'last=3');
      write(a.path, file, aText);
      assert.equal(read(b.path, file), base);
      write(b.path, file, bText);
      assert.equal(read(a.path, file), aText);
      assert.equal(read(ctx.repo, file), base);
      const aTip = commit(ctx, a.path, 'change first setting');
      const bTip = commit(ctx, b.path, 'second task settings');
      ctx.cli(landArgs(a, 'settings-a'));
      assert.equal(read(ctx.repo, file), aText);
      assert.equal(read(b.path, file), bText);

      let combined = aText.replace('last=1', 'last=3');
      if (overlap) {
        const conflict = ctx.fail(landArgs(b, 'settings-b'));
        assert.equal(conflict.code, 'merge_conflict');
        assert.deepEqual(field(conflict.details).paths, [file]);
        assert.equal(ctx.git(['rev-parse', 'HEAD']), aTip);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], b.path), bTip);
        assert.equal(read(b.path, file), bText);
        ctx.git(['fetch', '--no-tags', '--', ctx.repo, 'refs/heads/main'], b.path);
        const merge = ctx.command('git', ['merge', '--no-edit', 'FETCH_HEAD'], b.path);
        assert.equal(merge.status, 1);
        assert.equal(ctx.git(['diff', '--name-only', '--diff-filter=U'], b.path), file);
        combined = base.replace('first=1', 'first=4');
        write(b.path, file, combined);
        commit(ctx, b.path, 'resolve both proposed values');
      }
      ctx.cli(landArgs(b, 'settings-b'));
      assert.equal(read(ctx.repo, file), combined);
      for (const tip of [aTip, bTip]) ctx.git(['merge-base', '--is-ancestor', tip, 'HEAD']);
      ctx.git(['fsck', '--full']);
    },
  })),
  {
    name: 'landing reports real conflict paths and preserves unmerged work',
    run(ctx) {
      const workspace = ctx.spawn('conflict');
      write(ctx.repo, 'tracked.txt', 'ours\n');
      const old = commit(ctx, ctx.repo, 'ours');
      write(workspace.path, 'private.txt', 'discarded intermediate content\n');
      const intermediate = commit(ctx, workspace.path, 'private intermediate');
      const privateBlob = ctx.git(['rev-parse', 'HEAD:private.txt'], workspace.path);
      rmSync(join(workspace.path, 'private.txt'));
      write(workspace.path, 'tracked.txt', 'theirs\n');
      const tip = commit(ctx, workspace.path, 'theirs');
      const fault = ctx.fail(landArgs(workspace, 'conflict', '--reconcile'));
      assert.equal(fault.code, 'merge_conflict');
      assert.equal(ctx.fail(landArgs(workspace, 'conflict', '--squash')).code, 'merge_conflict');
      for (const id of [tip, intermediate, privateBlob]) {
        assert.equal(ctx.command('git', ['cat-file', '-e', id], ctx.repo).status, 1);
        assert.equal(ctx.command('git', ['cat-file', '-e', id], workspace.path).status, 0);
      }
      const admin = ctx.git(['rev-parse', '--absolute-git-dir']);
      const format = ctx.git(['rev-parse', '--show-object-format']);
      const store = join(admin, 'graft', 'land-plans', `${format}-full`);
      if (existsSync(store)) assert.equal(ctx.git(['for-each-ref', '--format=%(refname)', 'refs/graft/import'], store), '');
      assert.deepEqual(readdirSync(dirname(store)).filter(name => name.startsWith('.pending-')), [],
        'repeated ordinary conflicts must release their private landing scratch');
      const details = field(fault.details);
      assert.deepEqual(details.paths, ['tracked.txt']);
      assert.ok(Array.isArray(details.conflicts));
      assert.ok(details.conflicts.some((entry: unknown) => {
        const conflict = field(entry);
        return String(conflict.type).includes('CONFLICT') && String(conflict.message).includes('tracked.txt');
      }));
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), old);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path).trim(), tip);
      assert.equal(read(workspace.path, 'tracked.txt'), 'theirs\n');
    },
  },
  {
    name: 'explicit commit runs normal Git hooks',
    run(ctx) {
      const hooks = join(ctx.root, 'hooks');
      mkdirSync(hooks, { recursive: true });
      const hook = join(hooks, 'pre-commit');
      const marker = join(ctx.root, 'hook-ran');
      writeFileSync(hook, '#!/bin/sh\nprintf ran > "$(git config --get graft.testmarker)"\n');
      chmodSync(hook, 0o755);
      const workspace = ctx.spawn('hook');
      ctx.git(['config', 'core.hooksPath', hooks], workspace.path);
      ctx.git(['config', 'graft.testmarker', marker], workspace.path);
      write(workspace.path, 'feature.txt', 'staged by graft\n');
      ctx.cli(landArgs(workspace, 'hook', '--commit', 'workspace commit'));
      assert.equal(readFileSync(marker, 'utf8'), 'ran');
      assert.equal(read(ctx.repo, 'feature.txt'), 'staged by graft\n');
      assert.equal(ctx.git(['show', '-s', '--format=%s', 'HEAD']).trim(), 'workspace commit');
    },
  },
  {
    name: 'landing leaves unmerged workspace index and files intact',
    run(ctx) {
      const workspace = ctx.spawn('unmerged');
      const branch = ctx.git(['branch', '--show-current'], workspace.path).trim();
      ctx.git(['switch', '-c', 'conflicting-side'], workspace.path);
      write(workspace.path, 'tracked.txt', 'side\n');
      commit(ctx, workspace.path, 'side');
      ctx.git(['switch', branch], workspace.path);
      write(workspace.path, 'tracked.txt', 'workspace\n');
      const tip = commit(ctx, workspace.path, 'workspace');
      const merge = ctx.command('git', ['merge', 'conflicting-side'], workspace.path);
      assert.notEqual(merge.status, 0);
      const index = ctx.git(['ls-files', '--unmerged'], workspace.path);
      assert.ok(index.includes('tracked.txt'));
      const contents = read(workspace.path, 'tracked.txt');
      const old = ctx.git(['rev-parse', 'HEAD']).trim();
      ctx.fail(landArgs(workspace, 'unmerged'));
      assert.equal(ctx.git(['ls-files', '--unmerged'], workspace.path), index);
      assert.equal(read(workspace.path, 'tracked.txt'), contents);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path).trim(), tip);
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), old);
    },
  },
  {
    name: 'explicit land commit refuses to overwrite distinct staged and unstaged content',
    run(ctx) {
      const ws = ctx.spawn('partial');
      const base = ctx.git(['rev-parse', 'HEAD']);
      const land = landArgs(ws, 'partial', '--commit', 'land changes');
      const stagedDiff = () => ctx.git(['diff', '--cached', '--binary'], ws.path);
      const assertRejected = (bytes: string) => {
        const index = stagedDiff();
        assert(index.includes('tracked.txt'));
        assert.equal(ctx.fail(land).code, 'partially_staged');
        assert.equal(stagedDiff(), index);
        assert.equal(read(ws.path, 'tracked.txt'), bytes);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), base);
        assert.equal(ctx.git(['rev-parse', 'HEAD']), base);
        assert.equal(read(ctx.repo, 'tracked.txt'), 'initial\n');
      };

      write(ws.path, 'tracked.txt', 'staged content\n');
      ctx.git(['add', '--', 'tracked.txt'], ws.path);
      write(ws.path, 'tracked.txt', 'unstaged content\n');
      assertRejected('unstaged content\n');
      assert.equal(ctx.git(['show', ':tracked.txt'], ws.path), 'staged content');

      ctx.git(['rm', '-f', '--', 'tracked.txt'], ws.path);
      write(ws.path, 'tracked.txt', 'recreated after staging deletion\n');
      assertRejected('recreated after staging deletion\n');
      ctx.git(['add', '--', 'tracked.txt'], ws.path);
      const result = ctx.cli(land);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), result.commit);
      assert.equal(read(ctx.repo, 'tracked.txt'), 'recreated after staging deletion\n');
      ctx.git(['fsck', '--full']);
    },
  },
  {
    name: 'prepared reference hook rejection leaves the checked-out destination untouched',
    run(ctx) {
      const ws = ctx.spawn('hook-reject');
      const base = ctx.git(['rev-parse', 'HEAD']);
      write(ws.path, 'tracked.txt', 'incoming change\n');
      write(ws.path, 'feature.txt', 'new path\n');
      const tip = commit(ctx, ws.path, 'incoming change');
      const hook = '.git/hooks/reference-transaction';
      const marker = join(ctx.root, 'prepared-ref-hook');
      ctx.env.GRAFT_E2E_HOOK_MARKER = marker.replaceAll('\\', '/');
      write(ctx.repo, hook, [
        '#!/bin/sh',
        '[ "$1" = prepared ] || exit 0',
        'while read -r old new reference; do',
        '  if [ "$reference" = refs/heads/main ]; then',
        '    printf "%s\\n" "$new" > "$GRAFT_E2E_HOOK_MARKER"',
        '    exit 1',
        '  fi',
        'done',
        'exit 0',
      ].join('\n') + '\n');
      chmodSync(join(ctx.repo, hook), 0o700);
      const args = landArgs(ws, 'hook-reject');
      const index = readFileSync(join(ctx.repo, '.git', 'index'));
      try {
        assert.match(String(ctx.fail(args).message), /hook|reference/i);
        assert.equal(readFileSync(marker, 'utf8').trim(), tip);
        assert.equal(ctx.git(['rev-parse', 'HEAD']), base);
        assert.deepEqual(readFileSync(join(ctx.repo, '.git', 'index')), index);
        assert.equal(read(ctx.repo, 'tracked.txt'), 'initial\n');
        assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip);
        const unpublished = ctx.command('git', ['cat-file', '-e', tip]);
        assert.ifError(unpublished.error);
        assert.equal(unpublished.status, 1);
      } finally {
        delete ctx.env.GRAFT_E2E_HOOK_MARKER;
        rmSync(join(ctx.repo, hook));
      }
      assert.equal(ctx.cli(args).commit, tip);
      assert.equal(read(ctx.repo, 'tracked.txt'), 'incoming change\n');
      assert.equal(read(ctx.repo, 'feature.txt'), 'new path\n');
    },
  },
  {
    name: 'prune with discard completes an applied landing and preserves undo and later edits',
    run(ctx) {
      const initialTree = ctx.git(['rev-parse', 'HEAD^{tree}']);
      const workspace = ctx.spawn('interrupted');
      const owned = (command: string) => [command, 'interrupted', '--lease', workspace.lease];
      write(workspace.path, 'feature.txt', 'landed feature\n');
      const tip = commit(ctx, workspace.path, 'landed feature');
      const hook = '.git/hooks/post-merge';
      // Simulate a concurrent workspace edit after Git updates the destination.
      write(ctx.repo, hook, '#!/bin/sh\nset -eu\nprintf "%s\\n" "edit during land" > "$GRAFT_E2E_PENDING_PATH/feature.txt"\n');
      chmodSync(join(ctx.repo, hook), 0o700);
      ctx.env.GRAFT_E2E_PENDING_PATH = process.platform === 'win32' ? workspace.path.replaceAll('\\', '/') : workspace.path;
      const land = owned('merge');
      try {
        assert.match(String(ctx.fail(land).message), /local changes/);
      } finally {
        delete ctx.env.GRAFT_E2E_PENDING_PATH;
        rmSync(join(ctx.repo, hook));
      }
      assert.equal(ctx.git(['rev-parse', 'HEAD']), tip);
      assert.equal(read(ctx.repo, 'feature.txt'), 'landed feature\n');
      assert.equal(read(workspace.path, 'feature.txt'), 'edit during land\n');
      assert.equal(ctx.cli(['status', 'interrupted']).phase, 'landing');
      ctx.git(['add', '--', 'feature.txt'], workspace.path);
      write(workspace.path, 'feature.txt', 'edit after failure\n');

      assert.match(String(ctx.fail(land).message), /local changes/);
      const drop = owned('remove');
      assert.match(String(ctx.fail(drop).message), /merge is pending/);
      assert.equal(read(workspace.path, 'feature.txt'), 'edit after failure\n');
      const retired = ctx.cli([...drop, '--discard']);
      assert.equal(retired.action, 'merge');
      assert.equal(retired.commit, tip);
      assert.equal(retired.recovery_lease, workspace.lease);
      assert(!existsSync(workspace.path));
      assert.deepEqual(ctx.cli([...drop, '--discard']), retired);

      ctx.cli(owned('restore'));
      const undone = ctx.cli([...owned('merge'), '--undo']);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), undone.commit);
      assert.equal(ctx.git(['rev-parse', 'HEAD^']), tip);
      assert.equal(ctx.git(['rev-parse', 'HEAD^{tree}']), initialTree);
      assert.equal(ctx.git(['show', ':feature.txt'], workspace.path), 'edit during land');
      assert.equal(read(workspace.path, 'feature.txt'), 'edit after failure\n');
      for (const repo of [ctx.repo, workspace.path]) ctx.git(['fsck', '--full'], repo);
    },
  },
  ...[false, true].map((advanced): Scenario => ({
    name: advanced
      ? 'discard completes an indexed landing after the destination advances'
      : 'an indexed landing retry preserves dirty work after the ref is committed',
    run(ctx) {
      const workspace = ctx.spawn('indexed-land');
      const base = ctx.git(['rev-parse', 'HEAD']);
      write(workspace.path, 'feature.txt', 'landed feature\n');
      const tip = commit(ctx, workspace.path, 'landed feature');
      const land = landArgs(workspace, 'indexed-land');
      const journal = leaveIndexedCheckoutJournal(ctx, land);
      assert.deepEqual(journal.plan, { target: 'main', old: base, new: tip, tip });
      assert.equal(ctx.git(['rev-parse', 'HEAD']), tip);

      let destination = tip;
      if (advanced) {
        write(ctx.repo, 'later.txt', 'later primary commit\n');
        destination = commit(ctx, ctx.repo, 'later primary commit');
        assert.equal(ctx.git(['rev-parse', 'HEAD^']), tip);
      }
      write(ctx.repo, 'tracked.txt', 'private primary edit\n');
      write(workspace.path, 'feature.txt', 'staged workspace edit\n');
      ctx.git(['add', '--', 'feature.txt'], workspace.path);
      write(workspace.path, 'feature.txt', 'unstaged workspace edit\n');
      write(workspace.path, 'local.txt', 'untracked workspace data\n');
      const index = readFileSync(join(ctx.repo, '.git', 'index'));
      const workspaceIndex = readFileSync(join(workspace.path, '.git', 'index'));
      const assertPrimary = () => {
        assert.equal(ctx.git(['rev-parse', 'HEAD']), destination);
        assert.deepEqual(readFileSync(join(ctx.repo, '.git', 'index')), index);
        assert.equal(read(ctx.repo, 'feature.txt'), 'landed feature\n');
        assert.equal(read(ctx.repo, 'tracked.txt'), 'private primary edit\n');
        if (advanced) assert.equal(read(ctx.repo, 'later.txt'), 'later primary commit\n');
      };
      const assertWorkspace = () => {
        assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path), tip);
        assert.deepEqual(readFileSync(join(workspace.path, '.git', 'index')), workspaceIndex);
        assert.equal(ctx.git(['show', ':feature.txt'], workspace.path), 'staged workspace edit');
        assert.equal(read(workspace.path, 'feature.txt'), 'unstaged workspace edit\n');
        assert.equal(read(workspace.path, 'local.txt'), 'untracked workspace data\n');
      };

      const owned = ['indexed-land', '--lease', workspace.lease];
      assert.match(String(ctx.fail(['remove', ...owned]).message), /merge is pending/i);
      assert(existsSync(journal.path));
      if (!advanced) {
        assert.match(String(ctx.fail(land).message), /local changes/i);
        assert.equal(existsSync(journal.path), false);
      }
      assertPrimary();
      assertWorkspace();

      const retired = ctx.cli(['remove', ...owned, '--discard']);
      assert.equal(retired.action, 'merge');
      assert.equal(retired.commit, tip);
      assert.equal(retired.previous, base);
      assert.equal(retired.recovery_lease, workspace.lease);
      assert.equal(existsSync(journal.path), false);
      assert.equal(existsSync(workspace.path), false);
      const restored = ctx.cli(['restore', ...owned]);
      assert.equal(restored.path, workspace.path);
      assert.notEqual(restored.lease, workspace.lease);
      assertPrimary();
      assertWorkspace();
      for (const repo of [ctx.repo, workspace.path]) ctx.git(['fsck', '--full'], repo);
    },
  })),
  {
    name: 'offline reconcile joins independent npm dependency additions',
    run(ctx) {
      requireNpm(ctx);
      packageFiles(ctx.repo);
      commit(ctx, ctx.repo, 'npm base');
      const workspace = ctx.spawn('npm-ok');
      packageFiles(ctx.repo, { name: 'alpha', version: '1.0.0' });
      write(ctx.repo, 'ours.txt', 'ours\n');
      const old = commit(ctx, ctx.repo, 'ours');
      packageFiles(workspace.path, { name: 'beta', version: '2.0.0' });
      write(workspace.path, 'theirs.txt', 'theirs\n');
      const tip = commit(ctx, workspace.path, 'theirs');
      assert.equal(ctx.fail(landArgs(workspace, 'npm-ok')).code, 'merge_conflict');
      ctx.cli(landArgs(workspace, 'npm-ok', '--reconcile'));
      assert.deepEqual(parents(ctx), [old, tip]);
      assert.deepEqual(JSON.parse(read(ctx.repo, 'package.json')).dependencies, { alpha: '1.0.0', beta: '2.0.0' });
      const lock = JSON.parse(read(ctx.repo, 'package-lock.json'));
      assert.equal(lock.packages['node_modules/alpha'].version, '1.0.0');
      assert.equal(lock.packages['node_modules/beta'].version, '2.0.0');
      assert.equal(read(ctx.repo, 'ours.txt'), 'ours\n');
      assert.equal(read(ctx.repo, 'theirs.txt'), 'theirs\n');
    },
  },
  {
    name: 'offline reconcile rejects dependency conflicts and unsupported sources',
    run(ctx) {
      requireNpm(ctx);
      packageFiles(ctx.repo);
      commit(ctx, ctx.repo, 'npm base');
      const workspace = ctx.spawn('npm-bad');
      packageFiles(ctx.repo, { name: 'alpha', version: '1.0.0' });
      const old = commit(ctx, ctx.repo, 'ours');
      packageFiles(workspace.path, { name: 'alpha', version: '2.0.0' });
      const tip = commit(ctx, workspace.path, 'theirs');
      const fault = ctx.fail(landArgs(workspace, 'npm-bad', '--reconcile'));
      assert.equal(fault.code, 'merge_conflict');
      assert.match(String(field(fault.details).reconcile_error), /conflicting dependency|incompatible/i);
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), old);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path).trim(), tip);
      assert.equal(JSON.parse(read(workspace.path, 'package.json')).dependencies.alpha, '2.0.0');
    },
  },
  ...[
    { name: 'section', item: { name: 'alpha', version: '1.0.0', section: 'devDependencies' }, error: /conflicting dependency/i },
    { name: 'local', item: { name: 'alpha', version: 'file:../alpha' }, error: /non-semver/i },
  ].map(({ name, item, error }): Scenario => ({
    name: `offline reconcile rejects ${name} conflict`,
    run(ctx) {
      requireNpm(ctx);
      packageFiles(ctx.repo);
      commit(ctx, ctx.repo, 'npm base');
      const workspace = ctx.spawn(`npm-${name}`);
      packageFiles(ctx.repo, { name: 'alpha', version: '1.0.0' });
      const old = commit(ctx, ctx.repo, 'ours');
      packageFiles(workspace.path, item);
      const tip = commit(ctx, workspace.path, 'theirs');
      const fault = ctx.fail(landArgs(workspace, `npm-${name}`, '--reconcile'));
      assert.equal(fault.code, 'merge_conflict');
      assert.match(String(field(fault.details).reconcile_error), error);
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), old);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace.path).trim(), tip);
      assert.equal(read(workspace.path, 'package.json').includes(item.version), true);
    },
  })),
];
