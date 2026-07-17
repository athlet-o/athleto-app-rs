-- B2B/B2C split, passwordless-auth support tables, orders, and the
-- retail-readiness item master (trade items, lots, shipments, EDI log).
--
-- The app connects as the table owner, so RLS below only constrains the
-- Supabase PostgREST surface (anon/authenticated roles): customers may read
-- their own rows; operational tables are server-only.

-- ---------------------------------------------------------------------------
-- Customer profiles: one row per Supabase auth user.
CREATE TYPE customer_type AS ENUM ('b2c', 'b2b');

CREATE TABLE customer_profiles (
    user_id UUID PRIMARY KEY,
    customer_type customer_type NOT NULL DEFAULT 'b2c',
    company_name TEXT,
    erp_contact_email TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT b2b_requires_company CHECK (customer_type <> 'b2b' OR company_name IS NOT NULL)
);

-- Successful sign-ins (source of truth behind the login page's remembered
-- emails, which are cached client-side in IndexedDB).
CREATE TABLE login_events (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL,
    email TEXT NOT NULL,
    aal TEXT NOT NULL DEFAULT 'aal1',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX login_events_user_idx ON login_events (user_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Orders. `channel` records where the order came from so retail/EDI volume
-- can coexist with D2C in one table.
CREATE TYPE order_kind AS ENUM ('one_time', 'recurring');
CREATE TYPE order_frequency AS ENUM ('weekly', 'biweekly', 'monthly', 'quarterly');
CREATE TYPE order_status AS ENUM ('placed', 'processing', 'fulfilled', 'cancelled');
CREATE TYPE order_channel AS ENUM ('d2c_web', 'b2b_portal', 'b2b_api', 'edi');

CREATE TABLE orders (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL,
    kind order_kind NOT NULL DEFAULT 'one_time',
    frequency order_frequency,
    status order_status NOT NULL DEFAULT 'placed',
    channel order_channel NOT NULL DEFAULT 'd2c_web',
    po_number TEXT,
    total_cents BIGINT NOT NULL DEFAULT 0,
    next_run_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT recurring_has_frequency CHECK (kind <> 'recurring' OR frequency IS NOT NULL)
);

CREATE INDEX orders_user_idx ON orders (user_id, created_at DESC);

CREATE TABLE order_items (
    id BIGSERIAL PRIMARY KEY,
    order_id UUID NOT NULL REFERENCES orders (id) ON DELETE CASCADE,
    product_id BIGINT NOT NULL REFERENCES products (id),
    qty INT NOT NULL CHECK (qty > 0),
    unit_price_cents INT NOT NULL,
    lot_id BIGINT
);

CREATE INDEX order_items_order_idx ON order_items (order_id);

-- ---------------------------------------------------------------------------
-- B2B API keys for ERP integrations. Only the SHA-256 hash is stored; the
-- prefix column keeps the first characters for display ("athk_1a2b3c4d...").
CREATE TABLE b2b_api_keys (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL,
    name TEXT NOT NULL,
    key_hash TEXT NOT NULL UNIQUE,
    prefix TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ
);

CREATE INDEX b2b_api_keys_user_idx ON b2b_api_keys (user_id);

-- ---------------------------------------------------------------------------
-- Inventory + 90-minute cart holds. A hold is business data, not a lock: a
-- row with an expiry, claimed under a milliseconds-long row lock on the
-- inventory row. Availability = on_hand - SUM(active holds); expiry is lazy
-- (queries ignore stale rows) and the in-process sweeper is hygiene only.
CREATE TABLE inventory (
    product_id BIGINT PRIMARY KEY REFERENCES products (id) ON DELETE CASCADE,
    on_hand INT NOT NULL DEFAULT 0 CHECK (on_hand >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO inventory (product_id, on_hand)
SELECT id, 500 FROM products
ON CONFLICT (product_id) DO NOTHING;

CREATE TABLE stock_holds (
    id BIGSERIAL PRIMARY KEY,
    cart_id UUID NOT NULL REFERENCES carts (id) ON DELETE CASCADE,
    product_id BIGINT NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    qty INT NOT NULL CHECK (qty > 0),
    held_until TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (cart_id, product_id)
);

CREATE INDEX stock_holds_active_idx ON stock_holds (product_id, held_until);

-- ---------------------------------------------------------------------------
-- Item master: retail trade-item hierarchy. Each sellable product has one row
-- per packaging level (each -> case -> pallet); GTINs stay NULL until a GS1
-- company prefix is licensed. `ti`/`hi` (cases per layer / layers per pallet)
-- live on the case level.
CREATE TYPE trade_item_level AS ENUM ('each', 'inner', 'case', 'pallet');

CREATE TABLE trade_items (
    id BIGSERIAL PRIMARY KEY,
    product_id BIGINT NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    level trade_item_level NOT NULL,
    gtin TEXT UNIQUE,
    qty_of_child INT,
    net_weight_g INT,
    gross_weight_g INT,
    length_mm INT,
    width_mm INT,
    height_mm INT,
    ti INT,
    hi INT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (product_id, level)
);

-- Lot/expiry tracking from day one: a recall without lot traceability is
-- existential for a food brand. qty_available is decremented at allocation.
CREATE TABLE lots (
    id BIGSERIAL PRIMARY KEY,
    product_id BIGINT NOT NULL REFERENCES products (id),
    lot_number TEXT NOT NULL,
    produced_at DATE,
    expiry_date DATE,
    qty_produced INT NOT NULL DEFAULT 0,
    qty_available INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (product_id, lot_number)
);

ALTER TABLE order_items
    ADD CONSTRAINT order_items_lot_fk FOREIGN KEY (lot_id) REFERENCES lots (id);

-- Shipments and GS1-128 carton/pallet labels. `sscc` rows are what an 856 ASN
-- serializes; retailers reconcile received SSCCs against the ASN.
CREATE TYPE shipment_status AS ENUM ('packing', 'shipped', 'delivered');

CREATE TABLE shipments (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id UUID NOT NULL REFERENCES orders (id),
    status shipment_status NOT NULL DEFAULT 'packing',
    carrier TEXT,
    tracking_number TEXT,
    ship_date DATE,
    asn_sent_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE shipment_cartons (
    id BIGSERIAL PRIMARY KEY,
    shipment_id UUID NOT NULL REFERENCES shipments (id) ON DELETE CASCADE,
    sscc TEXT UNIQUE,
    trade_item_id BIGINT REFERENCES trade_items (id),
    lot_id BIGINT REFERENCES lots (id),
    qty INT NOT NULL CHECK (qty > 0)
);

-- Raw EDI documents in/out (850, 855, 856, 810, 846, 820), stored as the
-- provider's JSON mapping. Orders created from an 850 link back via order_id.
CREATE TABLE edi_messages (
    id BIGSERIAL PRIMARY KEY,
    direction TEXT NOT NULL CHECK (direction IN ('in', 'out')),
    doc_type TEXT NOT NULL,
    partner TEXT,
    order_id UUID REFERENCES orders (id),
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    status TEXT NOT NULL DEFAULT 'received',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------------------
-- Seed the trade-item hierarchy for the existing catalog with placeholder
-- (NULL-GTIN) rows: cups case-pack 12, powders case-pack 24, plus an each row.
INSERT INTO trade_items (product_id, level, qty_of_child)
SELECT id, 'each'::trade_item_level, NULL FROM products
ON CONFLICT (product_id, level) DO NOTHING;

INSERT INTO trade_items (product_id, level, qty_of_child)
SELECT id, 'case'::trade_item_level, CASE WHEN format = 'cup' THEN 12 ELSE 24 END
FROM products
ON CONFLICT (product_id, level) DO NOTHING;

-- ---------------------------------------------------------------------------
-- RLS: customers can read their own commerce rows through PostgREST; writes
-- and operational tables stay server-only (no policies = denied).
ALTER TABLE customer_profiles ENABLE ROW LEVEL SECURITY;
ALTER TABLE login_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE orders ENABLE ROW LEVEL SECURITY;
ALTER TABLE order_items ENABLE ROW LEVEL SECURITY;
ALTER TABLE b2b_api_keys ENABLE ROW LEVEL SECURITY;
ALTER TABLE trade_items ENABLE ROW LEVEL SECURITY;
ALTER TABLE lots ENABLE ROW LEVEL SECURITY;
ALTER TABLE shipments ENABLE ROW LEVEL SECURITY;
ALTER TABLE shipment_cartons ENABLE ROW LEVEL SECURITY;
ALTER TABLE edi_messages ENABLE ROW LEVEL SECURITY;

CREATE POLICY customer_profiles_self_read ON customer_profiles
    FOR SELECT USING (auth.uid() = user_id);
CREATE POLICY login_events_self_read ON login_events
    FOR SELECT USING (auth.uid() = user_id);
CREATE POLICY orders_self_read ON orders
    FOR SELECT USING (auth.uid() = user_id);
CREATE POLICY order_items_self_read ON order_items
    FOR SELECT USING (EXISTS (
        SELECT 1 FROM orders o WHERE o.id = order_id AND o.user_id = auth.uid()
    ));
