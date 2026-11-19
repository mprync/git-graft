import assert from 'node:assert/strict';
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { objectResult, type Context } from './harness.ts';

/** Stop only journal cleanup, after native Git has committed the destination ref. */
export function leaveIndexedCheckoutJournal(ctx: Context, args: string[]) {
  const admin = ctx.git(['rev-parse', '--absolute-git-dir']);
  const path = join(admin, 'graft', 'checkout-update');
  const parked = join(ctx.root, 'interrupted checkout journal');
  const hook = join(admin, 'hooks', 'reference-transaction');
  mkdirSync(dirname(hook), { recursive: true });
  writeFileSync(hook, [
    '#!/bin/sh',
    '[ "$1" = committed ] || exit 0',
    '[ -f "$GRAFT_E2E_JOURNAL_PATH/record.json" ] || exit 0',
    `exec "$GRAFT_E2E_JOURNAL_NODE" -e 'require("node:fs").renameSync(process.argv[1], process.argv[2])' "$GRAFT_E2E_JOURNAL_PATH" "$GRAFT_E2E_JOURNAL_PARKED"`,
  ].join('\n') + '\n', { mode: 0o700, flag: 'wx' });
  const variables = {
    GRAFT_E2E_JOURNAL_NODE: process.execPath.replaceAll('\\', '/'),
    GRAFT_E2E_JOURNAL_PATH: path.replaceAll('\\', '/'),
    GRAFT_E2E_JOURNAL_PARKED: parked.replaceAll('\\', '/'),
  };
  const prior = Object.fromEntries(Object.keys(variables).map(key => [key, ctx.env[key]]));
  Object.assign(ctx.env, variables);
  try {
    ctx.fail(args);
  } finally {
    for (const [key, value] of Object.entries(prior)) {
      if (value === undefined) delete ctx.env[key];
      else ctx.env[key] = value;
    }
    rmSync(hook);
  }

  assert(existsSync(parked), 'The committed reference hook did not preserve the live journal');
  assert.equal(existsSync(path), false);
  assert.equal(existsSync(join(admin, 'index.lock')), false);
  // The journal binds its directory identity. Preserve that directory and its exact files.
  renameSync(parked, path);
  const record = objectResult(JSON.parse(readFileSync(join(path, 'record.json'), 'utf8')));
  assert.equal(record.phase, 'Indexed');
  assert.deepEqual(readFileSync(join(path, 'index.next')), readFileSync(join(admin, 'index')));
  return { path, plan: objectResult(record.plan) };
}
