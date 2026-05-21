# mosaic-sdk

TypeScript SDK for [Mosaic](../README.md) VCS. Talks to a running
`mosaic-serve` instance over HTTP. Designed for AI agents and tooling
written in JavaScript / TypeScript.

```ts
import { MosaicClient } from "mosaic-sdk";

const client = new MosaicClient("http://your-server:7700");

const health = await client.health();
console.log(`repo has ${health.changes} changes across ${health.branches} branches`);

for (const change of await client.changes()) {
  console.log(change.id.slice(0, 12), change.intent ?? "(no intent)");
}

// Pull the delta on `main` we don't have yet
const bundleBytes = await client.pullBundle("main", [/* hashes we have */]);
```

## Install

```bash
npm install
npm run build
```

## Test

Requires the Rust workspace built (`cargo build --workspace` at the repo root).

```bash
npm test
```

The tests spawn a real `mosaic-serve` process against a temporary
repository and verify round-trip behavior.

## API

- `MosaicClient(baseUrl)` — main entry point.
- `health()` — repo size and branch count.
- `branches()` / `branch(name)` — branch frontiers.
- `changes()` / `change(id)` — change history with metadata.
- `pullBundle(branch, have)` — fetch the delta the server has that you don't.
- `pushBundle(bytes)` — send a bincode-encoded bundle (built by the Rust
  CLI's `mos bundle create` or by the Rust SDK).
- `waitForChange(id, opts)` — poll until a remote commit appears.

## Why a TypeScript SDK?

Most modern AI agents and dev tooling run in JavaScript or TypeScript.
The Rust SDK (`mosaic-sdk` crate) is the in-process equivalent for native
Rust code. They share the same wire protocol.
