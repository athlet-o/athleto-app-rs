# jello-ws / athleto-app-rs

`jello-ws` is the cluster service identity for the AthletO shop app. The
canonical Rust repository and binary remain `athleto-app-rs` so the existing
deployment and Git history stay stable.

The AthletO shop app serves performance gelatin protein cups. *Wobble hard.
Recover clean.*

A self-contained Rust web app on the "mash" stack:

- **M**aud — server-rendered HTML (inline dark athletic theme, no asset pipeline)
- **A**xum — HTTP server and routing
- **S**QLx — runtime Postgres queries + embedded migrations (Supabase pooled Postgres)
- Supabase — GoTrue email/password auth via its REST API
- **H**TMX — add-to-cart / remove-from-cart fragment swaps

Products: **Athlet-O Starter** (20g gelatin protein, inulin fiber, vitamin C, electrolytes),
**Recover-O**, and **Pre-Game-O** — each as a ready cup (just add water was already done for you)
and a powder packet (just add water).

## Routes

| Route | What |
| --- | --- |
| `GET /` | Storefront product grid (both formats, prices, calories) |
| `GET /product/{slug}` | Product detail |
| `GET|POST /signup`, `GET|POST /login`, `POST /logout` | Supabase GoTrue auth; session tokens in HttpOnly Secure SameSite=Lax cookies |
| `GET /cart`, `POST /cart/items`, `POST /cart/items/{id}/delete` | Cart pages + htmx fragments; keyed by Supabase user id or anonymous cart cookie |
| `GET /healthz` | Liveness/readiness — always `ok`, no dependencies |

## Environment

| Variable | Default | Purpose |
| --- | --- | --- |
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `8080` | Bind port |
| `SUPABASE_URL` | *(unset)* | Supabase project URL, e.g. `https://xyz.supabase.co` |
| `SUPABASE_ANON_KEY` | *(unset)* | Supabase anon (publishable) key |
| `DATABASE_URL` | *(unset)* | Supabase pooled Postgres URL (e.g. the Supavisor `...pooler.supabase.com:6543/postgres` string) |
| `ATHLETO_PUBLIC_BASE_URL` / `ATHLETO_BIZ_PUBLIC_BASE_URL` | `https://app.athleto.store` / `https://biz.athleto.store` | Canonical browser origins for B2C and B2B redirects |
| `ATHLETO_OPERATIONS_API_KEY` | *(unset)* | Dedicated bearer credential for warehouse-only fulfillment writes |
| `ATHLETO_STRIPE_SECRET_KEY` | *(unset)* | Stripe API secret key (`sk_test_…` / `sk_live_…`); enables card checkout, B2B ACH debit, and Net-30 hosted invoices |
| `ATHLETO_STRIPE_PUBLISHABLE_KEY` | *(unset)* | Stripe publishable key (`pk_…`); reserved for client-side elements — hosted checkout doesn't need it server-side |
| `ATHLETO_STRIPE_WEBHOOK_SECRET` | *(unset)* | Stripe webhook signing secret (`whsec_…`) for `POST /webhooks/stripe` |
| `ATHLETO_PAYPAL_CLIENT_ID` / `ATHLETO_PAYPAL_CLIENT_SECRET` | *(unset)* | PayPal REST app credentials; enables PayPal one-time + subscriptions |
| `ATHLETO_PAYPAL_WEBHOOK_ID` | *(unset)* | PayPal webhook id used to verify `POST /webhooks/paypal` |
| `ATHLETO_PAYPAL_ENV` | `sandbox` | `sandbox` or `live` |
| `ATHLETO_SQUARE_ACCESS_TOKEN` / `ATHLETO_SQUARE_LOCATION_ID` | *(unset)* | Square access token + location; enables Square hosted checkout + subscription plans |
| `ATHLETO_SQUARE_WEBHOOK_SIGNATURE_KEY` | *(unset)* | Square webhook signature key for `POST /webhooks/square` |
| `ATHLETO_SQUARE_ENV` | `sandbox` | `sandbox` or `production` |
| `ATHLETO_BILLING_URL` | *(unset)* | Quaestor billing-server base URL (observer AR/AP ledger) |
| `ATHLETO_BILLING_API_KEY` | *(unset)* | Bearer token for the billing-server API |
| `ATHLETO_BILLING_TENANT_ID` | *(unset)* | AthletO tenant UUID in the multi-tenant ledger |
| `FIDUCIA_URL` / `FIDUCIA_API_KEY` | *(unset)* | fiducia.cloud endpoint + key; enables job-leadership leases and the KV secret overlay |
| `ATHLETO_SECRETS_KEY` | *(unset)* | base64 of a 32-byte AES-256 key; decrypts `v1:` envelopes read from the fiducia KV overlay. Unset ⇒ overlay disabled (env-only) |

The app starts and serves every page with **no** secrets set: `/healthz` passes, the
storefront renders from a built-in catalog, and auth/cart routes show a
"not configured" notice. Set all three variables to enable auth and cart persistence.
Payment processors are each independently optional — checkout only offers the ones
with keys present, and with none configured orders are placed as payment-pending.

**Secrets sourcing:** every variable above is read env-first; anything the
environment leaves unset is fetched once at boot from the **fiducia.cloud
config KV** (`secrets/athleto/<ENV_NAME>`, reachable with just `FIDUCIA_URL` +
`FIDUCIA_API_KEY` from any cloud provider). Env always wins; fiducia being
down is the same as unset. AWS Secrets Manager (via ESO) remains the
production secrets-of-record. Details and the `fiducia-secrets` roadmap:
[docs/secrets-management.md](docs/secrets-management.md).

## Payments

Checkout accepts **one-time, subscription, and recurring** payments through three
processors, all hosted/redirect flows (no card data touches this server — PCI
SAQ-A):

- **Stripe** — Checkout Sessions (cards; `mode=subscription` for recurring
  orders). B2B sessions additionally offer **ACH bank debit** (`us_bank_account`),
  and B2B can instead choose **Invoice my account (Net 30)**: the order ships
  against the PO and a hosted Stripe invoice (card / ACH / bank transfer) is
  emailed, due in 30 days. Net-30 requires an approved business profile and a
  purchase-order number; profile selection alone does not grant credit terms.
- **PayPal** — Orders v2 for one-time; catalog product → billing plan →
  subscription for recurring.
- **Square** — hosted payment links; catalog subscription plans for recurring
  (weekly / every-two-weeks / monthly / quarterly cadences).

Every payment is confirmed twice: server-side verification on the browser
return (`/pay/success`) and signed provider webhooks
(`/webhooks/{stripe,paypal,square}`), deduplicated via the `payment_events`
table. Orders carry `payment_status` (`pending → processing → paid`, or
`invoiced` for Net-30), and `/orders` offers **Pay now** retry for pending or
failed payments.

Settled money is mirrored into the **Quaestor billing-server**
([quaestor-ledger/billing-server.rs](https://github.com/quaestor-ledger/billing-server.rs)),
a Model-A *observer* AR/AP ledger — it records, reconciles, and proves; it never
moves money. Per settled order the app posts an invoice transaction (debit
`ar/<user>`, credit `revenue/athleto`) and a payment transaction (debit
`cash/<provider>`, credit `ar/<user>`), idempotency-keyed so webhook replays are
safe. The account page shows the customer's outstanding balance and credits from
`GET /v1/tenants/{tenant}/customers/by-email/{email}/billing-state`.

## Local run

```sh
export SUPABASE_URL=https://<project-ref>.supabase.co
export SUPABASE_ANON_KEY=<anon-key>
export DATABASE_URL=postgres://postgres.<project-ref>:<password>@<region>.pooler.supabase.com:6543/postgres
cargo run
# then open http://localhost:8080
```

Or just `cargo run` for the degraded (no-secrets) storefront.

Note: session and cart cookies are set with the `Secure` flag; modern browsers
accept them on `http://localhost`.

## Migrations

SQLx migrations live in `migrations/` and are **embedded in the binary**
(`sqlx::migrate!`). They run automatically in a background task at startup when
`DATABASE_URL` is set — startup and health checks never block on the database.
Schema: `products` (with `product_format` enum `cup|powder`), `carts` (one per
Supabase user id or anonymous cookie uuid), `cart_items`; a seed migration
inserts the 3 products x 2 formats.

To run them manually instead: `cargo install sqlx-cli --no-default-features --features rustls,postgres`
then `sqlx migrate run`.

All queries are runtime `sqlx::query`/`query_as` calls (no compile-time `query!`
macros), so the crate builds without a live `DATABASE_URL`. **New data access is
written with SeaORM** (`src/entities.rs`, over the same pool via
`AppState::orm`); the legacy sqlx queries in `db.rs` are being ported
incrementally.

### Go-forward: declarative migrations (dpm)

We can keep generating numbered SQL files in `migrations/` for now, but the
target workflow is **declarative migrations** via
[dpm](https://github.com/declarative-migrations/declarative-postgres-migrate.rs)
(github.com/declarative-migrations): a single `schema/schema.sql` is the source
of truth and the live database *converges* onto it — dpm materializes the schema
on a shadow server, introspects both sides, and emits ordered, reviewable SQL.
The Quaestor billing-server and the shared pg-defs contract already work this
way (its `migrations/` dir is frozen as an audit trail).

```sh
brew install declarative-migrations/tap/dpm

export SHADOW_DATABASE_URL=postgres://…   # server where dpm may create throwaway DBs
export TARGET_DATABASE_URL=postgres://…   # or DATABASE_URL

dpm diff      # print the migration SQL (never executes)
dpm verify    # rehearse on a shadow replica, prove convergence
dpm apply     # generate + execute (interactive confirm; destructive SQL gated)
```

When this app's schema moves to the **shared dd-platform Amazon RDS Postgres**,
it gets its **own database named `athleto`** (per-project database namespace —
never a shared `public` schema, so table names like `orders`/`payments` can't
collide with other projects). The shared schema contract lives in
`k8s-libs-and-shared-defs` → `pg-defs/` (checked out locally at
`~/codes/ores/k8s-cluster/remote/libs/pg-defs`, vendored into `k8s-cluster` as
`remote/libs`); porting this app's commerce schema there is an agreed follow-up
(currently blocked on Supabase `auth.uid()` RLS references).

## Deploy

Deployed at **https://app.athleto.store** from the ORESoftware `k8s-cluster`
repo. The public Ingress routes to the **`jello-ws`** ClusterIP Service, which
selects the existing **`dd-athleto-app-rs`** deployment pods. This repo is
vendored there as a git submodule at `remote/deployments/athleto-app-rs`. The
container image is the multi-stage
`Dockerfile` here (rust:1.90-bookworm build → debian:bookworm-slim, non-root
UID 10001, port 8080). `SUPABASE_URL` / `SUPABASE_ANON_KEY` / `DATABASE_URL` are
injected from cluster secrets; probes hit `/healthz`.
