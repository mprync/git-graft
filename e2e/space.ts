import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { mkdirSync, readFileSync, statSync, statfsSync, writeFileSync } from "node:fs";
import { isAbsolute, join, relative, resolve, sep } from "node:path";
import type { Scenario } from "./harness.ts";
import { dependencySpace } from "./dependency-space.ts";

type Context = Parameters<Scenario["run"]>[0];
type DiskSample = {
  blockSize: bigint;
  freeBytes: bigint;
  totalBytes: bigint;
  typeCode: string;
};
type WorkspaceSample = {
  name: string;
  phase: "cold" | "warm";
  wallSeconds: number;
  freeBytesBefore: string;
  freeBytesAfter: string;
  freeBytesDelta: string;
  logicalCheckoutBytes: string;
  logicalClonedBytes: string | null;
  parentReusedBytes: string;
  backend: string;
  snapshotReused: boolean;
};

function positiveEnv(key: string, fallback: number): number {
  const raw = process.env[key];
  if (raw === undefined) return fallback;
  const value = Number(raw);
  assert(Number.isSafeInteger(value) && value > 0, `${key} must be a positive integer`);
  return value;
}

function disk(ctx: Context): DiskSample {
  try {
    const stat = statfsSync(ctx.root, { bigint: true });
    return {
      blockSize: stat.bsize,
      freeBytes: stat.bfree * stat.bsize,
      totalBytes: stat.blocks * stat.bsize,
      typeCode: stat.type.toString(),
    };
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    ctx.unavailable(`node:fs.statfsSync is required for space measurements: ${detail}`);
  }
}

function receiptBytes(value: unknown, field: string): bigint {
  assert(typeof value === "number" && Number.isSafeInteger(value) && value >= 0,
    `storage.${field} must be a nonnegative safe integer`);
  return BigInt(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function validateCheckout(ctx: Context, path: string, files: string[]): void {
  for (const file of files) {
    assert(readFileSync(join(path, file)).equals(readFileSync(join(ctx.repo, file))),
      `${path} has different bytes for ${file}`);
  }
  assert.equal(ctx.git(["status", "--porcelain"], path), "", `${path} is not clean`);
}

function storageSample(
  name: string,
  phase: "cold" | "warm",
  wallSeconds: number,
  before: DiskSample,
  after: DiskSample,
  checkoutBytes: bigint,
  receipt: Record<string, unknown>,
): WorkspaceSample {
  const cloned = receiptBytes(receipt.cloned_bytes, "cloned_bytes");
  const parentReused = receiptBytes(receipt.parent_reused_bytes, "parent_reused_bytes");
  assert(typeof receipt.backend === "string", "storage.backend must be a string");
  assert.equal(before.typeCode, after.typeCode, "filesystem type changed during sample");
  return {
    name,
    phase,
    wallSeconds,
    freeBytesBefore: before.freeBytes.toString(),
    freeBytesAfter: after.freeBytes.toString(),
    freeBytesDelta: (before.freeBytes - after.freeBytes).toString(),
    logicalCheckoutBytes: checkoutBytes.toString(),
    logicalClonedBytes: cloned.toString(),
    parentReusedBytes: parentReused.toString(),
    backend: receipt.backend,
    snapshotReused: receipt.snapshot_reused === true,
  };
}

function sumSeconds(samples: WorkspaceSample[]): number {
  return samples.reduce((total, sample) => total + sample.wallSeconds, 0);
}

function checkWriteIsolation(ctx: Context, graft: string[], native: string[]): Record<string, boolean> {
  const relative = join("fixture", "part-0000.bin");
  const original = readFileSync(join(ctx.repo, relative));
  const changed = Buffer.from(original);
  changed.writeUInt8(changed.readUInt8(0) ^ 0xff, 0);
  writeFileSync(join(graft[0], relative), changed);
  const checks = {
    changedWorkspaceSeesWrite: readFileSync(join(graft[0], relative)).equals(changed),
    mainUnchanged: readFileSync(join(ctx.repo, relative)).equals(original),
    otherGraftWorkspacesUnchanged: graft.slice(1).every((path) =>
      readFileSync(join(path, relative)).equals(original)),
    nativeWorkspacesUnchanged: native.every((path) =>
      readFileSync(join(path, relative)).equals(original)),
    gitSeesTrackedEdit: ctx.git(["status", "--porcelain"], graft[0]).includes("part-0000.bin"),
  };
  return checks;
}

export const cases: Scenario[] = [{
  name: "native and graft disk space comparison",
  async run(ctx) {
    const fixtureMiB = positiveEnv("GIT_GRAFT_FIXTURE_MIB", 64);
    const workspaceCount = positiveEnv("GIT_GRAFT_WORKSPACES", 5);
    assert(workspaceCount >= 2, "At least two workspaces are required to measure reuse and test write isolation");
    const disabledHooks = join(ctx.root, "empty-hooks");
    mkdirSync(disabledHooks);
    ctx.git(["config", "user.name", "git-graft space E2E"]);
    ctx.git(["config", "user.email", "space-e2e@example.invalid"]);
    ctx.git(["config", "commit.gpgSign", "false"]);
    ctx.git(["config", "tag.gpgSign", "false"]);
    ctx.git(["config", "core.hooksPath", disabledHooks]);

    const fixtureDir = join(ctx.repo, "fixture");
    mkdirSync(fixtureDir);
    for (let index = 0; index < fixtureMiB; index += 1) {
      writeFileSync(join(fixtureDir, `part-${String(index).padStart(4, "0")}.bin`),
        randomBytes(1024 * 1024));
    }
    ctx.git(["add", "--all"]);
    ctx.git(["commit", "--no-gpg-sign", "-m", "space benchmark fixture"]);
    const files = ctx.git(["ls-files", "-z"]).split("\0").filter(Boolean);
    const checkoutBytes = files.reduce((total, file) =>
      total + BigInt(statSync(join(ctx.repo, file)).size), 0n);

    const nativePaths: string[] = [];
    const nativeSamples: WorkspaceSample[] = [];
    const nativeStart = disk(ctx);
    for (let index = 0; index < workspaceCount; index += 1) {
      const name = `native-${String(index + 1).padStart(3, "0")}`;
      const path = join(ctx.root, name);
      const before = disk(ctx);
      const started = performance.now();
      ctx.git(["worktree", "add", "--detach", path, "HEAD"]);
      const wallSeconds = (performance.now() - started) / 1000;
      const after = disk(ctx);
      nativePaths.push(path);
      validateCheckout(ctx, path, files);
      nativeSamples.push({
        name,
        phase: index === 0 ? "cold" : "warm",
        wallSeconds,
        freeBytesBefore: before.freeBytes.toString(),
        freeBytesAfter: after.freeBytes.toString(),
        freeBytesDelta: (before.freeBytes - after.freeBytes).toString(),
        logicalCheckoutBytes: checkoutBytes.toString(),
        logicalClonedBytes: null,
        parentReusedBytes: "0",
        backend: "git-worktree-checkout",
        snapshotReused: false,
      });
    }
    const nativeEnd = disk(ctx);

    const graftPaths: string[] = [];
    const graftSamples: WorkspaceSample[] = [];
    const graftStart = disk(ctx);
    for (let index = 0; index < workspaceCount; index += 1) {
      const name = `space-${String(index + 1).padStart(3, "0")}`;
      const before = disk(ctx);
      const started = performance.now();
      const spawned = ctx.spawn(name);
      const wallSeconds = (performance.now() - started) / 1000;
      const after = disk(ctx);
      const path = resolve(spawned.path);
      const fromRoot = relative(resolve(ctx.root), path);
      assert(fromRoot !== "" && fromRoot !== ".." && !fromRoot.startsWith(`..${sep}`)
        && !isAbsolute(fromRoot), "graft workspace must remain under the owned temporary parent");
      const storage = spawned.result.storage;
      assert(isRecord(storage),
        "spawn result must contain a storage receipt");
      const sample = storageSample(name, index === 0 ? "cold" : "warm", wallSeconds,
        before, after, checkoutBytes, storage);
      graftPaths.push(path);
      validateCheckout(ctx, path, files);
      graftSamples.push(sample);
    }
    const graftEnd = disk(ctx);
    assert.equal(nativeStart.typeCode, graftEnd.typeCode, "both methods must use the same filesystem");
    assert.equal(nativeStart.blockSize, graftEnd.blockSize, "filesystem block size changed during run");

    const savedBytes = (nativeStart.freeBytes - nativeEnd.freeBytes)
      - (graftStart.freeBytes - graftEnd.freeBytes);
    const nativeBytes = nativeStart.freeBytes - nativeEnd.freeBytes;
    const savedPercent = nativeBytes > 0n
      ? Number(savedBytes) * 100 / Number(nativeBytes)
      : null;
    const isolation = checkWriteIsolation(ctx, graftPaths, nativePaths);
    const report = {
      platform: process.platform,
      architecture: process.arch,
      filesystem: {
        method: "node:fs.statfsSync(root, { bigint: true }); free bytes = bfree * bsize",
        typeCode: graftEnd.typeCode,
        blockSize: graftEnd.blockSize.toString(),
        totalBytes: graftEnd.totalBytes.toString(),
      },
      sample: { fixtureMiB, fixtureBytes: (BigInt(fixtureMiB) * 1024n * 1024n).toString(),
        trackedFiles: files.length, trackedCheckoutBytes: checkoutBytes.toString(), workspaces: workspaceCount },
      native: {
        coldSeconds: nativeSamples[0].wallSeconds,
        warmSeconds: sumSeconds(nativeSamples.slice(1)),
        totalSeconds: sumSeconds(nativeSamples),
        freeBytesBefore: nativeStart.freeBytes.toString(),
        freeBytesAfter: nativeEnd.freeBytes.toString(),
        freeBytesDelta: (nativeStart.freeBytes - nativeEnd.freeBytes).toString(),
        workspaces: nativeSamples,
      },
      graft: {
        coldSeedCostSeconds: graftSamples[0].wallSeconds,
        warmSeconds: sumSeconds(graftSamples.slice(1)),
        totalSeconds: sumSeconds(graftSamples),
        freeBytesBefore: graftStart.freeBytes.toString(),
        freeBytesAfter: graftEnd.freeBytes.toString(),
        freeBytesDelta: (graftStart.freeBytes - graftEnd.freeBytes).toString(),
        workspaces: graftSamples,
      },
      savedBytes: savedBytes.toString(),
      savedPercent,
      savedPercentBasis: "savedBytes/native.freeBytesDelta*100; null if the native delta is not positive",
      writeIsolation: isolation,
      publicationCaveat: "Signed free-space deltas are volume-wide and can be affected by unrelated activity, filesystem metadata, snapshots, and delayed allocation. Logical cloned bytes come from the backend receipt (zero for Git fallback); logicalCheckoutBytes reports full tracked content. No savings threshold is asserted; copy fallback reports its observed result.",
    };
    ctx.measure("space", report);
    assert(Object.values(isolation).every(Boolean), "write isolation failed");
  },
}, dependencySpace];
