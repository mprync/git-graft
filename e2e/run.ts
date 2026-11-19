/** Run the shipped binary in disposable repositories and check its live CLI and MCP surfaces. */
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { platform, release } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Context, Unavailable, execute, objectResult, type Scenario } from './harness.ts';

const project = fileURLToPath(new URL('../', import.meta.url));
const groups = ['lifecycle', 'storage', 'mixed-history', 'local-history', 'dependencies', 'dependency-reuse', 'registered-managers', 'landing', 'recovery', 'undo', 'safety', 'edge-cases', 'mcp', 'workspace', 'space'];
const args = process.argv.slice(2);
const group = args[0] === '--group' ? args[1] : undefined;
if (args.length && (args.length !== 2 || !groups.includes(group ?? ''))) {
  throw new Error('Usage: node e2e/run.ts [--group ' + groups.join('|') + ']');
}
const rounds = group === 'space' ? Number(process.env.GIT_GRAFT_BENCH_REPEATS ?? 3) : 1;
assert(Number.isSafeInteger(rounds) && rounds > 0, 'GIT_GRAFT_BENCH_REPEATS must be a positive integer');

function releaseBinary(): string {
  const output = execFileSync('cargo', [
    'build', '--release', '--locked', '--bin', 'git-graft', '--message-format=json',
  ], { cwd: project, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'] });
  for (const line of output.trim().split('\n').reverse()) {
    const entry = objectResult(JSON.parse(line));
    if (entry.reason === 'compiler-artifact' && typeof entry.executable === 'string') {
      return entry.executable;
    }
  }
  throw new Error('Cargo did not report the git-graft executable');
}

type Result = {
  group: string;
  name: string;
  round: number;
  status: 'passed' | 'failed' | 'unavailable';
  seconds: number;
  facts: Record<string, unknown>;
  error?: string;
};

function surface(required: string[], advertised: string[], exercised: Set<string>) {
  const all = [...new Set([...required, ...advertised])].sort();
  const passed = all.filter(name => advertised.includes(name) && exercised.has(name));
  return {
    advertised,
    exercised: passed,
    missing: all.filter(name => !passed.includes(name)),
    passed: passed.length,
    total: all.length,
  };
}

function spaceSummary(samples: Record<string, unknown>[], installed: boolean): string {
  // Keep the timings paired with the volume readings from the same round.
  const signedBytes = (sample: Record<string, unknown>): number => {
    assert(typeof sample.savedBytes === 'string');
    const bytes = Number(sample.savedBytes);
    assert(Number.isSafeInteger(bytes));
    return bytes;
  };
  const sorted = samples.toSorted((a, b) => signedBytes(a) - signedBytes(b));
  const space = sorted[Math.floor((sorted.length - 1) / 2)];
  const native = objectResult(space.native);
  const graft = objectResult(space.graft);
  function mib(bytes: unknown): string {
    assert(typeof bytes === 'string' && /^-?\d+$/.test(bytes));
    return (Number(bytes) / 1024 ** 2).toFixed(2);
  }
  assert(typeof native.totalSeconds === 'number' && typeof graft.totalSeconds === 'number');
  return [
    installed ? 'Workspaces with installed npm dependencies' : 'Source checkouts',
    '',
    '| Method | Change in used space on the volume | Total time |',
    '| --- | ---: | ---: |',
    '| ' + (installed ? 'Git + npm ci' : 'git worktree add') + ' | ' + mib(native.freeBytesDelta) + ' MiB | ' + native.totalSeconds.toFixed(3) + ' s |',
    '| ' + (installed ? 'Graft + installed layer' : 'git graft spawn') + ' | ' + mib(graft.freeBytesDelta) + ' MiB | ' + graft.totalSeconds.toFixed(3) + ' s |',
    '',
    'The native volume change minus the Graft volume change was ' + mib(space.savedBytes) + ' MiB.',
    'These signed readings include other filesystem activity. For checkout storage, see the file-block measurements in docs/benchmarks.md.',
    ...(sorted.length > 1 ? [
      'This pair has the median difference across ' + sorted.length + ' rounds. The differences ranged from ' +
      mib(sorted[0].savedBytes) + ' to ' + mib(sorted[sorted.length - 1].savedBytes) + ' MiB.',
    ] : []),
  ].join('\n');
}

function identifyBinary(path: string) {
  const bytes = readFileSync(path);
  return { bytes: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') };
}

const binary = process.env.GIT_GRAFT_BIN ? resolve(process.env.GIT_GRAFT_BIN) : releaseBinary();
const binaryIdentity = identifyBinary(binary);
const help = execute(binary, ['--help'], project, process.env);
assert.equal(help.status, 0, help.stderr);
const advertisedCli = [...help.stdout.matchAll(/^  ([a-z][a-z-]+)\s{2,}/gm)]
  .map(match => match[1]).filter(name => name !== 'help').sort();
// Keep the advertised contract even when a regression removes a command from discovery.
const publicCli = 'add-dep doctor drop env exec gc heartbeat land layer list list-managers lock mcp move register-manager repair restore seed-layer spawn status undo-land unregister-manager unlock'.split(' ');
const publicTools = 'add_dependency discard doctor env exec gc heartbeat land_and_wipe layer list list_managers lock move register_manager repair restore seed_layer spawn status undo_land unregister_manager unlock'
  .split(' ').map(name => 'workspace_' + name);
const coveredCli = new Set<string>();
const coveredTools = new Set<string>();
const advertisedTools = new Set<string>();
const results: Result[] = [];
for (const suite of group ? [group] : groups) {
  const { cases }: { cases: Scenario[] } = await import('./' + suite + '.ts');
  assert(cases.length > 0, 'Empty group ' + suite);
  assert.equal(new Set(cases.map(item => item.name)).size, cases.length, 'Duplicate scenario in ' + suite);
  const runs = Array.from({ length: rounds }, (_, index) => cases.map(scenario => ({ scenario, round: index + 1 }))).flat();
  for (const { scenario, round } of runs) {
    const started = performance.now();
    let ctx: Context | undefined;
    let failure: unknown;
    try {
      ctx = new Context(binary);
      await scenario.run(ctx);
    } catch (error) {
      failure = error;
    }
    try {
      ctx?.close();
    } catch (error) {
      failure ??= error;
    }
    const status = failure instanceof Unavailable ? 'unavailable' : failure ? 'failed' : 'passed';
    const facts = ctx?.observations ?? {};
    if (status === 'passed' && ctx) {
      for (const command of ctx.commands) coveredCli.add(command);
      for (const tool of ctx.tools) coveredTools.add(tool);
      if (Array.isArray(facts.advertised_tools)) {
        for (const name of facts.advertised_tools) {
          assert(typeof name === 'string');
          advertisedTools.add(name);
        }
      }
    }
    const error = failure instanceof Error ? failure.stack ?? failure.message : failure ? String(failure) : undefined;
    results.push({ group: suite, name: scenario.name, round, status, seconds: (performance.now() - started) / 1000, facts, error });
    console.log(status.padEnd(11) + ' ' + suite + ' / ' + scenario.name + (rounds > 1 ? ' (round ' + round + ')' : ''));
    if (error) console.error(error);
  }
}

const cli = surface(publicCli, advertisedCli, coveredCli);
const mcp = surface(publicTools, [...advertisedTools].sort(), coveredTools);
const passed = results.filter(result => result.status === 'passed').length;
const unavailable = results.filter(result => result.status === 'unavailable').length;
const failed = results.length - passed - unavailable;
const ok = passed === results.length && (group !== undefined || (cli.missing.length === 0 && mcp.missing.length === 0));
assert.deepEqual(identifyBinary(binary), binaryIdentity, 'The executable changed during the end-to-end run');
const report = {
  generated_at: new Date().toISOString(),
  platform: platform(),
  os_release: release(),
  architecture: process.arch,
  node: process.version,
  git: execFileSync('git', ['--version'], { encoding: 'utf8' }).trim(),
  binary_version: execFileSync(binary, ['--version'], { encoding: 'utf8' }).trim(),
  binary: binaryIdentity,
  build: process.env.GIT_GRAFT_BUILD_KIND ?? (process.env.GIT_GRAFT_BIN ? 'supplied binary' : 'release'),
  group: group ?? 'all',
  ok,
  counts: { passed, failed, unavailable, total: results.length },
  cli, mcp, results,
};
const reportPath = resolve(process.env.GIT_GRAFT_REPORT
  ?? resolve(project, 'target', 'e2e', platform() + '-' + process.arch + '-' + (group ?? 'all') + '.json'));
mkdirSync(dirname(reportPath), { recursive: true });
writeFileSync(reportPath, JSON.stringify(report, null, 2) + '\n');
const summary = [passed + '/' + results.length + ' scenarios passed on ' + platform() + ' ' + process.arch +
  '. ' + failed + ' failed, ' + unavailable + ' unavailable.'];
if (!group) {
  summary.push('CLI ' + cli.passed + '/' + cli.total + ', MCP ' + mcp.passed + '/' + mcp.total + ' public operations exercised.');
  if (cli.missing.length || mcp.missing.length) console.error('Missing public operations:', [...cli.missing, ...mcp.missing].join(', '));
}
for (const key of ['space', 'dependency_space']) {
  const samples = results.filter(result => result.status === 'passed' && result.facts[key])
    .map(result => objectResult(result.facts[key]));
  if (samples.length) summary.push(spaceSummary(samples, key === 'dependency_space'));
}
console.log(summary.join('\n\n'));
console.log('Report:', reportPath);
if (process.env.GITHUB_STEP_SUMMARY) {
  appendFileSync(process.env.GITHUB_STEP_SUMMARY, '## git-graft on ' + platform() + ' ' + process.arch + '\n\n' + summary.join('\n\n') + '\n');
}
if (!ok) process.exitCode = 1;
