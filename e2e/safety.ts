import assert from 'node:assert/strict';
import { chmodSync, existsSync, mkdirSync, readFileSync, renameSync, statSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { objectResult, type Scenario } from './harness.ts';

export const cases: Scenario[] = [
  {
    name: 'new private roots have owner-only permissions under a permissive umask',
    run(ctx) {
      if (process.platform === 'win32') ctx.unavailable('Unix directory modes are unavailable on Windows');
      const previous = process.umask(0o022);
      try {
        const workspace = ctx.spawn('private-roots');
        const state = join(ctx.repo, '.git', 'graft');
        for (const path of [state, join(state, 'runtime'), join(state, 'cache'), dirname(workspace.path)]) {
          assert.equal(statSync(path).mode & 0o777, 0o700, path);
        }
      } finally {
        process.umask(previous);
      }
    },
  },
  {
    name: 'legacy permissive roots keep their modes and permit inspection before remediation',
    run(ctx) {
      if (process.platform === 'win32') ctx.unavailable('Unix directory modes are unavailable on Windows');
      const workspace = ctx.spawn('legacy-mode');
      const state = join(ctx.repo, '.git', 'graft');
      const root = dirname(workspace.path);
      const file = join(workspace.path, 'tracked.txt');
      chmodSync(state, 0o755);
      assert.equal(ctx.cli(['status', 'legacy-mode']).registered, true);
      assert.match(String(ctx.fail(['heartbeat', 'legacy-mode', '--lease', workspace.lease]).message), /chmod 700/i);
      assert.equal(statSync(state).mode & 0o777, 0o755);
      chmodSync(state, 0o700);

      chmodSync(root, 0o755);
      assert.equal(ctx.cli(['status', 'legacy-mode']).registered, true);
      assert.match(String(ctx.fail(['spawn', 'new-workspace']).message), /chmod 700/i);
      assert.equal(statSync(root).mode & 0o777, 0o755);
      assert.equal(existsSync(join(root, 'new-workspace')), false);
      assert.equal(readFileSync(file, 'utf8'), 'initial\n');
    },
  },
  {
    name: 'purge keeps the cache ownership marker and can retry',
    run(ctx) {
      ctx.cli(['doctor']);
      const cache = join(ctx.repo, '.git', 'graft', 'cache');
      const marker = join(cache, '.git-graft-id');
      const owned = statSync(marker, { bigint: true });
      const npm = join(cache, 'npm');
      const note = join(npm, 'cached.txt');
      const external = join(ctx.root, 'external');
      mkdirSync(external);
      const preserved = join(external, 'keep.txt');
      writeFileSync(preserved, 'keep linked content\n');
      for (let attempt = 0; attempt < 2; attempt++) {
        mkdirSync(npm);
        if (process.platform !== 'win32') symlinkSync(external, join(npm, 'linked'), 'dir');
        writeFileSync(note, 'disposable cache data\n');
        const result = objectResult(ctx.cli(['gc', '--purge-cache', '--apply']).cache);
        assert.equal(result.purged, true);
        assert.equal(existsSync(note), false);
        assert.equal(statSync(marker, { bigint: true }).ino, owned.ino);
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
      const result = objectResult(ctx.cli(['gc', '--purge-cache', '--apply']).cache);
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
      mkdirSync(state);
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
      assert.match(String(ctx.fail(['gc', '--purge-cache', '--apply']).message), /ordinary directory/i);
      assert.equal(readFileSync(note, 'utf8'), 'keep this file\n');
    },
  },
  {
    name: 'purge retains a cache containing a nested bare repository',
    run(ctx) {
      ctx.cli(['doctor']);
      const state = join(ctx.repo, '.git', 'graft');
      const npm = join(state, 'cache', 'npm');
      mkdirSync(npm);
      const nested = join(npm, 'nested.git');
      ctx.git(['clone', '--bare', '--no-hardlinks', ctx.repo, nested]);
      const head = ctx.git(['rev-parse', 'HEAD'], nested);
      const note = join(state, 'layers', 'keep.txt');
      writeFileSync(note, 'keep while purge is blocked\n');
      for (const extra of [[], ['--apply']]) {
        const cache = objectResult(ctx.cli(['gc', '--purge-cache', ...extra]).cache);
        assert.equal(cache.eligible, false);
        assert.equal(cache.purged, false);
        assert.match(JSON.stringify(cache.blocked), /nested bare Git repository/i);
        assert.equal(ctx.git(['rev-parse', 'HEAD'], nested), head);
        assert.equal(readFileSync(note, 'utf8'), 'keep while purge is blocked\n');
      }
    },
  },
  {
    name: 'GC retains unrecognized source snapshot directories',
    run(ctx) {
      const workspace = ctx.spawn('snapshot-fixture');
      const unknown = join(ctx.repo, '.git', 'graft', 'bases', '0'.repeat(40));
      mkdirSync(unknown);
      const note = join(unknown, 'keep.txt');
      writeFileSync(note, 'outside the source snapshot format\n');
      ctx.cli(['gc', '--apply']);
      assert.equal(readFileSync(note, 'utf8'), 'outside the source snapshot format\n');
      assert.equal(readFileSync(join(workspace.path, 'tracked.txt'), 'utf8'), 'initial\n');
    },
  },
  {
    name: 'a damaged workspace record cannot authorize operations',
    run(ctx) {
      const workspace = ctx.spawn('bad-record');
      const path = join(ctx.repo, '.git', 'graft', 'records', 'bad-record.json');
      const record = objectResult(JSON.parse(readFileSync(path, 'utf8')));
      const head = ctx.git(['rev-parse', 'HEAD'], workspace.path);
      for (const [fields, reason] of [
        [{ name: 'different-name' }, /record name mismatch/i],
        [{ base: 'invalid-object-id' }, /full Git object ID/i],
        [{ admin: ctx.root }, /invalid workspace administrative directory/i],
      ] as const) {
        const changed = JSON.stringify({ ...record, ...fields });
        writeFileSync(path, changed);
        assert.match(String(ctx.fail(['status', 'bad-record']).message), reason);
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
      assert.equal(ctx.cli(['status', 'hook-context']).registered, true);
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
      const land = ['land', 'hidden', '--lease', workspace.lease, '--into', 'main'];
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
        const args = ['land', 'exact-target', '--lease', workspace.lease, '--into', target];
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
      assert.equal(objectResult(workspace.result.storage).backend, 'git');
      writeFileSync(join(workspace.path, 'tracked.txt'), 'incoming edit\n');
      ctx.git(['commit', '-qam', 'Incoming changes'], workspace.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], workspace.path);
      const admin = ctx.git(['rev-parse', '--absolute-git-dir']);
      const index = join(admin, 'index');
      const before = readFileSync(index);
      const refLock = join(admin, 'refs', 'heads', 'main.lock');
      const land = ['land', 'locked', '--lease', workspace.lease, '--into', 'main'];
      writeFileSync(refLock, 'fixture ref lock\n');
      try {
        assert.match(String(ctx.fail(land).message), /destination branch is locked/i);
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
];
