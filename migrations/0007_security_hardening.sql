-- Security hardening: approved B2B terms, public Supabase exposure, and
-- shared login-rate-limit state.

ALTER TABLE customer_profiles
    ADD COLUMN b2b_approved_at TIMESTAMPTZ;

CREATE INDEX customer_profiles_b2b_approval_idx
    ON customer_profiles (b2b_approved_at)
    WHERE b2b_approved_at IS NOT NULL;

CREATE TABLE login_rate_limits (
    subject_hash TEXT PRIMARY KEY,
    window_started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts INT NOT NULL DEFAULT 1 CHECK (attempts > 0)
);

ALTER TABLE products ENABLE ROW LEVEL SECURITY;
ALTER TABLE carts ENABLE ROW LEVEL SECURITY;
ALTER TABLE cart_items ENABLE ROW LEVEL SECURITY;

REVOKE ALL ON TABLE products, carts, cart_items FROM anon, authenticated;
GRANT SELECT ON TABLE products TO anon, authenticated;

CREATE POLICY products_public_read ON products
    FOR SELECT USING (true);
