//! The provider callback idempotency ledger. The composite primary key is the
//! authority for whether a callback has already been applied.

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
