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
        "SELECT customer_type, company_name FROM customer_profiles WHERE user_id = $1",
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

    pub fn is_b2b(self) -> bool {
        !matches!(self, Self::D2cWeb)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "ship_method", rename_all = "lowercase")]
pub enum ShipMethod {
    Standard,
    Expedited,
    Freight,
}

impl ShipMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Expedited => "expedited",
            Self::Freight => "freight",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Standard => "Standard shipping",
            Self::Expedited => "Expedited shipping",
            Self::Freight => "Freight (LTL)",
        }
    }

    /// Flat shipping charged at checkout, in cents. Freight is billed on the
    /// business account after weigh/routing, so it books at 0 here.
    pub fn shipping_cents(self) -> i64 {
        match self {
            Self::Standard => 599,
            Self::Expedited => 1499,
            Self::Freight => 0,
        }
    }

    /// Estimated delivery window in business days (min, max) from order date.
    pub fn eta_business_days(self) -> (i64, i64) {
        match self {
            Self::Standard => (3, 5),
            Self::Expedited => (1, 2),
            Self::Freight => (5, 10),
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "standard" => Some(Self::Standard),
            "expedited" => Some(Self::Expedited),
            "freight" => Some(Self::Freight),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "payment_provider", rename_all = "lowercase")]
pub enum PaymentProvider {
    Stripe,
    Paypal,
    Square,
    /// B2B open account: ships against a PO, settled via a hosted Net-30
    /// Stripe invoice (card / ACH / bank transfer).
    Invoice,
}

impl PaymentProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stripe => "stripe",
            Self::Paypal => "paypal",
            Self::Square => "square",
            Self::Invoice => "invoice",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stripe => "Card / bank (Stripe)",
            Self::Paypal => "PayPal",
            Self::Square => "Square",
            Self::Invoice => "Invoice (Net 30)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "payment_status", rename_all = "lowercase")]
pub enum PaymentStatus {
    Pending,
    /// Payment initiated but not settled (e.g. ACH debit clearing).
    Processing,
    Paid,
    /// Net-30 invoice sent; AR is open in the ledger.
    Invoiced,
    Failed,
    Refunded,
}

impl PaymentStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "payment pending",
            Self::Processing => "payment processing",
            Self::Paid => "paid",
            Self::Invoiced => "invoiced net 30",
            Self::Failed => "payment failed",
            Self::Refunded => "refunded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "payment_kind", rename_all = "snake_case")]
pub enum PaymentKind {
    Charge,
    SubscriptionCycle,
    Refund,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "subscription_status", rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Pending,
    Active,
    PastDue,
    Cancelled,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrderRow {
    pub id: Uuid,
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
    pub next_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub payment_provider: Option<PaymentProvider>,
    pub payment_status: PaymentStatus,
    pub payment_ref: Option<String>,
    pub paid_at: Option<DateTime<Utc>>,
}

const ORDER_COLUMNS: &str = "id, kind, frequency, status, channel, ship_method, po_number, \
     subtotal_cents, shipping_cents, tax_cents, total_cents, next_run_at, created_at, \
     payment_provider, payment_status, payment_ref, paid_at";

impl OrderRow {
    /// Estimated delivery window (earliest, latest) as calendar dates,
    /// counting business days forward from the order date.
    pub fn delivery_window(&self) -> (chrono::NaiveDate, chrono::NaiveDate) {
        let (min, max) = self.ship_method.eta_business_days();
        let start = self.created_at.date_naive();
        (add_business_days(start, min), add_business_days(start, max))
    }

    pub fn short_id(&self) -> String {
        self.id.simple().to_string()[..8].to_uppercase()
    }
}

/// Add `n` business days (skipping Sat/Sun) to a date.
pub fn add_business_days(mut date: chrono::NaiveDate, n: i64) -> chrono::NaiveDate {
    use chrono::Datelike;
    let mut added = 0;
    while added < n {
        date += chrono::Duration::days(1);
        if !matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) {
            added += 1;
        }
    }
    date
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

pub async fn list_orders(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<OrderRow>> {
    sqlx::query_as::<_, OrderRow>(&format!(
        "SELECT {ORDER_COLUMNS} FROM orders WHERE user_id = $1 ORDER BY created_at DESC LIMIT 50"
    ))
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// One order scoped to its owner (None if not found or not theirs).
pub async fn get_order(pool: &PgPool, user_id: Uuid, order_id: Uuid) -> sqlx::Result<Option<OrderRow>> {
    sqlx::query_as::<_, OrderRow>(&format!(
        "SELECT {ORDER_COLUMNS} FROM orders WHERE id = $1 AND user_id = $2"
    ))
    .bind(order_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

pub async fn order_items(pool: &PgPool, order_id: Uuid) -> sqlx::Result<Vec<OrderItemRow>> {
    sqlx::query_as::<_, OrderItemRow>(
        "SELECT oi.order_id, p.name, p.subname, p.format, oi.qty, oi.unit_price_cents \
         FROM order_items oi JOIN products p ON p.id = oi.product_id \
         WHERE oi.order_id = $1 ORDER BY oi.id",
    )
    .bind(order_id)
    .fetch_all(pool)
    .await
}

/// Lines from a past order, for the reorder-into-cart action.
pub async fn order_reorder_lines(
    pool: &PgPool,
    user_id: Uuid,
    order_id: Uuid,
) -> sqlx::Result<Vec<(i64, i32)>> {
    sqlx::query_as(
        "SELECT oi.product_id, oi.qty FROM order_items oi \
         JOIN orders o ON o.id = oi.order_id \
         WHERE oi.order_id = $1 AND o.user_id = $2",
    )
    .bind(order_id)
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

// ---------------------------------------------------------------------------
// Shipments / fulfillment (carrier + tracking). Populated by ops or an EDI
// 856 mapping; surfaced on the order-detail/receipt page.

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "shipment_status", rename_all = "lowercase")]
pub enum ShipmentStatus {
    Packing,
    Shipped,
    Delivered,
}

impl ShipmentStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Packing => "packing",
            Self::Shipped => "shipped",
            Self::Delivered => "delivered",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "packing" => Some(Self::Packing),
            "shipped" => Some(Self::Shipped),
            "delivered" => Some(Self::Delivered),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Shipment {
    pub id: Uuid,
    pub status: ShipmentStatus,
    pub carrier: Option<String>,
    pub tracking_number: Option<String>,
    pub ship_date: Option<chrono::NaiveDate>,
    pub eta_earliest: Option<chrono::NaiveDate>,
    pub eta_latest: Option<chrono::NaiveDate>,
    pub delivered_at: Option<DateTime<Utc>>,
}

impl Shipment {
    /// Carrier tracking URL for the major carriers, else None.
    pub fn tracking_url(&self) -> Option<String> {
        let number = self.tracking_number.as_deref()?;
        let carrier = self.carrier.as_deref().unwrap_or("").to_lowercase();
        let url = if carrier.contains("ups") {
            format!("https://www.ups.com/track?tracknum={number}")
        } else if carrier.contains("fedex") {
            format!("https://www.fedex.com/fedextrack/?trknbr={number}")
        } else if carrier.contains("usps") {
            format!("https://tools.usps.com/go/TrackConfirmAction?tLabels={number}")
        } else if carrier.contains("dhl") {
            format!("https://www.dhl.com/us-en/home/tracking.html?tracking-id={number}")
        } else {
            return None;
        };
        Some(url)
    }
}

pub async fn shipments_for_order(pool: &PgPool, order_id: Uuid) -> sqlx::Result<Vec<Shipment>> {
    sqlx::query_as::<_, Shipment>(
        "SELECT id, status, carrier, tracking_number, ship_date, eta_earliest, eta_latest, delivered_at \
         FROM shipments WHERE order_id = $1 ORDER BY created_at",
    )
    .bind(order_id)
    .fetch_all(pool)
    .await
}

/// Record a fulfillment (ops / EDI 856): create a shipment with carrier +
/// tracking and advance the order to fulfilled. Ownership is checked so an
/// API key can only fulfill its own orders. Returns None if the order isn't
/// the user's.
pub async fn record_fulfillment(
    pool: &PgPool,
    user_id: Uuid,
    order_id: Uuid,
    carrier: &str,
    tracking_number: &str,
    ship_date: chrono::NaiveDate,
) -> sqlx::Result<Option<Uuid>> {
    let mut tx = pool.begin().await?;
    let owned: Option<(ShipMethod,)> =
        sqlx::query_as("SELECT ship_method FROM orders WHERE id = $1 AND user_id = $2 FOR UPDATE")
            .bind(order_id)
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((ship_method,)) = owned else {
        tx.rollback().await?;
        return Ok(None);
    };
    let (min, max) = ship_method.eta_business_days();
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO shipments \
         (order_id, status, carrier, tracking_number, ship_date, eta_earliest, eta_latest) \
         VALUES ($1, 'shipped', $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(order_id)
    .bind(carrier)
    .bind(tracking_number)
    .bind(ship_date)
    .bind(add_business_days(ship_date, min))
    .bind(add_business_days(ship_date, max))
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE orders SET status = 'fulfilled' WHERE id = $1")
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(id))
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

// ---------------------------------------------------------------------------
// Inventory + 90-minute cart holds.
//
// A hold is a row with an expiry, never a long-lived lock: claims take a
// milliseconds-long FOR UPDATE on the inventory row, availability treats
// expired holds as free (lazy expiry), and checkout converts hold -> sold in
// the same transaction as the order insert so they can never disagree.

pub const HOLD_MINUTES: i64 = 90;

#[derive(Debug, Clone, Copy)]
pub enum HoldOutcome {
    /// Hold placed or refreshed for another HOLD_MINUTES window.
    Held,
    /// Not enough unheld stock; `available` is what's left right now.
    Insufficient { available: i32 },
    /// Product has no inventory row; stock is untracked and unlimited.
    Untracked,
}

/// Claim (or refresh) this cart's hold for `qty` units of a product.
pub async fn ensure_hold(
    pool: &PgPool,
    cart_id: Uuid,
    product_id: i64,
    qty: i32,
) -> sqlx::Result<HoldOutcome> {
    let mut tx = pool.begin().await?;
    let on_hand: Option<(i32,)> =
        sqlx::query_as("SELECT on_hand FROM inventory WHERE product_id = $1 FOR UPDATE")
            .bind(product_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((on_hand,)) = on_hand else {
        tx.commit().await?;
        return Ok(HoldOutcome::Untracked);
    };

    let (held_elsewhere,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(qty), 0)::BIGINT FROM stock_holds \
         WHERE product_id = $1 AND cart_id <> $2 AND held_until > now()",
    )
    .bind(product_id)
    .bind(cart_id)
    .fetch_one(&mut *tx)
    .await?;

    let available = i64::from(on_hand) - held_elsewhere;
    if available < i64::from(qty) {
        tx.commit().await?;
        return Ok(HoldOutcome::Insufficient {
            available: available.max(0) as i32,
        });
    }

    sqlx::query(
        "INSERT INTO stock_holds (cart_id, product_id, qty, held_until) \
         VALUES ($1, $2, $3, now() + make_interval(mins => $4)) \
         ON CONFLICT (cart_id, product_id) DO UPDATE \
         SET qty = EXCLUDED.qty, held_until = EXCLUDED.held_until",
    )
    .bind(cart_id)
    .bind(product_id)
    .bind(qty)
    .bind(HOLD_MINUTES as i32)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(HoldOutcome::Held)
}

pub async fn release_hold(pool: &PgPool, cart_id: Uuid, product_id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM stock_holds WHERE cart_id = $1 AND product_id = $2")
        .bind(cart_id)
        .bind(product_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Earliest active hold expiry for the cart (None = nothing actively held).
pub async fn cart_hold_until(pool: &PgPool, cart_id: Uuid) -> sqlx::Result<Option<DateTime<Utc>>> {
    let row: Option<(Option<DateTime<Utc>>,)> = sqlx::query_as(
        "SELECT MIN(held_until) FROM stock_holds WHERE cart_id = $1 AND held_until > now()",
    )
    .bind(cart_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(min,)| min))
}

/// Hygiene only: the claim/availability queries already ignore expired rows.
pub async fn sweep_expired_holds(pool: &PgPool) -> sqlx::Result<u64> {
    let result = sqlx::query("DELETE FROM stock_holds WHERE held_until < now()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Materialize every recurring order whose `next_run_at` is due. Each order is
/// processed in its own transaction guarded by a transaction-scoped advisory
/// lock on the order id, so two runners (or a runner and a bypassed leader
/// guard) can never fire the same subscription twice. Returns how many child
/// orders were created.
pub async fn run_due_recurring_orders(pool: &PgPool) -> sqlx::Result<u64> {
    let due: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM orders \
         WHERE kind = 'recurring' AND status <> 'cancelled' \
         AND next_run_at IS NOT NULL AND next_run_at <= now() \
         ORDER BY next_run_at LIMIT 100",
    )
    .fetch_all(pool)
    .await?;

    let mut created = 0u64;
    for (subscription_id,) in due {
        // hashtextextended gives a stable bigint key for the advisory lock.
        let mut tx = pool.begin().await?;
        let got: bool = sqlx::query_scalar(
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1::text, 0))",
        )
        .bind(subscription_id.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if !got {
            tx.rollback().await?; // another runner owns this subscription
            continue;
        }

        // Re-read under the lock; skip if no longer due (a racing runner that
        // held the lock just before us already advanced it).
        let sub: Option<(Option<OrderFrequency>, OrderChannel, ShipMethod, DateTime<Utc>)> =
            sqlx::query_as(
                "SELECT frequency, channel, ship_method, next_run_at FROM orders \
                 WHERE id = $1 AND kind = 'recurring' AND status <> 'cancelled' \
                 AND next_run_at IS NOT NULL AND next_run_at <= now()",
            )
            .bind(subscription_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((frequency, channel, ship_method, _next)) = sub else {
            tx.rollback().await?;
            continue;
        };

        let user_id: (Uuid,) = sqlx::query_as("SELECT user_id FROM orders WHERE id = $1")
            .bind(subscription_id)
            .fetch_one(&mut *tx)
            .await?;
        let lines: Vec<(i64, i32, i32)> = sqlx::query_as(
            "SELECT product_id, qty, unit_price_cents FROM order_items WHERE order_id = $1",
        )
        .bind(subscription_id)
        .fetch_all(&mut *tx)
        .await?;

        // Decrement stock for tracked products; if any line is short, skip the
        // child this cycle but still advance the cursor so the subscription
        // isn't wedged (a real system would backorder).
        let mut short = false;
        for (product_id, qty, _) in &lines {
            let on_hand: Option<(i32,)> =
                sqlx::query_as("SELECT on_hand FROM inventory WHERE product_id = $1 FOR UPDATE")
                    .bind(product_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if let Some((on_hand,)) = on_hand {
                if on_hand < *qty {
                    short = true;
                    break;
                }
            }
        }

        if !short {
            for (product_id, qty, _) in &lines {
                sqlx::query(
                    "UPDATE inventory SET on_hand = on_hand - $2, updated_at = now() \
                     WHERE product_id = $1",
                )
                .bind(product_id)
                .bind(qty)
                .execute(&mut *tx)
                .await?;
            }
            let subtotal: i64 = lines
                .iter()
                .map(|(_, qty, price)| i64::from(*price) * i64::from(*qty))
                .sum();
            let shipping = ship_method.shipping_cents();
            let (child_id,): (Uuid,) = sqlx::query_as(
                "INSERT INTO orders \
                 (user_id, kind, channel, ship_method, subtotal_cents, shipping_cents, \
                  tax_cents, total_cents, recurs_from) \
                 VALUES ($1, 'one_time', $2, $3, $4, $5, 0, $6, $7) RETURNING id",
            )
            .bind(user_id.0)
            .bind(channel)
            .bind(ship_method)
            .bind(subtotal)
            .bind(shipping)
            .bind(subtotal + shipping)
            .bind(subscription_id)
            .fetch_one(&mut *tx)
            .await?;
            for (product_id, qty, price) in &lines {
                sqlx::query(
                    "INSERT INTO order_items (order_id, product_id, qty, unit_price_cents) \
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(child_id)
                .bind(product_id)
                .bind(qty)
                .bind(price)
                .execute(&mut *tx)
                .await?;
            }
            created += 1;
        } else {
            tracing::warn!(%subscription_id, "recurring order short on stock; skipping this cycle");
        }

        let interval = frequency.map(|f| f.interval_days()).unwrap_or(30);
        sqlx::query(
            "UPDATE orders SET next_run_at = next_run_at + make_interval(days => $2) WHERE id = $1",
        )
        .bind(subscription_id)
        .bind(interval as i32)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(created)
}

#[derive(Debug, Clone, Copy)]
pub struct InsufficientLine {
    pub product_id: i64,
    pub requested: i32,
    pub available: i32,
}

#[derive(Debug)]
pub enum OrderError {
    Insufficient(Vec<InsufficientLine>),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for OrderError {
    fn from(err: sqlx::Error) -> Self {
        Self::Db(err)
    }
}

/// Place an order, decrementing stock and consuming the cart's holds in the
/// same transaction. Inventory rows are locked in product-id order so
/// concurrent checkouts can't deadlock. Lines whose products are untracked
/// (no inventory row) skip the stock check.
#[allow(clippy::too_many_arguments)]
pub async fn place_order(
    pool: &PgPool,
    user_id: Uuid,
    kind: OrderKind,
    frequency: Option<OrderFrequency>,
    channel: OrderChannel,
    ship_method: ShipMethod,
    po_number: Option<&str>,
    lines: &[NewOrderLine],
    cart_id: Option<Uuid>,
) -> Result<Uuid, OrderError> {
    let mut sorted: Vec<NewOrderLine> = lines.to_vec();
    sorted.sort_by_key(|line| line.product_id);

    let mut tx = pool.begin().await?;
    let mut shortages = Vec::new();
    for line in &sorted {
        let on_hand: Option<(i32,)> =
            sqlx::query_as("SELECT on_hand FROM inventory WHERE product_id = $1 FOR UPDATE")
                .bind(line.product_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((on_hand,)) = on_hand else { continue };

        let (held_elsewhere,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(qty), 0)::BIGINT FROM stock_holds \
             WHERE product_id = $1 AND held_until > now() \
             AND cart_id IS DISTINCT FROM $2",
        )
        .bind(line.product_id)
        .bind(cart_id)
        .fetch_one(&mut *tx)
        .await?;

        let available = i64::from(on_hand) - held_elsewhere;
        if available < i64::from(line.qty) {
            shortages.push(InsufficientLine {
                product_id: line.product_id,
                requested: line.qty,
                available: available.max(0) as i32,
            });
            continue;
        }
        sqlx::query(
            "UPDATE inventory SET on_hand = on_hand - $2, updated_at = now() WHERE product_id = $1",
        )
        .bind(line.product_id)
        .bind(line.qty)
        .execute(&mut *tx)
        .await?;
    }
    if !shortages.is_empty() {
        tx.rollback().await?;
        return Err(OrderError::Insufficient(shortages));
    }

    let subtotal: i64 = sorted
        .iter()
        .map(|line| i64::from(line.unit_price_cents) * i64::from(line.qty))
        .sum();
    let shipping = ship_method.shipping_cents();
    let tax = 0; // Sales tax is calculated at fulfillment by jurisdiction.
    let total = subtotal + shipping + tax;
    let next_run_at = match (kind, frequency) {
        (OrderKind::Recurring, Some(freq)) => {
            Some(Utc::now() + chrono::Duration::days(freq.interval_days()))
        }
        _ => None,
    };
    let (order_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO orders \
         (user_id, kind, frequency, channel, ship_method, po_number, \
          subtotal_cents, shipping_cents, tax_cents, total_cents, next_run_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING id",
    )
    .bind(user_id)
    .bind(kind)
    .bind(frequency)
    .bind(channel)
    .bind(ship_method)
    .bind(po_number)
    .bind(subtotal)
    .bind(shipping)
    .bind(tax)
    .bind(total)
    .bind(next_run_at)
    .fetch_one(&mut *tx)
    .await?;

    for line in &sorted {
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

    if let Some(cart_id) = cart_id {
        sqlx::query("DELETE FROM stock_holds WHERE cart_id = $1")
            .bind(cart_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM cart_items WHERE cart_id = $1")
            .bind(cart_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(order_id)
}

// ---------------------------------------------------------------------------
// Payments: provider handles on orders, settled payments, provider-billed
// subscriptions, and the webhook idempotency ledger (see 0006_payments.sql).

/// Attach the provider checkout handle to an order right after the hosted
/// session / approval link / invoice is created.
pub async fn set_order_payment(
    pool: &PgPool,
    order_id: Uuid,
    provider: PaymentProvider,
    payment_ref: &str,
    status: PaymentStatus,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE orders SET payment_provider = $2, payment_ref = $3, payment_status = $4 \
         WHERE id = $1",
    )
    .bind(order_id)
    .bind(provider)
    .bind(payment_ref)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

/// Advance an order's payment status. `paid_at` is stamped once, on the
/// first transition into `Paid`.
pub async fn set_order_payment_status(
    pool: &PgPool,
    order_id: Uuid,
    status: PaymentStatus,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE orders SET payment_status = $2, \
         paid_at = CASE WHEN $2 = 'paid'::payment_status THEN COALESCE(paid_at, now()) \
                        ELSE paid_at END \
         WHERE id = $1",
    )
    .bind(order_id)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

/// Minimal order facts for webhook / return-URL handlers, which act on
/// behalf of the provider rather than a logged-in user.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OrderPaymentFacts {
    pub id: Uuid,
    pub user_id: Uuid,
    pub total_cents: i64,
    pub kind: OrderKind,
    pub frequency: Option<OrderFrequency>,
    pub payment_provider: Option<PaymentProvider>,
    pub payment_status: PaymentStatus,
    pub payment_ref: Option<String>,
}

pub async fn order_payment_facts(
    pool: &PgPool,
    order_id: Uuid,
) -> sqlx::Result<Option<OrderPaymentFacts>> {
    sqlx::query_as(
        "SELECT id, user_id, total_cents, kind, frequency, payment_provider, payment_status, \
                payment_ref \
         FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_optional(pool)
    .await
}

/// Record a settled (or failed/refunded) money movement. Returns `true` when
/// the row is new; a repeat of the same (provider, provider_ref) only
/// refreshes the status, so callers can skip double-posting to the ledger.
#[allow(clippy::too_many_arguments)]
pub async fn record_payment(
    pool: &PgPool,
    order_id: Option<Uuid>,
    user_id: Uuid,
    provider: PaymentProvider,
    kind: PaymentKind,
    provider_ref: &str,
    amount_cents: i64,
    status: PaymentStatus,
) -> sqlx::Result<bool> {
    let (inserted,): (bool,) = sqlx::query_as(
        "INSERT INTO payments (order_id, user_id, provider, kind, provider_ref, amount_cents, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (provider, provider_ref) \
         DO UPDATE SET status = EXCLUDED.status, updated_at = now() \
         RETURNING (xmax = 0)",
    )
    .bind(order_id)
    .bind(user_id)
    .bind(provider)
    .bind(kind)
    .bind(provider_ref)
    .bind(amount_cents)
    .bind(status)
    .fetch_one(pool)
    .await?;
    Ok(inserted)
}

pub async fn upsert_subscription(
    pool: &PgPool,
    user_id: Uuid,
    order_id: Option<Uuid>,
    provider: PaymentProvider,
    provider_ref: &str,
    status: SubscriptionStatus,
    frequency: OrderFrequency,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO payment_subscriptions (user_id, order_id, provider, provider_ref, status, frequency) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (provider, provider_ref) \
         DO UPDATE SET status = EXCLUDED.status, updated_at = now()",
    )
    .bind(user_id)
    .bind(order_id)
    .bind(provider)
    .bind(provider_ref)
    .bind(status)
    .bind(frequency)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_subscription_status(
    pool: &PgPool,
    provider: PaymentProvider,
    provider_ref: &str,
    status: SubscriptionStatus,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE payment_subscriptions SET status = $3, updated_at = now() \
         WHERE provider = $1 AND provider_ref = $2",
    )
    .bind(provider)
    .bind(provider_ref)
    .bind(status)
    .execute(pool)
    .await?;
    Ok(())
}

/// Webhook idempotency: returns `true` the first time an event id is seen.
/// Handlers bail out on `false` so provider retries can't double-apply.
pub async fn record_payment_event(
    pool: &PgPool,
    provider: PaymentProvider,
    event_id: &str,
    payload: &serde_json::Value,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "INSERT INTO payment_events (provider, event_id, payload) VALUES ($1, $2, $3) \
         ON CONFLICT DO NOTHING",
    )
    .bind(provider)
    .bind(event_id)
    .bind(payload)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}
