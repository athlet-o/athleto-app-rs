//! `order_items` -- per-order lines with the unit price frozen at purchase.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "order_items")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub order_id: Uuid,
    pub product_id: i64,
    pub qty: i32,
    pub unit_price_cents: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
