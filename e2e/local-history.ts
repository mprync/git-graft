/** Exercise local object snapshots and isolated history through the public CLI. */
import assert from 'node:assert/strict';
import { randomUUID } from 'node:crypto';
import { execFile } from 'node:child_process';
import { existsSync, lstatSync, readFileSync, readdirSync, renameSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { promisify } from 'node:util';
import { assertSeparateRegularFiles, objectResult, type Context, type Scenario } from './harness.ts';

const exec = promisify(execFile);

function objects(ctx: Context, path = ctx.repo): string[] {
  return ctx.git(['cat-file', '--batch-all-objects', '--batch-check=%(objectname)'], path).split('\n').sort();
}

function closure(ctx: Context, tip: string, path = ctx.repo): string[] {
  return ctx.git(['rev-list', '--objects', '--no-object-names', tip], path).split('\n').sort();
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
    name: 'local checkout applies the copied attributes file from the workspace root',
    run(ctx) {
      writeFileSync(join(ctx.repo, 'custom attrs'), '*.txt text eol=crlf\n');
      ctx.git(['config', '--local', 'core.attributesFile', 'custom attrs']);
      ctx.git(['config', '--local', 'core.autocrlf', 'false']);
      const ws = ctx.spawn('external-attrs', ['--history', 'local']);
      assert.equal(read(ws.path, 'tracked.txt'), 'initial\r\n');
      assert.equal(ctx.git(['show', ':tracked.txt'], ws.path), 'initial');
      assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
    },
  },
  {
    name: 'local and isolated spawns own independent objects, refs, indexes and checkouts',
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
      ctx.git(['config', '--local', 'graft.history', 'local']);
      const retry = ['--base', 'main', '--detach', '--request-id', randomUUID()];
      const b = ctx.spawn('local-b', retry);
      const isolated = ctx.spawn('exact', ['--history', 'isolated', '--base', 'main', '--branch', 'work']);
      ctx.git(['config', '--local', '--unset', 'graft.history']);
      assert.equal(ctx.spawn('local-b', retry).lease, b.lease, 'retry must retain the original history choice');
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

      const undo = ctx.cli(['undo-land', 'local-feature', '--lease', feature.lease]);
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
      const dropped = ctx.cli(['drop', 'local-sibling', '--lease', sibling.lease, '--discard']);
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
