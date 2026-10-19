-- ERP/EDI requests are cartless. An order and its durable replay identity
-- must commit in the same transaction as stock decrement.
CREATE TABLE order_idempotency (
    user_id UUID NOT NULL,
    request_key TEXT NOT NULL CHECK(length(request_key) BETWEEN 1 AND 128),
    request_digest TEXT NOT NULL CHECK(request_digest ~ '^[0-9a-f]{64}$'),
    order_id UUID REFERENCES orders(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(user_id, request_key)
);
-- Unauthenticated PostgREST roles must not enumerate or forge ERP claims.
ALTER TABLE order_idempotency ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON order_idempotency FROM PUBLIC;
