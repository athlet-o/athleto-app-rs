-- Close the RLS gap left by 0004: products, carts, and cart_items never had
-- row-level security enabled, so if the Supabase PostgREST surface is reachable
-- with the standard anon/authenticated grants, an authenticated user's JWT
-- could PATCH products.price_cents (poisoning the authoritative price every
-- server path trusts -> $0 orders) or read/tamper with other customers' carts.
-- The app itself connects as the table owner and bypasses RLS, so enabling it
-- here changes nothing for the server; it only fences off direct PostgREST
-- access, matching the model the 0004 RLS block already established.

-- The catalog is public information (the storefront shows it), so it stays
-- readable through PostgREST -- but there is no write policy, so anon and
-- authenticated roles can never mutate prices or products.
ALTER TABLE products ENABLE ROW LEVEL SECURITY;
CREATE POLICY products_public_read ON products
    FOR SELECT USING (true);

-- Carts and their items are session-scoped and managed only by the server
-- (anonymous carts are keyed by an opaque cookie id, authenticated carts by
-- user_id). No policy is defined, so PostgREST's anon/authenticated roles are
-- denied entirely while the owner-role server connection continues to manage
-- them normally.
ALTER TABLE carts ENABLE ROW LEVEL SECURITY;
ALTER TABLE cart_items ENABLE ROW LEVEL SECURITY;
