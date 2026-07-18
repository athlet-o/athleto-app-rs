//! `cart_items` -- one row per (cart, product), qty accumulated on conflict.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "cart_items")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub cart_id: Uuid,
    pub product_id: i64,
    pub qty: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
