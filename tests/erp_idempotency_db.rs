//! Real PostgreSQL ERP replay and concurrent-first-use test. No external
//! tenant credentials: CI starts an ephemeral postgres:16 service and applies
//! only the repository's committed migrations.
use athleto_app_rs::db::{self, NewOrderLine, OrderChannel, OrderError, OrderKind, ShipMethod};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use uuid::Uuid;

fn stmt(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}
async fn stock(conn: &sea_orm::DatabaseConnection, pid: i64) -> i32 {
    conn.query_one(stmt(
        "SELECT on_hand FROM inventory WHERE product_id=$1",
        vec![pid.into()],
    ))
    .await
    .unwrap()
    .unwrap()
    .try_get("", "on_hand")
    .unwrap()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL and migrated disposable PostgreSQL database"]
async fn parallel_erp_retries_commit_one_order_and_one_stock_decrement() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let a = db::build_pool(&url).await.expect("a");
    let b = db::build_pool(&url).await.expect("b");
    let user = Uuid::new_v4();
    let slug = format!("erp-test-{}", Uuid::new_v4().simple());
    let pid:i64=a.query_one(stmt(
        "INSERT INTO products (slug,name,description,format,calories,protein_g,price_cents) VALUES ($1,'ERP race','test','cup',100,20,500) RETURNING id",
        vec![slug.into()])).await.unwrap().unwrap().try_get("","id").unwrap();
    a.execute(stmt("INSERT INTO inventory(product_id,on_hand) VALUES ($1,10)",
        vec![pid.into()])).await.unwrap();

    let lines=[NewOrderLine{product_id:pid,qty:2,unit_price_cents:500}];
    let key=format!("erp-race-{}",Uuid::new_v4().simple());
    let digest="a".repeat(64);
    let (left,right)=tokio::join!(
        db::place_order_with_claim(&a,user,OrderKind::OneTime,None,
            OrderChannel::B2bApi,ShipMethod::Freight,Some("PO-ABC"),&lines,None,
            Some((&key,&digest))),
        db::place_order_with_claim(&b,user,OrderKind::OneTime,None,
            OrderChannel::B2bApi,ShipMethod::Freight,Some("PO-ABC"),&lines,None,
            Some((&key,&digest)))
    );
    let order=left.expect("first admitted");
    assert_eq!(right.expect("concurrent identical replay"),order);
    assert_eq!(stock(&a,pid).await,8,"exactly one stock decrement");

    let receipt=db::erp_order_receipt(&b,user,&key,&digest)
        .await.unwrap().expect("committed receipt");
    assert_eq!(receipt.order_id,order);
    assert_eq!(receipt.po_number.as_deref(),Some("PO-ABC"));
    assert_eq!(receipt.total_cents,1000+ShipMethod::Freight.shipping_cents());

    // Mutating catalog prices cannot change a committed receipt or charge.
    a.execute(stmt("UPDATE products SET price_cents=999 WHERE id=$1",vec![pid.into()]))
        .await.unwrap();
    let stable=db::erp_order_receipt(&a,user,&key,&digest).await.unwrap().unwrap();
    assert_eq!(receipt,stable);

    let conflict=db::erp_order_receipt(&a,user,&key,&"b".repeat(64)).await;
    assert!(matches!(conflict,Err(OrderError::IdempotencyConflict)));
    let conflicting=db::place_order_with_claim(&a,user,OrderKind::OneTime,None,
        OrderChannel::B2bApi,ShipMethod::Freight,Some("PO-OTHER"),&lines,None,
        Some((&key,&"b".repeat(64)))).await;
    assert!(matches!(conflicting,Err(OrderError::IdempotencyConflict)));
    assert_eq!(stock(&a,pid).await,8);

    // No stock => failed first-use transaction must roll back its claim.
    let bad_key=format!("erp-fail-{}",Uuid::new_v4().simple());
    let bad_lines=[NewOrderLine{product_id:pid,qty:99,unit_price_cents:999}];
    let result=db::place_order_with_claim(&a,user,OrderKind::OneTime,None,
        OrderChannel::B2bApi,ShipMethod::Freight,None,&bad_lines,None,
        Some((&bad_key,&digest))).await;
    assert!(matches!(result,Err(OrderError::Insufficient(_))));
    assert!(db::erp_order_receipt(&a,user,&bad_key,&digest).await.unwrap().is_none());

    // Verify from independent pool after an end-of-request simulated restart.
    let c=db::build_pool(&url).await.expect("restart pool");
    assert_eq!(db::erp_order_receipt(&c,user,&key,&digest)
        .await.unwrap().unwrap().order_id,order);
    let count:i64=c.query_one(stmt(
        "SELECT count(*)::bigint AS n FROM orders WHERE user_id=$1",vec![user.into()]))
        .await.unwrap().unwrap().try_get("","n").unwrap();
    assert_eq!(count,1);

    c.execute(stmt("DELETE FROM order_idempotency WHERE user_id=$1",vec![user.into()]))
        .await.unwrap();
    c.execute(stmt("DELETE FROM orders WHERE user_id=$1",vec![user.into()]))
        .await.unwrap();
    c.execute(stmt("DELETE FROM inventory WHERE product_id=$1",vec![pid.into()]))
        .await.unwrap();
    c.execute(stmt("DELETE FROM products WHERE id=$1",vec![pid.into()]))
        .await.unwrap();
}
