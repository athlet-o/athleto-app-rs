//! Provider-billed recurring orders (Stripe subscriptions, PayPal I- ids, or
//! Square subscriptions). Fulfilment cadence remains in the app's order runner.

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
