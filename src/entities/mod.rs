//! SeaORM entity modules for the core commerce tables. Written by hand against
//! the declarative `k8s-cluster/remote/libs/pg-defs` Athlet-O database
//! contract; columns the app never reads are deliberately omitted.
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
