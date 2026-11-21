import assert from 'node:assert/strict';
import { chmodSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { delimiter, dirname, join } from 'node:path';
import { execute, parseExact, type Scenario } from './harness.ts';

type Context = Parameters<Scenario['run']>[0];
type Workspace = ReturnType<Context['spawn']>;
function layer(workspace: Workspace, name: string, action: string, key = 'node-v1'): string[] {
  return ['deps', 'layer', action, name, '--lease', workspace.lease, '--key', key, ...(action === 'remove' ? [] : ['--copy'])];
}
function write(path: string, data: string): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, data);
}
function commit(ctx: Context, message: string): void {
  ctx.git(['add', '-A']);
  ctx.git(['commit', '-qm', message]);
}
function npmProject(ctx: Context, manifest: Record<string, unknown> = {}): void {
  write(join(ctx.repo, 'package.json'), JSON.stringify({ name: 'fixture', version: '1.0.0', ...manifest }) + '\n');
  write(join(ctx.repo, 'package-lock.json'), JSON.stringify({ name: 'fixture', version: '1.0.0', lockfileVersion: 3, requires: true, packages: { '': { name: 'fixture', version: '1.0.0' } } }) + '\n');
  commit(ctx, 'npm project');
}
function object(value: unknown): Record<string, unknown> {
  assert.ok(value && typeof value === 'object' && !Array.isArray(value));
  return Object.fromEntries(Object.entries(value));
}

export const cases: Scenario[] = [
  {
    name: 'Git discovers the subcommand from a project subdirectory and accepts a lease from the environment',
    run(ctx) {
      const nested = join(ctx.repo, 'subdirectory');
      mkdirSync(nested);
      const pathKey = Object.keys(ctx.env).find(key => key.toUpperCase() === 'PATH');
      assert(pathKey && ctx.env[pathKey]);
      ctx.env[pathKey] = dirname(ctx.bin) + delimiter + ctx.env[pathKey];
      const created = object(JSON.parse(ctx.git(['graft', 'add', 'subcommand'], nested)));
      assert(typeof created.lease === 'string' && typeof created.path === 'string');
      ctx.env.GIT_GRAFT_LEASE = created.lease;
      const heartbeat = object(JSON.parse(ctx.git(['graft', 'heartbeat', 'subcommand'], nested)));
      assert.equal(heartbeat.name, 'subcommand');
      assert.equal(heartbeat.phase, 'ready');
      assert.equal(ctx.git(['rev-parse', '--abbrev-ref', 'HEAD'], created.path), 'graft/subcommand');
      ctx.git(['graft', 'remove', 'subcommand'], nested);
      assert.equal(existsSync(created.path), false);
      assert.equal(ctx.git(['status', '--porcelain']), '');
    },
  },
  {
    name: 'graft creation validates portable names and keeps same-named primary branches private',
    run(ctx) {
      const head = ctx.git(['rev-parse', 'HEAD']);
      for (const name of ['../escape', 'CAPS', 'nul', '1agent', 'a'.repeat(49)]) {
        assert.equal(ctx.fail(['add', name]).code, 'invalid_name');
      }
      assert.equal(existsSync(join(ctx.root, 'escape')), false);
      ctx.git(['branch', 'graft/occupied']);
      const workspace = ctx.spawn('occupied');
      const owned = ['occupied', '--lease', workspace.lease];
      assert.equal(ctx.git(['branch', '--show-current'], workspace.path), 'graft/occupied');
      write(join(workspace.path, 'tracked.txt'), 'private same-name branch\n');
      ctx.git(['commit', '-qam', 'private commit'], workspace.path);
      ctx.cli(['remove', ...owned, '--discard']);
      ctx.cli(['restore', ...owned]);
      assert.equal(ctx.git(['show', 'graft/occupied:tracked.txt'], workspace.path), 'private same-name branch');
      assert.equal(ctx.git(['rev-parse', 'graft/occupied']), head);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), head);
      assert.equal(ctx.git(['status', '--porcelain']), '');
    },
  },
  {
    name: 'pending creation blocks workspace operations until recovered',
    run(ctx) {
      const ws = ctx.spawn('pending');
      write(join(ws.path, 'untracked'), 'preserve\n');
      assert(typeof ws.result.id === 'string');
      const path = join(ctx.repo, '.git', 'graft', 'workspaces', 'records', ws.result.id + '.json');
      const record = readFileSync(path, 'utf8');
      writeFileSync(path, JSON.stringify({ ...object(parseExact(record)), status: 'pending', initialized: false }));
      for (const command of [
        ['heartbeat', 'pending', '--lease', ws.lease],
        ['exec', 'pending', '--lease', ws.lease, '--', process.execPath, '-e', 'process.exit(0)'],
      ]) {
        const fault = ctx.fail(command);
        assert.match(String(fault.message), /pending operation/i);
        assert.equal(ctx.cli(['status', 'pending']).phase, 'creating');
      }
      assert.equal(readFileSync(join(ws.path, 'untracked'), 'utf8'), 'preserve\n');
      writeFileSync(path, record);
      ctx.cli(['heartbeat', 'pending', '--lease', ws.lease]);
      assert.equal(readFileSync(join(ws.path, 'untracked'), 'utf8'), 'preserve\n');
    },
  },
  {
    name: 'layer rejects empty key and tracked dirty source',
    run(ctx) {
      npmProject(ctx);
      const ws = ctx.spawn('dirty-layer');
      write(join(ws.path, 'node_modules', 'fixture', 'index.js'), 'module.exports = 1;\n');
      assert.match(String(ctx.fail(layer(ws, 'dirty-layer', 'save', ' ')).message), /nonempty/i);
      write(join(ws.path, 'tracked.txt'), 'user changes\n');
      assert.match(String(ctx.fail(layer(ws, 'dirty-layer', 'save')).message), /tracked source and index must be clean/i);
      assert.equal(readFileSync(join(ws.path, 'tracked.txt'), 'utf8'), 'user changes\n');
      assert.equal(readFileSync(join(ws.path, 'node_modules', 'fixture', 'index.js'), 'utf8'), 'module.exports = 1;\n');
      write(join(ws.path, 'tracked.txt'), 'initial\n');
      ctx.cli(layer(ws, 'dirty-layer', 'save'));
    },
  },
  {
    name: 'layer rejects multiple root lockfiles without touching installed files',
    run(ctx) {
      npmProject(ctx);
      write(join(ctx.repo, 'yarn.lock'), '# alternate lock\n');
      commit(ctx, 'second lock');
      const ws = ctx.spawn('multiple-locks');
      const module = join(ws.path, 'node_modules', 'fixture', 'index.js');
      write(module, 'keep\n');
      const fault = ctx.fail(layer(ws, 'multiple-locks', 'save'));
      assert.match(String(fault.message), /exactly one root lockfile/i);
      assert.equal(readFileSync(module, 'utf8'), 'keep\n');
      assert.equal(ctx.git(['status', '--porcelain'], ws.path), '');
    },
  },
  {
    name: 'layer rejects local dependencies before capturing cache',
    run(ctx) {
      npmProject(ctx, { dependencies: { local: 'file:../local' } });
      const ws = ctx.spawn('local-layer');
      const module = join(ws.path, 'node_modules', 'local', 'index.js');
      write(module, 'keep local\n');
      assert.match(String(ctx.fail(layer(ws, 'local-layer', 'save')).message), /local and workspace dependencies/i);
      assert.equal(readFileSync(module, 'utf8'), 'keep local\n');
    },
  },
  {
    name: 'layer rejects duplicate save and corrupt provenance on restore or remove',
    run(ctx) {
      npmProject(ctx);
      const producer = ctx.spawn('producer');
      const consumer = ctx.spawn('consumer');
      const installed = join(producer.path, 'node_modules', 'fixture', 'index.js');
      write(installed, 'original\n');
      const saved = ctx.cli(layer(producer, 'producer', 'save'));
      assert.equal(typeof saved.digest, 'string');
      assert.match(String(ctx.fail(layer(producer, 'producer', 'save')).message), /already exists/i);
      const env = ctx.cli(['status', 'producer', '--env']);
      assert.equal(typeof env.cache_dir, 'string');
      const provenance = join(dirname(String(env.cache_dir)), 'layers', String(saved.digest), 'provenance.json');
      const valid = readFileSync(provenance, 'utf8');
      const broken = object(parseExact(valid));
      broken.compatibility = 'wrong';
      writeFileSync(provenance, JSON.stringify(broken));
      for (const action of ['restore', 'remove']) {
        assert.match(String(ctx.fail(layer(consumer, 'consumer', action)).message), /invalid layer provenance/i);
      }
      assert.equal(existsSync(join(consumer.path, 'node_modules')), false);
      assert.equal(readFileSync(installed, 'utf8'), 'original\n');
      assert.equal(readFileSync(provenance, 'utf8'), JSON.stringify(broken));
      writeFileSync(provenance, valid);
      ctx.cli(layer(consumer, 'consumer', 'restore'));
      assert.equal(readFileSync(join(consumer.path, 'node_modules', 'fixture', 'index.js'), 'utf8'), 'original\n');
      ctx.cli(layer(producer, 'producer', 'remove'));
      assert.equal(existsSync(provenance), false);
      assert.equal(readFileSync(installed, 'utf8'), 'original\n');
      rmSync(join(consumer.path, 'node_modules'), { recursive: true });
    },
  },
  {
    name: 'a pre-commit hook that edits files during merge --commit keeps the commit and the edits',
    run(ctx) {
      const ws = ctx.spawn('hooked');
      const before = ctx.git(['rev-parse', 'HEAD']);
      const hooks = join(ctx.root, 'editing-hooks');
      write(join(hooks, 'pre-commit'), '#!/bin/sh\nprintf "hook edit\\n" >> tracked.txt\nprintf "hook output\\n" > hook.txt\n');
      chmodSync(join(hooks, 'pre-commit'), 0o755);
      ctx.git(['config', '--local', 'core.hooksPath', hooks], ws.path);
      writeFileSync(join(ws.path, 'tracked.txt'), 'agent work\n');
      const merge = ['merge', 'hooked', '--lease', ws.lease, '--commit', 'Agent work'];
      // The hook runs after staging, so its edits are not in the commit. The
      // merge must refuse instead of archiving them or merging a partial tree.
      assert.match(String(ctx.fail(merge).message), /uncommitted changes/i);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), before);
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'initial\n');
      assert.equal(existsSync(join(ctx.repo, 'hook.txt')), false);
      assert.equal(ctx.git(['log', '-1', '--format=%s'], ws.path), 'Agent work');
      assert.equal(ctx.git(['show', 'HEAD:tracked.txt'], ws.path), 'agent work');
      assert.equal(readFileSync(join(ws.path, 'tracked.txt'), 'utf8'), 'agent work\nhook edit\n');
      assert.equal(readFileSync(join(ws.path, 'hook.txt'), 'utf8'), 'hook output\n');
      // Committing the hook's edits lets the retry merge everything.
      ctx.git(['config', '--local', '--unset', 'core.hooksPath'], ws.path);
      ctx.git(['add', '-A'], ws.path);
      ctx.git(['commit', '-qm', 'Hook edits'], ws.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], ws.path);
      assert.equal(ctx.cli(['merge', 'hooked', '--lease', ws.lease]).commit, tip);
      assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'agent work\nhook edit\n');
      assert.equal(readFileSync(join(ctx.repo, 'hook.txt'), 'utf8'), 'hook output\n');
    },
  },
  {
    name: 'initialised submodules stay untouched in the primary and block archiving in a graft',
    run(ctx) {
      writeFileSync(join(ctx.env.HOME!, 'gitconfig'), '[core]\n\tautocrlf = false\n[protocol "file"]\n\tallow = always\n');
      const library = join(ctx.root, 'library');
      mkdirSync(library);
      ctx.git(['init', '-q', '-b', 'main'], library);
      writeFileSync(join(library, 'lib.txt'), 'library\n');
      ctx.git(['add', '.'], library);
      ctx.git(['commit', '-qm', 'library'], library);
      ctx.git(['submodule', '-q', 'add', library, 'vendor/lib']);
      ctx.git(['commit', '-qm', 'Add submodule']);
      const submodule = join(ctx.repo, 'vendor', 'lib');
      const subHead = ctx.git(['rev-parse', 'HEAD'], submodule);
      const ws = ctx.spawn('outer');
      writeFileSync(join(ws.path, 'tracked.txt'), 'outer change\n');
      ctx.git(['commit', '-qam', 'Outer change'], ws.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], ws.path);
      // Local work inside the primary's submodule blocks the merge.
      writeFileSync(join(submodule, 'lib.txt'), 'library\nlocal edit\n');
      writeFileSync(join(submodule, 'scratch.txt'), 'untracked in submodule\n');
      assert.match(String(ctx.fail(['merge', 'outer', '--lease', ws.lease]).message), /local changes/i);
      assert.equal(readFileSync(join(submodule, 'lib.txt'), 'utf8'), 'library\nlocal edit\n');
      assert.equal(readFileSync(join(submodule, 'scratch.txt'), 'utf8'), 'untracked in submodule\n');
      assert.equal(ctx.git(['rev-parse', 'HEAD'], submodule), subHead);
      ctx.git(['checkout', '-q', '--', '.'], submodule);
      rmSync(join(submodule, 'scratch.txt'));
      // A clean merge leaves the submodule checkout and its history alone.
      assert.equal(ctx.cli(['merge', 'outer', '--lease', ws.lease]).commit, tip);
      assert.equal(ctx.git(['rev-parse', 'HEAD'], submodule), subHead);
      assert.equal(readFileSync(join(submodule, 'lib.txt'), 'utf8'), 'library\n');
      assert.equal(ctx.git(['status', '--porcelain']), '');

      // A submodule initialised inside a graft is refused, never archived partially.
      const inner = ctx.spawn('inner');
      ctx.git(['submodule', '-q', 'update', '--init'], inner.path);
      const innerSub = join(inner.path, 'vendor', 'lib');
      writeFileSync(join(innerSub, 'work.txt'), 'submodule work\n');
      for (const args of [['merge', 'inner'], ['remove', 'inner'], ['remove', 'inner', '--discard']]) {
        assert.equal(ctx.fail([...args, '--lease', inner.lease]).code, 'nested_git_metadata', args.join(' '));
      }
      assert.equal(readFileSync(join(innerSub, 'work.txt'), 'utf8'), 'submodule work\n');
      ctx.git(['fsck', '--full'], innerSub);
      assert.equal(ctx.cli(['status', 'inner']).name, 'inner');
    },
  },
  {
    name: 'Unicode file names survive merge, archive and restore',
    run(ctx) {
      const names = ['café-nfc.txt', 'café-nfd.txt', '日本語.txt', '\u{1f333}-graft.txt', 'résumé/été.txt'];
      const ws = ctx.spawn('unicode');
      for (const name of names) write(join(ws.path, name), `contents of ${name}\n`);
      ctx.git(['add', '-A'], ws.path);
      ctx.git(['commit', '-qm', 'Unicode names'], ws.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], ws.path);
      write(join(ws.path, 'übung-notes.txt'), 'uncommitted Unicode\n');
      const restore = ctx.spawn('unicode-archive');
      for (const name of names) write(join(restore.path, name), `untracked ${name}\n`);
      ctx.cli(['remove', 'unicode-archive', '--lease', restore.lease, '--discard']);
      const restored = String(ctx.cli(['restore', 'unicode-archive', '--lease', restore.lease]).path);
      for (const name of names) assert.equal(readFileSync(join(restored, name), 'utf8'), `untracked ${name}\n`, name);

      rmSync(join(ws.path, 'übung-notes.txt'));
      assert.equal(ctx.cli(['merge', 'unicode', '--lease', ws.lease]).commit, tip);
      for (const name of names) assert.equal(readFileSync(join(ctx.repo, name), 'utf8'), `contents of ${name}\n`, name);
      assert.equal(ctx.git(['status', '--porcelain']), '');
      const back = String(ctx.cli(['restore', 'unicode', '--lease', ws.lease]).path);
      for (const name of names) assert.equal(readFileSync(join(back, name), 'utf8'), `contents of ${name}\n`, name);
    },
  },
  {
    name: 'case-only renames merge and case-colliding trees are never silently archived',
    run(ctx) {
      const ws = ctx.spawn('renamed');
      ctx.git(['mv', 'tracked.txt', 'Tracked.txt'], ws.path);
      ctx.git(['commit', '-qm', 'Rename by case'], ws.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], ws.path);
      assert.equal(ctx.cli(['merge', 'renamed', '--lease', ws.lease]).commit, tip);
      assert.equal(ctx.git(['ls-files']), '.gitignore\nTracked.txt');
      assert(readdirSync(ctx.repo).includes('Tracked.txt'), 'the primary kept the old spelling');
      assert.equal(readFileSync(join(ctx.repo, 'Tracked.txt'), 'utf8'), 'initial\n');
      assert.equal(ctx.git(['status', '--porcelain']), '');

      // A tree holding two names that differ only by case.
      const file = join(ctx.root, 'blob');
      writeFileSync(file, 'lower\n');
      const lo = ctx.git(['hash-object', '-w', file]);
      writeFileSync(file, 'upper\n');
      const up = ctx.git(['hash-object', '-w', file]);
      // Build the tree in a private index, leaving the primary's untouched.
      const env = { ...ctx.env, GIT_INDEX_FILE: join(ctx.root, 'collide.index') };
      const run = (args: string[]) => {
        const out = execute('git', args, ctx.repo, env);
        assert.equal(out.status, 0, out.stderr);
        return out.stdout.trim();
      };
      run(['read-tree', 'HEAD']);
      run(['update-index', '--add', '--cacheinfo', `100644,${lo},case.txt`, '--cacheinfo', `100644,${up},CASE.txt`]);
      const tree = run(['write-tree']);
      const collide = ctx.git(['commit-tree', tree, '-p', 'HEAD', '-m', 'Names that differ by case']);
      ctx.git(['branch', 'collide', collide]);
      const primaryIndex = readFileSync(join(ctx.repo, '.git', 'index'));
      const coll = ctx.spawn('collide', ['--base', 'collide']);
      assert.equal(ctx.git(['show', 'HEAD:case.txt'], coll.path), 'lower');
      assert.equal(ctx.git(['show', 'HEAD:CASE.txt'], coll.path), 'upper');
      if (ctx.git(['status', '--porcelain'], coll.path) !== '') {
        // One checkout file holds both names. Graft reports that as local work.
        assert.match(String(ctx.fail(['remove', 'collide', '--lease', coll.lease]).message), /local changes/i);
        assert.match(String(ctx.fail(['merge', 'collide', '--lease', coll.lease, '--into', 'collide']).message), /uncommitted changes/i);
      }
      assert.equal(ctx.git(['rev-parse', 'collide']), collide);
      assert.deepEqual(readFileSync(join(ctx.repo, '.git', 'index')), primaryIndex);
      assert.equal(ctx.git(['status', '--porcelain']), '');
    },
  },
  {
    name: 'very long paths survive add, merge, archive and restore',
    run(ctx) {
      writeFileSync(join(ctx.env.HOME!, 'gitconfig'), '[core]\n\tautocrlf = false\n\tlongpaths = true\n');
      const segments = Array.from({ length: 12 }, (_, i) => `directory-segment-${String(i).padStart(2, '0')}-xxxxxx`);
      const relative = join(...segments, 'deeply-nested-file-name.txt');
      assert(relative.length > 300, 'the fixture path should exceed 300 characters');
      write(join(ctx.repo, relative), 'deep base\n');
      commit(ctx, 'Deep path');
      const ws = ctx.spawn('deep');
      assert.equal(readFileSync(join(ws.path, relative), 'utf8'), 'deep base\n');
      writeFileSync(join(ws.path, relative), 'deep change\n');
      ctx.git(['commit', '-qam', 'Deep change'], ws.path);
      const tip = ctx.git(['rev-parse', 'HEAD'], ws.path);
      write(join(ws.path, ...segments, 'untracked-deep-notes.txt'), 'deep notes\n');
      ctx.cli(['remove', 'deep', '--lease', ws.lease, '--discard']);
      const restored = ctx.cli(['restore', 'deep', '--lease', ws.lease]);
      const path = String(restored.path);
      assert.equal(readFileSync(join(path, ...segments, 'untracked-deep-notes.txt'), 'utf8'), 'deep notes\n');
      rmSync(join(path, ...segments, 'untracked-deep-notes.txt'));
      assert.equal(ctx.cli(['merge', 'deep', '--lease', String(restored.lease)]).commit, tip);
      assert.equal(readFileSync(join(ctx.repo, relative), 'utf8'), 'deep change\n');
      assert.equal(ctx.git(['status', '--porcelain']), '');
    },
  },
  ...([['decomposed', 'CAFÉ', ['darwin']], ['precomposed', 'CAFÉ', ['darwin', 'win32']]] as const)
    .map(([form, spelling, folding]): Scenario => ({
      name: `an ignored ${form} name that differs only by Unicode case is never overwritten`,
      run(ctx) {
        // The primary's ignored directory is uppercase; the graft's incoming
        // path is lowercase and precomposed (NFC). APFS folds case and
        // normalisation, NTFS folds case only, and Linux folds neither.
        const ignored = join(ctx.repo, spelling);
        write(join(ignored, 'secret.env'), 'keep this ignored file\n');
        write(join(ctx.repo, '.git', 'info', 'exclude'), '/CAF*/\n');
        const folds = existsSync(join(ctx.repo, 'café'));
        if ((folding as readonly string[]).includes(process.platform)) assert(folds, 'expected a case-insensitive volume');
        const ws = ctx.spawn('unicode-collision');
        write(join(ws.path, 'café', 'secret.env'), 'incoming tracked file\n');
        ctx.git(['add', '-A'], ws.path);
        ctx.git(['commit', '-qm', 'Incoming path'], ws.path);
        const tip = ctx.git(['rev-parse', 'HEAD'], ws.path);
        const before = ctx.git(['rev-parse', 'HEAD']);
        const merge = ['merge', 'unicode-collision', '--lease', ws.lease];
        if (folds) {
          assert.match(String(ctx.fail(merge).message), /ignored path would be overwritten/i);
          assert.equal(ctx.git(['rev-parse', 'HEAD']), before);
          assert.equal(ctx.git(['rev-parse', 'HEAD'], ws.path), tip);
        } else {
          assert.equal(ctx.cli(merge).commit, tip);
        }
        assert.equal(readFileSync(join(ignored, 'secret.env'), 'utf8'), 'keep this ignored file\n');
      },
    })),
];
