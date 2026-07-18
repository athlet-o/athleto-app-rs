//! `inventory` -- on-hand stock per product; absence of a row means the
//! product's stock is untracked (unlimited).

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "inventory")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub product_id: i64,
    pub on_hand: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
