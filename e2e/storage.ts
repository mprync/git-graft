import assert from 'node:assert/strict';
import { constants, copyFileSync, existsSync, linkSync, lstatSync, mkdirSync, readFileSync, readdirSync, readlinkSync, rmSync, symlinkSync, writeFileSync, chmodSync } from 'node:fs';
import { join } from 'node:path';
import { assertSeparateRegularFiles, type Scenario } from './harness.ts';

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
    name: 'many small source files share verified data and stay isolated',
    run(ctx) {
      ctx.git(['config', '--local', 'graft.history', 'isolated']);
      ctx.git(['config', 'core.symlinks', 'true']);
      const count = 320;
      mkdirSync(join(ctx.repo, 'small'));
      for (let i = 0; i < count; i++) writeFileSync(join(ctx.repo, 'small', `${i}.txt`), `committed ${i}\n`);
      ctx.git(['add', '.']);
      ctx.git(['commit', '-qm', 'many small files']);
      writeFileSync(join(ctx.repo, 'small/0.txt'), 'wrong bytes!\n');
      writeFileSync(join(ctx.repo, 'small/1.txt'), 'staged bytes\n');
      ctx.git(['add', 'small/1.txt']);
      writeFileSync(join(ctx.repo, 'small/2.txt'), 'unstaged data\n');
      mkdirSync(join(ctx.repo, 'ignored'));
      writeFileSync(join(ctx.repo, 'ignored/private'), 'secret');
      const status = ctx.git(['status', '--porcelain']);
      const first = ctx.spawn('small-cold');
      const second = ctx.spawn('small-warm');
      const cold = receipt(first.result), warm = receipt(second.result);
      const probe = object(ctx.cli(['doctor']).source_storage).native_clone;
      if (probe) {
        assert.equal(cold.backend, process.platform === 'darwin' ? 'clonefile' : 'reflink');
        assert.equal(cold.cloned_files, count + 2); // includes the fixture's tracked.txt and .gitignore
        assert.equal(cold.private_files, 3);
        assert.equal(cold.private_bytes, Buffer.byteLength('committed 0\ncommitted 1\ncommitted 2\n'));
        assert.equal(cold.primary_reused_files, count - 3 + 2);
        assert.equal(warm.cloned_files, count + 2);
        assert.equal(warm.private_files, 0);
        assert.equal(warm.snapshot_reused, true);
      } else assert.equal(cold.backend, 'git');
      for (const workspace of [first, second]) {
        for (let i = 0; i < count; i++) assert.equal(readFileSync(join(workspace.path, 'small', `${i}.txt`), 'utf8'), `committed ${i}\n`);
        assert.equal(existsSync(join(workspace.path, 'ignored/private')), false);
        assert.equal(ctx.git(['status', '--porcelain'], workspace.path), '');
        ctx.git(['fsck', '--full'], workspace.path);
      }
      writeFileSync(join(first.path, 'small/3.txt'), 'workspace edit');
      assert.equal(readFileSync(join(second.path, 'small/3.txt'), 'utf8'), 'committed 3\n');
      assert.equal(readFileSync(join(ctx.repo, 'small/3.txt'), 'utf8'), 'committed 3\n');
      assert.equal(ctx.git(['status', '--porcelain']), status);
      assert.equal(readFileSync(join(ctx.repo, 'ignored/private'), 'utf8'), 'secret');
    },
  },
  {
    name: 'source clone checkout isolation and parent reuse',
    run(ctx) {
      ctx.git(['config', '--local', 'graft.history', 'isolated']);
      const large = Buffer.alloc(3 * 1024 * 1024, 97);
      mkdirSync(join(ctx.repo, 'nested'), { recursive: true });
      writeFileSync(join(ctx.repo, 'large.bin'), large);
      writeFileSync(join(ctx.repo, 'nested', 'changed.txt'), 'old\n');
      writeFileSync(join(ctx.repo, 'removed.txt'), 'deleted\n');
      writeFileSync(join(ctx.repo, 'rename-before.txt'), 'renamed\n');
      writeFileSync(join(ctx.repo, 'executable.sh'), '#!/bin/sh\nexit 0\n');
      writeFileSync(join(ctx.repo, 'converted.bat'), 'one\r\ntwo\0\r\n');
      writeFileSync(join(ctx.repo, 'different.bat'), 'Git equivalent\r\n');
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
      const primaryWorktrees = ctx.git(['worktree', 'list', '--porcelain']);
      const primaryRefs = ctx.git(['for-each-ref', '--format=%(refname)']);
      writeFileSync(join(ctx.repo, 'tracked.txt'), 'dirty main\n');
      writeFileSync(join(ctx.repo, 'different.bat'), 'Git equivalent\n');
      mkdirSync(join(ctx.repo, 'ignored'), { recursive: true });
      writeFileSync(join(ctx.repo, 'ignored', 'secret'), 'private');
      writeFileSync(join(ctx.repo, 'untracked-secret'), 'private');
      const independentProbe = nativeCloneAvailable(ctx.root);
      const doctor = object(ctx.cli(['doctor']).source_storage);
      assert.equal(typeof doctor.native_clone, 'boolean');
      if (independentProbe !== null) assert.equal(doctor.native_clone, independentProbe, 'Git Graft and independent native clone probe disagree');
      const probe = doctor.native_clone;
      const first = ctx.spawn('storage-cold');
      assert.equal(first.result.path, first.path);
      assert(lstatSync(join(first.path, '.git')).isDirectory());
      assert.equal(ctx.git(['rev-parse', '--git-common-dir'], first.path), '.git');
      assert.equal(ctx.git(['worktree', 'list', '--porcelain']), primaryWorktrees);
      assert.equal(ctx.git(['for-each-ref', '--format=%(refname)']), primaryRefs);
      // Plumbing consumers can ask this before any porcelain command refreshes the index.
      assert.equal(ctx.git(['diff-files', '--quiet'], first.path), '');
      const status = ctx.cli(['status', 'storage-cold']);
      assert.equal(status.path, first.path);
      assert.equal(status.head, parent);
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
        assert.ok(Number(cold.primary_reused_bytes) >= large.length + Buffer.byteLength('one\r\ntwo\0\r\n'));
        assert.equal(cold.private_files, 2);
        assert.equal(cold.private_bytes, Buffer.byteLength('initial\nGit equivalent\r\n'));
      }
      assert.deepEqual(file(join(first.path, 'converted.bat')), Buffer.from('one\r\ntwo\0\r\n'));
      assert.equal(readFileSync(join(first.path, 'different.bat'), 'utf8'), 'Git equivalent\r\n');
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
      assert.equal(ctx.git(['diff-files', '--quiet'], warm.path), '');
      const warmReceipt = receipt(warm.result);
      if (probe) assert.equal(warmReceipt.snapshot_reused, true);
      else { assert.equal(warmReceipt.backend, 'git'); assert.equal(warmReceipt.snapshot_reused, false); }
      assert.equal(warmReceipt.primary_reused_bytes, 0);
      if (probe) assert.equal(warmReceipt.private_bytes, 0);
      assert.equal(ctx.git(['status', '--porcelain'], warm.path), '');
      assertSeparateRegularFiles([ctx.repo, first.path, warm.path].map(root => join(root, 'large.bin')));
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
    name: 'cold source reuse excludes tracked donor substitutions and private metadata',
    run(ctx) {
      ctx.git(['config', '--local', 'graft.history', 'isolated']);
      ctx.git(['config', 'core.symlinks', 'true']);
      const original = 'same bytes\n';
      const tests = ['assumed.txt', 'staged.txt', 'missing.txt', 'hardlinked.txt', 'route/tracked.txt'];
      if (process.platform === 'darwin') tests.push('xattr.txt', 'flags.txt', 'resource.txt');
      mkdirSync(join(ctx.repo, 'route'));
      for (const path of tests) writeFileSync(join(ctx.repo, path), original);
      writeFileSync(join(ctx.repo, 'clean.bin'), Buffer.from([0, 1, 2, 13, 10, 255]));
      ctx.git(['add', '.']);
      ctx.git(['commit', '-qm', 'tracked source files']);
      if (process.platform !== 'win32') chmodSync(join(ctx.repo, 'clean.bin'), 0o755);
      ctx.git(['update-index', '--assume-unchanged', 'assumed.txt']);
      writeFileSync(join(ctx.repo, 'assumed.txt'), 'private!!!\n');
      writeFileSync(join(ctx.repo, 'staged.txt'), 'private!!!\n');
      ctx.git(['add', 'staged.txt']);
      rmSync(join(ctx.repo, 'missing.txt'));
      const outside = join(ctx.root, 'outside');
      mkdirSync(outside);
      writeFileSync(join(outside, 'tracked.txt'), original);
      writeFileSync(join(outside, 'private.txt'), 'outside content');
      if (process.platform !== 'win32') {
        rmSync(join(ctx.repo, 'hardlinked.txt'));
        linkSync(join(outside, 'tracked.txt'), join(ctx.repo, 'hardlinked.txt'));
        rmSync(join(ctx.repo, 'route'), { recursive: true });
        symlinkSync(outside, join(ctx.repo, 'route'), 'dir');
      }
      if (process.platform === 'darwin') {
        const xattr = ctx.command('xattr', ['-w', 'user.graft-test', 'private attribute', 'xattr.txt']);
        assert.equal(xattr.status, 0, xattr.stderr);
        const flags = ctx.command('chflags', ['hidden', 'flags.txt']);
        assert.equal(flags.status, 0, flags.stderr);
        writeFileSync(join(ctx.repo, 'resource.txt', '..namedfork', 'rsrc'), 'private resource fork');
      }
      const workspace = ctx.spawn('donor-substitutions');
      const result = receipt(workspace.result);
      const probe = object(ctx.cli(['doctor']).source_storage).native_clone;
      if (process.platform !== 'win32') {
        assert.equal(lstatSync(join(workspace.path, 'clean.bin')).mode & 0o111, 0);
        assert.notEqual(lstatSync(join(ctx.repo, 'clean.bin')).mode & 0o111, 0);
      }
      for (const path of tests) assert.equal(readFileSync(join(workspace.path, path), 'utf8'), original, path);
      assert.equal(lstatSync(join(workspace.path, 'route')).isDirectory(), true);
      assert.equal(existsSync(join(workspace.path, 'route/private.txt')), false);
      if (probe) {
        assert.equal(result.private_files, tests.length, JSON.stringify(result));
        assert.equal(result.private_bytes, tests.length * Buffer.byteLength(original));
        assert.equal(result.primary_reused_files, 3);
        assert.notEqual(lstatSync(join(ctx.repo, 'clean.bin')).ino, lstatSync(join(workspace.path, 'clean.bin')).ino);
      }
      assert.equal(ctx.git(['status', '--porcelain'], workspace.path), '');
      writeFileSync(join(workspace.path, 'route/tracked.txt'), 'workspace edit');
      writeFileSync(join(workspace.path, 'hardlinked.txt'), 'workspace edit');
      assert.equal(readFileSync(join(outside, 'tracked.txt'), 'utf8'), original);
      if (process.platform === 'darwin') {
        assert.equal(ctx.command('xattr', ['-p', 'user.graft-test', 'xattr.txt'], workspace.path).status, 1);
        assert.equal(ctx.command('xattr', ['-p', 'user.graft-test', 'xattr.txt']).stdout.trim(), 'private attribute');
        assert.equal(ctx.command('xattr', ['-p', 'com.apple.ResourceFork', 'resource.txt'], workspace.path).status, 1);
        assert.equal(readFileSync(join(ctx.repo, 'resource.txt', '..namedfork', 'rsrc'), 'utf8'), 'private resource fork');
        assert.match(ctx.command('stat', ['-f', '%Sf', 'flags.txt']).stdout, /\bhidden\b/);
        assert.doesNotMatch(ctx.command('stat', ['-f', '%Sf', 'flags.txt'], workspace.path).stdout, /\bhidden\b/);
      }
    },
  },
  {
    name: 'polluted and foreign base generations are preserved without being reused',
    run(ctx) {
      ctx.git(['config', '--local', 'graft.history', 'isolated']);
      const oid = ctx.git(['rev-parse', 'HEAD']);
      const first = ctx.spawn('cache-prime');
      const bases = join(ctx.repo, '.git', 'graft', 'bases');
      const foreign = join(bases, '0'.repeat(oid.length));
      // The bases cache exists only where Graft clones files; create it on Git-checkout volumes too.
      mkdirSync(foreign, { recursive: true });
      writeFileSync(join(foreign, 'foreign.txt'), 'foreign directory');
      const base = join(bases, oid);
      let verifiedBase: string | undefined;
      let tamperedFile: string | undefined;
      if (receipt(first.result).backend !== 'git') {
        const [generation] = readdirSync(base);
        const tree = join(base, generation, 'tree');
        const privatePath = join(tree, 'untracked.txt');
        writeFileSync(privatePath, 'private cache file');
        const warm = ctx.spawn('polluted-warm');
        assert.equal(receipt(warm.result).snapshot_reused, false);
        assert.equal(existsSync(join(warm.path, 'untracked.txt')), false);
        assert.equal(readFileSync(privatePath, 'utf8'), 'private cache file');
        rmSync(privatePath);
        // A changed cached tracked file of exactly the same size also invalidates
        // the generation, even though the recorded Git blob ID still matches.
        const trackedPath = join(tree, 'tracked.txt');
        writeFileSync(trackedPath, 'private\n');
        const changed = ctx.spawn('invalidated-warm');
        assert.equal(receipt(changed.result).snapshot_reused, false);
        assert.equal(readFileSync(join(changed.path, 'tracked.txt'), 'utf8'), 'initial\n');
        writeFileSync(join(ctx.repo, 'added.txt'), 'new file\n');
        ctx.git(['add', 'added.txt']);
        ctx.git(['commit', '-qm', 'advance past polluted base']);
        const child = ctx.spawn('invalidated-parent');
        assert.equal(receipt(child.result).parent_reused_files, 0);
        assert.equal(readFileSync(join(child.path, 'tracked.txt'), 'utf8'), 'initial\n');
        assert.equal(readFileSync(trackedPath, 'utf8'), 'private\n');
        tamperedFile = trackedPath;
        verifiedBase = join(bases, ctx.git(['rev-parse', 'HEAD']));
        assert(existsSync(verifiedBase));
        for (const [name, lease] of [
          ['polluted-warm', warm.lease], ['invalidated-warm', changed.lease], ['invalidated-parent', child.lease],
        ]) ctx.cli(['prune', name, '--lease', lease]);
      }
      ctx.cli(['prune', 'cache-prime', '--lease', first.lease]);
      ctx.cli(['prune','--expired', '--apply']);
      const preview = object(ctx.cli(['prune','--expired', '--purge-cache']).cache);
      assert(Array.isArray(preview.candidates));
      assert(Array.isArray(preview.blocked));
      assert(preview.blocked.some(entry => object(entry).path === foreign));
      if (verifiedBase) {
        assert(preview.candidates.includes(verifiedBase));
        assert(preview.blocked.some(entry => object(entry).path === base));
        assert(existsSync(verifiedBase));
      }
      const applied = object(ctx.cli(['prune','--expired', '--purge-cache', '--apply']).cache);
      assert(Array.isArray(applied.removed));
      if (verifiedBase) {
        assert(applied.removed.includes(verifiedBase));
        assert(!existsSync(verifiedBase));
      }
      if (tamperedFile) assert.equal(readFileSync(tamperedFile, 'utf8'), 'private\n');
      assert.equal(readFileSync(join(foreign, 'foreign.txt'), 'utf8'), 'foreign directory');
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
        ctx.fail(['new', name, '--sparse', path]);
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
      ctx.git(['config', '--local', 'graft.history', 'isolated']);
      // Exercise the conversion decision even on Git installations that
      // default to symlink emulation. This fixture contains no symlinks.
      ctx.git(['config', 'core.symlinks', 'true']);
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
  {
    name: 'Git symlink emulation checks out link contents as independent ordinary files',
    run(ctx) {
      ctx.git(['config', '--local', 'graft.history', 'isolated']);
      ctx.git(['config', 'core.symlinks', 'false']);
      const link = join(ctx.repo, 'tracked-link');
      writeFileSync(link, 'tracked.txt');
      const blob = ctx.git(['hash-object', '-w', 'tracked-link']);
      ctx.git(['update-index', '--add', '--cacheinfo', `120000,${blob},tracked-link`]);
      ctx.git(['commit', '-qm', 'emulated link']);
      const workspace = ctx.spawn('emulated-link');
      const storage = receipt(workspace.result);
      assert.equal(storage.backend, 'git');
      assert.equal(storage.reason, 'Git symlink emulation');
      const privateLink = join(workspace.path, 'tracked-link');
      assert(lstatSync(privateLink).isFile());
      assert.equal(readFileSync(privateLink, 'utf8'), 'tracked.txt');
      writeFileSync(privateLink, 'changed');
      assert.equal(readFileSync(link, 'utf8'), 'tracked.txt');
      assert.equal(readFileSync(join(workspace.path, 'tracked.txt'), 'utf8'), 'initial\n');
    },
  },
];
