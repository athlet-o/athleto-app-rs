//! SeaORM entities — the data-access convention for this app going forward.
//!
//! All new tables (payments, payment_subscriptions, payment_events) are
//! accessed exclusively through these entities; `orders` has a partial
//! entity covering the columns the payment flow touches. The handwritten
//! sqlx queries in `db.rs` predate this and are ported incrementally (the
//! Quaestor billing-server followed the same sqlx -> SeaORM path). Both run
//! over the same Postgres pool: `AppState::orm` wraps `AppState::pool`.
//!
//! Postgres enum types live in `db.rs` with dual derives (`sqlx::Type` +
//! `DeriveActiveEnum`) so both layers share one Rust type per enum.

/// Partial view of `orders` (see 0004/0005/0006 migrations): only the
/// columns the payment lifecycle reads and writes. SeaORM always issues
/// explicit column lists, so the missing columns are never touched.
pub mod order {
    use sea_orm::entity::prelude::*;

    use crate::db::{OrderFrequency, OrderKind, PaymentProvider, PaymentStatus};

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "orders")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub user_id: Uuid,
        pub kind: OrderKind,
        pub frequency: Option<OrderFrequency>,
        pub total_cents: i64,
        pub payment_provider: Option<PaymentProvider>,
        pub payment_status: PaymentStatus,
        pub payment_ref: Option<String>,
        pub paid_at: Option<DateTimeWithTimeZone>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// One row per money movement reported by a provider.
pub mod payment {
    use sea_orm::entity::prelude::*;

    use crate::db::{PaymentKind, PaymentProvider, PaymentStatus};

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "payments")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub order_id: Option<Uuid>,
        pub user_id: Uuid,
        pub provider: PaymentProvider,
        pub kind: PaymentKind,
        pub provider_ref: String,
        pub amount_cents: i64,
        pub currency: String,
        pub status: PaymentStatus,
        pub created_at: DateTimeWithTimeZone,
        pub updated_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Provider-billed recurring orders (Stripe subscription, PayPal I-…,
/// Square subscription).
pub mod payment_subscription {
    use sea_orm::entity::prelude::*;

    use crate::db::{OrderFrequency, PaymentProvider, SubscriptionStatus};

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "payment_subscriptions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub user_id: Uuid,
        pub order_id: Option<Uuid>,
        pub provider: PaymentProvider,
        pub provider_ref: String,
        pub status: SubscriptionStatus,
        pub frequency: OrderFrequency,
        pub created_at: DateTimeWithTimeZone,
        pub updated_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

/// Webhook idempotency ledger: composite PK (provider, event_id); first
/// insert wins.
pub mod payment_event {
    use sea_orm::entity::prelude::*;

    use crate::db::PaymentProvider;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "payment_events")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub provider: PaymentProvider,
        #[sea_orm(primary_key, auto_increment = false)]
        pub event_id: String,
        pub payload: Json,
        pub received_at: DateTimeWithTimeZone,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}
