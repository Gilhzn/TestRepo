import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { MosaicClient, MosaicError } from "../src/index.js";

const MOS = process.env.MOS_BIN ?? "/home/user/TestRepo/target/debug/mos";
const SERVE = process.env.MOS_SERVE_BIN ?? "/home/user/TestRepo/target/debug/mosaic-serve";

function runMos(repo: string, args: string[]): void {
  const result = spawnSync(MOS, args, { cwd: repo });
  if (result.status !== 0) {
    throw new Error(`mos ${args.join(" ")} failed: ${result.stderr}`);
  }
}

import { spawnSync } from "node:child_process";

async function freePort(): Promise<number> {
  // Bind 0 to get an OS-assigned port, then close.
  const net = await import("node:net");
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      if (!addr || typeof addr === "string") {
        srv.close();
        reject(new Error("no addr"));
        return;
      }
      const port = addr.port;
      srv.close(() => resolve(port));
    });
  });
}

async function waitForServer(client: MosaicClient, timeoutMs = 5_000): Promise<void> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      await client.health();
      return;
    } catch {
      await new Promise((r) => setTimeout(r, 100));
    }
  }
  throw new Error("server never became healthy");
}

async function startServer(repo: string): Promise<{ url: string; proc: ChildProcess }> {
  const port = await freePort();
  const proc = spawn(SERVE, ["--repo", repo, "--bind", `127.0.0.1:${port}`], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  const url = `http://127.0.0.1:${port}`;
  const client = new MosaicClient(url);
  await waitForServer(client);
  return { url, proc };
}

function fresh(): string {
  const dir = mkdtempSync(join(tmpdir(), "mosaic-ts-"));
  runMos(dir, ["init"]);
  runMos(dir, ["id", "setup", "--email", "alice@example.com", "--name", "Alice"]);
  return dir;
}

test("health endpoint returns repo state", async () => {
  const repo = fresh();
  const { url, proc } = await startServer(repo);
  try {
    const c = new MosaicClient(url);
    const h = await c.health();
    assert.equal(h.ok, true);
    assert.equal(h.changes, 0);
    assert.equal(h.branches, 0);
  } finally {
    proc.kill();
    rmSync(repo, { recursive: true, force: true });
  }
});

test("branches and changes list after commits", async () => {
  const repo = fresh();
  const fs = await import("node:fs");
  fs.writeFileSync(join(repo, "first.txt"), "first\n");
  runMos(repo, ["commit", "-i", "first via ts test", "-f", "first.txt"]);
  fs.writeFileSync(join(repo, "hello.txt"), "hello world\n");
  runMos(repo, ["commit", "-i", "second via ts test", "-f", "hello.txt"]);

  const { url, proc } = await startServer(repo);
  try {
    const c = new MosaicClient(url);
    const branches = await c.branches();
    assert.equal(branches.length, 1);
    assert.equal(branches[0].name, "main");

    const changes = await c.changes();
    assert.equal(changes.length, 2);
    const intents = changes.map((c) => c.intent ?? "");
    assert.ok(intents.some((i) => i.includes("first via ts test")));
    assert.ok(intents.some((i) => i.includes("second via ts test")));
  } finally {
    proc.kill();
    rmSync(repo, { recursive: true, force: true });
  }
});

test("bundle pull returns binary, push round-trips through server", async () => {
  const sourceRepo = fresh();
  const fs = await import("node:fs");
  fs.writeFileSync(join(sourceRepo, "a.txt"), "alpha\n");
  runMos(sourceRepo, ["commit", "-i", "alpha via ts", "-f", "a.txt"]);

  const serverRepo = fresh();
  const { url, proc } = await startServer(serverRepo);
  try {
    // Build a bundle on the source by invoking the CLI to write one to disk,
    // then push it via the TS client.
    const bundlePath = join(sourceRepo, "out.bundle");
    runMos(sourceRepo, ["bundle", "create", "-o", bundlePath, "--branch", "main"]);
    const bytes = new Uint8Array(fs.readFileSync(bundlePath));

    const c = new MosaicClient(url);
    const report = await c.pushBundle(bytes);
    assert.equal(report.applied.length, 1);
    assert.equal(report.skipped.length, 0);

    // Server should now expose the branch + change.
    const branches = await c.branches();
    assert.equal(branches.length, 1);

    // Fresh receiver should be able to pullBundle and observe the change.
    const pulled = await c.pullBundle("main", []);
    assert.ok(pulled.byteLength > 0);
    assert.equal(pulled[0], "M".charCodeAt(0)); // MOSAICB1 magic
  } finally {
    proc.kill();
    rmSync(sourceRepo, { recursive: true, force: true });
    rmSync(serverRepo, { recursive: true, force: true });
  }
});

test("missing branch returns MosaicError with status", async () => {
  const repo = fresh();
  const { url, proc } = await startServer(repo);
  try {
    const c = new MosaicClient(url);
    let caught: MosaicError | null = null;
    try {
      await c.branch("does-not-exist");
    } catch (e) {
      caught = e as MosaicError;
    }
    assert.ok(caught instanceof MosaicError);
    assert.equal(caught.status, 404);
  } finally {
    proc.kill();
    rmSync(repo, { recursive: true, force: true });
  }
});
