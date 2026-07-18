//! Settled or pending provider money movements. Provider reference uniqueness
//! makes browser-return and webhook handling safely idempotent.

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
