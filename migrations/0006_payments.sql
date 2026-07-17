-- Online payment acceptance: Stripe (cards + ACH + hosted Net-30 invoices),
-- PayPal (orders + subscriptions), Square (hosted checkout + subscription
-- plans), plus the provider-agnostic bookkeeping the app needs before the
-- Quaestor billing-server (the AR/AP ledger) takes over reconciliation.
--
-- 'invoice' is the B2B open-account path: the order ships against a PO and
-- the customer pays a hosted Stripe invoice on Net-30 terms.

CREATE TYPE payment_provider AS ENUM ('stripe', 'paypal', 'square', 'invoice');
CREATE TYPE payment_status AS ENUM ('pending', 'processing', 'paid', 'invoiced', 'failed', 'refunded');
CREATE TYPE payment_kind AS ENUM ('charge', 'subscription_cycle', 'refund');
CREATE TYPE subscription_status AS ENUM ('pending', 'active', 'past_due', 'cancelled');

-- Orders learn how (and whether) they were paid. payment_ref holds the
-- provider's checkout handle (Stripe session id, PayPal order/subscription
-- id, Square payment-link order id, or Stripe invoice id for Net-30).
ALTER TABLE orders
    ADD COLUMN payment_provider payment_provider,
    ADD COLUMN payment_status payment_status NOT NULL DEFAULT 'pending',
    ADD COLUMN payment_ref TEXT,
    ADD COLUMN paid_at TIMESTAMPTZ;

-- One row per money movement reported by a provider. (provider, provider_ref)
-- dedupes replayed webhooks and return-URL races.
CREATE TABLE payments (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    order_id UUID REFERENCES orders(id) ON DELETE SET NULL,
    user_id UUID NOT NULL,
    provider payment_provider NOT NULL,
    kind payment_kind NOT NULL DEFAULT 'charge',
    provider_ref TEXT NOT NULL,
    amount_cents BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'USD',
    status payment_status NOT NULL DEFAULT 'paid',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider, provider_ref)
);
CREATE INDEX payments_order_idx ON payments (order_id);
CREATE INDEX payments_user_idx ON payments (user_id);

-- Provider-billed recurring orders. The in-app recurring runner still
-- materializes fulfillment orders on schedule; the provider subscription is
-- the billing source of truth and its cycle webhooks land in payments.
CREATE TABLE payment_subscriptions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL,
    order_id UUID REFERENCES orders(id) ON DELETE SET NULL,
    provider payment_provider NOT NULL,
    provider_ref TEXT NOT NULL,
    status subscription_status NOT NULL DEFAULT 'pending',
    frequency order_frequency NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (provider, provider_ref)
);
CREATE INDEX payment_subscriptions_user_idx ON payment_subscriptions (user_id);

-- Webhook idempotency: first insert of (provider, event_id) wins; handlers
-- skip events whose insert conflicts.
CREATE TABLE payment_events (
    provider payment_provider NOT NULL,
    event_id TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (provider, event_id)
);

-- Server-only tables (same posture as orders/shipments in 0004/0005): no
-- PostgREST exposure beyond RLS-off default deny for anon/authenticated.
ALTER TABLE payments ENABLE ROW LEVEL SECURITY;
ALTER TABLE payment_subscriptions ENABLE ROW LEVEL SECURITY;
ALTER TABLE payment_events ENABLE ROW LEVEL SECURITY;
