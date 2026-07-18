//! SeaORM entity modules for the core commerce tables. Written by hand
//! against the embedded SQL migrations (which stay the schema's source of
//! truth); columns the app never reads (created_at defaults, ERP extras) are
//! deliberately omitted from the models.
//!
//! The Postgres enum types map onto the ActiveEnums defined in `crate::db`
//! (ProductFormat, CustomerType, OrderKind, ...), keeping those types usable
//! across the app exactly as before the SeaORM port.

pub mod b2b_api_key;
pub mod cart;
pub mod cart_item;
pub mod customer_profile;
pub mod inventory;
pub mod login_event;
pub mod order;
pub mod order_item;
pub mod payment;
pub mod payment_event;
pub mod payment_subscription;
pub mod product;
pub mod stock_hold;
