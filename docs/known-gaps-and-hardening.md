# Known gaps & hardening

Running list of correctness gaps, half-built features, and hardening work,
with file:line evidence and the concrete fix for each. Keep this current as
items are resolved. Companion: [testing.md](testing.md) for the test-coverage
side.

Line numbers drift as the code moves — treat them as a starting point and
re-grep the named function/symbol.

---

## Resolved (keep for context)

- **B2B accounts were dead-on-arrival.** The approval *gate*
  (`is_b2b_approved()` / `customer_profiles.b2b_approved_at`, `db.rs`) blocked
  ordering, the ERP API, checkout, and API keys, but nothing ever *set*
  `b2b_approved_at` — only test fixtures. Fixed by adding the ops action
  `POST /api/v1/ops/customers/{user_id}/approval` + `db::set_b2b_approval`,
  gated by the operations credential.
- **Recurring runner double-fired provider subscriptions.** `place_order` sets
  `next_run_at` for every recurring order, while provider checkout also stands
  up a provider-managed subscription — so both fired each cycle (provider
  charged the card; the internal runner minted a *separate unpaid orphan child
  order* and double-decremented stock). Fixed by guarding
  `db::run_due_recurring_orders` to skip any order with a `payment_subscriptions`
  row. Covered by `tests/recurring_runner_db.rs`.
- **Migration 0006 checksum drift** — see [Migration discipline](#migration-discipline).

---

## Open functional gaps (ranked by impact)

### 1. Subscriptions cannot be cancelled in-app; provider cancel doesn't stop the internal runner
- No subscription/cancel route exists in `lib.rs`'s `router()`. `next_run_at`
  is only ever *set* at checkout and *advanced* by the runner — never cleared.
- The UI advertises the subscription and its next run (`orders.rs`) but renders
  **no cancel/pause control**.
- `db::set_subscription_status(Cancelled)` is reachable only from provider
  webhooks; it updates `payment_subscriptions.status` and does **not** touch the
  parent order's `next_run_at`/`status`. (After the double-fire fix the internal
  runner already skips provider-backed orders, so a provider cancel no longer
  leaves an internal loop — but an *internally-driven* Net-30 recurring order
  still has no cancel path at all.)
- **Fix:** add `POST /orders/{id}/cancel` (customer) + an ops equivalent that
  sets `status='cancelled'` and `next_run_at=NULL`, calls the provider's
  cancel API when a `payment_subscriptions` row exists, and renders a
  cancel/pause control in the orders UI. Wire it into `router()`.

### 2. Order cancellation is entirely absent — `OrderStatus::Cancelled` is a write-never state
- `Cancelled` is only ever *read*: CSS class, payment-badge mapping,
  `payment_retryable` guard, the B2B "Order management" **status filter offers
  it**, and raw SQL `status <> 'cancelled'`. No code path ever *writes*
  `'cancelled'`, so the filter option matches nothing and `payment_retryable`'s
  `status != Cancelled` check is permanently true.
- **Fix:** a cancel action (customer while unfulfilled, ops any time) that
  writes `status='cancelled'`, restores stock (`inventory.on_hand += qty`), and
  triggers a refund when already paid (see #3).

### 3. Refunds are scaffolded but unbuilt
- `PaymentStatus::Refunded`, `PaymentKind::Refund` exist and are encoding-tested;
  `payment_class` renders Refunded; `dollars()` handles negatives. But **none of
  the three webhook handlers has a refund branch** (Stripe `charge.refunded`,
  PayPal `PAYMENT.CAPTURE.REFUNDED`, Square `refund.*`), and there is no ops
  refund endpoint. Nothing ever produces `Refunded`/`Refund`.
- **Fix:** add the refund webhook branches (write a `PaymentKind::Refund`
  payment row + set order `payment_status='refunded'`) and an ops refund
  endpoint that calls the provider refund API. Post the reversal to the Quaestor
  ledger the same way settlement does.

### 4. Internal recurring runner produces unpaid children (for the orders it *does* own)
- After the provider-subscription guard, the runner only fires recurring orders
  with **no** provider subscription (Net-30 / unpaid recurring). It still inserts
  child orders with `payment_status='pending'` and **no payment/invoice step** —
  `stripe_net30_invoice` is only called from checkout, never from the runner.
- **Fix:** have the runner initiate the per-cycle invoice/charge for the orders
  it owns (call the Net-30 invoice path for B2B, or the configured provider),
  and notify the customer.

### 5. `OrderStatus::Processing` is unreachable
- Orders default to `'placed'`; `record_fulfillment` writes `'fulfilled'`
  directly. Nothing writes `'processing'`, yet it's offered in the B2B filter
  and given a CSS class.
- **Fix:** either drop `Processing` from the enum/filter, or write it when a
  payment settles / an order is picked but not yet shipped.

### 6. (Minor) PayPal subscription order marked Paid on activation with no amount check
- `BILLING.SUBSCRIPTION.ACTIVATED` calls `settle_order(..., charged=None)`, so
  the order flips to `Paid` before any money is verified (inconsistent with the
  return path, which sets `Processing`). `settle_order` skips `charge_matches`
  when `charged` is `None`.
- **Fix:** treat activation as `Processing`/subscription-active, and only mark
  `Paid` on the first captured cycle payment with a verified amount.

---

## Migration discipline

The shared Supabase DB runs the embedded `sqlx::migrate!` migrations. **Never
edit a migration that has already been applied** — sqlx stores each migration's
SHA-384 checksum and aborts the *entire* run at the first modified one, so every
later migration silently stops applying. This bit us: `0006_payments.sql` was
edited after it was applied, so `0007` (which adds `customer_profiles.b2b_approved_at`)
and `0008` never ran → account setup broke at runtime with `column ... does not
exist`.

- To add a change: **write a new numbered migration**, never touch an old one.
- If a checksum has already drifted and the schema is confirmed correct, reconcile
  the recorded checksum (this is what unblocked us):
  ```sh
  NEW=$(shasum -a 384 migrations/000N_name.sql | awk '{print $1}')
  psql "$DATABASE_URL" -c "UPDATE _sqlx_migrations SET checksum = decode('$NEW','hex') WHERE version = N;"
  # then reboot the app; sqlx applies any pending migrations
  ```
  Only do this after verifying the migration's objects already exist in the DB.
- The migrator takes an advisory lock through the Supabase session pooler; under
  contention it can hit the pooler's ~120s `statement_timeout` (harmless when no
  migrations are pending, but noisy). Prefer running migrations on a direct
  (non-pooler) connection with a bounded `lock_timeout`.
- Go-forward: the [README](../README.md) "declarative migrations (dpm)" section
  and the shared `k8s-libs-and-shared-defs/pg-defs` contract are the target;
  moving this schema to the shared RDS (own `athleto` database) is the plan.

---

## Hardening posture already in place (don't regress)

- **CSRF**: double-submit token on every state-changing form + htmx header;
  `/api/v1` is the only exemption (bearer-auth, no ambient cookie); webhooks are
  exempt but verify provider signatures. Constant-time token compare.
- **Payment webhooks fail closed**: no signing secret ⇒ reject; HMAC over the
  **raw** body; replay-deduped via `payment_events (provider, event_id)`.
- **Host allowlist** (`ALLOWED_HOSTS`) for auth-redirect bases + biz-host chrome;
  permissive-with-warning when unset.
- **Login-flow pinning**: `athleto_login_flow` cookie must match the `flow` param
  on confirm, so a leaked magic link can't be completed in another browser.
- **B2B requires approval *then* 2FA** before ordering/API (`require_b2b_ready`).
