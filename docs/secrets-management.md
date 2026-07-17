# Secrets management — current state and the fiducia.cloud plan

*2026-07-17. Applies to athleto-app-rs first, but the pattern is meant for
every dd service.*

## Where secrets live today (unchanged, still the source of record)

| Layer | Mechanism |
|---|---|
| Local dev | `~/.config/athlet-o/secrets.env` (chmod 600, auto-sourced by the nix dev shell) |
| Kubernetes | AWS Secrets Manager `dd/remote-dev/agent-secrets` (us-east-1) → External Secrets Operator → `dd-agent-secrets` → env |
| CI | GitHub Actions secrets on `athlet-o/athleto-app-rs` (currently `PLACEHOLDER` values; wired in `.github/workflows/ci.yml`) |

AWS Secrets Manager gives us the things a secrets store must have — encryption
at rest under KMS, IAM-scoped access, rotation hooks, audit via CloudTrail —
so it stays **production secrets-of-record** until fiducia offers equivalents.

## Audit outcome (2026-07)

A security audit of fiducia-node confirmed the KV is **plaintext at rest and
in transit**: values sit in cleartext in the Raft log and snapshots on every
node's disk, and cross the peer network over plain HTTP. It is *not* an
encrypted vault. Verdict: **the KV is not safe to hold raw production secrets
as-is.** Two required changes before it is — (1) values must be ciphertext
fiducia never sees, (2) the secrets namespace must not be readable by a
general org-wide `kv:read` scope.

We implemented (1) on the client immediately (below). (2) and server-side
at-rest encryption / peer mTLS are tracked as fiducia work.

## Can fiducia.cloud be the cross-provider secrets manager?

**Yes as the distribution plane now (holding only ciphertext); yes as the
store later, after it grows a real encrypted secrets API.** The honest split:

What fiducia already has that fits:

- Org-scoped auth (API keys, edge-injected org identity) that every dd service
  already carries as `FIDUCIA_URL` + `FIDUCIA_API_KEY` — athleto uses it today
  for singleton-job leases.
- A **Raft-replicated, linearizable, versioned config KV with SSE watches**
  (`/v1/kv`, etcd-style, org-namespaced). Versioning + watch is exactly the
  rotation-propagation mechanism secrets need.
- Cross-provider by construction: a VM on Hetzner, a lambda on AWS, or a pod
  anywhere can all reach the same fiducia endpoint with one key — no
  per-cloud IAM plumbing.

What it does **not** have yet (and must, before real secrets live in it):

1. **Encryption at rest.** KV values sit in the Raft log/state machine in
   plaintext. Secrets need envelope encryption: a per-org data key, wrapped by
   a KMS root (AWS KMS first, pluggable later), values encrypted before they
   enter the log.
2. **Secret-grade access control.** One API key currently grants the whole
   org's KV. Secrets need read/write/admin scopes per prefix, so a shop app
   key can read `secrets/athleto/*` and nothing else.
3. **Audit.** Who read which secret when — an append-only access log.
4. **Hygiene.** Values excluded from debug output, `observe`/MCP diagnostics,
   and backups-in-plaintext.

That is the `fiducia-secrets` proposal: a `/v1/secrets/*` API (same node,
same Raft) that stores envelope-encrypted values, reuses KV revisions for
versioned rotation, reuses SSE watch for push-rotation, and adds scoped keys +
audit. Services then need only `FIDUCIA_URL`/`FIDUCIA_API_KEY` on any cloud —
that's the end state.

## What is implemented in this app now

`src/secrets.rs` + `coordinate::FiduciaClient::kv_get`:

- At boot, for the explicit allowlist `secrets::MANAGED_KEYS` (Supabase,
  `DATABASE_URL`, all `ATHLETO_*` payment/billing vars), any name the
  **environment does not set** is fetched once from the fiducia config KV at
  `secrets/athleto/<ENV_NAME>` (org-namespaced by the API key).
- **Client-side envelope encryption (the audit fix).** KV values are
  AES-256-GCM ciphertext with a `v1:` prefix (`v1:` + base64(nonce‖ct‖tag)).
  The app decrypts them with `ATHLETO_SECRETS_KEY` (base64 of a 32-byte key)
  sourced from env only — i.e. from AWS SM / secrets.env, **never** from
  fiducia. So fiducia only ever holds opaque ciphertext; a KV-disk or
  peer-network compromise reveals nothing.
- **Fail-safe on the key:** if `ATHLETO_SECRETS_KEY` is unset, the overlay is
  disabled entirely (env-only) with a warning — the app will not read
  plaintext out of the KV. A value that isn't a decryptable `v1:` envelope is
  ignored, never accepted as a secret.
- **Precedence: env always wins.** ESO/AWS-SM deployments behave exactly as
  before; the overlay only fills gaps. Fiducia down / key missing / undecryptable
  ⇒ identical to unset env (the app boots degraded, as always).
- The allowlist is deliberate: nothing outside it is ever read from KV, so a
  writable KV can't inject `PATH`-style variables. Log lines name filled keys,
  never values.

Publishing a value (seal client-side, then PUT the ciphertext; put only
test-mode keys here until the fiducia scope work below lands):

```sh
# Seal with the same AES-256 key the app decrypts with (base64 32 bytes).
# `secrets::seal_envelope` produces the "v1:<base64>" string; store THAT:
curl -X PUT "$FIDUCIA_URL/v1/kv?key=secrets/athleto/ATHLETO_STRIPE_SECRET_KEY" \
  -H "Authorization: Bearer $FIDUCIA_API_KEY" \
  -H 'content-type: application/json' \
  -d '{"value": "v1:BASE64_SEALED_CIPHERTEXT"}'
```

The plaintext never leaves the machine that holds `ATHLETO_SECRETS_KEY`;
fiducia stores only the `v1:` blob.

## Remaining fiducia-side work (from the audit)

1. **Narrow the read scope for secrets.** Today any credential with org-wide
   `kv:read` can enumerate `secrets/*`. Give the secrets namespace a dedicated
   `secrets:`-scoped grant (or per-prefix scoping) so a general KV key can't
   read the whole set. (HIGH)
2. **At-rest encryption + peer mTLS in fiducia-node**, so non-app-encrypted
   coordination data (lock holders, KV used by other services) is also
   protected. Then a true `/v1/secrets/*` API can supersede this client-side
   scheme — at which point the seam is `kv_get` + `decrypt_envelope`. (MEDIUM)
3. **Per-org KV quota + key validation** to remove the memory-exhaustion DoS
   on a shared coordination dependency. (MEDIUM)

## Rotation story

- Today: rotate in AWS SM (cluster) / secrets.env (local); pods pick up on
  ESO refresh + restart.
- With fiducia: rotate by writing a new revision; consumers either restart or
  subscribe to `GET /v1/kv?prefix=secrets/athleto/&watch=true` (SSE) and
  rebuild provider clients on change. The boot-time overlay is step one; the
  watch loop is a follow-up if we want restart-free rotation.
