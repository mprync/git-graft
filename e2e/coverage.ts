/** Real Rust source coverage from the full TypeScript E2E suite.
 * Prerequisites: Node 24+, Rust/Cargo with the locked dependencies, Git and
 * the tools required by e2e/run.ts, plus llvm-cov and llvm-profdata matching
 * rustc's LLVM major (install `rustup component add llvm-tools-preview`, or
 * set LLVM_COV and LLVM_PROFDATA to absolute paths). Run: node e2e/coverage.ts
 * Add --html for annotated source. No network access or test selection is used.
 */
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, isAbsolute, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const outputRoot = join(root, 'target', 'e2e');
const usage = 'Install `rustup component add llvm-tools-preview`, or set LLVM_COV and LLVM_PROFDATA to absolute paths for tools matching rustc LLVM major.';

type Output = { stdout: string; stderr: string };
function command(exe: string, args: string[], options: { env?: NodeJS.ProcessEnv; capture?: boolean } = {}): Output {
  const result = spawnSync(exe, args, {
    cwd: root, env: options.env ?? process.env, encoding: 'utf8',
    stdio: options.capture ? ['ignore', 'pipe', 'pipe'] : 'inherit',
    maxBuffer: 256 * 1024 * 1024, windowsHide: true,
  });
  if (result.error) throw new Error(`Cannot run ${exe}: ${result.error.message}`);
  if (result.status !== 0) throw new Error(`${exe} exited ${result.status ?? `with signal ${result.signal}`}${options.capture ? `: ${result.stderr}` : ''}`);
  return { stdout: result.stdout ?? '', stderr: result.stderr ?? '' };
}

function llvmTool(envKey: string, name: string, sysrootBin: string, major: number): { path: string; version: string } {
  const override = process.env[envKey];
  if (override && !isAbsolute(override)) throw new Error(`${envKey} must be an absolute path. ${usage}`);
  const bundled = join(sysrootBin, name + (process.platform === 'win32' ? '.exe' : ''));
  const candidates = override ? [override] : [bundled, name];
  for (const path of candidates) {
    const result = spawnSync(path, ['--version'], { encoding: 'utf8', windowsHide: true });
    if (result.status !== 0) continue;
    const version = `${result.stdout ?? ''}\n${result.stderr ?? ''}`.trim();
    const parsed = /(?:LLVM|version)\s+version\s+(\d+)|LLVM version\s+(\d+)|version\s+(\d+)/i.exec(version);
    const detected = parsed && Number(parsed[1] ?? parsed[2] ?? parsed[3]);
    if (detected === major) return { path, version: version.split(/\r?\n/)[0] };
    if (override) throw new Error(`${envKey} has LLVM major ${detected || 'unknown'}, expected ${major}. ${usage}`);
  }
  throw new Error(`Cannot find ${name} for LLVM ${major}. ${usage}`);
}

function rustFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap(entry => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return rustFiles(path);
    return entry.isFile() && entry.name.endsWith('.rs') ? [path] : [];
  }).sort();
}

type Metric = { count: number; covered: number; percent: number };
type FileEntry = { filename: string; summary: { lines: Metric; functions: Metric; regions: Metric } };
type CoverageExport = { data: { files: FileEntry[] }[] };
function metric(entries: FileEntry[], key: 'lines' | 'functions' | 'regions'): Metric {
  const count = entries.reduce((total, entry) => total + entry.summary[key].count, 0);
  const covered = entries.reduce((total, entry) => total + entry.summary[key].covered, 0);
  return { count, covered, percent: count ? 100 * covered / count : 0 };
}

function main(): void {
  if (process.argv.slice(2).some(arg => arg !== '--html')) throw new Error('Usage: node e2e/coverage.ts [--html]');
  const rustVersion = command('rustc', ['-vV'], { capture: true }).stdout;
  const host = /^host:\s*(\S+)/m.exec(rustVersion)?.[1];
  const llvm = /^LLVM version:\s*(\d+)(?:\.\d+)*/m.exec(rustVersion);
  if (!host || !llvm) throw new Error('Cannot determine rustc host and LLVM version from rustc -vV');
  const sysroot = command('rustc', ['--print', 'sysroot'], { capture: true }).stdout.trim();
  const toolDir = join(sysroot, 'lib', 'rustlib', host, 'bin');
  const cov = llvmTool('LLVM_COV', 'llvm-cov', toolDir, Number(llvm[1]));
  const profdata = llvmTool('LLVM_PROFDATA', 'llvm-profdata', toolDir, Number(llvm[1]));
  mkdirSync(outputRoot, { recursive: true });
  const runDir = mkdtempSync(join(outputRoot, `coverage-${process.platform}-${process.arch}-`));
  const target = join(runDir, 'cargo-target');
  const rawDir = join(runDir, 'raw');
  mkdirSync(rawDir);
  let completed = false;
  try {
    const bin = join(target, 'debug', `git-graft${process.platform === 'win32' ? '.exe' : ''}`);
    const env = { ...process.env, RUSTC_WRAPPER: '', CARGO_BUILD_BUILD_DIR: join(runDir, 'cargo-build'),
      RUSTFLAGS: '', CARGO_ENCODED_RUSTFLAGS: '' };
    console.log(`Building instrumented debug binary in ${runDir}`);
    command('cargo', ['rustc', '--locked', '--bin', 'git-graft', '--target-dir', target, '--', '-C', 'instrument-coverage'], { env });
    const e2eReport = join(runDir, 'e2e-report.json');
    console.log('Running the full TypeScript E2E suite with the instrumented debug binary; timings are not release benchmarks.');
    const suiteEnv: NodeJS.ProcessEnv = { ...env, GIT_GRAFT_BIN: bin, GIT_GRAFT_BUILD_KIND: 'instrumented debug coverage',
      GIT_GRAFT_REPORT: e2eReport, LLVM_PROFILE_FILE: join(rawDir, '%m-%p.profraw') };
    delete suiteEnv.GITHUB_STEP_SUMMARY;
    command(process.execPath, [join(root, 'e2e', 'run.ts')], { env: suiteEnv });
    if (!existsSync(e2eReport)) throw new Error(`Full E2E runner did not produce ${e2eReport}`);
    const raw = readdirSync(rawDir).filter(name => name.endsWith('.profraw')).map(name => join(rawDir, name));
    if (!raw.length) throw new Error('No LLVM profiles were produced by the E2E processes');
    const profile = join(runDir, 'coverage.profdata');
    command(profdata.path, ['merge', '-sparse', rawDir, '-o', profile]);
    const sources = rustFiles(join(root, 'src'));
    if (!sources.length) throw new Error('No Rust source files found in src/');
    const exported = command(cov.path, ['export', bin, `--instr-profile=${profile}`, '--sources', ...sources], { capture: true }).stdout;
    const parsed: CoverageExport = JSON.parse(exported);
    const entries = parsed.data.flatMap(data => data.files).filter(entry => {
      const path = relative(join(root, 'src'), resolve(root, entry.filename));
      return path !== '' && path !== '..' && !path.startsWith(`..${sep}`) && !isAbsolute(path) && path.endsWith('.rs');
    });
    if (!entries.length) throw new Error('LLVM export contains no src/ Rust coverage records');
    const files = entries.map(entry => ({ path: relative(root, resolve(root, entry.filename)).split(sep).join('/'),
      lines: entry.summary.lines, functions: entry.summary.functions, regions: entry.summary.regions })).sort((a, b) => a.path.localeCompare(b.path));
    const included = new Set(files.map(file => file.path));
    const unreported = sources.map(path => relative(root, path).split(sep).join('/')).filter(path => !included.has(path));
    const report = {
      platform: process.platform, arch: process.arch, rustc: rustVersion.trim(),
      llvm_cov: cov, llvm_profdata: profdata,
      scope: 'Rust src/**/*.rs coverage mappings emitted for the non-test instrumented debug binary exercised by the full TypeScript E2E suite. Unmapped source is not counted.',
      benchmark_timing: 'Instrumented debug E2E timings are separate from release timings and must not be compared as release benchmarks.',
      e2e_feature_report: e2eReport,
      profiles: raw.length,
      totals: { lines: metric(entries, 'lines'), functions: metric(entries, 'functions'), regions: metric(entries, 'regions') },
      files, source_files_without_coverage_mapping: unreported,
    };
    const reportPath = join(runDir, 'rust-coverage.json');
    writeFileSync(reportPath, JSON.stringify(report, null, 2) + '\n');
    if (process.argv.includes('--html')) {
      const htmlDir = join(runDir, 'html');
      command(cov.path, ['show', bin, `--instr-profile=${profile}`, '--format=html', `--output-dir=${htmlDir}`, '--sources', ...sources]);
      console.log(`Annotated HTML: ${join(htmlDir, 'index.html')}`);
    }
    console.log(`Rust coverage: ${reportPath}`);
    console.log(`Lines ${report.totals.lines.covered}/${report.totals.lines.count} (${report.totals.lines.percent.toFixed(2)}%); functions ${report.totals.functions.covered}/${report.totals.functions.count} (${report.totals.functions.percent.toFixed(2)}%); regions ${report.totals.regions.covered}/${report.totals.regions.count} (${report.totals.regions.percent.toFixed(2)}%).`);
    if (unreported.length) console.log(`Source files without emitted coverage mappings: ${unreported.join(', ')}`);
    console.log(`Separate TypeScript feature report: ${e2eReport}`);
    completed = true;
  } catch (error) {
    writeFileSync(join(runDir, 'failure.txt'), `${error instanceof Error ? error.message : String(error)}\n`);
    throw error;
  } finally {
    // Only delete scratch created by this invocation; retain compact reports.
    for (const path of [target, join(runDir, 'cargo-build'), rawDir, join(runDir, 'coverage.profdata'), ...(!completed ? [join(runDir, 'html')] : [])]) {
      rmSync(path, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
    }
  }
}

try { main(); } catch (error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
}
