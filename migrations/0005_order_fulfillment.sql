-- Order management, receipts, delivery estimates, and shipment tracking.
--
-- Receipts need a subtotal/shipping/tax breakdown, so `total_cents` becomes
-- subtotal + shipping + tax at creation. Delivery estimates are computed in
-- the app from `ship_method` + `created_at` (no stored ETA to drift). Tracking
-- lives on the existing `shipments` table, populated by ops/EDI fulfillment.

CREATE TYPE ship_method AS ENUM ('standard', 'expedited', 'freight');

ALTER TABLE orders
    ADD COLUMN ship_method ship_method NOT NULL DEFAULT 'standard',
    ADD COLUMN subtotal_cents BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN shipping_cents BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN tax_cents BIGINT NOT NULL DEFAULT 0;

-- Backfill: existing orders' total was the line subtotal, shipping/tax zero.
UPDATE orders SET subtotal_cents = total_cents WHERE subtotal_cents = 0;

-- A shipment gets an estimated-delivery window and (once delivered) a
-- delivered timestamp so tracking UIs can show "delivered" vs "in transit".
ALTER TABLE shipments
    ADD COLUMN eta_earliest DATE,
    ADD COLUMN eta_latest DATE,
    ADD COLUMN delivered_at TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS shipments_order_idx ON shipments (order_id);
