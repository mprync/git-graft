/** Exercise mixed primary packs through the public workspace CLI. */
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { objectBootstrap } from './benchmark-receipts.ts';
import { objectResult, type Context, type Scenario } from './harness.ts';
import { assertNoRecord, packRecords } from './pack-records.ts';

function fixture(ctx: Context, format: 'sha1' | 'sha256') {
  if (format === 'sha256') {
    rmSync(ctx.repo, { recursive: true, force: true });
    const init = ctx.command('git', ['init', '-q', '-b', 'main', '--object-format=sha256', ctx.repo], ctx.root);
    if (init.status !== 0 && /unknown option|unknown hash algorithm|unsupported/i.test(init.stderr)) {
      ctx.unavailable('Installed Git does not support SHA-256 repositories');
    }
    assert.equal(init.status, 0, init.stderr);
    writeFileSync(join(ctx.repo, 'tracked.txt'), 'initial\n');
    ctx.git(['add', '.']);
    ctx.git(['commit', '-qm', 'initial']);
  }
  const large = randomBytes(2 * 1024 * 1024);
  writeFileSync(join(ctx.repo, 'large.bin'), large);
  ctx.git(['add', 'large.bin']);
  ctx.git(['commit', '-qm', 'selected history']);
  const tip = ctx.git(['rev-parse', 'HEAD']);
  const blob = ctx.git(['rev-parse', 'HEAD:large.bin']);
  const selected = ctx.git(['rev-list', '--objects', '--no-object-names', tip]).split('\n').sort();
  const extra: string[] = [];
  for (let i = 0; i < 3; i++) {
    const file = join(ctx.root, `unrelated-${i}`);
    writeFileSync(file, randomBytes(256));
    const id = ctx.git(['hash-object', '-w', file]);
    ctx.git(['update-ref', `refs/scratch/unrelated-${i}`, id]);
    extra.push(id);
  }
  // The small first record absorbs an interior gap; the large following blob
  // keeps its original offset. The last unrelated record tests truncation.
  const rest = selected.filter(id => id !== blob);
  const order = [rest[0], extra[0], blob, ...rest.slice(1), extra[1], extra[2]];
  const prefix = join(ctx.repo, '.git', 'objects', 'pack', 'pack');
  const packed = spawnSync('git', ['pack-objects', '--no-reuse-delta', '--window=0', prefix], {
    cwd: ctx.repo, env: ctx.env, input: order.join('\n') + '\n', encoding: 'utf8', windowsHide: true,
  });
  assert.equal(packed.status, 0, packed.stderr);
  const index = `${prefix}-${packed.stdout.trim()}.idx`;
  const records = ctx.git(['verify-pack', '-v', index]).split('\n')
    .map(line => line.match(/^([0-9a-f]+)\s+\S+\s+\d+\s+\d+\s+(\d+)$/))
    .filter(match => match !== null).sort((a, b) => Number(a[2]) - Number(b[2])).map(match => match[1]);
  assert.deepEqual(records, order, 'fixture pack must preserve explicit object order');
  ctx.git(['prune-packed']);
  return { large, tip, selected, extra, index };
}

function run(ctx: Context, format: 'sha1' | 'sha256') {
  const { large, tip, selected, extra, index } = fixture(ctx, format);
  const donor = readFileSync(index.replace(/\.idx$/, '.pack'));
  const probe = objectResult(ctx.cli(['doctor']).source_storage).native_clone;
  assert.equal(typeof probe, 'boolean');
  const first = ctx.spawn(`mixed-${format}`);
  const objects = objectResult(objectResult(first.result.storage).objects);
  if (probe) {
    assert.ok(typeof objects.filtered_primary_packs === 'number' && objects.filtered_primary_packs > 0, JSON.stringify(objects));
    assert.ok(typeof objects.filtered_unchanged_bytes === 'number' && objects.filtered_unchanged_bytes > 1024 * 1024);
  } else {
    assert.equal(objects.filtered_primary_packs ?? 0, 0);
    assert.equal(objects.source, 'local-export');
  }
  const sibling = ctx.spawn(`mixed-${format}-sibling`);
  const excludedRecord = packRecords(ctx, index).find(record => record.id === extra[0]);
  assert(excludedRecord);
  const excludedBytes = donor.subarray(excludedRecord.offset, excludedRecord.offset + excludedRecord.size);
  assert(excludedBytes.length > 128);
  assertNoRecord(first.path, excludedBytes);
  assertNoRecord(sibling.path, excludedBytes);
  const admin = join(ctx.repo, '.git');
  const parked = join(ctx.repo, '.git-parked');
  renameSync(admin, parked);
  try {
    for (const workspace of [first.path, sibling.path]) {
      assert.equal(ctx.git(['rev-parse', 'HEAD'], workspace), tip);
      ctx.git(['fsck', '--full'], workspace);
      const actual = ctx.git(['cat-file', '--batch-all-objects', '--batch-check=%(objectname)'], workspace).split('\n').sort();
      assert.deepEqual(actual, selected, 'workspace must own exactly the selected revision closure');
      for (const id of extra) assert(!actual.includes(id));
      assert.deepEqual(readFileSync(join(workspace, 'large.bin')), large);
    }
    writeFileSync(join(first.path, 'tracked.txt'), 'first workspace edit\n');
    writeFileSync(join(sibling.path, 'tracked.txt'), 'sibling edit\n');
    writeFileSync(join(first.path, 'large.bin'), 'changed in first workspace\n');
    assert.equal(readFileSync(join(first.path, 'tracked.txt'), 'utf8'), 'first workspace edit\n');
    assert.equal(readFileSync(join(sibling.path, 'tracked.txt'), 'utf8'), 'sibling edit\n');
    assert.deepEqual(readFileSync(join(sibling.path, 'large.bin')), large);
  } finally {
    renameSync(parked, admin);
  }
  assert.equal(readFileSync(join(ctx.repo, 'tracked.txt'), 'utf8'), 'initial\n');
  assert.deepEqual(readFileSync(join(ctx.repo, 'large.bin')), large);
  assert.deepEqual(readFileSync(index.replace(/\.idx$/, '.pack')), donor, 'primary donor pack changed');
  ctx.git(['fsck', '--full']);
  ctx.measure('mixed_history', { format, native_clone: probe, objects: objectBootstrap(objects, false) });
}

export const cases: Scenario[] = (['sha1', 'sha256'] as const).map(format => ({
  name: `mixed primary pack isolation (${format})`,
  run: ctx => run(ctx, format),
}));

function makePack(ctx: Context, entries: string[], delta: boolean): string {
  const prefix = join(ctx.repo, '.git', 'objects', 'pack', 'pack');
  const args = delta ? ['--window=10', '--depth=10'] : ['--no-reuse-delta', '--window=0'];
  const result = spawnSync('git', ['pack-objects', ...args, prefix], {
    cwd: ctx.repo, env: ctx.env, input: entries.join('\n') + '\n', encoding: 'utf8', windowsHide: true,
  });
  assert.equal(result.status, 0, result.stderr);
  ctx.git(['prune-packed']);
  return `${prefix}-${result.stdout.trim()}.idx`;
}

function verifyIndependent(ctx: Context, workspace: string, selected: string[], data: Buffer) {
  const admin = join(ctx.repo, '.git');
  const parked = join(ctx.repo, '.git-parked');
  renameSync(admin, parked);
  try {
    ctx.git(['fsck', '--full'], workspace);
    assert.deepEqual(ctx.git(['cat-file', '--batch-all-objects', '--batch-check=%(objectname)'], workspace).split('\n').sort(), selected);
    assert.deepEqual(readFileSync(join(workspace, 'selected.bin')), data);
  } finally {
    renameSync(parked, admin);
  }
}

function defensiveFixture(ctx: Context, mode: 'delta' | 'compact') {
  const data = mode === 'delta' ? randomBytes(2 * 1024 * 1024) : randomBytes(8192);
  writeFileSync(join(ctx.repo, 'selected.bin'), data);
  ctx.git(['add', 'selected.bin']);
  ctx.git(['commit', '-qm', 'selected data']);
  const tip = ctx.git(['rev-parse', 'HEAD']);
  const selectedBlob = ctx.git(['rev-parse', 'HEAD:selected.bin']);
  const selected = ctx.git(['rev-list', '--objects', '--no-object-names', tip]).split('\n').sort();
  const unrelated = mode === 'delta' ? Buffer.concat([data, randomBytes(4096)]) : randomBytes(2 * 1024 * 1024);
  const input = join(ctx.root, 'unrelated-data');
  writeFileSync(input, unrelated);
  const excluded = ctx.git(['hash-object', '-w', input]);
  ctx.git(['update-ref', 'refs/scratch/unrelated', excluded]);
  const others = selected.filter(id => id !== selectedBlob);
  const entries = mode === 'delta'
    ? [`${excluded} selected.bin`, `${selectedBlob} selected.bin`, ...others]
    : [others[0], excluded, selectedBlob, ...others.slice(1)];
  const index = makePack(ctx, entries, mode === 'delta');
  const records = packRecords(ctx, index);
  const excludedRecord = records.find(record => record.id === excluded);
  const selectedRecord = records.find(record => record.id === selectedBlob);
  assert(excludedRecord && selectedRecord);
  if (mode === 'delta') {
    assert.equal(selectedRecord.base, excluded, 'selected object must be a native Git delta against excluded history');
  } else {
    assert.ok(excludedRecord.size > 10 * (records.reduce((sum, record) => sum + record.size, 0) - excludedRecord.size));
    assert.ok(excludedRecord.offset < selectedRecord.offset, 'excluded record must be an interior gap');
  }
  const packPath = index.replace(/\.idx$/, '.pack');
  const donor = readFileSync(packPath);
  const ws = ctx.spawn(`defensive-${mode}`);
  const receipt = objectResult(objectResult(ws.result.storage).objects);
  if (mode === 'compact') {
    assert.equal(receipt.filtered_primary_packs ?? 0, 0, JSON.stringify(receipt));
    assert.equal(receipt.filtered_unchanged_bytes ?? 0, 0, JSON.stringify(receipt));
    assert.equal(receipt.source, 'local-export');
  } else {
    const filtered = receipt.filtered_primary_packs ?? 0;
    const unchanged = receipt.filtered_unchanged_bytes ?? 0;
    assert.ok(typeof filtered === 'number' && Number.isSafeInteger(filtered) && filtered >= 0);
    assert.ok(typeof unchanged === 'number' && Number.isSafeInteger(unchanged) && unchanged >= 0);
    assert.equal(filtered > 0, unchanged > 0, JSON.stringify(receipt));
  }
  verifyIndependent(ctx, ws.path, selected, data);
  assert.deepEqual(readFileSync(packPath), donor);
  assert.deepEqual(readFileSync(join(ctx.repo, 'selected.bin')), data);
  ctx.git(['fsck', '--full']);
  ctx.measure('defensive_history', { mode, objects: objectBootstrap(receipt, false) });
}

cases.push(...(['delta', 'compact'] as const).map(mode => ({
  name: `mixed primary pack ${mode} fallback`,
  run: (ctx: Context) => defensiveFixture(ctx, mode),
})));
