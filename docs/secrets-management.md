# Secrets management — Fiducia KV and external key stores

*2026-07-17. Applies to athleto-app-rs first, but the pattern is meant for
every dd service.*

## Source and injection layers

| Layer | Mechanism |
|---|---|
| Local dev | `~/.config/athlet-o/secrets.env` (chmod 600, auto-sourced by the nix dev shell) |
| Kubernetes | External Secrets Operator or Secrets Store CSI → env; the upstream store may be Vault, AWS, GCP, Azure, or another provider |
| CI | GitHub Actions secrets on `athlet-o/athleto-app-rs` (currently `PLACEHOLDER` values; wired in `.github/workflows/ci.yml`) |

Environment injection remains the first-precedence path. Fiducia is the
cloud-neutral distribution plane for missing values; it does not require a
service to import an AWS/GCP/Azure SDK.

## Audit outcome (2026-07)

A security audit found that the original KV stored plaintext in Raft logs and
snapshots. Fiducia now seals values **before** replication with either:

- a versioned local AES-256-GCM keyring; or
- HashiCorp Vault Transit, where Fiducia never receives the encryption key.

Ciphertext is authenticated against the org-scoped storage key, malformed or
unreadable envelopes fail closed, and reads report non-secret
`protection.at_rest`, provider, key id, and external key version metadata.
Legacy client-side `v1:` envelopes remain readable during migration. Prefix-
scoped secret authorization, access auditing, and peer mTLS remain separate
hardening work.

## Can fiducia.cloud be the cross-provider secrets manager?

**Yes.** Fiducia can use Vault Transit as the external, cloud-neutral key
authority while Raft provides linearizable distribution and revisions.

What fiducia already has that fits:

- Org-scoped auth (API keys, edge-injected org identity) that every dd service
  already carries as `FIDUCIA_URL` + `FIDUCIA_API_KEY` — athleto uses it today
  for singleton-job leases.
- A **Raft-replicated, linearizable, versioned config KV with SSE watches**
  (`/v1/kv`, etcd-style, org-namespaced). Versioning + watch is exactly the
  rotation-propagation mechanism secrets need.
- Cross-provider by construction: a VM on Hetzner, a function on any cloud, or a pod
  anywhere can all reach the same fiducia endpoint with one key — no
  per-cloud IAM plumbing.

What still needs hardening before it should be treated as a complete secret
management product:

1. **Secret-grade access control.** One API key currently grants the whole
   org's KV. Secrets need read/write/admin scopes per prefix, so a shop app
   key can read `secrets/athleto/*` and nothing else.
2. **Audit.** Who read which secret when — an append-only access log.
3. **Peer transport.** Require mTLS for Raft and internal HTTP even though
   secret values are already ciphertext at that boundary.
4. **Hygiene.** Keep values excluded from debug output, `observe`/MCP diagnostics,
   and backups-in-plaintext.

A future `/v1/secrets/*` facade can add prefix scopes and audit while reusing
the protected KV storage, revisions, and SSE watch machinery.

## What is implemented in this app now

`src/secrets.rs` + `coordinate::FiduciaClient::kv_get`:

- At boot, for the explicit allowlist `secrets::MANAGED_KEYS` (Supabase,
  `DATABASE_URL`, all `ATHLETO_*` payment/billing vars), any name the
  **environment does not set** is fetched once from the fiducia config KV at
  `secrets/athleto/<ENV_NAME>` (org-namespaced by the API key).
- **Three migration-safe storage modes.** Node-protected encrypted values are
  accepted using `protection.at_rest=encrypted`; an operator may explicitly
  write a non-sensitive value with `plaintext:true`, which is accepted only
  when the node reports `at_rest=plaintext`; legacy AES-256-GCM `v1:` client
  envelopes are decrypted with `ATHLETO_SECRETS_KEY`.
- **Fail-safe metadata:** raw values from older nodes that omit protection
  metadata are rejected. A legacy `v1:` envelope is rejected when its external
  key is absent, malformed, wrong, or fails authentication.
- **Precedence: env always wins.** Externally injected deployments behave exactly as
  before; the overlay only fills gaps. Fiducia down / key missing / undecryptable
  ⇒ identical to unset env (the app boots degraded, as always).
- The allowlist is deliberate: nothing outside it is ever read from KV, so a
  writable KV can't inject `PATH`-style variables. Log lines name filled keys,
  never values.

### Client and transport boundary

The app does **not** currently declare the upstream Rust `fiducia-client`
crate from `fiducia-cloud/fiducia-clients`. The audited upstream revision
(`e2fa3c0`, 2026-07-18) is an unreleased blocking client whose Cargo manifest
depends on an unpublished sibling `fiducia-interfaces` path, so it cannot
resolve as a normal git or registry dependency. It also models trusted
internal-header authentication, while this app uses the deployed
`Authorization: Bearer $FIDUCIA_API_KEY` edge contract.

`src/coordinate.rs` therefore keeps a small async `reqwest` adapter that
matches the shared lock/KV wire protocol: it disables redirects, bounds every
request, accepts only `https` public endpoints (or recognized local/in-cluster
`http` addresses), and requires the committed `result.output.fencing_token`
grant before leader-only work can start. Do not add an unpinned git dependency
on `fiducia-clients`; migrate only after a versioned, independently resolvable
Rust package supports the deployed bearer-auth mode.

Publishing a node-encrypted value (the default when Fiducia has a local keyring
or Vault Transit configured):

```sh
curl -X PUT "$FIDUCIA_URL/v1/kv?key=secrets/athleto/ATHLETO_STRIPE_SECRET_KEY" \
  -H "Authorization: Bearer $FIDUCIA_API_KEY" \
  -H 'content-type: application/json' \
  -d '{"value": "SECRET_VALUE"}'
```

Fiducia seals the value before it enters the Raft log. For a deliberately
unencrypted, non-sensitive value, add `"plaintext":true`. To retain the legacy
client-envelope mode, PUT the output of `secrets::seal_envelope`; the app will
unwrap it with `ATHLETO_SECRETS_KEY`.

## Remaining fiducia-side work (from the audit)

1. **Narrow the read scope for secrets.** Today any credential with org-wide
   `kv:read` can enumerate `secrets/*`. Give the secrets namespace a dedicated
   `secrets:`-scoped grant (or per-prefix scoping) so a general KV key can't
   read the whole set. (HIGH)
2. **Peer mTLS in fiducia-node**, so the internal trust boundary is mutually
   authenticated in addition to carrying ciphertext. (MEDIUM)
3. **Per-org KV quota + key validation** to remove the memory-exhaustion DoS
   on a shared coordination dependency. (MEDIUM)

## Rotation story

- External injectors: rotate in Vault/cloud store; pods pick up on injector
  refresh + restart.
- Vault Transit backend: rotate the Vault transit key; new writes use the new
  Vault key version and old ciphertext stays decryptable. The read response
  exposes `key_version` for audit without exposing key material.
- Local keyring backend: add the new key alongside the old one, set it active,
  roll the cluster, then rewrite values before retiring the old key id.
- Secret values: write a new KV revision; consumers either restart or
  subscribe to `GET /v1/kv?prefix=secrets/athleto/&watch=true` (SSE) and
  rebuild provider clients on change. The boot-time overlay is step one; the
  watch loop is a follow-up if we want restart-free rotation.
