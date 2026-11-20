// Renders the package-manager manifests in packaging/ for one release.
//
//   node scripts/render-packages.ts --version 0.1.0 --sums dist/SHA256SUMS --out dist/packages
//
// Inputs are the release version and the release SHA256SUMS file. Output is one
// directory per package manager with every placeholder filled in. The release workflow
// copies those directories into the tap, bucket, Chocolatey, winget and AUR repositories.
// Node 24 runs this file directly (type stripping), so it uses no enums or parameter
// properties.

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';

/** GitHub repository (owner/name). Change it here, or pass --repo. */
const DEFAULT_REPO = 'mprync/git-graft';

/** The winget package identifier; winget requires Publisher.Package form. */
const WINGET_PACKAGE_ID = 'mprync.git-graft';

/** Rust target triple to archive extension for every release archive. */
const ARCHIVES: Readonly<Record<string, 'tar.gz' | 'zip'>> = {
  'x86_64-unknown-linux-musl': 'tar.gz',
  'aarch64-unknown-linux-musl': 'tar.gz',
  'x86_64-apple-darwin': 'tar.gz',
  'aarch64-apple-darwin': 'tar.gz',
  'x86_64-pc-windows-msvc': 'zip',
  'aarch64-pc-windows-msvc': 'zip',
};

/** Template path (under packaging/) to output path (under the output directory). */
function outputs(): ReadonlyArray<readonly [string, string]> {
  return [
    ['homebrew/git-graft.rb.tmpl', 'homebrew/git-graft.rb'],
    ['scoop/git-graft.json.tmpl', 'scoop/git-graft.json'],
    ['chocolatey/git-graft.nuspec.tmpl', 'chocolatey/git-graft.nuspec'],
    ['chocolatey/tools/chocolateyinstall.ps1.tmpl', 'chocolatey/tools/chocolateyinstall.ps1'],
    ['winget/version.yaml.tmpl', `winget/${WINGET_PACKAGE_ID}.yaml`],
    ['winget/installer.yaml.tmpl', `winget/${WINGET_PACKAGE_ID}.installer.yaml`],
    ['winget/locale.en-US.yaml.tmpl', `winget/${WINGET_PACKAGE_ID}.locale.en-US.yaml`],
    ['aur/PKGBUILD.tmpl', 'aur/PKGBUILD'],
    ['aur/SRCINFO.tmpl', 'aur/.SRCINFO'],
  ];
}

const SEMVER = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;
const REPO_PATTERN = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/;

/** Maps archive file name to its lowercase SHA-256 from a SHA256SUMS document. */
function parseSums(text: string): Map<string, string> {
  const sums = new Map<string, string>();
  for (const line of text.split(/\r?\n/)) {
    const match = /^([0-9A-Fa-f]{64})\s+\*?(\S+)$/.exec(line.trim());
    if (match?.[1] && match[2]) sums.set(match[2], match[1].toLowerCase());
  }
  return sums;
}

/** SHA-256 of each release archive, keyed by target triple. Throws on a missing archive. */
function archiveHashes(version: string, sums: Map<string, string>): Map<string, string> {
  const hashes = new Map<string, string>();
  const missing: string[] = [];
  for (const [target, extension] of Object.entries(ARCHIVES)) {
    const name = `git-graft-${version}-${target}.${extension}`;
    const hash = sums.get(name);
    if (hash === undefined) missing.push(name);
    else hashes.set(target, hash);
  }
  if (missing.length > 0) {
    throw new Error(`SHA256SUMS is missing: ${missing.join(', ')}`);
  }
  return hashes;
}

interface Context {
  readonly values: ReadonlyMap<string, string>;
  readonly hashes: ReadonlyMap<string, string>;
}

/** Replaces every {{NAME}} and {{NAME:target}} placeholder; unknown names are errors. */
function render(template: string, context: Context, source: string): string {
  return template.replace(/\{\{([A-Z0-9_]+)(?::([a-z0-9_-]+))?\}\}/g, (_whole, name: string, arg?: string) => {
    if (name === 'SHA256' || name === 'SHA256_UPPER') {
      const hash = arg === undefined ? undefined : context.hashes.get(arg);
      if (hash === undefined) throw new Error(`${source}: no checksum for {{${name}:${arg ?? ''}}}`);
      return name === 'SHA256' ? hash : hash.toUpperCase();
    }
    const value = context.values.get(name);
    if (value === undefined || arg !== undefined) throw new Error(`${source}: unknown placeholder {{${name}}}`);
    return value;
  });
}

function main(): void {
  const { values: options } = parseArgs({
    options: {
      version: { type: 'string' },
      sums: { type: 'string' },
      out: { type: 'string', default: 'dist/packages' },
      repo: { type: 'string', default: DEFAULT_REPO },
      date: { type: 'string' },
    },
    strict: true,
  });

  const rawVersion = options.version;
  const sumsPath = options.sums;
  if (rawVersion === undefined || sumsPath === undefined) {
    throw new Error('usage: render-packages.ts --version <x.y.z> --sums <SHA256SUMS> [--out <dir>] [--repo <owner/name>] [--date <YYYY-MM-DD>]');
  }
  const version = rawVersion.replace(/^v/, '');
  if (!SEMVER.test(version)) throw new Error(`invalid version: ${rawVersion}`);
  const repo = options.repo ?? DEFAULT_REPO;
  if (!REPO_PATTERN.test(repo)) throw new Error(`invalid repository: ${repo}`);
  const owner = repo.split('/')[0] ?? repo;
  const date = options.date ?? new Date().toISOString().slice(0, 10);
  if (!/^\d{4}-\d{2}-\d{2}$/.test(date)) throw new Error(`invalid date: ${date}`);

  const hashes = archiveHashes(version, parseSums(readFileSync(sumsPath, 'utf8')));
  const homepage = `https://github.com/${repo}`;
  const context: Context = {
    hashes,
    values: new Map([
      ['VERSION', version],
      ['REPO', repo],
      ['OWNER', owner],
      ['HOMEPAGE', homepage],
      ['RELEASE_URL', `${homepage}/releases/download/v${version}`],
      ['RELEASE_DATE', date],
      ['PACKAGE_ID', WINGET_PACKAGE_ID],
    ]),
  };

  const packagingDir = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'packaging');
  const outDir = resolve(options.out ?? 'dist/packages');
  for (const [templatePath, outputPath] of outputs()) {
    const rendered = render(readFileSync(join(packagingDir, templatePath), 'utf8'), context, templatePath);
    const destination = join(outDir, outputPath);
    mkdirSync(dirname(destination), { recursive: true });
    writeFileSync(destination, rendered);
    console.log(`rendered ${outputPath}`);
  }
}

try {
  main();
} catch (error) {
  console.error(`render-packages: ${error instanceof Error ? error.message : String(error)}`);
  process.exitCode = 1;
}
