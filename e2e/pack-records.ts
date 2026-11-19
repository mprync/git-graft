/** Inspect the physical records Git indexed in an end-to-end fixture. */
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import type { Context } from './harness.ts';

type PackRecord = { id: string; size: number; offset: number; base?: string };

export function packRecords(ctx: Context, index: string, repo = ctx.repo): PackRecord[] {
  return ctx.git(['verify-pack', '-v', index], repo).split('\n').flatMap(line => {
    const match = line.match(/^([0-9a-f]+)\s+\S+\s+\d+\s+(\d+)\s+(\d+)(?:\s+\d+\s+([0-9a-f]+))?$/);
    return match ? [{ id: match[1], size: Number(match[2]), offset: Number(match[3]), base: match[4] }] : [];
  });
}

export function assertNoRecord(workspace: string, excluded: Buffer) {
  const dir = join(workspace, '.git', 'objects', 'pack');
  for (const name of readdirSync(dir).filter(name => name.endsWith('.pack'))) {
    assert.equal(readFileSync(join(dir, name)).includes(excluded), false, 'excluded compressed record leaked into workspace pack');
  }
}
