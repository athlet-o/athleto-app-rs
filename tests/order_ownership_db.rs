//! DB-backed proof that order-scoped reads enforce ownership in SQL (ignored
//! by default; needs a real DATABASE_URL).
//!
//! Because the app connects as the table owner and bypasses RLS, the
//! `user_id` predicate inside `order_items` / `shipments_for_order` is the
//! authorization boundary — not the caller's own check. This test locks that
//! in: the owner sees the row, a different user sees nothing.
//!
//!   DATABASE_URL=... cargo test --test order_ownership_db -- --ignored --nocapture

use athleto_app_rs::db;
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use uuid::Uuid;

fn stmt(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

#[tokio::test]
#[ignore]
async fn order_items_and_shipments_are_scoped_to_the_owner() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let conn = db::build_pool(&url).await.expect("pool");

    let owner = Uuid::new_v4();
    let attacker = Uuid::new_v4();
    let order_id = Uuid::new_v4();

    // Seed one order owned by `owner`, with a line item and a shipment.
    // product 1 is assumed present from the base catalog seed.
    conn.execute(stmt(
        "INSERT INTO orders (id, user_id, total_cents) VALUES ($1, $2, 1000)",
        vec![order_id.into(), owner.into()],
    ))
    .await
    .expect("seed order");
    conn.execute(stmt(
        "INSERT INTO order_items (order_id, product_id, qty, unit_price_cents) \
         VALUES ($1, 1, 2, 500)",
        vec![order_id.into()],
    ))
    .await
    .expect("seed order item");
    conn.execute(stmt(
        "INSERT INTO shipments (order_id, carrier, status) \
         VALUES ($1, 'ups', 'pending')",
        vec![order_id.into()],
    ))
    .await
    .expect("seed shipment");

    // The owner sees the line item and the shipment.
    let owner_items = db::order_items(&conn, owner, order_id).await.expect("items");
    assert_eq!(owner_items.len(), 1, "owner must see their own line item");
    let owner_ships = db::shipments_for_order(&conn, owner, order_id)
        .await
        .expect("shipments");
    assert_eq!(owner_ships.len(), 1, "owner must see their own shipment");

    // A different user, passing the SAME order id, sees nothing — this is the
    // IDOR that the in-query predicate closes.
    let attacker_items = db::order_items(&conn, attacker, order_id)
        .await
        .expect("items");
    assert!(
        attacker_items.is_empty(),
        "another user must not read this order's line items"
    );
    let attacker_ships = db::shipments_for_order(&conn, attacker, order_id)
        .await
        .expect("shipments");
    assert!(
        attacker_ships.is_empty(),
        "another user must not read this order's shipments"
    );

    // Cleanup (order_items cascades; shipments do not necessarily).
    let _ = conn
        .execute(stmt(
            "DELETE FROM shipments WHERE order_id = $1",
            vec![order_id.into()],
        ))
        .await;
    let _ = conn
        .execute(stmt(
            "DELETE FROM orders WHERE id = $1",
            vec![order_id.into()],
        ))
        .await;
}
