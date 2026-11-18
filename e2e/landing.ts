import assert from 'node:assert/strict';
import { chmodSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import type { Scenario } from './harness.ts';

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
  return ['land', name, '--lease', workspace.lease, ...extra];
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
    name: 'landing fast-forward preserves workspace commit',
    run(ctx) {
      const workspace = ctx.spawn('ff');
      const old = ctx.git(['rev-parse', 'HEAD']).trim();
      write(workspace.path, 'feature.txt', 'feature\n');
      const tip = commit(ctx, workspace.path, 'feature');
      ctx.cli(landArgs(workspace, 'ff'));
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), tip);
      assert.deepEqual(parents(ctx), [old]);
      assert.equal(read(ctx.repo, 'feature.txt'), 'feature\n');
      assert.equal(existsSync(workspace.path), false);
    },
  },
  ...[false, true].map((squash): Scenario => ({
    name: squash ? 'landing squash creates one parent' : 'landing divergence creates two parents',
    run(ctx) {
      const workspace = ctx.spawn(squash ? 'squash' : 'merge');
      write(ctx.repo, 'ours.txt', 'ours\n');
      const old = commit(ctx, ctx.repo, 'ours');
      write(workspace.path, 'theirs.txt', 'theirs\n');
      const tip = commit(ctx, workspace.path, 'theirs');
      ctx.cli(landArgs(workspace, squash ? 'squash' : 'merge', ...(squash ? ['--squash'] : [])));
      assert.deepEqual(parents(ctx), squash ? [old] : [old, tip]);
      assert.equal(read(ctx.repo, 'ours.txt'), 'ours\n');
      assert.equal(read(ctx.repo, 'theirs.txt'), 'theirs\n');
      assert.equal(ctx.git(['status', '--porcelain']).trim(), '');
    },
  })),
  {
    name: 'landing protects dirty destination and ignored collision',
    run(ctx) {
      const workspace = ctx.spawn('dirty');
      write(workspace.path, 'collision.txt', 'landed\n');
      commit(ctx, workspace.path, 'feature');
      const old = ctx.git(['rev-parse', 'HEAD']).trim();
      write(ctx.repo, 'tracked.txt', 'local edit\n');
      assert.match(String(ctx.fail(landArgs(workspace, 'dirty')).message), /local changes/i);
      assert.equal(read(ctx.repo, 'tracked.txt'), 'local edit\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), old);
      write(ctx.repo, 'tracked.txt', 'initial\n');
      const gitDir = ctx.git(['rev-parse', '--absolute-git-dir']).trim();
      write(gitDir, 'info/exclude', 'collision.txt\n');
      write(ctx.repo, 'collision.txt', 'ignored user data\n');
      assert.match(String(ctx.fail(landArgs(workspace, 'dirty')).message), /collision\.txt/i);
      assert.equal(read(ctx.repo, 'collision.txt'), 'ignored user data\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), old);
      rmSync(join(ctx.repo, 'collision.txt'));
      ctx.cli(landArgs(workspace, 'dirty'));
      assert.equal(read(ctx.repo, 'collision.txt'), 'landed\n');
    },
  },
  {
    name: 'landing respects locked linked destination',
    run(ctx) {
      const workspace = ctx.spawn('linked');
      const dest = join(ctx.root, 'destination with spaces');
      ctx.git(['worktree', 'add', '-b', 'target', dest]);
      write(workspace.path, 'feature.txt', 'feature\n');
      const tip = commit(ctx, workspace.path, 'feature');
      const main = ctx.git(['rev-parse', 'HEAD']).trim();
      ctx.git(['worktree', 'lock', dest]);
      assert.match(String(ctx.fail(landArgs(workspace, 'linked', '--into', 'target')).message), /locked/i);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], dest).trim(), main);
      ctx.git(['worktree', 'unlock', dest]);
      ctx.cli(landArgs(workspace, 'linked', '--into', 'target'));
      assert.equal(ctx.git(['rev-parse', 'HEAD'], dest).trim(), tip);
      assert.equal(ctx.git(['rev-parse', 'HEAD']).trim(), main);
      assert.equal(read(dest, 'feature.txt'), 'feature\n');
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
    name: 'landing reports real conflict paths and preserves unmerged work',
    run(ctx) {
      const workspace = ctx.spawn('conflict');
      write(ctx.repo, 'tracked.txt', 'ours\n');
      const old = commit(ctx, ctx.repo, 'ours');
      write(workspace.path, 'tracked.txt', 'theirs\n');
      const tip = commit(ctx, workspace.path, 'theirs');
      const fault = ctx.fail(landArgs(workspace, 'conflict', '--reconcile'));
      assert.equal(fault.code, 'merge_conflict');
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
      ctx.git(['config', 'core.hooksPath', hooks]);
      ctx.git(['config', 'graft.testmarker', marker]);
      const workspace = ctx.spawn('hook');
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
