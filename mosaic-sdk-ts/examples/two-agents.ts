/**
 * End-to-end demo: two AI agents work on the same file at once.
 *
 *   1. Spawn a mosaic-serve instance against a fresh repo.
 *   2. Agent A and Agent B both connect to the WebSocket room for
 *      "payments.ts" and watch each other's live edits.
 *   3. They each commit their work via the HTTP API — Mosaic verifies
 *      every signature and registers the changes into the DAG.
 *   4. We print the resulting branch state and recent change log.
 *
 * Run with:
 *   npx tsc && node dist/examples/two-agents.js
 */

import { spawn, type ChildProcess, spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { MosaicClient, LiveSession } from "../src/index.js";

const MOS = process.env.MOS_BIN ?? "/home/user/TestRepo/target/debug/mos";
const SERVE = process.env.MOS_SERVE_BIN ?? "/home/user/TestRepo/target/debug/mosaic-serve";

const enc = new TextEncoder();
const dec = new TextDecoder();

async function freePort(): Promise<number> {
  const net = await import("node:net");
  return new Promise((resolve) => {
    const srv = net.createServer();
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address() as { port: number };
      const port = addr.port;
      srv.close(() => resolve(port));
    });
  });
}

async function waitHealthy(c: MosaicClient): Promise<void> {
  for (let i = 0; i < 50; i++) {
    try {
      await c.health();
      return;
    } catch {}
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error("server never became healthy");
}

function log(actor: string, msg: string): void {
  const ts = new Date().toISOString().slice(11, 19);
  console.log(`[${ts}] ${actor.padEnd(10)} ${msg}`);
}

async function main(): Promise<void> {
  const serverRepo = mkdtempSync(join(tmpdir(), "mosaic-server-"));
  const aliceRepo = mkdtempSync(join(tmpdir(), "mosaic-alice-"));

  for (const repo of [serverRepo, aliceRepo]) {
    spawnSync(MOS, ["init"], { cwd: repo });
  }
  spawnSync(MOS, ["id", "setup", "--email", "alice@example.com", "--name", "Alice"], { cwd: aliceRepo });

  const port = await freePort();
  const url = `http://127.0.0.1:${port}`;
  const wsBase = `ws://127.0.0.1:${port}`;

  log("system", `starting mosaic-serve at ${url}`);
  const server: ChildProcess = spawn(SERVE, ["--repo", serverRepo, "--bind", `127.0.0.1:${port}`], {
    stdio: ["ignore", "pipe", "pipe"],
  });

  try {
    const client = new MosaicClient(url);
    await waitHealthy(client);
    log("system", "server healthy");

    // Two agents connect to the same document for live co-editing.
    log("system", "agents joining live room /ws/doc/payments.ts");
    const agentA = await LiveSession.connect(`${wsBase}/ws/doc/payments.ts`);
    const agentB = await LiveSession.connect(`${wsBase}/ws/doc/payments.ts`);

    agentA.onUpdate((bytes) => log("agent-A", `received update (${bytes.byteLength}B): ${dec.decode(bytes)}`));
    agentB.onUpdate((bytes) => log("agent-B", `received update (${bytes.byteLength}B): ${dec.decode(bytes)}`));

    // Real-time exchange of "updates" (the payload is opaque to the server).
    await new Promise((r) => setTimeout(r, 100));
    log("agent-A", "sending: 'fn chargeCard() { ... }'");
    agentA.send(enc.encode("fn chargeCard() { ... }"));

    await new Promise((r) => setTimeout(r, 100));
    log("agent-B", "sending: 'fn fraudCheck() { ... }'");
    agentB.send(enc.encode("fn fraudCheck() { ... }"));

    await new Promise((r) => setTimeout(r, 200));

    // Now they each commit through the HTTP API. We use the Rust CLI to
    // build a signed bundle in the agent's local repo, then push it.
    log("agent-A", "writing payments.ts and committing locally");
    writeFileSync(
      join(aliceRepo, "payments.ts"),
      "fn chargeCard() { return api.charge(); }\n",
    );
    spawnSync(MOS, ["commit", "-i", "agent-A: implement chargeCard", "-f", "payments.ts"], {
      cwd: aliceRepo,
    });

    log("agent-A", "pushing to server");
    const bundlePath = join(aliceRepo, "out.bundle");
    spawnSync(MOS, ["bundle", "create", "-o", bundlePath, "--branch", "main"], {
      cwd: aliceRepo,
    });
    const bytes = new Uint8Array(readFileSync(bundlePath));
    const report = await client.pushBundle(bytes);
    log("agent-A", `push result: ${report.applied.length} applied, ${report.skipped.length} skipped`);

    // Server state
    const health = await client.health();
    log("system", `server now has ${health.changes} change(s) on ${health.branches} branch(es)`);

    const changes = await client.changes();
    for (const c of changes) {
      log("system", `  ${c.id.slice(0, 12)}  ${c.intent ?? "(no intent)"}`);
    }

    const liveStats = (await fetch(`${url}/api/v1/live-stats`).then((r) => r.json())) as { open_docs: number };
    log("system", `live rooms open: ${liveStats.open_docs}`);

    await agentA.close();
    await agentB.close();
  } finally {
    server.kill();
    rmSync(serverRepo, { recursive: true, force: true });
    rmSync(aliceRepo, { recursive: true, force: true });
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
