//! Pool setup plus product, cart, customer, order, and API-key queries.
//!
//! All queries are runtime `sqlx::query` / `query_as` calls (no compile-time
//! `query!` macros) so the crate builds without a live DATABASE_URL.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "product_format", rename_all = "lowercase")]
pub enum ProductFormat {
    Cup,
    Powder,
}

impl ProductFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cup => "ready cup",
            Self::Powder => "powder packet",
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Product {
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

/// A cart is owned either by a logged-in Supabase user or by an anonymous
/// cart cookie uuid.
#[derive(Debug, Clone, Copy)]
pub enum CartOwner {
    User(Uuid),
    Anon(Uuid),
}

impl CartOwner {
    fn column(&self) -> &'static str {
        match self {
            Self::User(_) => "user_id",
            Self::Anon(_) => "anon_id",
        }
    }

    fn id(&self) -> Uuid {
        match self {
            Self::User(id) | Self::Anon(id) => *id,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CartLine {
    pub item_id: i64,
    pub product_id: i64,
    pub name: String,
    pub subname: Option<String>,
    pub format: ProductFormat,
    pub calories: i32,
    pub price_cents: i32,
    pub qty: i32,
}

impl CartLine {
    pub fn line_total_cents(&self) -> i64 {
        i64::from(self.price_cents) * i64::from(self.qty)
    }
}

/// Build a lazy pool: never connects at startup, so the app boots and serves
/// pages even when the database is unreachable.
pub fn build_pool(database_url: &str) -> Option<PgPool> {
    match PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy(database_url)
    {
        Ok(pool) => Some(pool),
        Err(err) => {
            tracing::error!(error = %err, "invalid DATABASE_URL; continuing without a database");
            None
        }
    }
}

const PRODUCT_COLUMNS: &str =
    "id, slug, name, subname, description, format, calories, protein_g, price_cents";

pub async fn list_products(pool: &PgPool) -> sqlx::Result<Vec<Product>> {
    sqlx::query_as::<_, Product>(&format!(
        "SELECT {PRODUCT_COLUMNS} FROM products ORDER BY id"
    ))
    .fetch_all(pool)
    .await
}

pub async fn product_by_slug(pool: &PgPool, slug: &str) -> sqlx::Result<Option<Product>> {
    sqlx::query_as::<_, Product>(&format!(
        "SELECT {PRODUCT_COLUMNS} FROM products WHERE slug = $1"
    ))
    .bind(slug)
    .fetch_optional(pool)
    .await
}

/// Find the owner's cart id if one exists.
pub async fn find_cart(pool: &PgPool, owner: &CartOwner) -> sqlx::Result<Option<Uuid>> {
    let sql = format!("SELECT id FROM carts WHERE {} = $1", owner.column());
    let row: Option<(Uuid,)> = sqlx::query_as(&sql)
        .bind(owner.id())
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(id,)| id))
}

/// Find or create the owner's cart, returning its id. The no-op DO UPDATE
/// makes the upsert always RETURN the row id.
pub async fn find_or_create_cart(pool: &PgPool, owner: &CartOwner) -> sqlx::Result<Uuid> {
    let col = owner.column();
    let sql = format!(
        "INSERT INTO carts ({col}) VALUES ($1) \
         ON CONFLICT ({col}) DO UPDATE SET {col} = EXCLUDED.{col} \
         RETURNING id"
    );
    let (id,): (Uuid,) = sqlx::query_as(&sql)
        .bind(owner.id())
        .fetch_one(pool)
        .await?;
    Ok(id)
}

pub async fn add_cart_item(
    pool: &PgPool,
    cart_id: Uuid,
    product_id: i64,
    qty: i32,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO cart_items (cart_id, product_id, qty) VALUES ($1, $2, $3) \
         ON CONFLICT (cart_id, product_id) DO UPDATE SET qty = cart_items.qty + EXCLUDED.qty",
    )
    .bind(cart_id)
    .bind(product_id)
    .bind(qty)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete_cart_item(pool: &PgPool, cart_id: Uuid, item_id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM cart_items WHERE id = $1 AND cart_id = $2")
        .bind(item_id)
        .bind(cart_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn cart_lines(pool: &PgPool, cart_id: Uuid) -> sqlx::Result<Vec<CartLine>> {
    sqlx::query_as::<_, CartLine>(
        "SELECT ci.id AS item_id, ci.product_id, p.name, p.subname, p.format, p.calories, p.price_cents, ci.qty \
         FROM cart_items ci \
         JOIN products p ON p.id = ci.product_id \
         WHERE ci.cart_id = $1 \
         ORDER BY ci.id",
    )
    .bind(cart_id)
    .fetch_all(pool)
    .await
}

pub async fn clear_cart(pool: &PgPool, cart_id: Uuid) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM cart_items WHERE cart_id = $1")
        .bind(cart_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn cart_count(pool: &PgPool, cart_id: Uuid) -> sqlx::Result<i64> {
    let (count,): (i64,) =
        sqlx::query_as("SELECT COALESCE(SUM(qty), 0)::BIGINT FROM cart_items WHERE cart_id = $1")
            .bind(cart_id)
            .fetch_one(pool)
            .await?;
    Ok(count)
}

/// Built-in catalog mirroring the seed migration, used so the storefront still
/// renders when the database is not configured or not yet migrated.
pub fn fallback_products() -> Vec<Product> {
    // The built-in catalog mirrors the database columns explicitly; keeping
    // each seed in one call makes the six fallback SKUs easy to compare.
    #[allow(clippy::too_many_arguments)]
    fn product(
        id: i64,
        slug: &str,
        subname: &str,
        description: &str,
        format: ProductFormat,
        calories: i32,
        protein_g: i32,
        price_cents: i32,
    ) -> Product {
        Product {
            id,
            slug: slug.to_string(),
            name: "AthletO".to_string(),
            subname: Some(subname.to_string()),
            description: description.to_string(),
            format,
            calories,
            protein_g,
            price_cents,
        }
    }

    vec![
        product(
            1,
            "athlet-o-starter-cup",
            "starter",
            "Lime-citrus protein wobble for daily training. 20g gelatin protein, inulin fiber, vitamin C, and electrolytes in a grab-and-go ready cup.",
            ProductFormat::Cup,
            90,
            20,
            449,
        ),
        product(
            2,
            "athlet-o-starter-powder",
            "starter",
            "Lime-citrus protein wobble for daily training. 20g gelatin protein, inulin fiber, vitamin C, and electrolytes -- just add water and chill.",
            ProductFormat::Powder,
            80,
            20,
            299,
        ),
        product(
            3,
            "recover-o-cup",
            "recover",
            "Berry-orange recovery wobble for the ride home. Gelatin protein plus magnesium, potassium, vitamin C, fiber, and live cultures in a ready cup.",
            ProductFormat::Cup,
            90,
            22,
            499,
        ),
        product(
            4,
            "recover-o-powder",
            "recover",
            "Berry-orange recovery wobble for the ride home. Gelatin protein plus magnesium, potassium, vitamin C, fiber, and live cultures -- just add water and chill.",
            ProductFormat::Powder,
            80,
            22,
            329,
        ),
        product(
            5,
            "pre-game-o-cup",
            "pre-game",
            "Citrus-punch prep cup for pre-game rituals. Sodium, potassium, and vitamin C with gelatin protein and no sugar rush, ready to eat.",
            ProductFormat::Cup,
            85,
            15,
            399,
        ),
        product(
            6,
            "pre-game-o-powder",
            "pre-game",
            "Citrus-punch prep for pre-game rituals. Sodium, potassium, and vitamin C with gelatin protein and no sugar rush -- just add water and chill.",
            ProductFormat::Powder,
            75,
            15,
            249,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Customer profiles (B2C vs B2B) and login events.

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "customer_type", rename_all = "lowercase")]
pub enum CustomerType {
    B2c,
    B2b,
}

impl CustomerType {
    pub fn label(self) -> &'static str {
        match self {
            Self::B2c => "Personal",
            Self::B2b => "Business",
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CustomerProfile {
    pub user_id: Uuid,
    pub customer_type: CustomerType,
    pub company_name: Option<String>,
}

impl CustomerProfile {
    pub fn is_b2b(&self) -> bool {
        self.customer_type == CustomerType::B2b
    }
}

pub async fn get_profile(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Option<CustomerProfile>> {
    sqlx::query_as::<_, CustomerProfile>(
        "SELECT user_id, customer_type, company_name FROM customer_profiles WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

pub async fn upsert_profile(
    pool: &PgPool,
    user_id: Uuid,
    customer_type: CustomerType,
    company_name: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO customer_profiles (user_id, customer_type, company_name) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (user_id) DO UPDATE \
         SET customer_type = EXCLUDED.customer_type, \
             company_name = EXCLUDED.company_name, \
             updated_at = now()",
    )
    .bind(user_id)
    .bind(customer_type)
    .bind(company_name)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_login_event(
    pool: &PgPool,
    user_id: Uuid,
    email: &str,
    aal: &str,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO login_events (user_id, email, aal) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(email)
        .bind(aal)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LoginEvent {
    pub email: String,
    pub aal: String,
    pub created_at: DateTime<Utc>,
}

pub async fn recent_login_events(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> sqlx::Result<Vec<LoginEvent>> {
    sqlx::query_as::<_, LoginEvent>(
        "SELECT email, aal, created_at FROM login_events \
         WHERE user_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

// ---------------------------------------------------------------------------
// Orders.

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Deserialize)]
#[sqlx(type_name = "order_kind", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum OrderKind {
    OneTime,
    Recurring,
}

impl OrderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::OneTime => "one-time",
            Self::Recurring => "recurring",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneTime => "one_time",
            Self::Recurring => "recurring",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Deserialize)]
#[sqlx(type_name = "order_frequency", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum OrderFrequency {
    Weekly,
    Biweekly,
    Monthly,
    Quarterly,
}

impl OrderFrequency {
    pub fn label(self) -> &'static str {
        match self {
            Self::Weekly => "weekly",
            Self::Biweekly => "every 2 weeks",
            Self::Monthly => "monthly",
            Self::Quarterly => "quarterly",
        }
    }

    pub fn interval_days(self) -> i64 {
        match self {
            Self::Weekly => 7,
            Self::Biweekly => 14,
            Self::Monthly => 30,
            Self::Quarterly => 90,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "order_status", rename_all = "lowercase")]
pub enum OrderStatus {
    Placed,
    Processing,
    Fulfilled,
    Cancelled,
}

impl OrderStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Placed => "placed",
            Self::Processing => "processing",
            Self::Fulfilled => "fulfilled",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "order_channel", rename_all = "snake_case")]
pub enum OrderChannel {
    D2cWeb,
    B2bPortal,
    B2bApi,
    Edi,
}

impl OrderChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::D2cWeb => "d2c_web",
            Self::B2bPortal => "b2b_portal",
            Self::B2bApi => "b2b_api",
            Self::Edi => "edi",
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrderRow {
    pub id: Uuid,
    pub kind: OrderKind,
    pub frequency: Option<OrderFrequency>,
    pub status: OrderStatus,
    pub channel: OrderChannel,
    pub po_number: Option<String>,
    pub total_cents: i64,
    pub next_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrderItemRow {
    pub order_id: Uuid,
    pub name: String,
    pub subname: Option<String>,
    pub format: ProductFormat,
    pub qty: i32,
    pub unit_price_cents: i32,
}

/// A validated order line ready for insertion.
#[derive(Debug, Clone, Copy)]
pub struct NewOrderLine {
    pub product_id: i64,
    pub qty: i32,
    pub unit_price_cents: i32,
}

/// Insert an order plus its items in one transaction; returns the order id.
pub async fn create_order(
    pool: &PgPool,
    user_id: Uuid,
    kind: OrderKind,
    frequency: Option<OrderFrequency>,
    channel: OrderChannel,
    po_number: Option<&str>,
    lines: &[NewOrderLine],
) -> sqlx::Result<Uuid> {
    let total: i64 = lines
        .iter()
        .map(|line| i64::from(line.unit_price_cents) * i64::from(line.qty))
        .sum();
    let next_run_at = match (kind, frequency) {
        (OrderKind::Recurring, Some(freq)) => {
            Some(Utc::now() + chrono::Duration::days(freq.interval_days()))
        }
        _ => None,
    };

    let mut tx = pool.begin().await?;
    let (order_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO orders (user_id, kind, frequency, channel, po_number, total_cents, next_run_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
    )
    .bind(user_id)
    .bind(kind)
    .bind(frequency)
    .bind(channel)
    .bind(po_number)
    .bind(total)
    .bind(next_run_at)
    .fetch_one(&mut *tx)
    .await?;

    for line in lines {
        sqlx::query(
            "INSERT INTO order_items (order_id, product_id, qty, unit_price_cents) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(order_id)
        .bind(line.product_id)
        .bind(line.qty)
        .bind(line.unit_price_cents)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(order_id)
}

pub async fn list_orders(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<OrderRow>> {
    sqlx::query_as::<_, OrderRow>(
        "SELECT id, kind, frequency, status, channel, po_number, total_cents, next_run_at, created_at \
         FROM orders WHERE user_id = $1 ORDER BY created_at DESC LIMIT 50",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn order_items_for_user(
    pool: &PgPool,
    user_id: Uuid,
) -> sqlx::Result<Vec<OrderItemRow>> {
    sqlx::query_as::<_, OrderItemRow>(
        "SELECT oi.order_id, p.name, p.subname, p.format, oi.qty, oi.unit_price_cents \
         FROM order_items oi \
         JOIN orders o ON o.id = oi.order_id \
         JOIN products p ON p.id = oi.product_id \
         WHERE o.user_id = $1 \
         ORDER BY oi.id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn product_prices(pool: &PgPool) -> sqlx::Result<Vec<(i64, String, i32)>> {
    let rows: Vec<(i64, String, i32)> =
        sqlx::query_as("SELECT id, slug, price_cents FROM products ORDER BY id")
            .fetch_all(pool)
            .await?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// B2B API keys. Only SHA-256 hashes are stored.

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKeyRow {
    pub id: Uuid,
    pub name: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub async fn insert_api_key(
    pool: &PgPool,
    user_id: Uuid,
    name: &str,
    key_hash: &str,
    prefix: &str,
) -> sqlx::Result<Uuid> {
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO b2b_api_keys (user_id, name, key_hash, prefix) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(user_id)
    .bind(name)
    .bind(key_hash)
    .bind(prefix)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Resolve an API key hash to its owning user, touching last_used_at.
pub async fn api_key_user(pool: &PgPool, key_hash: &str) -> sqlx::Result<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "UPDATE b2b_api_keys SET last_used_at = now() \
         WHERE key_hash = $1 AND revoked_at IS NULL \
         RETURNING user_id",
    )
    .bind(key_hash)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(user_id,)| user_id))
}

pub async fn list_api_keys(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<ApiKeyRow>> {
    sqlx::query_as::<_, ApiKeyRow>(
        "SELECT id, name, prefix, created_at, last_used_at, revoked_at \
         FROM b2b_api_keys WHERE user_id = $1 ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

pub async fn revoke_api_key(pool: &PgPool, user_id: Uuid, key_id: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE b2b_api_keys SET revoked_at = now() \
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(key_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}
