//! DB-backed test for the recurring-order runner (ignored by default; needs a
//! real DATABASE_URL). Proves the provider-subscription guard: the internal
//! runner materializes a recurring order it OWNS, but skips one that is driven
//! by a provider subscription (else every cycle would mint an unpaid orphan
//! child order and double-decrement stock).
//!
//!   DATABASE_URL=... cargo test --test recurring_runner_db -- --ignored --nocapture
use athleto_app_rs::db;
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use uuid::Uuid;

fn stmt(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

#[tokio::test]
#[ignore]
async fn runner_fires_owned_recurring_but_skips_provider_subscription() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let conn = db::build_pool(&url).expect("pool");
    let user = Uuid::new_v4();

    // Two recurring orders, both due a day ago, product 3, weekly.
    let mk = |channel: &str| {
        stmt(
            "INSERT INTO orders (user_id, kind, frequency, channel, ship_method, \
                subtotal_cents, shipping_cents, tax_cents, total_cents, next_run_at) \
             VALUES ($1, 'recurring', 'weekly', $2::order_channel, 'freight', 499, 0, 0, 499, \
                     now() - interval '1 day') RETURNING id",
            vec![user.into(), channel.into()],
        )
    };
    let owned: Uuid = conn.query_one(mk("b2b_portal")).await.unwrap().unwrap().try_get("", "id").unwrap();
    let provider: Uuid = conn.query_one(mk("d2c_web")).await.unwrap().unwrap().try_get("", "id").unwrap();
    for oid in [owned, provider] {
        conn.execute(stmt(
            "INSERT INTO order_items (order_id, product_id, qty, unit_price_cents) VALUES ($1, 3, 1, 499)",
            vec![oid.into()],
        )).await.unwrap();
    }
    // Only `provider` is backed by a provider subscription.
    conn.execute(stmt(
        "INSERT INTO payment_subscriptions (user_id, order_id, provider, frequency, status) \
         VALUES ($1, $2, 'stripe', 'weekly', 'active')",
        vec![user.into(), provider.into()],
    )).await.unwrap();

    let due_before = |id: Uuid| stmt("SELECT next_run_at <= now() AS due FROM orders WHERE id = $1", vec![id.into()]);
    assert!(get_bool(&conn, due_before(owned)).await, "owned is due before");
    assert!(get_bool(&conn, due_before(provider)).await, "provider is due before");

    let created = db::run_due_recurring_orders(&conn).await.expect("runner ok");
    println!("runner created {created} child order(s)");

    // Owned order fired: it has a child and its cursor advanced to the future.
    let owned_children = count(&conn, owned).await;
    let provider_children = count(&conn, provider).await;
    println!("children -> owned: {owned_children}, provider: {provider_children}");
    assert_eq!(owned_children, 1, "owned recurring order should fire once");
    assert_eq!(provider_children, 0, "provider-managed subscription must NOT be fired internally");
    assert!(!get_bool(&conn, due_before(owned)).await, "owned cursor advanced");
    assert!(get_bool(&conn, due_before(provider)).await, "provider cursor untouched (still 'due', ignored)");

    // Cleanup.
    conn.execute(stmt("DELETE FROM orders WHERE user_id = $1 OR recurs_from IN (SELECT id FROM orders WHERE user_id = $1)", vec![user.into()])).await.ok();
    conn.execute(stmt("DELETE FROM payment_subscriptions WHERE user_id = $1", vec![user.into()])).await.ok();
    conn.execute(stmt("DELETE FROM orders WHERE user_id = $1", vec![user.into()])).await.ok();
}

async fn get_bool(conn: &sea_orm::DatabaseConnection, s: Statement) -> bool {
    conn.query_one(s).await.unwrap().unwrap().try_get::<bool>("", "due").unwrap()
}

async fn count(conn: &sea_orm::DatabaseConnection, parent: Uuid) -> i64 {
    conn.query_one(stmt(
        "SELECT count(*)::bigint AS n FROM orders WHERE recurs_from = $1",
        vec![parent.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get::<i64>("", "n")
    .unwrap()
}
