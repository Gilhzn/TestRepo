# Mosaic Operations Runbook

How to deploy, secure, back up, and maintain a Mosaic sync server in
production. Aimed at the operator, not the end-user developer.

## Deploying `mosaic-serve`

```bash
# Build the release binaries
cargo build --release
# → target/release/{mos, mosaic-serve, mosaic-lsp, mosaic-mount, mosaic-mcp}

# Run the server against a repo directory
mosaic-serve --repo /var/lib/mosaic/team --bind 127.0.0.1:7700
```

Always put `mosaic-serve` **behind a TLS-terminating reverse proxy**
(nginx, Caddy, a cloud load balancer). The server speaks plain HTTP; the
proxy provides TLS, rate limiting, and IP allowlisting.

Example Caddy block:

```
mosaic.example.com {
    reverse_proxy 127.0.0.1:7700
}
```

Run it under a process supervisor (systemd, etc.):

```ini
[Unit]
Description=Mosaic sync server
After=network.target

[Service]
ExecStart=/usr/local/bin/mosaic-serve --repo /var/lib/mosaic/team --bind 127.0.0.1:7700
Restart=on-failure
User=mosaic
# If using KMS-supplied encryption key, fetch it into the environment here.

[Install]
WantedBy=multi-user.target
```

## Authentication & authorization

Two layers, both server-side:

1. **Push allowlist** (`auth::Policy`) — `.mosaic/allowed_signers.txt`,
   one hex ed25519 pubkey per line. Manage with:
   ```bash
   # On a client, get your pubkey:
   mos trust me
   # On the server's repo, trust it:
   mos trust add <hex-pubkey>
   mos trust list
   mos trust remove <hex-pubkey>
   ```
   In **open mode** (no file present) any well-signed change is accepted —
   use only for local dev. In **allowlist mode**, list only the team's
   *human* long-term keys; agents push under session keys covered by an
   attestation issued by a trusted human (see Architecture L4).

2. **Branch & path protection** (`protection::ProtectionRules`) —
   `.mosaic/protection.json`:
   ```bash
   mos protection set-branch main --require-signed --approvals 2 --no-force-push
   mos protection set-path "payments/**" --allow-only human:cfo@example.com
   mos protection show
   ```

## Key management

- The signing identity lives in `<repo>/.mosaic/identity/key.secret`.
  `chmod 600` it; it never leaves the machine.
- For **encryption at rest**, the CAS master key is at `<store>/key`.
  Back it up to a secrets manager — **losing it means losing the data.**
  In production, prefer fetching it at boot from a KMS and passing it via
  `EncryptedCas::with_key` rather than letting it sit on disk.
- **Key rotation / revocation**: remove a compromised key from
  `allowed_signers.txt` and reload the server. Already-landed changes
  remain (they were valid when accepted); future pushes under that key
  are rejected. To retroactively excise an agent's work, use
  `mos rollback <session-id>`.

## Backups

The entire repository state is the `.mosaic/` directory. It is
append-mostly (changes, reviews, issues, audit are append-only; refs and
config are small mutable files). Back up with any file-level tool:

```bash
# Consistent snapshot: pause writes (or use a filesystem snapshot), then
rsync -a /var/lib/mosaic/team/.mosaic/ backup-host:/backups/team/
```

If the CAS is offloaded to S3 (`HttpCas`/`TieredCas`), the object store's
own durability + the local `.mosaic/{changes,refs}` are what you back up.

## Garbage collection

Unreachable changes/blobs accumulate (abandoned branches, amended/squashed
tips, rolled-back sessions). Reclaim space on a schedule:

```bash
mos gc --dry-run      # preview: live vs prunable counts + bytes
mos gc                # mark-and-sweep from all branch frontiers
```

GC only deletes objects with no reachable reference. **Re-add any branch
ref you care about before running gc** — reachability is computed from
the refs that exist at gc time.

## Monitoring

- `GET /api/v1/health` — liveness + change/branch counts. Wire it to your
  uptime checker.
- `GET /api/v1/live-stats` — number of active live-editing rooms.
- Audit log (`<repo>/.mosaic/audit/`) is append-only JSONL; ship it to
  your SIEM for compliance. Query locally with `mos audit ...`.

## Webhooks (CI / chat / deploy)

`.mosaic/webhooks.json`:

```json
{ "endpoints": [
  { "url": "https://ci.example.com/mosaic", "secret": "shared", "events": ["push"] }
]}
```

On each accepted push the server POSTs a `{ type: "push", branch, tips,
applied, skipped }` body, signed (if `secret` set) in the
`X-Mosaic-Signature` header.

## Disaster recovery checklist

1. Restore `.mosaic/` from backup (or the S3 CAS + local refs).
2. Restore the encryption master key from your secrets manager.
3. `mosaic-serve --repo <restored> ...` and hit `/api/v1/health`.
4. Have one client `mos pull origin` to confirm the frontier is intact.
