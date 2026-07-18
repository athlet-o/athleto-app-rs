-- Security hardening: approved B2B terms. Product/cart RLS is established
-- in 0008_rls_products_carts.sql and login throttling is handled by the
-- application security middleware, so this migration intentionally owns only
-- the approval state introduced after those controls shipped.

ALTER TABLE customer_profiles
    ADD COLUMN b2b_approved_at TIMESTAMPTZ;

CREATE INDEX customer_profiles_b2b_approval_idx
    ON customer_profiles (b2b_approved_at)
    WHERE b2b_approved_at IS NOT NULL;
