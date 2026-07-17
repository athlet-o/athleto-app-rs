-- Athlet-O core schema: products, carts, cart items.

CREATE TYPE product_format AS ENUM ('cup', 'powder');

CREATE TABLE products (
    id BIGSERIAL PRIMARY KEY,
    slug TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    format product_format NOT NULL,
    calories INT NOT NULL,
    protein_g INT NOT NULL,
    price_cents INT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One cart per Supabase user id (logged in) or per anonymous cart cookie uuid.
CREATE TABLE carts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID UNIQUE,
    anon_id UUID UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT carts_owner_present CHECK (user_id IS NOT NULL OR anon_id IS NOT NULL)
);

CREATE TABLE cart_items (
    id BIGSERIAL PRIMARY KEY,
    cart_id UUID NOT NULL REFERENCES carts (id) ON DELETE CASCADE,
    product_id BIGINT NOT NULL REFERENCES products (id) ON DELETE CASCADE,
    qty INT NOT NULL DEFAULT 1 CHECK (qty > 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (cart_id, product_id)
);

CREATE INDEX cart_items_cart_id_idx ON cart_items (cart_id);
