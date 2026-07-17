# AGENTS.md — athleto-app-rs

Rules of the road for agents (and humans) working in this repo.

## Stack

Maud + Axum + SeaORM/SQLx + Supabase + HTMX. Serves two storefronts from one
binary by Host header: app.athleto.store (B2C) and biz.athleto.store (B2B).
Boots degraded with zero secrets — every new feature must keep that property
(missing config ⇒ "not configured" notice, never a crash).

## Data access: SeaORM for new code

- **All new tables and new queries use SeaORM** — entities in
  `src/entities.rs`, handle at `AppState::orm` (wraps the same Postgres pool
  as `AppState::pool`).
- Postgres enum types get **dual derives** (`sqlx::Type` +
  `sea_orm::DeriveActiveEnum`) in `db.rs` so both layers share one Rust type.
- The handwritten sqlx queries in `db.rs` are **legacy**: port them to SeaORM
  opportunistically when you touch them; don't add new sqlx queries.

## Migrations

- You can keep generating numbered sqlx files in `migrations/` for now
  (embedded via `sqlx::migrate!`, applied at boot), **but the go-forward is
  declarative migrations** with
  [dpm](https://github.com/declarative-migrations/declarative-postgres-migrate.rs)
  (org: [github.com/declarative-migrations](https://github.com/declarative-migrations)):
  edit a declarative `schema/schema.sql`, let the database converge onto it,
  and review the SQL dpm emits. Install: `brew install
  declarative-migrations/tap/dpm` (Linux: `scripts/install.sh` in the dpm
  repo). See the billing-server repo for the finished pattern (`schema/` +
  `scripts/dpm.sh`, `migrations/` frozen as an audit trail).
- **RDS namespace rule:** when the schema lands on the shared dd-platform
  Amazon RDS instance, this app uses its **own database named `athleto`** —
  one database per project, never a shared `public` schema, so our table
  names can't collide with other projects.
- The shared cross-service schema contract is `pg-defs/` in
  `k8s-libs-and-shared-defs` (local checkout:
  `~/codes/ores/k8s-cluster/remote/libs/pg-defs`); prefer registering
  cross-service tables there.

## Payments

- Providers: Stripe (cards, B2B ACH, Net-30 hosted invoices), PayPal
  (orders + subscriptions), Square (payment links + subscription plans). All
  hosted/redirect — no PAN ever touches this server.
- Config is env-driven and each provider is independently optional
  (`ATHLETO_STRIPE_*`, `ATHLETO_PAYPAL_*`, `ATHLETO_SQUARE_*`; see README).
  Placeholder values live in GitHub Actions secrets; real values in
  `~/.config/athlet-o/secrets.env` locally and AWS Secrets Manager
  (`dd/remote-dev/agent-secrets`) for the cluster.
- **Secrets sourcing** goes through `src/secrets.rs`: env first, fiducia
  config KV (`secrets/athleto/<ENV_NAME>`) as the cross-provider overlay for
  gaps. New secret env vars must be added to `secrets::MANAGED_KEYS` (an
  explicit allowlist — never widen it to arbitrary names) and to the README
  table + CI workflow. AWS SM stays production secrets-of-record until the
  encrypted `fiducia-secrets` API exists; see docs/secrets-management.md.
- Webhooks must stay idempotent: every handler records
  `(provider, event_id)` in `payment_events` first and bails on replay.
  Ledger postings use idempotency keys (`athleto:order:…`,
  `athleto:payment:…`).
- The Quaestor billing-server (`ATHLETO_BILLING_*`) is an **observer** ledger
  (Model A): post AR/payment transactions, read billing-state; never treat it
  as the payment rail. Writes are fire-and-forget — the ledger being down must
  never block checkout.

## Testing

`cargo test` must pass with no database and no network. Pure-logic tests live
in each module (`#[cfg(test)]`); HTTP/provider calls are not unit-tested here.
