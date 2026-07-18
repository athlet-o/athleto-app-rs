//! `orders` -- D2C and B2B orders across every channel (web, portal, API,
//! EDI). Inserted by the raw `place_order` transaction, read via SeaORM.

use sea_orm::entity::prelude::*;

use crate::db::{
    OrderChannel, OrderFrequency, OrderKind, OrderStatus, PaymentProvider, PaymentStatus,
    ShipMethod,
};

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "orders")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub user_id: Uuid,
    pub kind: OrderKind,
    pub frequency: Option<OrderFrequency>,
    pub status: OrderStatus,
    pub channel: OrderChannel,
    pub ship_method: ShipMethod,
    pub po_number: Option<String>,
    pub subtotal_cents: i64,
    pub shipping_cents: i64,
    pub tax_cents: i64,
    pub total_cents: i64,
    pub next_run_at: Option<DateTimeUtc>,
    pub created_at: DateTimeUtc,
    pub payment_provider: Option<PaymentProvider>,
    pub payment_status: PaymentStatus,
    pub payment_ref: Option<String>,
    pub paid_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
