# Maintenance & hardening notes — athleto-app-rs

Known gaps and how to shore them up, most-actionable first. Findings verified
against `main` on 2026-07-18. Each item lists the evidence, the risk, and a
concrete fix.

## P0 — none open

The e2e login-form failure is resolved: `e2e/security.test.mjs` now gates the
magic-link test on `hasAuth()` (matching `orders.test.mjs` / `b2b-approval.test.mjs`),
so CI is green without secrets. See "CI secrets" below to also run the
authenticated journeys.

## P1

### 1. Latent request-path panics on the API auth guard
- **Where:** `src/api.rs:156` and `src/api.rs:377` — `state.pool.as_ref().expect("authenticate checked pool")`.
- **Risk:** these are guarded today (the preceding `authenticate` call guarantees a
  pool), but if that invariant ever breaks the handler panics → 500 and a poisoned
  task instead of a clean error.
- **Fix:** return `AppError`/503 ("database not configured") instead of `.expect()`.
  Pattern: `let Some(pool) = state.pool.as_ref() else { return Err(AppError::unavailable("db")) };`

### 2. Rate-limiter mutex can poison the whole request path
- **Where:** `src/security.rs:283` — `self.entries.lock().expect("rate limiter lock")`
  runs in middleware on **every** request.
- **Risk:** if any thread panics while holding this lock, the mutex is poisoned and
  every subsequent request panics.
- **Fix:** recover the poisoned guard instead of panicking:
  `let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());`

### 3. The `@athleto/sync` local-first SDK is not wired in
- **Evidence:** no references to `@athleto/sync`, the `athleto-optimistic` htmx
  extension, or `/api/sync` anywhere in `src/`. The cart surface uses the app's own
  `/ws` (`src/ws.rs`) with the **stock** htmx ws extension (`src/assets.rs`), not the
  SDK's optimistic IndexedDB client.
- **Risk:** the sync layer we built (`athleto-sync`) is currently dead weight for this
  app; offline/optimistic cart behavior isn't actually delivered.
- **Fix (design decision):** either (a) vendor `@athleto/sync` and mount
  `registerOptimisticExtension` + `startSync` on at least the cart page, adding a
  `/api/sync` catch-up endpoint and a Postgres `version`/`sync_sequence` column set;
  or (b) explicitly document that the SDK is not yet adopted here and the app's own
  `/ws` is the intended path. Do not leave it ambiguous.

## P2

### 4. DB-backed handlers are only covered by the e2e suite (needs Postgres)
- **Context:** the Rust integration tests (`tests/integration.rs`) run in **degraded,
  no-DB mode**, so the SeaORM-backed order/payment handlers
  (`src/orders.rs` checkout / pay_now / reorder / quick_order_submit, `src/billing.rs`
  billing_summary, `src/api.rs` orders_list) are exercised only by `e2e.yml`, which
  spins up a throwaway Postgres. That coverage is real but lives only in CI.
- **Fix:** add an opt-in `#[ignore]`-by-default Rust integration harness against an
  ephemeral Postgres (testcontainers or a `DATABASE_URL`-gated module) so these paths
  can be exercised from `cargo test` locally, not only through the browser in CI.

### 5. `fmt` / `clippy` are non-blocking
- **Where:** `.github/workflows/ci.yml` — the `fmt` and `clippy` jobs are
  `continue-on-error: true`.
- **Fix:** once the tree is clean, drop `continue-on-error` so style/lint regressions
  block merge (athleto-sync already does this).

### 6. Transitive advisory (track, no upstream fix)
- `cargo audit`: `RUSTSEC-2023-0071` (rsa 0.9 "Marvin Attack" timing sidechannel,
  medium) via the SQLx/SeaORM MySQL stack; plus `proc-macro-error2` unmaintained.
- **Action:** no fix available upstream; low risk for this workload. Track and
  re-check on dependency bumps. If the MySQL driver is unused, disabling that SeaORM
  feature drops the `rsa` dependency entirely.

## CI secrets (to run the full e2e matrix)

`e2e.yml` provisions its own Postgres, so the guest + DB journeys run with no secrets.
To also run the authenticated journeys (login, orders, B2B approval), set these repo
secrets — without them those tests **skip** (they do not fail):
- `ATHLETO_SUPABASE_URL`, `ATHLETO_SUPABASE_ANON_KEY`, `ATHLETO_SUPABASE_SERVICE_KEY`
- `ATHLETO_OPERATIONS_API_KEY` (B2B ops-approval test)

## Deployment note — nginx `/jello` gateway not yet cut over

The shared cluster gateway still routes `/jello` to the old service, not this app:
- **File:** `~/codes/ores/k8s-cluster/remote/argocd/dd-next-runtime/dd-remote-gateway.configmap.yaml`
- `location = /jello` (line ~213) and `/jello/sample` (~229) set their upstream to
  `dd-remote-web-home.default.svc.cluster.local:8080` (`$dd_up_3` / `$dd_up_4`).
- Athleto is reachable only via its dedicated Ingress (`jello-ws:8145`).
- **Fix:** repoint `$dd_up_3`/`$dd_up_4` to `jello-ws.default.svc.cluster.local:8145`
  and update the guard in `remote/tests/general/athleto-app-config.test.ts` (which
  currently asserts the *old* wiring on purpose).
