/** Exercise local object snapshots and isolated history through the public CLI. */
import assert from 'node:assert/strict';
import { randomBytes, randomUUID } from 'node:crypto';
import { execFile } from 'node:child_process';
import { existsSync, lstatSync, mkdirSync, readFileSync, readdirSync, readlinkSync, renameSync, unlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { assertSeparateRegularFiles, objectResult, type Context, type Scenario } from './harness.ts';
import { assertNoRecord, packRecords } from './pack-records.ts';

const exec = promisify(execFile);

function objects(ctx: Context, path = ctx.repo): string[] {
  return ctx.git(['cat-file', '--batch-all-objects', '--batch-check=%(objectname)'], path).split('\n').sort();
}

function closure(ctx: Context, tip: string, path = ctx.repo): string[] {
  return ctx.git(['rev-list', '--objects', '--no-object-names', tip], path).split('\n').sort();
}

function tipObjects(ctx: Context, tip: string, repo = ctx.repo): string[] {
  const tree = ctx.git(['rev-parse', tip + '^{tree}'], repo);
  const entries = ctx.git(['ls-tree', '-rt', '--format=%(objectname)', tip], repo);
  return [...new Set([tip, tree, ...entries.split('\n').filter(Boolean)])].sort();
}

function checkout(ctx: Context, path: string, tip: string, ref: string) {
  const admin = join(path, '.git');
  assert(lstatSync(admin).isDirectory(), 'workspace must have a standalone Git directory');
  for (const name of ['alternates', 'http-alternates']) {
    assert.throws(() => lstatSync(join(admin, 'objects', 'info', name)), { code: 'ENOENT' });
  }
  assert.equal(ctx.git(['rev-parse', 'HEAD'], path), tip);
  assert.equal(ctx.git(['for-each-ref', '--format=%(refname)'], path), ref);
  assert.equal(ctx.git(['status', '--porcelain'], path), '');
}

function commitFile(ctx: Context, path: string, file: string, content: string): string {
  writeFileSync(join(path, file), content);
  ctx.git(['add', '--', file], path);
  ctx.git(['commit', '-qm', file], path);
  return ctx.git(['rev-parse', 'HEAD'], path);
}

function looseBlob(ctx: Context, path: string, name: string, content: string): string {
  const file = join(ctx.root, name);
  writeFileSync(file, content);
  return ctx.git(['hash-object', '-w', file], path);
}

function read(path: string, file: string): string {
  return readFileSync(join(path, file), 'utf8');
}

export const cases: Scenario[] = [
  {
    name: 'private checkout settings retain branch-selected values after a move',
    run(ctx) {
      const settings = new Map([
        ['user.name', ' #"quoted" \\ name; '],
        ['user.email', 'workspace@example.invalid'],
        ['author.name', 'Workspace Author'],
        ['core.safecrlf', 'false'],
      ]);
      const include = join(ctx.root, 'primary-config');
      for (const [key, value] of settings) ctx.git(['config', '--file', include, key, value]);
      ctx.git(['config', '--global', 'includeIf.onbranch:main.path', include]);
      const name = 'private-settings';
      const ws = ctx.spawn(name, ['--history', 'tip']);
      const moved = join(ctx.root, 'moved-private-settings');
      ctx.cli(['move', name, moved, '--lease', ws.lease]);
      for (const [key, value] of settings) {
        assert.equal(ctx.git(['config', '--no-includes', '--local', '--get-all', key], moved), value);
      }
      assert.equal(ctx.git(['status', '--porcelain'], moved), '');
    },
  },
  {
    name: 'tip checkout installs committed symlinks alongside regular files from a dirty primary',
    run(ctx) {
      const nested = join(ctx.repo, 'nested');
      mkdirSync(nested);
      for (let i = 0; i < 260; i++) writeFileSync(join(nested, String(i)), String(i));
      ctx.git(['add', 'nested']);
      const links = [['nested/link', '42'], ['links/deep/dangling', 'missing']];
      for (const [path, target] of links) {
        const blob = looseBlob(ctx, ctx.repo, 'link-target', target);
        ctx.git(['update-index', '--add', '--cacheinfo', `120000,${blob},${path}`]);
      }
      ctx.git(['commit', '-qm', 'regular files and symlinks']);
      const reference = join(ctx.root, 'native-links');
      ctx.git(['worktree', 'add', '-q', '--detach', reference, 'HEAD']);
      writeFileSync(join(nested, 'link'), 'elsewhere');
      writeFileSync(join(nested, '42'), 'xx');
      const originalPath = ctx.env.PATH;
      const observe = process.platform === 'darwin' && objectResult(ctx.cli(['doctor']).source_storage).native_clone;
      if (observe) {
        const bin = join(ctx.root, 'observe-link-checkout');
        mkdirSync(bin);
        ctx.env.GRAFT_TEST_GIT = ctx.command('sh', ['-c', 'command -v git']).stdout.trim();
        ctx.env.GRAFT_TEST_PARENT = join(ctx.repo + '.graft', 'links', 'links', 'deep');
        ctx.env.GRAFT_TEST_OBSERVED = join(ctx.root, 'link-checkout-observed');
        writeFileSync(join(bin, 'git'), `#!/bin/sh
case " $* " in
  *" checkout-index --stdin -z "*)
    test -d "$GRAFT_TEST_PARENT" || exit 77
    touch "$GRAFT_TEST_OBSERVED";;
esac
exec "$GRAFT_TEST_GIT" "$@"
`, {mode: 0o700});
        ctx.env.PATH = bin + ':' + originalPath;
      }
      const ws = ctx.spawn('links', ['--history', 'tip']);
      ctx.env.PATH = originalPath;
      if (observe) assert(existsSync(join(ctx.root, 'link-checkout-observed')));
      for (const [name] of links) {
        const path = join(ws.path, name);
        const expected = join(reference, name);
        const symlink = lstatSync(expected).isSymbolicLink();
        assert.equal(lstatSync(path).isSymbolicLink(), symlink);
        if (symlink) assert.equal(readlinkSync(path), readlinkSync(expected));
        else assert.deepEqual(readFileSync(path), readFileSync(expected));
      }
      assert.equal(read(ws.path, 'nested/42'), '42');
      assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
      assert.equal(ctx.git(['rev-list', '--all', '--count'], ws.path), '1');
      assert.equal(read(ctx.repo, 'nested/link'), 'elsewhere');
      assert.equal(read(ctx.repo, 'nested/42'), 'xx');
    },
  },
  {
    name: 'first status detects edits to files cloned through the verified tip index',
    run(ctx) {
      const supportsClones = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
      const trace = join(ctx.root, 'tip-checkout.trace');
      ctx.env.GIT_TRACE2_EVENT = trace;
      const workspace = ctx.spawn('verified-index', ['--history', 'tip']);
      delete ctx.env.GIT_TRACE2_EVENT;
      if (supportsClones) {
        const calls = readFileSync(trace, 'utf8').trimEnd().split('\n').map(line => JSON.parse(line));
        assert(calls.some(call => call.event === 'start' &&
          call.argv.includes('check-attr') && call.argv.includes('--cached')));
        assert.equal(objectResult(workspace.result.storage).cloned_files, 2);
      }
      const index = join(workspace.path, '.git', 'index');
      const initialIndex = readFileSync(index);
      assert.equal(ctx.git(['--no-optional-locks', 'status', '--porcelain'], workspace.path), '');
      assert.deepEqual(readFileSync(index), initialIndex);
      writeFileSync(join(workspace.path, 'tracked.txt'), 'changed\n');
      assert.equal(ctx.git(['status', '--porcelain'], workspace.path), ' M tracked.txt');
      assert.equal(ctx.git(['show', ':tracked.txt'], workspace.path), 'initial');
      assert.equal(read(ctx.repo, 'tracked.txt'), 'initial\n');
    },
  },
  {
    name: 'ordinary files share blocks beside an uninitialized submodule',
    run(ctx) {
      const moduleName = 'vendor/module';
      const module = join(ctx.repo, moduleName);
      mkdirSync(module, {recursive:true});
      ctx.git(['init', '-q', module]);
      const revision = commitFile(ctx, module, 'child.txt', 'submodule content\n');
      writeFileSync(join(ctx.repo, '.gitmodules'), '[submodule "example"]\n\tpath = vendor/module\n\turl = ./unused\n');
      ctx.git(['add', '.gitmodules']);
      ctx.git(['update-index', '--add', '--cacheinfo', '160000,' + revision + ',' + moduleName]);
      ctx.git(['commit', '-qm', 'record submodule']);
      writeFileSync(join(module, 'child.txt'), 'private donor edit\n');
      const native = join(ctx.root, 'native-gitlink');
      ctx.git(['worktree', 'add', '--detach', native, 'HEAD']);
      assert.deepEqual(readdirSync(join(native, moduleName)), []);
      const expectedIndex = ctx.git(['ls-files', '--stage'], native);
      const nativeClone = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
      for (const history of ['local', 'tip']) {
        const ws = ctx.spawn('module-' + history, ['--history', history]);
        assert.deepEqual(readdirSync(join(ws.path, moduleName)), []);
        assert.equal(ctx.git(['ls-files', '--stage'], ws.path), expectedIndex);
        assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
        assert.equal(read(ws.path, '.gitmodules'), read(native, '.gitmodules'));
        if (nativeClone) assert(Number(objectResult(ws.result.storage).cloned_files) > 0);
        assert([1,128].includes(ctx.command('git', ['cat-file', '-e', revision], ws.path).status ?? -1));
      }
      ctx.git(['config', '--global', 'submodule.recurse', 'true']);
      const recursive = ctx.spawn('recursive-gitlink', ['--history', 'tip']);
      assert.equal(objectResult(recursive.result.storage).backend, 'git');
      assert.equal(ctx.git(['ls-files', '--stage'], recursive.path), expectedIndex);
      assert.deepEqual(readdirSync(join(recursive.path, moduleName)), []);
      assert.equal(read(module, 'child.txt'), 'private donor edit\n');
    },
  },
  ...['sha1', 'sha256'].map((format): Scenario => ({
    name: `advancing a cached tip excludes replaced, deleted and parent objects (${format})`,
    run(ctx) {
      const donor = join(ctx.root, format);
      ctx.git(['init', '-q', '-b', 'main', '--object-format=' + format, donor]);
      for (let i = 0; i < 96; i++) writeFileSync(join(donor, `file-${i}`), randomBytes(16 * 1024));
      ctx.git(['add', '.'], donor);
      ctx.git(['commit', '-qm', 'base'], donor);
      const base = ctx.git(['rev-parse', 'HEAD'], donor);
      const parent = ctx.spawn('parent', ['--history', 'tip'], donor);
      const initialObjects = objects(ctx, parent.path);
      const changed = readFileSync(join(donor, 'file-20'));
      const deleted = readFileSync(join(donor, 'file-21'));
      writeFileSync(join(donor, 'file-20'), 'replacement\n');
      ctx.git(['rm', '-q', '--', 'file-21'], donor);
      ctx.git(['commit', '-qam', 'replace and delete'], donor);
      const next = ctx.git(['rev-parse', 'HEAD'], donor);
      const expected = tipObjects(ctx, next, donor);
      const child = ctx.spawn('child', ['--history', 'tip'], donor);
      const receipt = objectResult(objectResult(child.result.storage).objects);
      const nativeClone = objectResult(ctx.cli(['doctor'], donor).source_storage).native_clone;
      if (nativeClone) {
        assert.equal(receipt.source, 'parent-tip-cache');
        assert.equal(receipt.filtered_parent_packs, 1);
      }
      assert.deepEqual(objects(ctx, child.path), expected);
      assert.equal(ctx.git(['rev-list', '--all'], child.path), next);
      assert.equal(read(child.path, 'file-20'), 'replacement\n');
      assert(!existsSync(join(child.path, 'file-21')));
      assert.equal(ctx.git(['fsck', '--full'], child.path), '');
      const advanced = commitFile(ctx, donor, 'added', 'one more commit\n');
      const grandchild = ctx.spawn('grandchild', ['--history', 'tip'], donor);
      assert.deepEqual(objects(ctx, grandchild.path), tipObjects(ctx, advanced, donor));
      assert.equal(ctx.git(['fsck', '--full'], grandchild.path), '');
      if (nativeClone) {
        const reuse = objectResult(objectResult(grandchild.result.storage).objects);
        assert.equal(reuse.source, 'parent-tip-cache');
        assert(Number(reuse.reused_parent_packs) >= 1);
      }
      const removed = new Set(initialObjects.filter(id => !expected.includes(id)));
      const dir = join(parent.path, '.git', 'objects', 'pack');
      for (const index of readdirSync(dir).filter(name => name.endsWith('.idx'))) {
        const bytes = readFileSync(join(dir, index.replace(/\.idx$/, '.pack')));
        for (const record of packRecords(ctx, join(dir, index), parent.path)) {
          if (!removed.delete(record.id)) continue;
          const excluded = bytes.subarray(record.offset, record.offset + record.size);
          assertNoRecord(child.path, excluded);
          assertNoRecord(grandchild.path, excluded);
        }
      }
      assert.equal(removed.size, 0);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], parent.path), base);
      assert.deepEqual(objects(ctx, parent.path), initialObjects);
      assert.deepEqual(readFileSync(join(parent.path, 'file-20')), changed);
      assert.deepEqual(readFileSync(join(parent.path, 'file-21')), deleted);
      if (nativeClone) {
        const cache = join(donor, '.git', 'graft', 'independent-packs-v2');
        const generations = readdirSync(cache).filter(name => name.startsWith(format + '-' + advanced + '-shallow-'));
        assert.equal(generations.length, 1);
        const receipt = join(cache, generations[0], 'verified.json');
        const held = join(ctx.root, 'held-receipt');
        renameSync(receipt, held);
        try {
          const last = commitFile(ctx, donor, 'added', 'updated content\n');
          const fallback = ctx.spawn('fallback', ['--history', 'tip'], donor);
          assert.equal(objectResult(objectResult(fallback.result.storage).objects).source, 'local-export');
          assert.deepEqual(objects(ctx, fallback.path), tipObjects(ctx, last, donor));
          assert.equal(read(grandchild.path, 'added'), 'one more commit\n');
        } finally {
          renameSync(held, receipt);
        }
      }
    },
  })),
  {
    name: 'checkout honors literal filter names that also describe Git attribute states',
    run(ctx) {
      for (const filter of ['unspecified', 'unset']) {
        ctx.git(['config', '--global', `filter.${filter}.smudge`, 'tr a-z A-Z']);
        ctx.git(['config', '--global', `filter.${filter}.clean`, 'tr A-Z a-z']);
      }
      commitFile(ctx, ctx.repo, 'other.txt', 'second\n');
      const base = commitFile(ctx, ctx.repo, '.gitattributes',
        'tracked.txt filter=unspecified\nother.txt filter=unset\n');
      const native = join(ctx.root, 'native-filter');
      ctx.git(['worktree', 'add', '--quiet', '--detach', native, base]);
      const expected = read(native, 'tracked.txt');
      assert.equal(expected, 'INITIAL\n');
      const other = read(native, 'other.txt');
      assert.equal(other, 'SECOND\n');
      for (const history of ['tip', 'local', 'isolated']) {
        const ws = ctx.spawn('filtered-' + history, ['--history', history, '--base', base]);
        assert.equal(read(ws.path, 'tracked.txt'), expected);
        assert.equal(read(ws.path, 'other.txt'), other);
        assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
      }
      assert.equal(read(ctx.repo, 'tracked.txt'), 'initial\n');
      assert.equal(read(ctx.repo, 'other.txt'), 'second\n');
    },
  },
  {
    name: 'a failed tip export leaves an unpublished index and a recoverable workspace',
    run(ctx) {
      const tip = commitFile(ctx, ctx.repo, 'selected', 'content missing from the export\n');
      const blob = ctx.git(['rev-parse', tip + ':selected']);
      const loose = join(ctx.repo, '.git', 'objects', blob.slice(0, 2), blob.slice(2));
      const held = join(ctx.root, 'held-blob');
      renameSync(loose, held);
      let failure;
      try {
        failure = ctx.fail(['new', 'incomplete', '--base', tip, '--history', 'tip']);
      } finally {
        renameSync(held, loose);
      }
      assert.equal(failure.code, 'new_incomplete');
      const {path, lease} = objectResult(failure.details);
      assert(typeof path === 'string' && typeof lease === 'string');
      assert(!existsSync(join(path, '.git', 'index')));
      writeFileSync(join(path, 'my-recovery-note'), 'retain this file\n');
      const args = ['incomplete', '--lease', lease];
      ctx.cli(['prune', ...args, '--discard']);
      assert(!existsSync(path));
      const restored = ctx.cli(['restore', ...args]);
      assert.equal(restored.path, path);
      assert.equal(read(path, 'my-recovery-note'), 'retain this file\n');
      assert.equal(read(ctx.repo, 'selected'), 'content missing from the export\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD']), tip);
      assert.equal(ctx.git(['status', '--porcelain']), '');
    },
  },
  {
    name: 'cold tip checkout preserves dirty donor files and falls back for a missing file',
    run(ctx) {
      ctx.git(['config', 'core.autocrlf', 'false']);
      writeFileSync(join(ctx.repo, '.gitattributes'), '*.txt text eol=crlf\n');
      writeFileSync(join(ctx.repo, 'converted.txt'), 'selected\n');
      writeFileSync(join(ctx.repo, 'plain'), 'original\n');
      ctx.git(['add', '.gitattributes', 'converted.txt', 'plain']);
      const link = looseBlob(ctx, ctx.repo, 'link-target', 'converted.txt');
      ctx.git(['update-index', '--add', '--cacheinfo', '120000', link, 'selected-link']);
      ctx.git(['commit', '-qm', 'selected checkout']);
      const base = ctx.git(['rev-parse', 'HEAD']);
      const tree = ctx.git(['rev-parse', `${base}^{tree}`]);
      const expected = tipObjects(ctx, base);
      ctx.git(['switch', '-c', 'unrelated']);
      const unrelated = commitFile(ctx, ctx.repo, 'other.txt', 'other branch\n');
      ctx.git(['switch', 'main']);
      const reference = join(ctx.root, 'native-reference');
      ctx.git(['clone', '-q', '--no-hardlinks', '--no-checkout', ctx.repo, reference], ctx.root);
      ctx.git(['checkout', '-q', '--detach', base], reference);
      assert.equal(read(reference, 'converted.txt'), 'selected\r\n');

      ctx.git(['update-index', '--assume-unchanged', 'tracked.txt', 'plain']);
      writeFileSync(join(ctx.repo, 'tracked.txt'), 'changed\n');
      // This same-sized edit is eligible for direct cloning until its bytes are hashed.
      writeFileSync(join(ctx.repo, 'plain'), 'modified\n');
      unlinkSync(join(ctx.repo, 'converted.txt'));
      writeFileSync(join(ctx.repo, 'staged.txt'), 'staged only\n');
      ctx.git(['add', 'staged.txt']);
      const staged = ctx.git(['rev-parse', ':staged.txt']);
      const orphan = looseBlob(ctx, ctx.repo, 'orphan-tip', 'orphan only\n');
      const index = join(ctx.repo, '.git', 'index');
      const indexBytes = readFileSync(index);
      const dirty = join(ctx.repo, 'tracked.txt');
      const before = lstatSync(dirty, { bigint: true });
      const originalBytes = readFileSync(dirty);
      const stagedFile = join(ctx.repo, 'staged.txt');
      const stagedBytes = readFileSync(stagedFile);
      const stagedBefore = lstatSync(stagedFile, { bigint: true });
      assert.equal(originalBytes.length, Buffer.byteLength('initial\n'));

      const check = (path: string, name: string) => {
        checkout(ctx, path, base, `refs/heads/graft/${name}`);
        assert.equal(ctx.git(['rev-parse', 'HEAD^{tree}'], path), tree);
        assert.deepEqual(objects(ctx, path), expected);
        assert([unrelated, staged, orphan].every(id => !objects(ctx, path).includes(id)));
        for (const file of ['plain', 'tracked.txt', 'converted.txt', '.gitattributes']) {
          assert.deepEqual(readFileSync(join(path, file)), readFileSync(join(reference, file)), file);
        }
        const expectedLink = join(reference, 'selected-link');
        const actualLink = join(path, 'selected-link');
        assert.equal(lstatSync(actualLink).isSymbolicLink(), lstatSync(expectedLink).isSymbolicLink());
        if (lstatSync(expectedLink).isSymbolicLink()) assert.equal(readlinkSync(actualLink), readlinkSync(expectedLink));
        else assert.deepEqual(readFileSync(actualLink), readFileSync(expectedLink));
        assert(!existsSync(join(path, 'staged.txt')));
        assertSeparateRegularFiles([index, join(path, '.git', 'index')]);
        assert.deepEqual(readFileSync(index), indexBytes);
        assert.deepEqual(readFileSync(dirty), originalBytes);
        assert.equal(read(ctx.repo, 'plain'), 'modified\n');
        assert(!existsSync(join(ctx.repo, 'converted.txt')));
        const after = lstatSync(dirty, { bigint: true });
        assert.deepEqual([after.dev, after.ino, after.mode, after.size, after.mtimeNs, after.ctimeNs],
          [before.dev, before.ino, before.mode, before.size, before.mtimeNs, before.ctimeNs]);
        assert.deepEqual(readFileSync(stagedFile), stagedBytes);
        const stagedAfter = lstatSync(stagedFile, { bigint: true });
        assert.deepEqual([stagedAfter.dev, stagedAfter.ino, stagedAfter.mode, stagedAfter.size, stagedAfter.mtimeNs, stagedAfter.ctimeNs],
          [stagedBefore.dev, stagedBefore.ino, stagedBefore.mode, stagedBefore.size, stagedBefore.mtimeNs, stagedBefore.ctimeNs]);
      };

      const first = ctx.spawn('cold-tip', ['--history', 'tip', '--base', base]);
      check(first.path, 'cold-tip');
      assertSeparateRegularFiles([dirty, join(first.path, 'tracked.txt')]);
      writeFileSync(join(first.path, 'plain'), 'new edit\n');
      assert.equal(ctx.git(['status', '--porcelain'], first.path).trim(), 'M plain');
      assert.equal(read(ctx.repo, 'plain'), 'modified\n');

      const fallback = ctx.spawn('missing-tip', ['--history', 'tip', '--base', base]);
      check(fallback.path, 'missing-tip');
      assert(!existsSync(join(ctx.repo, 'converted.txt')));
      writeFileSync(join(fallback.path, 'tracked.txt'), 'edited!\n');
      assert.match(ctx.git(['status', '--porcelain'], fallback.path), / M tracked\.txt/);
      assert.deepEqual(readFileSync(index), indexBytes);
      assert.deepEqual(readFileSync(dirty), originalBytes);
    },
  },
  {
    name: 'tip history exposes exactly the selected commit and tree, and lands back onto full history',
    run(ctx) {
      const parent = ctx.git(['rev-parse', 'HEAD']);
      ctx.git(['switch', '-c', 'topic']);
      const base = commitFile(ctx, ctx.repo, 'selected.txt', 'selected from topic\n');
      ctx.git(['switch', 'main']);
      const unrelated = commitFile(ctx, ctx.repo, 'other.txt', 'main only\n');
      ctx.git(['repack', '-ad']);
      const expected = tipObjects(ctx, base);

      // Warm the full-ancestry cache first. The narrower visibility must survive a cache hit.
      const full = ctx.spawn('ancestry', ['--history', 'isolated', '--base', base]);
      assert(objects(ctx, full.path).includes(parent));
      ctx.git(['config', '--local', 'graft.history', 'tip']);
      const args = ['--request-id', randomUUID(), '--base', 'topic'];
      const work = ctx.spawn('tip-branch', args);
      ctx.git(['config', '--local', 'graft.history', 'local']);
      assert.equal(ctx.spawn('tip-branch', args).lease, work.lease);
      const second = ctx.spawn('tip-commit', ['--history', 'tip', '--base', base]);
      for (const ws of [work, second]) {
        assert.deepEqual(objects(ctx, ws.path), expected);
        assert.equal(ctx.git(['rev-list', '--all'], ws.path), base);
        checkout(ctx, ws.path, base, 'refs/heads/graft/' + (ws === work ? 'tip-branch' : 'tip-commit'));
      }
      const tip = commitFile(ctx, work.path, 'selected.txt', 'landed edit\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD^'], work.path), base);
      ctx.cli(['land', 'tip-branch', '--lease', work.lease, '--into', 'topic']);
      assert.equal(ctx.git(['rev-parse', 'topic']), tip);
      assert.equal(ctx.git(['rev-parse', 'topic~2']), parent);
      assert.equal(ctx.git(['rev-parse', 'main']), unrelated);
      assert.equal(ctx.git(['rev-parse', '--is-shallow-repository']), 'false');
      const restored = ctx.cli(['restore', 'tip-branch', '--lease', work.lease]);
      assert.equal(restored.path, work.path);
      assert.equal(ctx.git(['show', 'HEAD:selected.txt'], work.path), 'landed edit');
      assert.deepEqual(objects(ctx, second.path), expected);
      ctx.git(['fsck', '--full']);
    },
  },
  {
    name: 'local and tip checkouts share raw files while honoring external checkout attributes',
    run(ctx) {
      const plain = randomBytes(1024 * 1024);
      writeFileSync(join(ctx.repo, 'plain.bin'), plain);
      writeFileSync(join(ctx.repo, 'other.txt'), 'other\n');
      writeFileSync(join(ctx.repo, 'expanded.txt'), '$Id$\n');
      ctx.git(['add', 'plain.bin', 'other.txt', 'expanded.txt']);
      ctx.git(['commit', '-qm', 'raw and converted files']);
      const attributes = join(ctx.repo, 'custom attrs');
      writeFileSync(attributes, '*.txt text eol=crlf\n');
      mkdirSync(join(ctx.repo, '.git/info'), { recursive: true });
      writeFileSync(join(ctx.repo, '.git/info/attributes'), 'other.txt text=auto eol=lf\nexpanded.txt ident eol=lf\n');
      ctx.git(['config', '--local', 'core.attributesFile', 'custom attrs']);
      ctx.git(['config', '--local', 'core.autocrlf', 'false']);
      const reference = join(ctx.root, 'native');
      ctx.git(['-c', 'core.attributesFile=' + attributes, 'worktree', 'add', '-q', '--detach', reference, 'HEAD']);
      assert.equal(read(reference, 'tracked.txt'), 'initial\r\n');
      assert.equal(read(reference, 'other.txt'), 'other\n');
      assert.match(read(reference, 'expanded.txt'), /^\$Id: [a-f0-9]+ \$\n$/);
      const nativeClone = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
      for (const history of ['local', 'tip']) {
        if (history === 'tip') {
          ctx.git(['config', '--local', '--unset', 'core.attributesFile']);
          const config = join(ctx.root, 'home', '.config', 'git');
          mkdirSync(config, {recursive:true});
          writeFileSync(join(config, 'attributes'), readFileSync(attributes));
        }
        const ws = ctx.spawn('external-' + history, ['--history', history]);
        for (const file of ['tracked.txt', 'other.txt', 'expanded.txt', 'plain.bin']) {
          assert.deepEqual(readFileSync(join(ws.path, file)), readFileSync(join(reference, file)));
        }
        assert.equal(ctx.git(['write-tree'], ws.path), ctx.git(['rev-parse', 'HEAD^{tree}']));
        assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
        if (nativeClone && ['darwin', 'linux'].includes(process.platform)) {
          const bytes = objectResult(ws.result.storage).cloned_bytes;
          const expected = ['.gitignore', 'plain.bin', 'other.txt']
            .reduce((sum, name) => sum + readFileSync(join(reference, name)).length, 0);
          assert.equal(bytes, expected, 'expected the unchanged files to share donor data');
        }
        writeFileSync(join(ws.path, 'plain.bin'), 'private edit');
        assert.deepEqual(readFileSync(join(ctx.repo, 'plain.bin')), plain);
      }
    },
  },
  {
    name: 'local and isolated new grafts own independent objects, refs, indexes and checkouts',
    async run(ctx) {
      ctx.git(['config', 'core.autocrlf', 'true']);
      writeFileSync(join(ctx.repo, '.gitattributes'), '*.lf text eol=lf\n*.raw -text\n');
      writeFileSync(join(ctx.repo, 'converted.txt'), 'one\ntwo\n');
      writeFileSync(join(ctx.repo, 'forced.lf'), 'explicit\nLF\n');
      writeFileSync(join(ctx.repo, 'binary.raw'), '\0binary\r\nbytes\n');
      ctx.git(['add', '.']);
      ctx.git(['commit', '-qm', 'checkout conversions']);
      ctx.git(['checkout-index', '--force', '--all']);
      const base = ctx.git(['rev-parse', 'HEAD']);
      ctx.git(['switch', '-c', 'side']);
      const side = commitFile(ctx, ctx.repo, 'side.txt', 'packed side branch\n');
      ctx.git(['tag', 'donor-tag', side]);
      ctx.git(['switch', 'main']);
      ctx.git(['repack', '-ad']);

      const orphan = looseBlob(ctx, ctx.repo, 'orphan-input', 'unreferenced donor blob\n');
      const primaryFile = join(ctx.repo, 'tracked.txt');
      writeFileSync(primaryFile, 'primary staged\r\n');
      ctx.git(['add', 'tracked.txt']);
      const staged = ctx.git(['rev-parse', ':tracked.txt']);
      writeFileSync(primaryFile, 'primary unstaged\r\n');
      writeFileSync(join(ctx.repo, 'donor-untracked.txt'), 'untracked donor bytes\n');
      const donorObjects = objects(ctx);
      const donorRefs = ctx.git(['for-each-ref', '--format=%(refname) %(objectname)']);
      const selected = closure(ctx, base);
      assert([side, staged, orphan].every(id => donorObjects.includes(id) && !selected.includes(id)));

      const probe = objectResult(ctx.cli(['doctor']).source_storage);
      assert.equal(typeof probe.native_clone, 'boolean');
      const a = ctx.spawn('local-a', ['--history', 'local', '--base', 'main', '--branch', 'work']);
      ctx.command('git', ['config', '--local', '--unset-all', 'graft.history'], ctx.repo);
      const retry = ['--base', 'main', '--detach', '--request-id', randomUUID()];
      const b = ctx.spawn('local-b', retry);
      ctx.git(['config', '--local', 'graft.history', 'isolated']);
      assert.equal(ctx.spawn('local-b', retry).lease, b.lease, 'retry must retain the original history choice');
      const legacyRetry = ['--base', 'main', '--request-id', randomUUID()];
      const configured = ctx.spawn('configured-isolated', legacyRetry);
      assert.deepEqual(objects(ctx, configured.path), selected, 'project default must exclude unrelated objects');
      const overridden = ctx.spawn('override-local', ['--history', 'local', '--base', 'main']);
      assert.deepEqual(objects(ctx, overridden.path), probe.native_clone ? donorObjects : selected, 'explicit policy must override project default');
      ctx.git(['config', '--local', 'graft.history', 'local']);
      assert.equal(ctx.spawn('configured-isolated', legacyRetry).lease, configured.lease, 'legacy isolated reservation must retry after reconfiguration');
      const isolated = ctx.spawn('exact', ['--history', 'isolated', '--base', 'main', '--branch', 'work']);
      ctx.git(['config', '--local', '--unset', 'graft.history']);
      const workspaces = [a, b, isolated];
      for (const ws of workspaces) {
        const local = ws !== isolated && probe.native_clone;
        const receipt = objectResult(objectResult(ws.result.storage).objects);
        assert.equal(receipt.object_format, 'sha1');
        assert.equal(typeof receipt.source, 'string');
        if (local) {
          assert.equal(receipt.source, 'local-snapshot');
          assert.equal(receipt.method, probe.backend);
          assert(typeof receipt.pack_count === 'number' && receipt.pack_count > 0);
          assert(typeof receipt.loose_count === 'number' && receipt.loose_count >= 2);
          assert.equal(ctx.git(['cat-file', '-p', staged], ws.path), 'primary staged');
          assert.equal(ctx.git(['cat-file', '-p', orphan], ws.path), 'unreferenced donor blob');
        } else {
          assert.notEqual(receipt.source, 'local-snapshot');
        }
        assert.deepEqual(objects(ctx, ws.path), local ? donorObjects : selected, ws.path);
        checkout(ctx, ws.path, base, ws === b ? '' : 'refs/heads/work');
        assert.equal(ctx.git(['branch', '--show-current'], ws.path), ws === b ? '' : 'work');
        assert.equal(ctx.git(['show', ':tracked.txt'], ws.path), 'initial');
        assert.equal(ctx.git(['show', ':converted.txt'], ws.path), 'one\ntwo');
        assert.equal(read(ws.path, 'tracked.txt'), 'initial\r\n');
        assert.equal(read(ws.path, 'converted.txt'), 'one\r\ntwo\r\n');
        assert.equal(read(ws.path, 'forced.lf'), 'explicit\nLF\n');
        assert.equal(read(ws.path, 'binary.raw'), '\0binary\r\nbytes\n');
        assert.equal(existsSync(join(ws.path, 'donor-untracked.txt')), false);
        assert.equal(existsSync(join(ws.path, 'side.txt')), false);
      }
      assertSeparateRegularFiles([ctx.repo, ...workspaces.map(ws => ws.path)].map(path => join(path, 'tracked.txt')));
      if (probe.native_clone) {
        const store = join('.git', 'objects');
        const packed = readdirSync(join(ctx.repo, store, 'pack')).filter(name => /\.(pack|idx)$/.test(name));
        const objectFiles = [join(orphan.slice(0, 2), orphan.slice(2)), ...packed.map(name => join('pack', name))];
        for (const file of objectFiles) {
          assertSeparateRegularFiles([ctx.repo, a.path, b.path].map(path => join(path, store, file)));
        }
      }
      ctx.measure('native_object_snapshot', probe.native_clone);

      // Both workspaces can stage and commit different edits to the same path at once.
      writeFileSync(join(a.path, 'tracked.txt'), 'first agent\r\n');
      writeFileSync(join(b.path, 'tracked.txt'), 'second agent\r\n');
      ctx.git(['add', 'tracked.txt'], a.path);
      ctx.git(['add', 'tracked.txt'], b.path);
      assert.equal(ctx.git(['show', ':tracked.txt'], a.path), 'first agent');
      assert.equal(ctx.git(['show', ':tracked.txt'], b.path), 'second agent');
      const commits = await Promise.allSettled([a, b].map(ws =>
        exec('git', ['commit', '-qm', 'workspace edit'], { cwd: ws.path, env: ctx.env, timeout: 120_000 })
      ));
      for (const commit of commits) if (commit.status === 'rejected') throw commit.reason;
      const first = ctx.git(['rev-parse', 'refs/heads/work'], a.path);
      const second = ctx.git(['rev-parse', 'HEAD'], b.path);
      assert.notEqual(first, second);
      assert.equal(ctx.git(['rev-parse', 'HEAD^'], a.path), base);
      assert.equal(ctx.git(['rev-parse', 'HEAD^'], b.path), base);
      assert.equal(ctx.git(['show', 'HEAD:tracked.txt'], a.path), 'first agent');
      assert.equal(ctx.git(['show', 'HEAD:tracked.txt'], b.path), 'second agent');
      checkout(ctx, a.path, first, 'refs/heads/work');
      checkout(ctx, b.path, second, '');
      checkout(ctx, isolated.path, base, 'refs/heads/work');
      assert.deepEqual(objects(ctx), donorObjects, 'workspace commits changed the primary object database');
      assert.deepEqual(objects(ctx, isolated.path), selected);
      assert(!objects(ctx, a.path).includes(second), 'detached commit leaked to sibling');
      assert(!objects(ctx, b.path).includes(first), 'branch commit leaked to sibling');
      assert.equal(ctx.git(['for-each-ref', '--format=%(refname) %(objectname)']), donorRefs);
      assert.equal(ctx.git(['show', ':tracked.txt']), 'primary staged');
      assert.equal(read(ctx.repo, 'tracked.txt'), 'primary unstaged\r\n');
      assert.equal(read(isolated.path, 'tracked.txt'), 'initial\r\n');

      // Change the donor pack and prune a loose object the native snapshot owns.
      const advanced = ctx.git(['commit-tree', `${side}^{tree}`, '-p', side, '-m', 'primary advanced']);
      ctx.git(['update-ref', 'refs/heads/side', advanced, side]);
      ctx.git(['repack', '-ad']);
      ctx.git(['gc', '--prune=now']);
      assert(!objects(ctx).includes(orphan), 'fixture Git GC must remove the donor orphan');
      const parked = join(ctx.root, 'moved-primary');
      renameSync(ctx.repo, parked);
      try {
        for (const ws of workspaces) {
          ctx.git(['fsck', '--full'], ws.path);
          const tip = ws === a ? first : ws === b ? second : base;
          assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip);
          assert.equal(ctx.git(['show', `${base}:tracked.txt`], ws.path), 'initial');
          assert(!objects(ctx, ws.path).includes(advanced), 'post-spawn primary object leaked into the snapshot');
        }
        if (probe.native_clone) {
          for (const ws of [a, b]) {
            assert.equal(ctx.git(['cat-file', '-p', orphan], ws.path), 'unreferenced donor blob');
            assert.equal(ctx.git(['show', `${side}:side.txt`], ws.path), 'packed side branch');
          }
        }
      } finally {
        renameSync(parked, ctx.repo);
      }
    },
  },
  {
    name: 'local history falls back to a standalone exact export for an alternate object store',
    run(ctx) {
      const base = ctx.git(['rev-parse', 'HEAD']);
      const selected = closure(ctx, base);
      const unrelated = looseBlob(ctx, ctx.repo, 'alternate-orphan', 'unreferenced alternate blob\n');
      const borrower = join(ctx.root, 'borrower');
      ctx.git(['clone', '--shared', ctx.repo, borrower], ctx.root);
      assert.equal(ctx.git(['cat-file', '-p', unrelated], borrower), 'unreferenced alternate blob');

      const ws = ctx.spawn('alternate', ['--history', 'local'], borrower);
      const receipt = objectResult(objectResult(ws.result.storage).objects);
      assert.equal(typeof receipt.source, 'string');
      assert.notEqual(receipt.source, 'local-snapshot');
      renameSync(ctx.repo, join(ctx.root, 'moved-alternate'));
      checkout(ctx, ws.path, base, 'refs/heads/graft/alternate');
      assert.deepEqual(objects(ctx, ws.path), selected);
      ctx.git(['fsck', '--full'], ws.path);
    },
  },
  {
    name: 'local-history land and undo transfer only the selected work and allow recovery',
    run(ctx) {
      const base = ctx.git(['rev-parse', 'HEAD']);
      const primaryObjects = objects(ctx);
      const feature = ctx.spawn('local-feature', ['--history', 'local']);
      const sibling = ctx.spawn('local-sibling', ['--history', 'local']);

      ctx.git(['switch', '--detach', base], feature.path);
      const privateTip = commitFile(ctx, feature.path, 'private.txt', 'unrelated work\n');
      ctx.git(['branch', 'private-work', privateTip], feature.path);
      const unreferenced = looseBlob(ctx, feature.path, 'workspace-orphan', 'workspace-only loose blob\n');
      ctx.git(['switch', 'graft/local-feature'], feature.path);
      const tip = commitFile(ctx, feature.path, 'feature.txt', 'selected work\n');
      const selected = closure(ctx, tip, feature.path);
      const siblingObjects = objects(ctx, sibling.path);
      assert([privateTip, unreferenced].every(id => !selected.includes(id) && !siblingObjects.includes(id)));
      assert.deepEqual(objects(ctx), primaryObjects);

      const siblingTip = commitFile(ctx, sibling.path, 'other.txt', 'sibling-only work\n');
      const siblingFile = join(sibling.path, 'tracked.txt');
      writeFileSync(siblingFile, 'sibling staged\n');
      ctx.git(['add', 'tracked.txt'], sibling.path);
      writeFileSync(siblingFile, 'sibling unstaged\n');
      writeFileSync(join(sibling.path, 'untracked.txt'), 'sibling untracked\n');
      const siblingStatus = ctx.git(['status', '--porcelain'], sibling.path);

      const landed = ctx.cli(['land', 'local-feature', '--lease', feature.lease, '--into', 'main']);
      assert.equal(landed.previous, base);
      assert.equal(landed.commit, tip);
      assert.equal(landed.recovery_lease, feature.lease);
      assert.equal(existsSync(feature.path), false);
      assert.equal(ctx.git(['rev-parse', 'main']), tip);
      assert.equal(read(ctx.repo, 'feature.txt'), 'selected work\n');
      assert.deepEqual(objects(ctx), [...new Set([...primaryObjects, ...selected])].sort(), 'landing transferred unrelated objects');
      assert.equal(ctx.git(['for-each-ref', '--format=%(refname)']), 'refs/heads/main');
      assert(!objects(ctx, sibling.path).includes(tip), 'landing published the commit into the sibling');

      const undo = ctx.cli(['land', 'local-feature', '--undo', '--lease', feature.lease]);
      assert.equal(ctx.git(['rev-parse', 'main']), undo.commit);
      assert.equal(ctx.git(['rev-parse', 'main^']), tip);
      assert.equal(ctx.git(['rev-parse', 'main^{tree}']), ctx.git(['rev-parse', `${base}^{tree}`]));
      assert.equal(existsSync(join(ctx.repo, 'feature.txt')), false);
      ctx.git(['fsck', '--full']);

      const restoredFeature = ctx.cli(['restore', 'local-feature', '--lease', feature.lease]);
      assert.equal(restoredFeature.path, feature.path);
      assert.equal(typeof restoredFeature.lease, 'string');
      assert.notEqual(restoredFeature.lease, feature.lease);
      assert.equal(ctx.git(['rev-parse', 'HEAD', 'private-work'], feature.path), `${tip}\n${privateTip}`);
      assert.equal(ctx.git(['cat-file', '-p', unreferenced], feature.path), 'workspace-only loose blob');
      assert.equal(read(feature.path, 'feature.txt'), 'selected work\n');
      ctx.git(['fsck', '--full'], feature.path);

      assert.equal(ctx.git(['status', '--porcelain'], sibling.path), siblingStatus);
      const dropped = ctx.cli(['prune', 'local-sibling', '--lease', sibling.lease, '--discard']);
      assert.equal(dropped.recovery_lease, sibling.lease);
      assert.equal(existsSync(sibling.path), false);
      ctx.git(['gc', '--prune=now']);
      const restoredSibling = ctx.cli(['restore', 'local-sibling', '--lease', sibling.lease]);
      assert.equal(restoredSibling.path, sibling.path);
      assert.equal(typeof restoredSibling.lease, 'string');
      assert.notEqual(restoredSibling.lease, sibling.lease);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], sibling.path), siblingTip);
      assert.equal(ctx.git(['status', '--porcelain'], sibling.path), siblingStatus);
      assert.equal(ctx.git(['show', ':tracked.txt'], sibling.path), 'sibling staged');
      assert.equal(read(sibling.path, 'tracked.txt'), 'sibling unstaged\n');
      assert.equal(read(sibling.path, 'untracked.txt'), 'sibling untracked\n');
      ctx.git(['fsck', '--full'], sibling.path);
    },
  },
];
