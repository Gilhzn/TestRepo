import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, type ChildProcess, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { MosaicClient, LiveSession } from "../src/index.js";

const MOS = process.env.MOS_BIN ?? "/home/user/TestRepo/target/debug/mos";
const SERVE = process.env.MOS_SERVE_BIN ?? "/home/user/TestRepo/target/debug/mosaic-serve";

async function freePort(): Promise<number> {
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
  const dir = mkdtempSync(join(tmpdir(), "mosaic-ts-live-"));
  spawnSync(MOS, ["init"], { cwd: dir });
  spawnSync(MOS, ["id", "setup", "--email", "live@example.com", "--name", "Live"], { cwd: dir });
  return dir;
}

test("two peers exchange binary updates through the relay", async () => {
  const repo = fresh();
  const { url, proc } = await startServer(repo);
  const wsBase = url.replace("http://", "ws://");
  try {
    const alice = await LiveSession.connect(`${wsBase}/ws/doc/payments.rs`);
    await new Promise((r) => setTimeout(r, 50));
    const bob = await LiveSession.connect(`${wsBase}/ws/doc/payments.rs`);
    await new Promise((r) => setTimeout(r, 50));

    const received: Uint8Array[] = [];
    bob.onUpdate((bytes) => received.push(bytes));

    alice.send(new Uint8Array([1, 2, 3, 4]));

    // wait up to 2s for the relay
    const start = Date.now();
    while (received.length === 0 && Date.now() - start < 2_000) {
      await new Promise((r) => setTimeout(r, 50));
    }

    assert.equal(received.length, 1);
    assert.deepEqual(Array.from(received[0]), [1, 2, 3, 4]);

    await alice.close();
    await bob.close();
  } finally {
    proc.kill();
    rmSync(repo, { recursive: true, force: true });
  }
});

test("late joiner receives the replay of prior updates", async () => {
  const repo = fresh();
  const { url, proc } = await startServer(repo);
  const wsBase = url.replace("http://", "ws://");
  try {
    const alice = await LiveSession.connect(`${wsBase}/ws/doc/notes.txt`);
    await new Promise((r) => setTimeout(r, 50));
    alice.send(new Uint8Array([0xaa, 0xbb]));
    alice.send(new Uint8Array([0xcc, 0xdd]));
    await new Promise((r) => setTimeout(r, 100));

    const carol = await LiveSession.connect(`${wsBase}/ws/doc/notes.txt`);
    const replays: Uint8Array[] = [];
    carol.onUpdate((bytes) => replays.push(bytes));

    const start = Date.now();
    while (replays.length < 2 && Date.now() - start < 2_000) {
      await new Promise((r) => setTimeout(r, 50));
    }
    assert.equal(replays.length, 2);
    assert.deepEqual(Array.from(replays[0]), [0xaa, 0xbb]);
    assert.deepEqual(Array.from(replays[1]), [0xcc, 0xdd]);

    await alice.close();
    await carol.close();
  } finally {
    proc.kill();
    rmSync(repo, { recursive: true, force: true });
  }
});
