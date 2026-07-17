//! `products` -- the sellable catalog. `Model` doubles as the app-wide
//! `db::Product` struct (the built-in fallback catalog constructs it too).

use sea_orm::entity::prelude::*;

use crate::db::ProductFormat;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "products")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i64,
    pub slug: String,
    pub name: String,
    /// Line sub-name rendered under the AthletO wordmark
    /// ("starter" / "recover" / "pre-game"). Nullable in the database until
    /// the rebrand migration has run, hence the Option.
    pub subname: Option<String>,
    pub description: String,
    pub format: ProductFormat,
    pub calories: i32,
    pub protein_g: i32,
    pub price_cents: i32,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
