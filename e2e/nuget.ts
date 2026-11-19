/** Exercise a registered third-party manager using an offline PackageReference project. */
import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync, renameSync, statSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { Context, objectResult } from './harness.ts';

const binary = process.env.GIT_GRAFT_BIN ?? join('target', 'release', process.platform === 'win32' ? 'git-graft.exe' : 'git-graft');
const ctx = new Context(resolve(binary));
try {
  const feed = join(ctx.root, 'feed');
  mkdirSync(feed);
  const inherited = join(ctx.root, 'unused-packages');
  Object.assign(ctx.env, {
    DOTNET_CLI_HOME: join(ctx.root, 'home'), NUGET_PACKAGES: inherited,
    NUGET_HTTP_CACHE_PATH: join(ctx.root, 'http-cache'), DOTNET_NOLOGO: '1',
    DOTNET_CLI_TELEMETRY_OPTOUT: '1', DOTNET_SKIP_FIRST_TIME_EXPERIENCE: '1',
    DOTNET_GENERATE_ASPNET_CERTIFICATE: 'false', DOTNET_CLI_WORKLOAD_UPDATE_NOTIFY_DISABLE: 'true',
    MSBUILDDISABLENODEREUSE: '1', DOTNET_CLI_DO_NOT_USE_MSBUILD_SERVER: '1',
  });
  const sdk = ctx.command('dotnet', ['--version']);
  assert.equal(sdk.status, 0, sdk.error?.message ?? sdk.stderr);
  const major = Number(sdk.stdout.trim().split('.')[0]);
  assert(Number.isInteger(major) && major >= 8, 'Install a .NET SDK version 8 or later');
  const framework = 'net' + major + '.0';
  const python = process.platform === 'win32' ? 'python' : 'python3';
  for (const version of ['1.0.0', '2.0.0']) {
    const zip = ctx.command(python, ['-I', '-B', '-c', [
      'import zipfile, sys',
      'with zipfile.ZipFile(sys.argv[1], "w") as z:',
      ' z.writestr("Graft.Fixture.nuspec", f"<package><metadata><id>Graft.Fixture</id><version>{sys.argv[2]}</version><authors>Fixture</authors><description>Local fixture</description></metadata></package>")',
      ' z.writestr(f"lib/{sys.argv[3]}/_._", b"")',
    ].join('\n'), join(feed, 'Graft.Fixture.' + version + '.nupkg'), version, framework]);
    assert.equal(zip.status, 0, zip.error?.message ?? zip.stderr);
  }
  const feedXml = feed.replaceAll('&', '&amp;').replaceAll('"', '&quot;').replaceAll('<', '&lt;');
  writeFileSync(join(ctx.repo, 'NuGet.Config'), '<configuration><packageSources><clear/><add key="local" value="' + feedXml + '"/></packageSources></configuration>');
  const project = (version: string) => '<Project Sdk="Microsoft.NET.Sdk"><PropertyGroup><TargetFramework>' + framework +
    '</TargetFramework><NuGetAudit>false</NuGetAudit></PropertyGroup><ItemGroup><PackageReference Include="Graft.Fixture" Version="[' +
    version + ']"/></ItemGroup></Project>';
  writeFileSync(join(ctx.repo, 'GraftCheck.csproj'), project('1.0.0'));
  writeFileSync(join(ctx.repo, 'Check.cs'), 'public class Check {}');
  ctx.git(['add', '.']);
  ctx.git(['commit', '-qm', 'Local NuGet project']);
  const registration = ctx.cli(['register-manager', 'dotnet', '--cache-env', 'NUGET_PACKAGES']);
  const packages = registration.cache_dir;
  assert(typeof packages === 'string');
  const first = ctx.spawn('alpha'), second = ctx.spawn('beta');
  function dotnet(ws: typeof first, ...args: string[]) {
    const result = ctx.cli(['exec', String(ws.result.name), '--lease', ws.lease, '--', 'dotnet', ...args]);
    assert.equal(result.success, true, String(result.stderr) + String(result.stdout));
  }
  function restore(ws: typeof first) {
    dotnet(ws, 'restore', 'GraftCheck.csproj', '--configfile', 'NuGet.Config', '--disable-build-servers');
  }
  function checkAssets(ws: typeof first, version: string) {
    const assets = objectResult(JSON.parse(readFileSync(join(ws.path, 'obj/project.assets.json'), 'utf8')));
    assert.deepEqual(Object.keys(objectResult(assets.libraries)), ['Graft.Fixture/' + version]);
    assert.equal(objectResult(objectResult(assets.project).restore).packagesPath, packages);
  }
  restore(first);
  const original = join(packages, 'graft.fixture/1.0.0/graft.fixture.1.0.0.nupkg');
  const identity = statSync(original, { bigint: true }).ino;
  renameSync(join(feed, 'Graft.Fixture.1.0.0.nupkg'), join(ctx.root, 'offline-package'));
  restore(second);
  checkAssets(first, '1.0.0');
  checkAssets(second, '1.0.0');
  writeFileSync(join(first.path, 'GraftCheck.csproj'), project('2.0.0'));
  restore(first);
  checkAssets(first, '2.0.0');
  checkAssets(second, '1.0.0');
  assert.equal(statSync(original, { bigint: true }).ino, identity);
  assert.equal(readFileSync(join(ctx.repo, 'GraftCheck.csproj'), 'utf8'), project('1.0.0'));
  for (const ws of [first, second]) {
    dotnet(ws, 'build', 'GraftCheck.csproj', '--no-restore', '--disable-build-servers', '-p:UseSharedCompilation=false');
    assert(existsSync(join(ws.path, 'bin', 'Debug', framework, 'GraftCheck.dll')));
  }
  ctx.cli(['unregister-manager', 'dotnet']);
  assert(existsSync(original));
  assert(!existsSync(join(inherited, 'graft.fixture')));
  assert(!existsSync(join(ctx.repo, 'obj/project.assets.json')));
  console.log(JSON.stringify({ sdk: sdk.stdout.trim(), versions: ['2.0.0', '1.0.0'],
    restoredWithFeedPackageRemoved: true, projectCacheRetained: true, privateBuilds: 2 }));
} finally {
  ctx.close();
}
