/** Total disk and time for many workspaces of one real repository.
 * node e2e/scale-bench.ts [--repo /opt/homebrew] [--bin target/release/git-graft]
 *   [--workspaces 100] [--output PATH]
 *
 * Makes a disposable local clone of the repository, warms its index with `git status`
 * as after `git clone`, then creates the same number of checkouts two ways: with
 * `git worktree add` and with `git graft add`. Disk is the increase in physical APFS
 * file data under the fixture once every checkout exists, measured by
 * e2e/apfs-file-data.py. The source repository is only read. Needs macOS on APFS. */
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: {
  repo: { type: 'string' }, bin: { type: 'string' }, workspaces: { type: 'string' }, output: { type: 'string' },
} });
const source = resolve(values.repo ?? '/opt/homebrew');
const bin = resolve(values.bin ?? 'target/release/git-graft');
const count = Number(values.workspaces ?? 100);
assert(Number.isSafeInteger(count) && count > 0, '--workspaces must be a positive integer');
const output = resolve(values.output ?? 'target/e2e/scale-bench.json');
const observer = resolve(import.meta.dirname, 'apfs-file-data.py');

/** Git runs with default settings, as for a new user, so personal configuration such
 * as core.autocrlf cannot change what either arm writes. */
const gitConfig = join(realpathSync(mkdtempSync(join(tmpdir(), 'graft-bench-config-'))), 'gitconfig');
writeFileSync(gitConfig, '[core]\n\tautocrlf = false\n');
const env = { ...process.env, GIT_CONFIG_GLOBAL: gitConfig, GIT_CONFIG_NOSYSTEM: '1' };

function run(command: string, args: string[], cwd: string): string {
  const result = spawnSync(command, args, { cwd, env, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
  assert.equal(result.status, 0, `${command} ${args.join(' ')} failed in ${cwd}:\n${result.stderr}`);
  return result.stdout;
}

/** Physical APFS file data, in bytes, under root. */
function fileData(root: string): bigint {
  const result = JSON.parse(run('python3', ['-I', '-B', observer, root], root));
  assert(result.available, `APFS file data is unavailable: ${result.reason}`);
  return BigInt(result.bytes);
}

function arm(name: 'worktree' | 'graft') {
  const root = realpathSync(mkdtempSync(join(tmpdir(), `graft-scale-${name}-`)));
  try {
    const repo = join(root, 'primary');
    run('git', ['clone', '--quiet', '--no-hardlinks', source, repo], root);
    run('git', ['status', '--porcelain'], repo);
    const files = Number(run('git', ['ls-files', '-z'], repo).split('\0').filter(Boolean).length);
    const before = fileData(root);
    const start = performance.now();
    for (let index = 1; index <= count; index++) {
      if (name === 'worktree') run('git', ['worktree', 'add', '-q', '--detach', join(root, `task-${index}`)], repo);
      else run(bin, ['--repo', repo, '--json', 'add', `task-${index}`, '--detach'], repo);
    }
    const seconds = (performance.now() - start) / 1000;
    const bytes = Number(fileData(root) - before);
    return { files, seconds, bytes, mib: bytes / 2 ** 20, perWorkspaceMiB: bytes / 2 ** 20 / count };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

const results = { worktree: arm('worktree'), graft: arm('graft') };
mkdirSync(resolve(output, '..'), { recursive: true });
writeFileSync(output, JSON.stringify({ source, workspaces: count, git: run('git', ['--version'], '.').trim(), results }, null, 2));
console.table(results);
console.log(`Written to ${output}`);
