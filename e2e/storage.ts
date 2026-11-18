import assert from 'node:assert/strict';
import { constants, copyFileSync, existsSync, lstatSync, mkdirSync, readFileSync, readlinkSync, rmSync, writeFileSync, chmodSync } from 'node:fs';
import { join } from 'node:path';
import type { Scenario } from './harness.ts';

function object(value: unknown): Record<string, unknown> {
  assert.ok(value !== null && typeof value === 'object' && !Array.isArray(value));
  return value as Record<string, unknown>;
}

function receipt(result: Record<string, unknown>): Record<string, unknown> {
  return object(result.storage);
}

function file(path: string): Buffer {
  return readFileSync(path);
}

// An independent native clone probe checks the actual filesystem used by this
// fixture. COPYFILE_FICLONE_FORCE fails instead of silently copying on failure.
function nativeCloneAvailable(root: string): boolean | null {
  // Node's FICLONE_FORCE probe is Linux-specific; on macOS the product's
  // doctor performs an actual clonefile and validates write isolation.
  if (process.platform !== 'linux') return null;
  const source = join(root, 'clone-probe-source');
  const dest = join(root, 'clone-probe-dest');
  try {
    writeFileSync(source, 'probe');
    copyFileSync(source, dest, constants.COPYFILE_FICLONE_FORCE);
    writeFileSync(dest, 'different');
    assert.equal(readFileSync(source, 'utf8'), 'probe');
    return true;
  } catch (error) {
    // Only unsupported-clone errors establish an unavailable backend. Other
    // failures such as permissions or disk exhaustion remain visible failures.
    if (error instanceof Error && 'code' in error && ['ENOTSUP', 'EOPNOTSUPP', 'ENOSYS', 'EINVAL', 'EXDEV', 'ENOTTY'].includes(String(error.code))) return false;
    throw error;
  } finally {
    rmSync(source, { force: true });
    rmSync(dest, { force: true });
  }
}

export const cases: Scenario[] = [
  {
    name: 'source clone checkout isolation and parent reuse',
    run(ctx) {
      const large = Buffer.alloc(3 * 1024 * 1024, 97);
      mkdirSync(join(ctx.repo, 'nested'), { recursive: true });
      writeFileSync(join(ctx.repo, 'large.bin'), large);
      writeFileSync(join(ctx.repo, 'nested', 'changed.txt'), 'old\n');
      writeFileSync(join(ctx.repo, 'removed.txt'), 'deleted\n');
      writeFileSync(join(ctx.repo, 'rename-before.txt'), 'renamed\n');
      writeFileSync(join(ctx.repo, 'executable.sh'), '#!/bin/sh\nexit 0\n');
      // Common committed attributes must be supported by the CoW path.
      writeFileSync(join(ctx.repo, '.gitattributes'), '* text=auto eol=lf\n*.bat text eol=crlf\n*.bin binary\n');
      if (process.platform !== 'win32') chmodSync(join(ctx.repo, 'executable.sh'), 0o755);
      ctx.git(['add', '.']);
      // Add a symlink directly to Git's index; this exercises Windows Git's
      // configured symlink emulation without needing filesystem privileges.
      writeFileSync(join(ctx.repo, 'link-target-input'), 'tracked.txt');
      const linkBlob = ctx.git(['hash-object', '-w', 'link-target-input']).trim();
      rmSync(join(ctx.repo, 'link-target-input'));
      ctx.git(['update-index', '--add', '--cacheinfo', `120000,${linkBlob},tracked-link`]);
      ctx.git(['commit', '-m', 'source fixtures']);
      ctx.git(['checkout-index', '--', 'tracked-link']);
      const parent = ctx.git(['rev-parse', 'HEAD']).trim();
      writeFileSync(join(ctx.repo, 'tracked.txt'), 'dirty main\n');
      mkdirSync(join(ctx.repo, 'ignored'), { recursive: true });
      writeFileSync(join(ctx.repo, 'ignored', 'secret'), 'private');
      writeFileSync(join(ctx.repo, 'untracked-secret'), 'private');
      const independentProbe = nativeCloneAvailable(ctx.root);
      const doctor = object(ctx.cli(['doctor']).source_storage);
      assert.equal(typeof doctor.native_clone, 'boolean');
      if (independentProbe !== null) assert.equal(doctor.native_clone, independentProbe, 'Git Graft and independent native clone probe disagree');
      const probe = doctor.native_clone;
      const first = ctx.spawn('storage-cold');
      const cold = receipt(first.result);
      const platformBackend = process.platform === 'darwin' ? 'clonefile' : process.platform === 'linux' ? 'reflink' : 'git';
      const expected = process.env.GIT_GRAFT_EXPECT_BACKEND;
      if (expected !== undefined && expected !== '') {
        assert.ok(['clonefile', 'reflink', 'git'].includes(expected), 'invalid GIT_GRAFT_EXPECT_BACKEND');
        assert.equal(cold.backend, expected);
      } else if (probe) {
        assert.equal(cold.backend, platformBackend, `CoW available but storage fell back: ${JSON.stringify(cold)}`);
      } else {
        assert.equal(cold.backend, 'git');
      }
      ctx.measure('storage_backend', cold.backend);
      ctx.measure('native_clone_probe', probe);
      ctx.measure('native_clone_probe_method', independentProbe === null ? 'doctor native filesystem probe' : 'doctor and independent reflink');
      ctx.measure('symlink_platform', process.platform);
      assert.equal(cold.snapshot_reused, false);
      assert.equal(readFileSync(join(first.path, 'tracked.txt'), 'utf8'), 'initial\n');
      assert.equal(existsSync(join(first.path, 'untracked-secret')), false);
      assert.equal(existsSync(join(first.path, 'ignored', 'secret')), false);
      assert.equal(ctx.git(['status', '--porcelain'], first.path), '');
      assert.deepEqual(file(join(first.path, 'large.bin')), large);
      if (probe) {
        assert.ok(typeof cold.cloned_files === 'number' && cold.cloned_files > 0);
        assert.ok(typeof cold.cloned_bytes === 'number' && cold.cloned_bytes >= large.length);
      }
      const reference = join(ctx.root, 'native-reference');
      ctx.git(['worktree', 'add', '--detach', reference, parent]);
      const actualLink = join(first.path, 'tracked-link');
      const expectedLink = join(reference, 'tracked-link');
      const nativeSymlink = lstatSync(expectedLink).isSymbolicLink();
      assert.equal(lstatSync(actualLink).isSymbolicLink(), nativeSymlink);
      if (nativeSymlink) assert.equal(readlinkSync(actualLink), readlinkSync(expectedLink));
      else assert.deepEqual(file(actualLink), file(expectedLink));
      ctx.measure('native_checkout_uses_symlink', nativeSymlink);
      if (process.platform !== 'win32') {
        assert.equal(lstatSync(join(first.path, 'executable.sh')).mode & 0o111, lstatSync(join(reference, 'executable.sh')).mode & 0o111);
        assert.notEqual(lstatSync(join(first.path, 'executable.sh')).mode & 0o111, 0);
      }
      const warm = ctx.spawn('storage-warm');
      const warmReceipt = receipt(warm.result);
      if (probe) assert.equal(warmReceipt.snapshot_reused, true);
      else { assert.equal(warmReceipt.backend, 'git'); assert.equal(warmReceipt.snapshot_reused, false); }
      assert.equal(ctx.git(['status', '--porcelain'], warm.path), '');
      writeFileSync(join(first.path, 'large.bin'), 'workspace change');
      assert.deepEqual(file(join(warm.path, 'large.bin')), large);
      assert.deepEqual(file(join(ctx.repo, 'large.bin')), large);
      // Advance the committed main revision, retaining the parent snapshot.
      writeFileSync(join(ctx.repo, 'tracked.txt'), 'initial\n');
      writeFileSync(join(ctx.repo, 'nested', 'changed.txt'), 'new\n');
      rmSync(join(ctx.repo, 'removed.txt'));
      ctx.git(['mv', 'rename-before.txt', 'rename-after.txt']);
      if (process.platform !== 'win32') chmodSync(join(ctx.repo, 'executable.sh'), 0o644);
      ctx.git(['add', '-u']);
      ctx.git(['commit', '-m', 'advance source']);
      const next = ctx.spawn('storage-child');
      const incremental = receipt(next.result);
      assert.equal(incremental.snapshot_reused, false);
      if (probe) {
        assert.equal(incremental.backend, platformBackend);
        assert.ok(typeof incremental.parent_reused_files === 'number' && incremental.parent_reused_files > 0, JSON.stringify(incremental));
        assert.ok(typeof incremental.parent_reused_bytes === 'number' && incremental.parent_reused_bytes >= large.length, JSON.stringify(incremental));
      } else assert.equal(incremental.backend, 'git');
      ctx.measure('parent_reused_files', incremental.parent_reused_files);
      ctx.measure('parent_reused_bytes', incremental.parent_reused_bytes);
      const childReference = join(ctx.root, 'native-child-reference');
      ctx.git(['worktree', 'add', '--detach', childReference, 'HEAD']);
      assert.equal(ctx.git(['ls-files', '-s'], next.path), ctx.git(['ls-files', '-s'], childReference));
      assert.equal(readFileSync(join(next.path, 'nested', 'changed.txt'), 'utf8'), 'new\n');
      assert.deepEqual(file(join(next.path, 'nested', 'changed.txt')), file(join(childReference, 'nested', 'changed.txt')));
      assert.equal(existsSync(join(next.path, 'removed.txt')), false);
      assert.equal(existsSync(join(next.path, 'rename-before.txt')), false);
      assert.equal(readFileSync(join(next.path, 'rename-after.txt'), 'utf8'), 'renamed\n');
      assert.equal(ctx.git(['status', '--porcelain'], next.path), '');
      assert.deepEqual(file(join(next.path, 'large.bin')), large);
      if (process.platform !== 'win32') {
        assert.equal(lstatSync(join(next.path, 'executable.sh')).mode & 0o111, lstatSync(join(childReference, 'executable.sh')).mode & 0o111);
        assert.equal(lstatSync(join(next.path, 'executable.sh')).mode & 0o111, 0);
      }
      writeFileSync(join(next.path, 'large.bin'), 'child modification');
      assert.deepEqual(file(join(warm.path, 'large.bin')), large);
      const again = ctx.spawn('storage-child-warm');
      assert.deepEqual(file(join(again.path, 'large.bin')), large);
      if (probe) assert.equal(receipt(again.result).snapshot_reused, true);
      // A changed conversion rule invalidates parent reuse, and Git determines
      // the new bytes for every affected tracked file.
      writeFileSync(join(ctx.repo, '.gitattributes'), '* text=auto eol=lf\n*.txt text eol=crlf\n*.bin binary\n');
      ctx.git(['add', '.gitattributes']);
      ctx.git(['commit', '-m', 'change conversion attributes']);
      const attr = ctx.spawn('storage-attributes-changed');
      assert.equal(receipt(attr.result).parent_reused_bytes, 0);
      const attrReference = join(ctx.root, 'native-attributes-reference');
      ctx.git(['worktree', 'add', '--detach', attrReference, 'HEAD']);
      assert.deepEqual(file(join(attr.path, 'nested', 'changed.txt')), file(join(attrReference, 'nested', 'changed.txt')));
      assert.equal(file(join(attr.path, 'nested', 'changed.txt')).toString(), 'new\r\n');
      assert.equal(ctx.git(['status', '--porcelain'], attr.path), '');
    },
  },
  {
    name: 'sparse cones validate before allocating',
    run(ctx) {
      mkdirSync(join(ctx.repo, 'included', 'deep'), { recursive: true });
      mkdirSync(join(ctx.repo, 'excluded'), { recursive: true });
      writeFileSync(join(ctx.repo, 'included', 'deep', 'yes.txt'), 'included\n');
      writeFileSync(join(ctx.repo, 'excluded', 'no.txt'), 'excluded\n');
      ctx.git(['add', '.']);
      ctx.git(['commit', '-m', 'sparse fixtures']);
      const before = ctx.git(['worktree', 'list', '--porcelain']);
      const branches = ctx.git(['branch', '--list', 'graft/*']);
      const registry = ctx.cli(['list']);
      for (const [name, path] of [['sparse-escape', '../escape'], ['sparse-missing', 'missing'], ['sparse-file', 'tracked.txt']]) {
        ctx.fail(['spawn', name, '--sparse', path]);
        assert.equal(ctx.git(['worktree', 'list', '--porcelain']), before);
        assert.equal(ctx.git(['branch', '--list', 'graft/*']), branches);
        assert.deepEqual(ctx.cli(['list']), registry);
        assert.equal(existsSync(join(`${ctx.repo}.graft`, name)), false);
      }
      const sparse = ctx.spawn('sparse-valid', ['--sparse', 'included']);
      const storage = receipt(sparse.result);
      assert.equal(storage.backend, 'git');
      assert.equal(storage.reason, 'sparse checkout');
      assert.equal(readFileSync(join(sparse.path, 'included', 'deep', 'yes.txt'), 'utf8'), 'included\n');
      assert.equal(existsSync(join(sparse.path, 'excluded', 'no.txt')), false);
      assert.equal(readFileSync(join(sparse.path, 'tracked.txt'), 'utf8'), 'initial\n');
      assert.equal(ctx.git(['status', '--porcelain'], sparse.path), '');
      ctx.measure('sparse_backend', storage.backend);
    },
  },
  {
    name: 'conversion filters and gitlinks use native Git',
    run(ctx) {
      writeFileSync(join(ctx.repo, '.gitattributes'), '*.dat filter=example\n');
      writeFileSync(join(ctx.repo, 'data.dat'), 'contents\n');
      ctx.git(['add', '.']);
      ctx.git(['commit', '-m', 'filter attribute']);
      const filtered = ctx.spawn('filter-fallback');
      const filterStorage = receipt(filtered.result);
      assert.equal(filterStorage.backend, 'git');
      assert.equal(filterStorage.reason, 'path-dependent checkout conversion');
      assert.equal(ctx.git(['status', '--porcelain'], filtered.path), '');
      const reference = join(ctx.root, 'native-filter-reference');
      ctx.git(['worktree', 'add', '--detach', reference, 'HEAD']);
      assert.deepEqual(file(join(filtered.path, 'data.dat')), file(join(reference, 'data.dat')));
      rmSync(join(ctx.repo, '.gitattributes'));
      ctx.git(['add', '-u']);
      ctx.git(['commit', '-m', 'remove filter']);
      const parent = ctx.git(['rev-parse', 'HEAD']).trim();
      ctx.git(['update-index', '--add', '--cacheinfo', `160000,${parent},module`]);
      ctx.git(['commit', '-m', 'add gitlink']);
      const linked = ctx.spawn('gitlink-fallback');
      const linkStorage = receipt(linked.result);
      assert.equal(linkStorage.backend, 'git');
      assert.equal(linkStorage.reason, 'submodules');
      assert.equal(ctx.git(['status', '--porcelain'], linked.path), '');
      assert.equal(ctx.git(['ls-files', '-s', 'module'], linked.path).startsWith('160000 '), true);
      const gitlinkReference = join(ctx.root, 'native-gitlink-reference');
      ctx.git(['worktree', 'add', '--detach', gitlinkReference, 'HEAD']);
      assert.equal(ctx.git(['ls-files', '-s'], linked.path), ctx.git(['ls-files', '-s'], gitlinkReference));
      ctx.measure('filter_fallback', filterStorage.reason);
      ctx.measure('gitlink_fallback', linkStorage.reason);
    },
  },
];
