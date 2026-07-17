//! `stock_holds` -- 90-minute cart reservations. A hold is business data
//! with an expiry, never a lock; expired rows are ignored (lazy expiry).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "stock_holds")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub cart_id: Uuid,
    pub product_id: i64,
    pub qty: i32,
    pub held_until: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
