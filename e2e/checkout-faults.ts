/** Interrupt the primary checkout update during merge and prove a retry finishes without losing anything. */
import assert from 'node:assert/strict';
import { spawn as spawnProcess, spawnSync } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { objectResult, type Context, type ObjectResult, type Scenario } from './harness.ts';

type Workspace = ReturnType<Context['spawn']>;

/** How the global smudge filter behaves at its chosen call. */
export type FaultMode = 'fail' | 'block';

/**
 * A global smudge filter for `*.fault` files in the primary. It counts its
 * calls; at call `at` it either fails (a required filter, so Git stops the
 * update) or blocks until released, so the test can kill processes at a
 * known point in the middle of `read-tree -m -u`.
 */
export class CheckoutFault {
  readonly dir: string;
  private readonly ctx: Context;

  constructor(ctx: Context) {
    this.ctx = ctx;
    this.dir = join(ctx.root, 'checkout fault');
    mkdirSync(this.dir);
    const script = join(this.dir, 'smudge.sh');
    const dir = this.dir.replaceAll('\\', '/');
    writeFileSync(script, [
      '#!/bin/sh',
      `dir='${dir}'`,
      'n=$(( $(cat "$dir/count" 2>/dev/null || echo 0) + 1 ))',
      'echo "$n" > "$dir/count"',
      'if [ "$n" = "$(cat "$dir/at" 2>/dev/null)" ]; then',
      '  case "$(cat "$dir/mode")" in',
      '    fail) cat >/dev/null; exit 1 ;;',
      '    block) : > "$dir/ready"; while [ ! -f "$dir/release" ]; do sleep 0.1; done ;;',
      '  esac',
      'fi',
      'exec cat',
    ].join('\n') + '\n', { mode: 0o755 });
    const config = ctx.env.GIT_CONFIG_GLOBAL;
    assert(typeof config === 'string');
    appendFileSync(config, `[filter "fault"]\n\tsmudge = sh '${script.replaceAll('\\', '/')}'\n\tclean = cat\n\trequired = true\n`);
    const attributes = join(ctx.git(['rev-parse', '--absolute-git-dir']), 'info', 'attributes');
    mkdirSync(dirname(attributes), { recursive: true });
    appendFileSync(attributes, '*.fault filter=fault\n');
    this.disarm();
  }

  arm(mode: FaultMode, at: number) {
    for (const name of ['ready', 'release']) this.remove(name);
    writeFileSync(join(this.dir, 'count'), '0\n');
    writeFileSync(join(this.dir, 'mode'), mode + '\n');
    writeFileSync(join(this.dir, 'at'), `${at}\n`);
  }
  disarm() { writeFileSync(join(this.dir, 'at'), '0\n'); }
  ready() { return existsSync(join(this.dir, 'ready')); }
  release() { writeFileSync(join(this.dir, 'release'), 'release\n'); }
  private remove(name: string) { rmSync(join(this.dir, name), { force: true }); }
}

export async function waitFor(condition: () => boolean, label: string, timeout = 30_000) {
  const deadline = Date.now() + timeout;
  while (!condition()) {
    assert(Date.now() < deadline, `timed out waiting for ${label}`);
    await delay(50);
  }
}

/** A running CLI command whose process, or whole process tree, a test can kill. */
export class Running {
  readonly child: ReturnType<typeof spawnProcess>;
  readonly exited: Promise<{ code: number | null; signal: NodeJS.Signals | null }>;

  constructor(ctx: Context, args: string[]) {
    this.child = spawnProcess(ctx.bin, ['--repo', ctx.repo, '--json', ...args], {
      cwd: ctx.repo, env: ctx.env, stdio: 'ignore', windowsHide: true,
      // A separate process group lets a test kill Git and its filters too.
      detached: process.platform !== 'win32',
    });
    this.exited = new Promise((resolve, reject) => {
      this.child.once('error', reject);
      this.child.once('exit', (code, signal) => resolve({ code, signal }));
    });
  }
  running() { return this.child.exitCode === null && this.child.signalCode === null; }
  /** Kill only the Graft process. Git keeps running as an orphan. */
  killCli() { assert(this.child.kill('SIGKILL'), 'could not kill the merge'); }
  /** Kill Graft and the native file update it recorded in its journal. */
  killWithWorker(ctx: Context) {
    const record = objectResult(JSON.parse(readFileSync(join(journal(ctx), 'record.json'), 'utf8')));
    assert(typeof record.worker === 'number', 'the journal did not record the Git update');
    this.killCli();
    process.kill(record.worker, 'SIGKILL');
  }
  /** Kill Graft, Git and the filter at once, as a crash or a power loss would. */
  killTree() {
    const pid = this.child.pid;
    assert(pid, 'the merge has no process ID');
    if (process.platform === 'win32') {
      const out = spawnSync('taskkill', ['/F', '/T', '/PID', String(pid)], { windowsHide: true, encoding: 'utf8' });
      assert.equal(out.status, 0, out.stderr);
    } else {
      process.kill(-pid, 'SIGKILL');
    }
  }
  async exit() {
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      return await Promise.race([this.exited, new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error('the merge did not exit')), 20_000);
      })]);
    } finally {
      if (timer) clearTimeout(timer);
    }
  }
}

function write(root: string, path: string, content: string) {
  const file = join(root, path);
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, content);
}
function read(root: string, path: string) { return readFileSync(join(root, path), 'utf8'); }
function commit(ctx: Context, root: string, message: string) {
  ctx.git(['add', '-A'], root);
  ctx.git(['commit', '-qm', message], root);
  return ctx.git(['rev-parse', 'HEAD'], root);
}
function admin(ctx: Context) { return ctx.git(['rev-parse', '--absolute-git-dir']); }
function journal(ctx: Context) { return join(admin(ctx), 'graft', 'checkout-update'); }
function details(error: ObjectResult) { return objectResult(error.details); }

/**
 * Commit files the graft will change, start a graft and give it a change
 * that modifies, adds and deletes files. The smudge filter runs for the
 * `*.fault` files in index order: 0-added, a, b, z-added. Failing call 2
 * leaves the deletion and 0-added.fault written and a.fault, b.fault and
 * z-added.fault at their previous state.
 */
export function prepareFaultedMerge(ctx: Context, name: string) {
  write(ctx.repo, 'a.fault', 'a before\n');
  write(ctx.repo, 'b.fault', 'b before\n');
  write(ctx.repo, 'gone.txt', 'deleted by the merge\n');
  write(ctx.repo, 'unrelated.txt', 'unrelated before\n');
  const old = commit(ctx, ctx.repo, 'files the merge changes');
  write(ctx.repo, 'ignored/keep.txt', 'ignored primary data\n');
  const workspace = ctx.spawn(name);
  write(workspace.path, '0-added.fault', 'added first\n');
  write(workspace.path, 'a.fault', 'a merged\n');
  write(workspace.path, 'b.fault', 'b merged\n');
  write(workspace.path, 'z-added.fault', 'added last\n');
  ctx.git(['rm', '-q', 'gone.txt'], workspace.path);
  const tip = commit(ctx, workspace.path, 'graft change');
  return { workspace, old, tip };
}

export function assertMerged(ctx: Context, tip: string) {
  assert.equal(ctx.git(['rev-parse', 'HEAD']), tip);
  assert.equal(read(ctx.repo, '0-added.fault'), 'added first\n');
  assert.equal(read(ctx.repo, 'a.fault'), 'a merged\n');
  assert.equal(read(ctx.repo, 'b.fault'), 'b merged\n');
  assert.equal(read(ctx.repo, 'z-added.fault'), 'added last\n');
  assert.equal(existsSync(join(ctx.repo, 'gone.txt')), false);
  assert.equal(read(ctx.repo, 'ignored/keep.txt'), 'ignored primary data\n');
  assert.equal(existsSync(journal(ctx)), false);
  assert.equal(existsSync(join(admin(ctx), 'index.lock')), false);
}

/** The interrupted state: nothing committed, the original index intact. */
function assertInterrupted(ctx: Context, old: string, error: ObjectResult, index: Buffer) {
  assert.equal(error.code, 'checkout_recovery_required', String(error.message));
  assert.doesNotMatch(String(error.message), /git clean/);
  const recovery = String(details(error).recovery_path);
  assert.equal(recovery, journal(ctx));
  assert.deepEqual(readFileSync(join(recovery, 'index.before')), index);
  assert.deepEqual(readFileSync(join(admin(ctx), 'index')), index);
  assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
  assert.equal(ctx.git(['rev-parse', 'refs/heads/main']), old);
  assert.equal(read(ctx.repo, 'ignored/keep.txt'), 'ignored primary data\n');
  assertPartial(ctx);
}

/** Git stopped at a.fault: it had removed the deletion, written
 * 0-added.fault and unlinked a.fault to replace it. */
function assertPartial(ctx: Context) {
  assert.equal(read(ctx.repo, '0-added.fault'), 'added first\n');
  assert.equal(existsSync(join(ctx.repo, 'a.fault')), false);
  assert.equal(read(ctx.repo, 'b.fault'), 'b before\n');
  assert.equal(existsSync(join(ctx.repo, 'z-added.fault')), false);
  assert.equal(existsSync(join(ctx.repo, 'gone.txt')), false);
}

type PrimaryState = {
  name: string;
  /** Change the primary after the interruption, as a person might. */
  apply(ctx: Context): void;
  /** Prove the change is still there after the retry. */
  check(ctx: Context): void;
};

const states: PrimaryState[] = [
  { name: 'a clean primary', apply() {}, check(ctx) {
    assert.equal(ctx.git(['status', '--porcelain']), '');
  } },
  { name: 'staged changes', apply(ctx) {
    write(ctx.repo, 'staged.txt', 'staged after the interruption\n');
    write(ctx.repo, 'unrelated.txt', 'staged unrelated edit\n');
    ctx.git(['add', 'staged.txt', 'unrelated.txt']);
  }, check(ctx) {
    assert.equal(ctx.git(['diff', '--cached', '--name-only']), 'staged.txt\nunrelated.txt');
    assert.equal(ctx.git(['show', ':unrelated.txt']), 'staged unrelated edit');
    assert.equal(ctx.git(['diff', '--name-only']), '');
  } },
  { name: 'unstaged edits to unrelated files', apply(ctx) {
    write(ctx.repo, 'unrelated.txt', 'unstaged edit\n');
  }, check(ctx) {
    assert.equal(read(ctx.repo, 'unrelated.txt'), 'unstaged edit\n');
    assert.equal(ctx.git(['diff', '--name-only']), 'unrelated.txt');
    assert.equal(ctx.git(['diff', '--cached', '--name-only']), '');
  } },
  { name: 'untracked and ignored files', apply(ctx) {
    write(ctx.repo, 'untracked.txt', 'untracked after the interruption\n');
    write(ctx.repo, 'ignored/later.txt', 'ignored after the interruption\n');
  }, check(ctx) {
    assert.equal(read(ctx.repo, 'untracked.txt'), 'untracked after the interruption\n');
    assert.equal(read(ctx.repo, 'ignored/later.txt'), 'ignored after the interruption\n');
    assert.equal(ctx.git(['status', '--porcelain']), '?? untracked.txt');
  } },
];

function retryFinishes(ctx: Context, workspace: Workspace, name: string, tip: string) {
  const receipt = ctx.cli(['merge', name, '--lease', workspace.lease]);
  assert.equal(receipt.action, 'merge');
  assert.equal(receipt.removed, true);
  assert.equal(receipt.commit, tip);
  assertMerged(ctx, tip);
}

export const cases: Scenario[] = [
  ...states.map((state): Scenario => ({
    name: `a failed checkout filter stops the merge and a retry finishes with ${state.name}`,
    run(ctx) {
      const name = 'filtered';
      const { workspace, old, tip } = prepareFaultedMerge(ctx, name);
      const fault = new CheckoutFault(ctx);
      const index = readFileSync(join(admin(ctx), 'index'));
      fault.arm('fail', 2);
      const error = ctx.fail(['merge', name, '--lease', workspace.lease]);
      assertInterrupted(ctx, old, error, index);
      assert.equal(ctx.cli(['status', name]).phase, 'landing');

      // The journal stays until it can finish; a retry that fails again
      // changes nothing.
      fault.arm('fail', 1);
      assertInterrupted(ctx, old, ctx.fail(['merge', name, '--lease', workspace.lease]), index);

      state.apply(ctx);
      fault.disarm();
      retryFinishes(ctx, workspace, name, tip);
      state.check(ctx);
    },
  })),
  {
    name: 'a merge killed with its Git file update resumes from the files on disk',
    async run(ctx) {
      const name = 'killed';
      const { workspace, old, tip } = prepareFaultedMerge(ctx, name);
      const fault = new CheckoutFault(ctx);
      const index = readFileSync(join(admin(ctx), 'index'));
      fault.arm('block', 2);
      const merge = new Running(ctx, ['merge', name, '--lease', workspace.lease]);
      try {
        await waitFor(() => fault.ready() || !merge.running(), 'the checkout filter');
        assert(fault.ready(), 'the merge exited before the checkout update');
        merge.killWithWorker(ctx);
        await merge.exit();
      } finally {
        if (merge.running()) merge.killCli();
        fault.release();
      }
      // Graft's own index lock and Git's private index lock are left behind.
      assert(existsSync(join(admin(ctx), 'index.lock')));
      assert(existsSync(join(journal(ctx), 'index.next.lock')));
      assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
      assert.deepEqual(readFileSync(join(admin(ctx), 'index')), index);
      assert.equal(read(ctx.repo, 'ignored/keep.txt'), 'ignored primary data\n');
      assertPartial(ctx);
      write(ctx.repo, 'untracked.txt', 'untracked after the crash\n');
      write(ctx.repo, 'unrelated.txt', 'unstaged edit after the crash\n');

      fault.disarm();
      retryFinishes(ctx, workspace, name, tip);
      assert.equal(read(ctx.repo, 'untracked.txt'), 'untracked after the crash\n');
      assert.equal(read(ctx.repo, 'unrelated.txt'), 'unstaged edit after the crash\n');
    },
  },
  {
    name: 'a crash of the whole merge reports the ref locks Git left and finishes once they are gone',
    async run(ctx) {
      const name = 'crashed';
      const { workspace, old, tip } = prepareFaultedMerge(ctx, name);
      const fault = new CheckoutFault(ctx);
      fault.arm('block', 2);
      const merge = new Running(ctx, ['merge', name, '--lease', workspace.lease]);
      try {
        await waitFor(() => fault.ready() || !merge.running(), 'the checkout filter');
        assert(fault.ready(), 'the merge exited before the checkout update');
        merge.killTree();
        await merge.exit();
      } finally {
        if (merge.running()) merge.killCli();
        fault.release();
      }
      // Killing Git's ref transaction leaves its own lock files. Graft cannot
      // prove which process owns them, so it reports them and changes nothing.
      const head = join(admin(ctx), 'HEAD.lock');
      const branch = join(admin(ctx), 'refs', 'heads', 'main.lock');
      assert(existsSync(head) && existsSync(branch));
      const error = ctx.fail(['merge', name, '--lease', workspace.lease]);
      assert.equal(error.code, 'checkout_recovery_required');
      assert.match(String(error.message), /HEAD\.lock/);
      assertPartial(ctx);
      assert.equal(ctx.git(['rev-parse', 'refs/heads/main']), old);

      // As Git advises once no Git command is running, remove its locks.
      rmSync(head);
      rmSync(branch);
      fault.disarm();
      retryFinishes(ctx, workspace, name, tip);
    },
  },
  {
    name: 'a retry waits for an orphaned Git checkout and then finishes the merge',
    async run(ctx) {
      const name = 'orphaned';
      const { workspace, old, tip } = prepareFaultedMerge(ctx, name);
      const fault = new CheckoutFault(ctx);
      fault.arm('block', 2);
      const merge = new Running(ctx, ['merge', name, '--lease', workspace.lease]);
      const lock = join(journal(ctx), 'index.next.lock');
      try {
        await waitFor(() => fault.ready() || !merge.running(), 'the checkout filter');
        assert(fault.ready(), 'the merge exited before the checkout update');
        merge.killCli();
        await merge.exit();
        assert(existsSync(lock));
        if (process.platform !== 'win32') {
          // Git is still writing files, so the retry must not touch them.
          const error = ctx.fail(['merge', name, '--lease', workspace.lease]);
          assert.equal(error.code, 'checkout_recovery_required');
          assert.match(String(error.message), /may still be writing/);
          assert(existsSync(lock));
          assert.equal(ctx.git(['rev-parse', 'HEAD']), old);
        }
      } finally {
        fault.release();
      }
      await waitFor(() => !existsSync(lock), 'the orphaned Git checkout to finish');
      fault.disarm();
      retryFinishes(ctx, workspace, name, tip);
    },
  },
  {
    name: 'a merge from another graft reverses a stopped checkout update and both merges land',
    run(ctx) {
      const first = 'stopped';
      const { workspace, old, tip } = prepareFaultedMerge(ctx, first);
      const second = ctx.spawn('second');
      write(second.path, 'second.txt', 'second graft\n');
      const secondTip = commit(ctx, second.path, 'second graft change');
      const fault = new CheckoutFault(ctx);
      const index = readFileSync(join(admin(ctx), 'index'));
      fault.arm('fail', 2);
      assertInterrupted(ctx, old, ctx.fail(['merge', first, '--lease', workspace.lease]), index);
      write(ctx.repo, 'ignored/later.txt', 'ignored after the interruption\n');
      fault.disarm();

      // The other graft returns the checkout to its previous commit first.
      const landed = ctx.cli(['merge', 'second', '--lease', second.lease]);
      assert.equal(landed.removed, true);
      assert.equal(ctx.git(['rev-parse', 'HEAD']), secondTip);
      assert.equal(read(ctx.repo, 'a.fault'), 'a before\n');
      assert.equal(read(ctx.repo, 'gone.txt'), 'deleted by the merge\n');
      assert.equal(existsSync(join(ctx.repo, '0-added.fault')), false);
      assert.equal(read(ctx.repo, 'second.txt'), 'second graft\n');
      assert.equal(read(ctx.repo, 'ignored/later.txt'), 'ignored after the interruption\n');
      assert.equal(existsSync(journal(ctx)), false);
      assert.equal(read(workspace.path, 'a.fault'), 'a merged\n');

      // The stopped graft kept its work. Its plan is stale, so the first
      // retry replans and the next one merges both histories.
      const stale = ctx.fail(['merge', first, '--lease', workspace.lease]);
      assert.match(String(stale.message), /changed after merge was planned/);
      const merged = ctx.cli(['merge', first, '--lease', workspace.lease]);
      assert.equal(merged.removed, true);
      assert.deepEqual(ctx.git(['show', '-s', '--format=%P', 'HEAD']).split(' '), [secondTip, tip]);
      assert.equal(read(ctx.repo, 'a.fault'), 'a merged\n');
      assert.equal(read(ctx.repo, 'second.txt'), 'second graft\n');
      assert.equal(existsSync(join(ctx.repo, 'gone.txt')), false);
      assert.equal(ctx.git(['status', '--porcelain']), '');
    },
  },
  {
    name: 'a checkout update whose branch moved is set aside and the merge replans',
    run(ctx) {
      const name = 'moved';
      const { workspace, old, tip } = prepareFaultedMerge(ctx, name);
      const fault = new CheckoutFault(ctx);
      const index = readFileSync(join(admin(ctx), 'index'));
      fault.arm('fail', 2);
      assertInterrupted(ctx, old, ctx.fail(['merge', name, '--lease', workspace.lease]), index);
      fault.disarm();
      // The person takes over: they finish by hand what Git had written.
      ctx.git(['checkout', '-q', tip, '--', '.']);
      const moved = commit(ctx, ctx.repo, 'finished by hand');
      const error = ctx.fail(['merge', name, '--lease', workspace.lease]);
      assert.equal(error.code, 'checkout_update_set_aside');
      const retained = String(details(error).recovery_path);
      assert(existsSync(join(retained, 'index.before')));
      assert.equal(ctx.git(['rev-parse', 'HEAD']), moved);
      assert.equal(ctx.git(['status', '--porcelain']), '');
      // The first retry set the update aside and dropped the stale plan.
      const receipt = ctx.cli(['merge', name, '--lease', workspace.lease]);
      assert.equal(receipt.removed, true);
      assert.deepEqual(ctx.git(['show', '-s', '--format=%P', 'HEAD']).split(' '), [moved, tip]);
      assert.equal(read(ctx.repo, 'a.fault'), 'a merged\n');
      assert.equal(ctx.git(['status', '--porcelain']), '');
    },
  },
];
