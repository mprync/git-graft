/** Disk and time for workspaces of a project with real npm dependencies.
 * node e2e/dependency-bench.ts [--bin target/release/git-graft] [--workspaces 3] [--output PATH]
 *
 * Builds a small web project whose dependencies are a typical React, Vite and TypeScript
 * toolchain, installs them once in the primary checkout, then compares two ways to give
 * each task a ready-to-test checkout:
 *   worktree  `git worktree add` followed by `npm ci` in the new checkout
 *   graft     `git graft spawn`, with the primary's installed tree registered once by
 *             `git graft seed-layer --shared`
 * Disk is the increase in physical APFS file data under the fixture after each workspace,
 * measured by e2e/apfs-file-data.py, so it counts only blocks a workspace really owns.
 * Needs macOS on APFS and npm. Installs run offline, so fill the npm cache first by
 * installing DEPENDENCIES once anywhere; both arms then install from the same cache. */
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { parseArgs } from 'node:util';

const { values } = parseArgs({ options: {
  bin: { type: 'string' }, workspaces: { type: 'string' }, output: { type: 'string' },
} });
const bin = resolve(values.bin ?? 'target/release/git-graft');
const count = Number(values.workspaces ?? 3);
assert(Number.isSafeInteger(count) && count > 0, '--workspaces must be a positive integer');
const output = resolve(values.output ?? 'target/e2e/dependency-bench.json');
const observer = resolve(import.meta.dirname, 'apfs-file-data.py');

/** A typical front-end toolchain; versions are pinned so the tree is reproducible. */
const DEPENDENCIES = {
  dependencies: { react: '19.1.0', 'react-dom': '19.1.0' },
  devDependencies: {
    vite: '7.1.3', '@vitejs/plugin-react': '5.0.1', typescript: '5.9.2', vitest: '3.2.4',
    eslint: '9.34.0', '@types/react': '19.1.12', '@types/react-dom': '19.1.9',
  },
};

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

function timed(steps: Array<[string, string[], string]>): number {
  const start = performance.now();
  for (const [command, args, cwd] of steps) run(command, args, cwd);
  return (performance.now() - start) / 1000;
}

/** Physical APFS file data, in bytes, under root. */
function fileData(root: string): bigint {
  const result = JSON.parse(run('python3', ['-I', '-B', observer, root], root));
  assert(result.available, `APFS file data is unavailable: ${result.reason}`);
  return BigInt(result.bytes);
}

function primary(root: string): string {
  const repo = join(root, 'app');
  mkdirSync(join(repo, 'src'), { recursive: true });
  writeFileSync(join(repo, 'package.json'), JSON.stringify({ name: 'app', private: true, ...DEPENDENCIES }, null, 2));
  writeFileSync(join(repo, '.gitignore'), 'node_modules/\n');
  writeFileSync(join(repo, 'src/main.ts'), 'export const answer = 42;\n');
  run('npm', ['install', '--offline', '--ignore-scripts', '--no-audit', '--no-fund'], repo);
  run('git', ['init', '-q', '-b', 'main'], repo);
  run('git', ['add', '.'], repo);
  run('git', ['-c', 'user.name=Bench', '-c', 'user.email=bench@example.invalid', 'commit', '-qm', 'app'], repo);
  return repo;
}

type Sample = { seconds: number; bytes: number };

function arm(name: 'worktree' | 'graft'): Sample[] {
  const root = realpathSync(mkdtempSync(join(tmpdir(), `graft-deps-${name}-`)));
  try {
    const repo = primary(root);
    if (name === 'graft') run(bin, ['--repo', repo, '--json', 'seed-layer', '--shared', '--key', 'bench-npm'], repo);
    const samples: Sample[] = [];
    let before = fileData(root);
    for (let index = 1; index <= count; index++) {
      const seconds = name === 'worktree'
        ? timed([
          ['git', ['worktree', 'add', '-q', '-b', `task-${index}`, join(root, `task-${index}`)], repo],
          ['npm', ['ci', '--offline', '--ignore-scripts', '--no-audit', '--no-fund'], join(root, `task-${index}`)],
        ])
        : timed([[bin, ['--repo', repo, '--json', 'spawn', `task-${index}`, '--base', 'main'], repo]]);
      const after = fileData(root);
      samples.push({ seconds, bytes: Number(after - before) });
      before = after;
    }
    return samples;
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

const median = (numbers: number[]) => [...numbers].sort((a, b) => a - b)[Math.floor(numbers.length / 2)];
const results = { worktree: arm('worktree'), graft: arm('graft') };
const summary = Object.fromEntries(Object.entries(results).map(([name, samples]) => [name, {
  medianSeconds: median(samples.map((sample) => sample.seconds)),
  medianMiB: median(samples.map((sample) => sample.bytes)) / 2 ** 20,
}]));
mkdirSync(resolve(output, '..'), { recursive: true });
writeFileSync(output, JSON.stringify({ dependencies: DEPENDENCIES, workspaces: count, results, summary }, null, 2));
console.table(summary);
console.log(`Written to ${output}`);
