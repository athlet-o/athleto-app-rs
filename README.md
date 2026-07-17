# jello-ws / athleto-app-rs

`jello-ws` is the cluster service identity for the AthletO shop app. The
canonical Rust repository and binary remain `athleto-app-rs` so the existing
deployment and Git history stay stable.

The AthletO shop app serves performance gelatin protein cups. *Wobble hard.
Recover clean.*

A self-contained Rust web app on the "MASH" stack:

- **M**aud — server-rendered HTML (inline dark athletic theme, no asset pipeline)
- **A**xum — HTTP server and routing (plus a websocket endpoint pushing HTML fragments)
- **S**eaORM — entity query builders over the Supabase pooled Postgres (raw SQL kept
  for the transactional hold/checkout paths; SQLx remains underneath for the
  embedded migrations)
- Supabase — GoTrue passwordless (magic link + MFA) auth via its REST API
- **H**TMX — add-to-cart / remove-from-cart fragment swaps (vendored, served same-origin)

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
| `ALLOWED_HOSTS` | *(unset)* | Comma-separated Host-header allowlist (e.g. `app.athleto.store,biz.athleto.store`); unset = permissive with a startup warning |

The app starts and serves every page with **no** secrets set: `/healthz` passes, the
storefront renders from a built-in catalog, and auth/cart routes show a
"not configured" notice. Set all three variables to enable auth and cart persistence.

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

Application queries go through SeaORM (`src/entities/` + `src/db.rs`); the
hold-claim and checkout transactions stay hand-written SQL executed via
`sea_orm::Statement` to preserve their locking semantics. Everything runs at
runtime against the pool, so the crate builds without a live `DATABASE_URL`.

## Deploy

Deployed at **https://app.athleto.store** from the ORESoftware `k8s-cluster`
repo. The public Ingress routes to the **`jello-ws`** ClusterIP Service, which
selects the existing **`dd-athleto-app-rs`** deployment pods. This repo is
vendored there as a git submodule at `remote/deployments/athleto-app-rs`. The
container image is the multi-stage
`Dockerfile` here (rust:1.90-bookworm build → debian:bookworm-slim, non-root
UID 10001, port 8080). `SUPABASE_URL` / `SUPABASE_ANON_KEY` / `DATABASE_URL` are
injected from cluster secrets; probes hit `/healthz`.
