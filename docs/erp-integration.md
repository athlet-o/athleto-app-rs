# ERP / EDI integration: how retail order flow maps onto this app

Big-retail customers (Walmart, Target, distributors like UNFI) don't email
POs — they exchange ANSI X12 documents through an EDI provider (SPS Commerce,
TrueCommerce, Stedi). This app's schema is already shaped for that flow; the
provider integration itself is added when the first real retail PO exists.

## The tables involved

| Concern | Table(s) | Notes |
| --- | --- | --- |
| Item master | `products`, `trade_items` | one `trade_items` row per packaging level (each/case/pallet); GTINs stay NULL until a GS1 prefix is licensed; Ti-Hi on the case row |
| Lot traceability | `lots`, `order_items.lot_id` | lot + expiry from day one; a recall without lot tracking is existential |
| Orders | `orders` (`channel`: d2c_web, b2b_portal, b2b_api, edi), `order_items` | one table for D2C and retail volume |
| Shipping / ASN | `shipments`, `shipment_cartons` (SSCC per carton) | what an 856 serializes; retailers reconcile received SSCCs against the ASN |
| Raw documents | `edi_messages` (direction, doc_type, payload jsonb, order_id) | audit trail of every 850/855/856/810/846/820 in or out |

## Inbound: 850 purchase order → order row

```
Retailer ──850──► EDI provider ──webhook (JSON)──► POST /api/v1/orders
                                                   Authorization: Bearer athk_…
                                                   { po_number, items:[{slug|product_id, qty}] }
```

1. The EDI provider maps the X12 850 to JSON and calls the existing B2B API
   with the retailer's dedicated API key (create one per trading partner on
   /account, so `last_used_at` doubles as a partner heartbeat).
2. `place_order` runs the same stock transaction as the web path with
   `channel = 'edi'` (today the API stamps `b2b_api`; add a
   `channel` override accepted only for EDI-partner keys when the provider is
   wired).
3. Store the raw provider payload in `edi_messages (direction='in',
   doc_type='850', order_id=…)`, then acknowledge with an 855 generated from
   the order row (`status = placed` → 855 accept).

## Outbound: shipment → 856 ASN → 810 invoice

```
warehouse packs order
  └─► shipments row (carrier, ship_date)
        └─► shipment_cartons rows (one per carton/pallet: SSCC, trade_item, lot, qty)
              └─► 856 ASN job: serialize cartons → provider POST → edi_messages(out, '856')
                    └─► mark shipments.asn_sent_at
order fulfilled → 810 invoice job from orders + order_items → edi_messages(out, '810')
inventory snapshots → 846 from `inventory` on a schedule (drop-ship programs)
```

The 856 must go out **before the truck arrives** — late/mismatched ASNs are
where chargebacks come from, which is why `shipment_cartons.sscc` is UNIQUE
and references both trade item and lot: the label on the pallet, the ASN, and
the database can never disagree.

## Background jobs this implies (not yet built)

- `asn-sender`: watches `shipments` with `status='shipped' AND asn_sent_at IS
  NULL`, builds the 856, posts to the provider, records `edi_messages`.
- `invoice-sender`: same shape for 810s on fulfillment.
- `recurring-runner`: materializes the next order from `orders` rows with
  `next_run_at <= now()` (applies to both B2C subscriptions and B2B
  replenishment), re-running the stock transaction each cycle.
- All are single-runner jobs: in-process while the app is one replica; behind
  leader election (fiducia) when it isn't.

## Sequencing for the business (from the retail-readiness review)

1. Regulatory classification per SKU (food vs supplement panel) — gates
   retailer item setup.
2. GS1 company prefix → fill `trade_items.gtin` for each/case; define Ti-Hi.
3. Co-packer with GFSI cert; lot numbers flow into `lots`.
4. Land UNFI/regional first (friendlier emerging-brand path), then
   Target/Walmart direct; add the EDI provider when the first big PO is real.
5. Keep accounting out of Postgres: nightly push of invoices/orders to the
   ledger via its API; this DB stays the operational truth.
