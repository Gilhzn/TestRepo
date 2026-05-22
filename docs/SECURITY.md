# Mosaic Security Model

The threat model and the mechanisms that address it. Mosaic's central
security premise: **in a world where AI agents write most of the code,
you must be able to prove who — human or agent — produced every change,
and to revoke or roll back that work cleanly.**

## Assets

- **Source history** — the change DAG and the blobs it references.
- **Identities & keys** — human long-term ed25519 keys; agent session keys.
- **Trust configuration** — the push allowlist + protection rules.

## Trust boundaries

```
   Untrusted: arbitrary network clients hitting mosaic-serve
        │  (TLS at the reverse proxy; allowlist + attestation at /bundle)
        ▼
   Semi-trusted: holders of an allowlisted human key, and the agents
                 they attest for
        │  (every Change is ed25519-signed; protection rules gate branches)
        ▼
   Trusted: the operator who controls .mosaic/{allowed_signers,protection}
            and the encryption master key
```

## Mechanisms

### Change integrity & authorship

Every `Change` is signed (ed25519) over its canonical bytes, and the
change id is the BLAKE3 of those bytes. Tampering with any field
invalidates both the signature and the id. `Repository::commit` and
`apply_bundle` both call `verify()` before accepting.

### Agent identity — the attestation chain

An agent claims `Agent { name, session_id, invoker: Human }`. On its own
that's just a string. The **attestation** makes it verifiable:

```
Human long-term key  ──signs──▶  Attestation {
                                   agent, session_pubkey,
                                   valid_from, valid_until }
                                        │
session_pubkey  ──signs──▶  Change { author = agent, ... }
```

`Attestation::authorize(change)` checks: the change is signed by the
attested session key, the change's author matches the attested agent,
and `change.ts` falls inside the validity window. The server
(`auth::Policy`) only accepts a session-key-signed change if a valid
attestation from an **allowlisted human key** covers it.

Consequence: an operator's allowlist holds only the team's human keys.
Agents rotate short-lived session keys freely; compromise of one session
key is bounded by its validity window and revocable by rolling back the
session.

### Push authorization

Two gates on `POST /api/v1/bundle`, in order:

1. `auth::Policy::authorize_bundle` — every change's author key is either
   directly allowlisted or covered by a trusted attestation.
2. `protection::ProtectionRules::check_push` — per-branch
   (`require_signed`, `require_approvals`, `allow_force_push`,
   author allow/deny) and per-path (author allow/deny) rules. Returns a
   structured 403 with the exact reason.

### Encryption at rest

`EncryptedCas` stores every blob ChaCha20-Poly1305-AEAD-encrypted under a
32-byte master key, content-addressed on the *plaintext* hash. Plaintext
never lands on disk; a tampered ciphertext fails the AEAD tag check; a
wrong key cannot decrypt. Key custody is the operator's responsibility
(KMS-at-boot recommended).

### Auditability & response

- **Audit log** — append-only, ed25519-signed `AuditEvent`s, queryable
  by session (`mos audit session <id>`) or actor. Ship to a SIEM.
- **Per-session rollback** — `mos rollback <session-id>` rewinds a branch
  past everything an agent did in a session (the session's changes and
  anything built on them), DAG-safe and recoverable.

## What Mosaic does NOT yet defend against (known gaps)

- **Transport security is delegated** — `mosaic-serve` speaks plain HTTP;
  you MUST front it with TLS. No built-in mTLS yet.
- **No rate limiting / DoS protection** in the server — do it at the proxy.
- **No secret scanning** on commit — a pre-commit hook ecosystem is future
  work.
- **Master-key custody** — if the encryption key leaks, at-rest encryption
  is void. Mosaic provides the mechanism, not the key-management policy.
- **No formal external audit yet.** The crypto uses well-reviewed RustCrypto
  primitives (ed25519-dalek, chacha20poly1305, blake3) but the composition
  has not been independently audited.

## Reporting

Security issues should be reported privately to the maintainers before
public disclosure. (Establish a real disclosure address before any public
beta.)
