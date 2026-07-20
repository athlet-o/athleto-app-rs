# Cart holds: 90-minute reservations as data, not locks

A hold is a **row with an expiry**, never a long-lived lock. The only lock the
system ever takes is a row-level `FOR UPDATE` on one `inventory` row, held for
milliseconds at claim time. This works through connection poolers (including
Supabase's transaction pooler), survives restarts, and keeps holds queryable
("who holds what until when") — none of which is true of advisory locks or an
external lease service.

## State machine

```
                    add to cart                     checkout
  available ───────────────────────► held ─────────────────────► sold
      ▲                               │        (same txn as the
      │        held_until < now()     │         order insert)
      └───────────────────────────────┘
           lazy expiry: expired holds are
           simply ignored by every query
```

- **available → held**: `db::ensure_hold` — lock the inventory row, compute
  `available = on_hand − SUM(other carts' unexpired holds)`, insert/refresh
  this cart's hold with `held_until = now() + 90 minutes`. Zero available →
  the add is refused with the remaining count.
- **held → sold**: `db::place_order` — re-checks availability line by line
  (inventory rows locked in product-id order to avoid deadlocks), decrements
  `on_hand`, deletes the cart's holds, and inserts the order + items, all in
  one transaction. The hold and the sale can never disagree.
- **held → available**: nothing happens at expiry time. Every availability
  check filters `held_until > now()`, so an expired hold is free stock the
  instant the clock passes it. The in-process sweeper deletes stale rows every
  15 minutes purely as hygiene.

Notes on edge cases:

- **Expired hold, stock still free**: checkout succeeds anyway — the re-check
  claims the stock directly. The hold is a promise of priority, not a
  precondition.
- **Expired hold, stock taken by someone else**: checkout fails cleanly with
  per-line shortages (web: banner on /cart; API: HTTP 409 with a `shortages`
  array).
- **Payment webhooks** (future): when a payment provider is added, the
  `held → sold` transition must move into the payment-confirmation handler and
  be guarded by `WHERE held_until > now()` on the holds so a webhook that
  lands after expiry either wins the re-check or fails loudly — never
  silently oversells.

## Client-visible lease semantics

- The cart page renders a countdown banner seeded from
  `MIN(held_until)` across the cart's holds.
- The browser re-syncs against `GET /cart/hold`
  (`{"active": bool, "seconds_left": n}`) at random 25–55 s intervals —
  lease-poll semantics without a lock service.
- Any cart mutation refreshes the 90-minute window; polling does not (a
  parked tab can't hold stock forever).

## Where a coordination service (fiducia.cloud) fits

Not in the hold path. Its legitimate roles here, when the app grows past one
replica: leader election for the sweeper and recurring-order runner, and
admission control (rate limiting) in front of Postgres for flash-sale spikes.
Seconds-long leases — the tool's sweet spot.

Fiducia's own RFC 0001 (`fiducia-node.rs/docs/rfcs/rfc-0001-reservations.md`,
still Draft — no `/v1/reservations` routes exist in the node) argues the same
conclusion from the other side, and is worth quoting because fiducia *will*
accept a 90-minute lock TTL and its validator has a test blessing exactly that
shape:

> Today they either misuse a lock (90-minute TTL on a mutex: no capacity
> semantics, no listing, holder death releases a hold the customer still
> believes they have) … Crash of the *claiming process* must NOT release the
> reservation … That single difference (no liveness coupling) is why this cannot
> be a mode on locks.

So "fiducia accepts it" is not "fiducia is the right home for it". For this
single-database app the `held_until` column remains the right answer, and it
would remain the right answer even after the reservations primitive lands —
until inventory is claimed by more than one service.

Where a lease *does* belong in the order path is mutual exclusion around
**external, non-transactional side effects** — creating a provider checkout
session or capturing a payment — because a Postgres transaction cannot roll
those back. See `known-gaps-and-hardening.md` §6b.
