# mosaic-sdk

Python SDK for [Mosaic](../README.md) VCS. Talks to a running
`mosaic-serve` instance over HTTP. Designed for AI agents and tooling
written in Python — pairs with the Rust crate (`mosaic-sdk`) and the
TypeScript package (`mosaic-sdk`).

Zero external dependencies for HTTP (`urllib` from stdlib). The
`LiveSession` real-time co-editing client requires `websockets`:

```bash
pip install mosaic-sdk            # HTTP only
pip install "mosaic-sdk[live]"    # + WebSocket real-time co-editing
```

## HTTP usage

```python
from mosaic import MosaicClient

client = MosaicClient("http://your-server:7700")

health = client.health()
print(f"repo has {health.changes} change(s) on {health.branches} branch(es)")

for c in client.changes():
    print(c.id[:12], c.intent or "(no intent)")

# Push / pull bundles produced by the Rust CLI:
bundle_bytes = open("changes.bundle", "rb").read()
report = client.push_bundle(bundle_bytes)
print(f"applied {len(report.applied)}, skipped {len(report.skipped)}")
```

## Real-time co-editing

```python
import asyncio
from mosaic import LiveSession

async def main():
    async with await LiveSession.connect("ws://server:7700/ws/doc/payments.py") as s:
        await s.send(b"hello from python")
        async for update in s.recv_iter():
            print(f"got {len(update)} bytes")

asyncio.run(main())
```

## Test

Requires the Rust workspace built (`cargo build --workspace` at the repo root).

```bash
pip install "mosaic-sdk[test]"
python -m unittest tests/test_client.py
```

## API

- `MosaicClient(base_url)` — main entry.
- `health()` — `HealthResponse(ok, repo, changes, branches)`.
- `branches()` / `branch(name)` — frontier tips.
- `changes()` / `change(id)` — change list / detail.
- `change_diff(id)` — per-file unified diff (`insert/delete/equal` hunks).
- `pull_bundle(branch, have)` — fetch the delta as raw bytes.
- `push_bundle(bytes)` — push a Rust-CLI-built bundle.
- `wait_for_change(id, timeout_s)` — poll for a remote commit.
- `LiveSession.connect(ws_url)` — duplex binary stream.
