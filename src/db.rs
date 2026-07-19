//! Connection setup plus product, cart, customer, order, and API-key queries.
//!
//! SeaORM edition: straightforward lookups go through the entity query
//! builders in `crate::entities`; upserts and the two transactional hot paths
//! (`ensure_hold`, `place_order`) stay as raw SQL via `sea_orm::Statement` so
//! their locking/conflict semantics remain explicit. Connection construction,
//! entities, transactions, and statements all use SeaORM's public API.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ColumnTrait, ConnectOptions, ConnectionTrait, Database,
    DatabaseConnection, DbBackend, DbErr, DeriveActiveEnum, EntityTrait, EnumIter, FromQueryResult,
    QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait, TryInsertResult, Value,
};
use uuid::Uuid;

use crate::entities::{
    b2b_api_key, cart, cart_item, customer_profile, login_event, order, payment, payment_event,
    payment_subscription, product, stock_hold,
};

pub use crate::entities::product::Model as Product;

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "product_format")]
pub enum ProductFormat {
    #[sea_orm(string_value = "cup")]
    Cup,
    #[sea_orm(string_value = "powder")]
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

#[derive(Debug, Clone, FromQueryResult)]
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

/// Build a SeaORM lazy connection. It validates configuration without waiting
/// for the database, preserving the app's degraded-start behavior.
pub async fn build_pool(database_url: &str) -> Option<DatabaseConnection> {
    let mut options = ConnectOptions::new(database_url.to_string());
    options
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy(true);

    match Database::connect(options).await {
        Ok(connection) => Some(connection),
        Err(err) => {
            tracing::error!(error = %err, "invalid DATABASE_URL; continuing without a database");
            None
        }
    }
}

/// Raw-SQL helper for the queries that intentionally stay hand-written.
fn stmt<I>(sql: &str, values: I) -> Statement
where
    I: IntoIterator<Item = Value>,
{
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

pub async fn list_products(conn: &DatabaseConnection) -> Result<Vec<Product>, DbErr> {
    product::Entity::find()
        .order_by_asc(product::Column::Id)
        .all(conn)
        .await
}

pub async fn product_by_slug(
    conn: &DatabaseConnection,
    slug: &str,
) -> Result<Option<Product>, DbErr> {
    product::Entity::find()
        .filter(product::Column::Slug.eq(slug))
        .one(conn)
        .await
}

/// Find the owner's cart id if one exists.
pub async fn find_cart(
    conn: &DatabaseConnection,
    owner: &CartOwner,
) -> Result<Option<Uuid>, DbErr> {
    let query = match owner {
        CartOwner::User(id) => cart::Entity::find().filter(cart::Column::UserId.eq(*id)),
        CartOwner::Anon(id) => cart::Entity::find().filter(cart::Column::AnonId.eq(*id)),
    };
    Ok(query.one(conn).await?.map(|cart| cart.id))
}

/// Find or create the owner's cart, returning its id. The no-op DO UPDATE
/// makes the upsert always RETURN the row id.
pub async fn find_or_create_cart(
    conn: &DatabaseConnection,
    owner: &CartOwner,
) -> Result<Uuid, DbErr> {
    let col = owner.column();
    let sql = format!(
        "INSERT INTO carts ({col}) VALUES ($1) \
         ON CONFLICT ({col}) DO UPDATE SET {col} = EXCLUDED.{col} \
         RETURNING id"
    );
    let row = conn
        .query_one(stmt(&sql, [owner.id().into()]))
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("cart upsert returned no row".to_string()))?;
    row.try_get::<Uuid>("", "id")
}

pub async fn add_cart_item(
    conn: &DatabaseConnection,
    cart_id: Uuid,
    product_id: i64,
    qty: i32,
) -> Result<(), DbErr> {
    conn.execute(stmt(
        "INSERT INTO cart_items (cart_id, product_id, qty) VALUES ($1, $2, $3) \
         ON CONFLICT (cart_id, product_id) DO UPDATE SET qty = cart_items.qty + EXCLUDED.qty",
        [cart_id.into(), product_id.into(), qty.into()],
    ))
    .await?;
    Ok(())
}

pub async fn delete_cart_item(
    conn: &DatabaseConnection,
    cart_id: Uuid,
    item_id: i64,
) -> Result<(), DbErr> {
    cart_item::Entity::delete_many()
        .filter(cart_item::Column::Id.eq(item_id))
        .filter(cart_item::Column::CartId.eq(cart_id))
        .exec(conn)
        .await?;
    Ok(())
}

pub async fn cart_lines(conn: &DatabaseConnection, cart_id: Uuid) -> Result<Vec<CartLine>, DbErr> {
    // Joined read; the ::text cast lets the ActiveEnum decode the PG enum.
    CartLine::find_by_statement(stmt(
        "SELECT ci.id AS item_id, ci.product_id, p.name, p.subname, p.format::text AS format, \
                p.calories, p.price_cents, ci.qty \
         FROM cart_items ci \
         JOIN products p ON p.id = ci.product_id \
         WHERE ci.cart_id = $1 \
         ORDER BY ci.id",
        [cart_id.into()],
    ))
    .all(conn)
    .await
}

pub async fn cart_count(conn: &DatabaseConnection, cart_id: Uuid) -> Result<i64, DbErr> {
    let row = conn
        .query_one(stmt(
            "SELECT COALESCE(SUM(qty), 0)::BIGINT AS count FROM cart_items WHERE cart_id = $1",
            [cart_id.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("cart count returned no row".to_string()))?;
    row.try_get::<i64>("", "count")
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "customer_type")]
pub enum CustomerType {
    #[sea_orm(string_value = "b2c")]
    B2c,
    #[sea_orm(string_value = "b2b")]
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

#[derive(Debug, Clone)]
pub struct CustomerProfile {
    pub customer_type: CustomerType,
    pub company_name: Option<String>,
    pub b2b_approved_at: Option<DateTime<Utc>>,
}

impl CustomerProfile {
    pub fn is_b2b(&self) -> bool {
        self.customer_type == CustomerType::B2b
    }

    pub fn is_b2b_approved(&self) -> bool {
        self.is_b2b() && self.b2b_approved_at.is_some()
    }
}

pub async fn get_profile(
    conn: &DatabaseConnection,
    user_id: Uuid,
) -> Result<Option<CustomerProfile>, DbErr> {
    Ok(customer_profile::Entity::find_by_id(user_id)
        .one(conn)
        .await?
        .map(|profile| CustomerProfile {
            customer_type: profile.customer_type,
            company_name: profile.company_name,
            b2b_approved_at: profile.b2b_approved_at,
        }))
}

pub async fn upsert_profile(
    conn: &DatabaseConnection,
    user_id: Uuid,
    customer_type: CustomerType,
    company_name: Option<&str>,
) -> Result<(), DbErr> {
    conn.execute(stmt(
        "INSERT INTO customer_profiles (user_id, customer_type, company_name) \
         VALUES ($1, $2::customer_type, $3) \
         ON CONFLICT (user_id) DO UPDATE \
         SET customer_type = EXCLUDED.customer_type, \
             company_name = EXCLUDED.company_name, \
             b2b_approved_at = CASE WHEN EXCLUDED.customer_type = 'b2c' \
                                    THEN NULL ELSE customer_profiles.b2b_approved_at END, \
             updated_at = now()",
        [
            user_id.into(),
            customer_type.into(),
            company_name.map(str::to_string).into(),
        ],
    ))
    .await?;
    Ok(())
}

/// Approve (or revoke) a business account. Ops calls this after vetting a
/// company; until then `b2b_approved_at` is NULL and `is_b2b_approved()` gates
/// off B2B ordering, the ERP API, and API keys. Returns `Some(is_approved)`
/// with the resulting state, or `None` if `user_id` is not a B2B account.
pub async fn set_b2b_approval(
    conn: &DatabaseConnection,
    user_id: Uuid,
    approved: bool,
) -> Result<Option<bool>, DbErr> {
    let row = conn
        .query_one(stmt(
            "UPDATE customer_profiles \
             SET b2b_approved_at = CASE WHEN $2 THEN COALESCE(b2b_approved_at, now()) ELSE NULL END, \
                 updated_at = now() \
             WHERE user_id = $1 AND customer_type = 'b2b' \
             RETURNING (b2b_approved_at IS NOT NULL) AS approved",
            [user_id.into(), approved.into()],
        ))
        .await?;
    match row {
        Some(row) => Ok(Some(row.try_get::<bool>("", "approved")?)),
        None => Ok(None),
    }
}

pub async fn record_login_event(
    conn: &DatabaseConnection,
    user_id: Uuid,
    email: &str,
    aal: &str,
) -> Result<(), DbErr> {
    login_event::ActiveModel {
        user_id: Set(user_id),
        email: Set(email.to_string()),
        aal: Set(aal.to_string()),
        ..Default::default()
    }
    .insert(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct LoginEvent {
    pub email: String,
    pub aal: String,
    pub created_at: DateTime<Utc>,
}

pub async fn recent_login_events(
    conn: &DatabaseConnection,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<LoginEvent>, DbErr> {
    Ok(login_event::Entity::find()
        .filter(login_event::Column::UserId.eq(user_id))
        .order_by_desc(login_event::Column::CreatedAt)
        .limit(limit.max(0) as u64)
        .all(conn)
        .await?
        .into_iter()
        .map(|event| LoginEvent {
            email: event.email,
            aal: event.aal,
            created_at: event.created_at,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Orders.

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum, serde::Deserialize)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "order_kind")]
#[serde(rename_all = "snake_case")]
pub enum OrderKind {
    #[sea_orm(string_value = "one_time")]
    OneTime,
    #[sea_orm(string_value = "recurring")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum, serde::Deserialize)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "order_frequency")]
#[serde(rename_all = "lowercase")]
pub enum OrderFrequency {
    #[sea_orm(string_value = "weekly")]
    Weekly,
    #[sea_orm(string_value = "biweekly")]
    Biweekly,
    #[sea_orm(string_value = "monthly")]
    Monthly,
    #[sea_orm(string_value = "quarterly")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "order_status")]
pub enum OrderStatus {
    #[sea_orm(string_value = "placed")]
    Placed,
    #[sea_orm(string_value = "processing")]
    Processing,
    #[sea_orm(string_value = "fulfilled")]
    Fulfilled,
    #[sea_orm(string_value = "cancelled")]
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

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "placed" => Some(Self::Placed),
            "processing" => Some(Self::Processing),
            "fulfilled" => Some(Self::Fulfilled),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "order_channel")]
pub enum OrderChannel {
    #[sea_orm(string_value = "d2c_web")]
    D2cWeb,
    #[sea_orm(string_value = "b2b_portal")]
    B2bPortal,
    #[sea_orm(string_value = "b2b_api")]
    B2bApi,
    #[sea_orm(string_value = "edi")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "ship_method")]
pub enum ShipMethod {
    #[sea_orm(string_value = "standard")]
    Standard,
    #[sea_orm(string_value = "expedited")]
    Expedited,
    #[sea_orm(string_value = "freight")]
    Freight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "payment_provider")]
pub enum PaymentProvider {
    #[sea_orm(string_value = "stripe")]
    Stripe,
    #[sea_orm(string_value = "paypal")]
    Paypal,
    #[sea_orm(string_value = "square")]
    Square,
    #[sea_orm(string_value = "invoice")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "payment_status")]
pub enum PaymentStatus {
    #[sea_orm(string_value = "pending")]
    Pending,
    #[sea_orm(string_value = "processing")]
    Processing,
    #[sea_orm(string_value = "paid")]
    Paid,
    #[sea_orm(string_value = "invoiced")]
    Invoiced,
    #[sea_orm(string_value = "failed")]
    Failed,
    #[sea_orm(string_value = "refunded")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "payment_kind")]
pub enum PaymentKind {
    #[sea_orm(string_value = "charge")]
    Charge,
    #[sea_orm(string_value = "subscription_cycle")]
    SubscriptionCycle,
    #[sea_orm(string_value = "refund")]
    Refund,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(
    rs_type = "String",
    db_type = "Enum",
    enum_name = "subscription_status"
)]
pub enum SubscriptionStatus {
    #[sea_orm(string_value = "pending")]
    Pending,
    #[sea_orm(string_value = "active")]
    Active,
    #[sea_orm(string_value = "past_due")]
    PastDue,
    #[sea_orm(string_value = "cancelled")]
    Cancelled,
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

#[derive(Debug, Clone)]
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

impl OrderRow {
    fn from_model(order: order::Model) -> Self {
        Self {
            id: order.id,
            kind: order.kind,
            frequency: order.frequency,
            status: order.status,
            channel: order.channel,
            ship_method: order.ship_method,
            po_number: order.po_number,
            subtotal_cents: order.subtotal_cents,
            shipping_cents: order.shipping_cents,
            tax_cents: order.tax_cents,
            total_cents: order.total_cents,
            next_run_at: order.next_run_at,
            created_at: order.created_at,
            payment_provider: order.payment_provider,
            payment_status: order.payment_status,
            payment_ref: order.payment_ref,
            paid_at: order.paid_at,
        }
    }

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

#[derive(Debug, Clone, FromQueryResult)]
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

pub async fn list_orders(conn: &DatabaseConnection, user_id: Uuid) -> Result<Vec<OrderRow>, DbErr> {
    Ok(order::Entity::find()
        .filter(order::Column::UserId.eq(user_id))
        .order_by_desc(order::Column::CreatedAt)
        .limit(50)
        .all(conn)
        .await?
        .into_iter()
        .map(OrderRow::from_model)
        .collect())
}

/// One order scoped to its owner (None if not found or not theirs).
pub async fn get_order(
    conn: &DatabaseConnection,
    user_id: Uuid,
    order_id: Uuid,
) -> Result<Option<OrderRow>, DbErr> {
    Ok(order::Entity::find()
        .filter(order::Column::Id.eq(order_id))
        .filter(order::Column::UserId.eq(user_id))
        .one(conn)
        .await?
        .map(OrderRow::from_model))
}

/// Line items for one of `user_id`'s orders.
///
/// The `user_id` predicate is not redundant with a caller's own ownership
/// check: because the app connects as the table owner and bypasses RLS, this
/// join is the only thing standing between a `Path<Uuid>` order id and another
/// customer's line items and prices. Keeping it in the query means a future
/// caller cannot reintroduce the IDOR by forgetting to call `get_order` first.
pub async fn order_items(
    conn: &DatabaseConnection,
    user_id: Uuid,
    order_id: Uuid,
) -> Result<Vec<OrderItemRow>, DbErr> {
    OrderItemRow::find_by_statement(stmt(
        "SELECT oi.order_id, p.name, p.subname, p.format::text AS format, oi.qty, oi.unit_price_cents \
         FROM order_items oi \
         JOIN products p ON p.id = oi.product_id \
         JOIN orders o ON o.id = oi.order_id \
         WHERE oi.order_id = $1 AND o.user_id = $2 ORDER BY oi.id",
        [order_id.into(), user_id.into()],
    ))
    .all(conn)
    .await
}

/// Lines from a past order, for the reorder-into-cart action.
pub async fn order_reorder_lines(
    conn: &DatabaseConnection,
    user_id: Uuid,
    order_id: Uuid,
) -> Result<Vec<(i64, i32)>, DbErr> {
    let rows = conn
        .query_all(stmt(
            "SELECT oi.product_id, oi.qty FROM order_items oi \
             JOIN orders o ON o.id = oi.order_id \
             WHERE oi.order_id = $1 AND o.user_id = $2",
            [order_id.into(), user_id.into()],
        ))
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get::<i64>("", "product_id")?,
                row.try_get::<i32>("", "qty")?,
            ))
        })
        .collect()
}

pub async fn order_items_for_user(
    conn: &DatabaseConnection,
    user_id: Uuid,
) -> Result<Vec<OrderItemRow>, DbErr> {
    OrderItemRow::find_by_statement(stmt(
        "SELECT oi.order_id, p.name, p.subname, p.format::text AS format, oi.qty, oi.unit_price_cents \
         FROM order_items oi \
         JOIN orders o ON o.id = oi.order_id \
         JOIN products p ON p.id = oi.product_id \
         WHERE o.user_id = $1 \
         ORDER BY oi.id",
        [user_id.into()],
    ))
    .all(conn)
    .await
}

pub async fn product_prices(conn: &DatabaseConnection) -> Result<Vec<(i64, String, i32)>, DbErr> {
    Ok(list_products(conn)
        .await?
        .into_iter()
        .map(|product| (product.id, product.slug, product.price_cents))
        .collect())
}

// ---------------------------------------------------------------------------
// B2B API keys. Only SHA-256 hashes are stored.

#[derive(Debug, Clone)]
pub struct ApiKeyRow {
    pub id: Uuid,
    pub name: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

pub async fn insert_api_key(
    conn: &DatabaseConnection,
    user_id: Uuid,
    name: &str,
    key_hash: &str,
    prefix: &str,
) -> Result<Uuid, DbErr> {
    let inserted = b2b_api_key::ActiveModel {
        user_id: Set(user_id),
        name: Set(name.to_string()),
        key_hash: Set(key_hash.to_string()),
        prefix: Set(prefix.to_string()),
        ..Default::default()
    }
    .insert(conn)
    .await?;
    Ok(inserted.id)
}

/// Resolve an API key hash to its owning user, touching last_used_at.
pub async fn api_key_user(
    conn: &DatabaseConnection,
    key_hash: &str,
) -> Result<Option<Uuid>, DbErr> {
    let row = conn
        .query_one(stmt(
            "UPDATE b2b_api_keys SET last_used_at = now() \
             WHERE key_hash = $1 AND revoked_at IS NULL \
             RETURNING user_id",
            [key_hash.into()],
        ))
        .await?;
    row.map(|row| row.try_get::<Uuid>("", "user_id"))
        .transpose()
}

pub async fn list_api_keys(
    conn: &DatabaseConnection,
    user_id: Uuid,
) -> Result<Vec<ApiKeyRow>, DbErr> {
    Ok(b2b_api_key::Entity::find()
        .filter(b2b_api_key::Column::UserId.eq(user_id))
        .order_by_desc(b2b_api_key::Column::CreatedAt)
        .all(conn)
        .await?
        .into_iter()
        .map(|key| ApiKeyRow {
            id: key.id,
            name: key.name,
            prefix: key.prefix,
            created_at: key.created_at,
            last_used_at: key.last_used_at,
            revoked_at: key.revoked_at,
        })
        .collect())
}

pub async fn revoke_api_key(
    conn: &DatabaseConnection,
    user_id: Uuid,
    key_id: Uuid,
) -> Result<(), DbErr> {
    b2b_api_key::Entity::update_many()
        .col_expr(
            b2b_api_key::Column::RevokedAt,
            Expr::current_timestamp().into(),
        )
        .filter(b2b_api_key::Column::Id.eq(key_id))
        .filter(b2b_api_key::Column::UserId.eq(user_id))
        .filter(b2b_api_key::Column::RevokedAt.is_null())
        .exec(conn)
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
// These transactions stay raw SQL on purpose (FOR UPDATE ordering and the
// upsert semantics are load-bearing).

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
    conn: &DatabaseConnection,
    cart_id: Uuid,
    product_id: i64,
    qty: i32,
) -> Result<HoldOutcome, DbErr> {
    let tx = conn.begin().await?;
    let on_hand = tx
        .query_one(stmt(
            "SELECT on_hand FROM inventory WHERE product_id = $1 FOR UPDATE",
            [product_id.into()],
        ))
        .await?;
    let Some(on_hand_row) = on_hand else {
        tx.commit().await?;
        return Ok(HoldOutcome::Untracked);
    };
    let on_hand: i32 = on_hand_row.try_get("", "on_hand")?;

    let held_row = tx
        .query_one(stmt(
            "SELECT COALESCE(SUM(qty), 0)::BIGINT AS held FROM stock_holds \
             WHERE product_id = $1 AND cart_id <> $2 AND held_until > now()",
            [product_id.into(), cart_id.into()],
        ))
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("hold sum returned no row".to_string()))?;
    let held_elsewhere: i64 = held_row.try_get("", "held")?;

    let available = i64::from(on_hand) - held_elsewhere;
    if available < i64::from(qty) {
        tx.commit().await?;
        return Ok(HoldOutcome::Insufficient {
            available: available.max(0) as i32,
        });
    }

    tx.execute(stmt(
        "INSERT INTO stock_holds (cart_id, product_id, qty, held_until) \
         VALUES ($1, $2, $3, now() + make_interval(mins => $4)) \
         ON CONFLICT (cart_id, product_id) DO UPDATE \
         SET qty = EXCLUDED.qty, held_until = EXCLUDED.held_until",
        [
            cart_id.into(),
            product_id.into(),
            qty.into(),
            (HOLD_MINUTES as i32).into(),
        ],
    ))
    .await?;
    tx.commit().await?;
    Ok(HoldOutcome::Held)
}

pub async fn release_hold(
    conn: &DatabaseConnection,
    cart_id: Uuid,
    product_id: i64,
) -> Result<(), DbErr> {
    stock_hold::Entity::delete_many()
        .filter(stock_hold::Column::CartId.eq(cart_id))
        .filter(stock_hold::Column::ProductId.eq(product_id))
        .exec(conn)
        .await?;
    Ok(())
}

/// Earliest active hold expiry for the cart (None = nothing actively held).
pub async fn cart_hold_until(
    conn: &DatabaseConnection,
    cart_id: Uuid,
) -> Result<Option<DateTime<Utc>>, DbErr> {
    let row = conn
        .query_one(stmt(
            "SELECT MIN(held_until) AS min_until FROM stock_holds \
             WHERE cart_id = $1 AND held_until > now()",
            [cart_id.into()],
        ))
        .await?;
    row.map(|row| row.try_get::<Option<DateTime<Utc>>>("", "min_until"))
        .transpose()
        .map(Option::flatten)
}

/// Hygiene only: the claim/availability queries already ignore expired rows.
pub async fn sweep_expired_holds(conn: &DatabaseConnection) -> Result<u64, DbErr> {
    // Self-guard so concurrent replicas don't all issue the same DELETE, using
    // a TRANSACTION-scoped advisory lock (pg_try_advisory_xact_lock) acquired
    // and released within this one transaction. That is the only advisory-lock
    // shape safe through the Supabase transaction pooler: a session-scoped lock
    // can have its acquire and release routed to different pooled backends,
    // leaking the lock permanently. This mirrors run_due_recurring_orders'
    // per-order guard, and it is what lets coordinate::run_singleton run this
    // job directly when no cross-cluster lease (fiducia) is configured.
    let tx = conn.begin().await?;
    let got: bool = tx
        .query_one(stmt(
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1::text, 0)) AS got",
            ["hold-sweeper".into()],
        ))
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("advisory lock returned no row".to_string()))?
        .try_get("", "got")?;
    if !got {
        // Another replica is sweeping this tick; nothing for us to do.
        tx.rollback().await?;
        return Ok(0);
    }
    let result = tx
        .execute(stmt("DELETE FROM stock_holds WHERE held_until < now()", []))
        .await?;
    tx.commit().await?;
    Ok(result.rows_affected())
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
    Db(DbErr),
}

impl From<DbErr> for OrderError {
    fn from(err: DbErr) -> Self {
        Self::Db(err)
    }
}

/// Place an order, decrementing stock and consuming the cart's holds in the
/// same transaction. Inventory rows are locked in product-id order so
/// concurrent checkouts can't deadlock. Lines whose products are untracked
/// (no inventory row) skip the stock check.
#[allow(clippy::too_many_arguments)]
pub async fn place_order(
    conn: &DatabaseConnection,
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

    let tx = conn.begin().await.map_err(OrderError::Db)?;
    let mut shortages = Vec::new();
    for line in &sorted {
        let on_hand = tx
            .query_one(stmt(
                "SELECT on_hand FROM inventory WHERE product_id = $1 FOR UPDATE",
                [line.product_id.into()],
            ))
            .await?;
        let Some(on_hand_row) = on_hand else { continue };
        let on_hand: i32 = on_hand_row.try_get("", "on_hand")?;

        let held_row = tx
            .query_one(stmt(
                "SELECT COALESCE(SUM(qty), 0)::BIGINT AS held FROM stock_holds \
                 WHERE product_id = $1 AND held_until > now() \
                 AND cart_id IS DISTINCT FROM $2",
                [line.product_id.into(), cart_id.into()],
            ))
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("hold sum returned no row".to_string()))?;
        let held_elsewhere: i64 = held_row.try_get("", "held")?;

        let available = i64::from(on_hand) - held_elsewhere;
        if available < i64::from(line.qty) {
            shortages.push(InsufficientLine {
                product_id: line.product_id,
                requested: line.qty,
                available: available.max(0) as i32,
            });
            continue;
        }
        tx.execute(stmt(
            "UPDATE inventory SET on_hand = on_hand - $2, updated_at = now() WHERE product_id = $1",
            [line.product_id.into(), line.qty.into()],
        ))
        .await?;
    }
    if !shortages.is_empty() {
        tx.rollback().await.map_err(OrderError::Db)?;
        return Err(OrderError::Insufficient(shortages));
    }

    let subtotal: i64 = sorted
        .iter()
        .map(|line| i64::from(line.unit_price_cents) * i64::from(line.qty))
        .sum();
    let shipping = ship_method.shipping_cents();
    let tax: i64 = 0; // Sales tax is calculated at fulfillment by jurisdiction.
    let total = subtotal + shipping + tax;
    let next_run_at = match (kind, frequency) {
        (OrderKind::Recurring, Some(freq)) => {
            Some(Utc::now() + chrono::Duration::days(freq.interval_days()))
        }
        _ => None,
    };
    let order_row = tx
        .query_one(stmt(
            "INSERT INTO orders \
             (user_id, kind, frequency, channel, ship_method, po_number, \
              subtotal_cents, shipping_cents, tax_cents, total_cents, next_run_at) \
             VALUES ($1, $2::order_kind, $3::order_frequency, $4::order_channel, \
                     $5::ship_method, $6, $7, $8, $9, $10, $11) \
             RETURNING id",
            [
                user_id.into(),
                kind.into(),
                frequency.into(),
                channel.into(),
                ship_method.into(),
                po_number.map(str::to_string).into(),
                subtotal.into(),
                shipping.into(),
                tax.into(),
                total.into(),
                next_run_at.into(),
            ],
        ))
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("order insert returned no row".to_string()))?;
    let order_id: Uuid = order_row.try_get("", "id")?;

    for line in &sorted {
        tx.execute(stmt(
            "INSERT INTO order_items (order_id, product_id, qty, unit_price_cents) \
             VALUES ($1, $2, $3, $4)",
            [
                order_id.into(),
                line.product_id.into(),
                line.qty.into(),
                line.unit_price_cents.into(),
            ],
        ))
        .await?;
    }

    if let Some(cart_id) = cart_id {
        tx.execute(stmt(
            "DELETE FROM stock_holds WHERE cart_id = $1",
            [cart_id.into()],
        ))
        .await?;
        tx.execute(stmt(
            "DELETE FROM cart_items WHERE cart_id = $1",
            [cart_id.into()],
        ))
        .await?;
    }
    tx.commit().await.map_err(OrderError::Db)?;
    Ok(order_id)
}

// ---------------------------------------------------------------------------
// Shipments / fulfillment (carrier + tracking). Populated by ops or an EDI
// 856 mapping; surfaced on the order-detail/receipt page. Raw SQL like the
// other new-table paths; entities can grow here once the schema settles.

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "String", db_type = "Enum", enum_name = "shipment_status")]
pub enum ShipmentStatus {
    #[sea_orm(string_value = "packing")]
    Packing,
    #[sea_orm(string_value = "shipped")]
    Shipped,
    #[sea_orm(string_value = "delivered")]
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
}

#[derive(Debug, Clone, FromQueryResult)]
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

#[derive(Debug, Clone, FromQueryResult)]
pub struct UserShipmentRow {
    pub order_id: Uuid,
    pub id: Uuid,
    pub status: ShipmentStatus,
    pub carrier: Option<String>,
    pub tracking_number: Option<String>,
    pub ship_date: Option<chrono::NaiveDate>,
    pub eta_earliest: Option<chrono::NaiveDate>,
    pub eta_latest: Option<chrono::NaiveDate>,
    pub delivered_at: Option<DateTime<Utc>>,
}

impl UserShipmentRow {
    pub fn shipment(&self) -> Shipment {
        Shipment {
            id: self.id,
            status: self.status,
            carrier: self.carrier.clone(),
            tracking_number: self.tracking_number.clone(),
            ship_date: self.ship_date,
            eta_earliest: self.eta_earliest,
            eta_latest: self.eta_latest,
            delivered_at: self.delivered_at,
        }
    }
}

const SHIPMENT_COLUMNS: &str = "s.id, s.status::text AS status, s.carrier, s.tracking_number, \
     s.ship_date, s.eta_earliest, s.eta_latest, s.delivered_at";

/// Shipments for one of `user_id`'s orders. The `orders` join scopes the read
/// to the owner for the same reason as [`order_items`]: RLS is bypassed, so
/// this predicate is the authorization boundary, not a redundant check.
pub async fn shipments_for_order(
    conn: &DatabaseConnection,
    user_id: Uuid,
    order_id: Uuid,
) -> Result<Vec<Shipment>, DbErr> {
    Shipment::find_by_statement(stmt(
        &format!(
            "SELECT {SHIPMENT_COLUMNS} FROM shipments s \
             JOIN orders o ON o.id = s.order_id \
             WHERE s.order_id = $1 AND o.user_id = $2 ORDER BY s.created_at"
        ),
        [order_id.into(), user_id.into()],
    ))
    .all(conn)
    .await
}

/// All shipments across a user's orders, for the order-list tracking column.
pub async fn shipments_for_user(
    conn: &DatabaseConnection,
    user_id: Uuid,
) -> Result<Vec<UserShipmentRow>, DbErr> {
    UserShipmentRow::find_by_statement(stmt(
        &format!(
            "SELECT s.order_id, {SHIPMENT_COLUMNS} FROM shipments s \
             JOIN orders o ON o.id = s.order_id \
             WHERE o.user_id = $1 ORDER BY s.created_at"
        ),
        [user_id.into()],
    ))
    .all(conn)
    .await
}

/// Record a fulfillment (ops / EDI 856): create a shipment with carrier +
/// tracking and advance the order to fulfilled. The caller is authenticated
/// separately with the dedicated operations credential; customer API keys
/// cannot mutate fulfillment state. Returns None for an unknown order.
pub async fn record_fulfillment(
    conn: &DatabaseConnection,
    order_id: Uuid,
    carrier: &str,
    tracking_number: &str,
    ship_date: chrono::NaiveDate,
) -> Result<Option<Uuid>, DbErr> {
    let tx = conn.begin().await?;
    let order = tx
        .query_one(stmt(
            "SELECT ship_method::text AS ship_method FROM orders \
             WHERE id = $1 FOR UPDATE",
            [order_id.into()],
        ))
        .await?;
    let Some(order) = order else {
        tx.rollback().await?;
        return Ok(None);
    };
    let ship_method = ShipMethod::parse(&order.try_get::<String>("", "ship_method")?)
        .unwrap_or(ShipMethod::Standard);

    let (min, max) = ship_method.eta_business_days();
    let inserted = tx
        .query_one(stmt(
            "INSERT INTO shipments \
             (order_id, status, carrier, tracking_number, ship_date, eta_earliest, eta_latest) \
             VALUES ($1, 'shipped', $2, $3, $4, $5, $6) RETURNING id",
            [
                order_id.into(),
                carrier.into(),
                tracking_number.into(),
                ship_date.into(),
                add_business_days(ship_date, min).into(),
                add_business_days(ship_date, max).into(),
            ],
        ))
        .await?
        .ok_or_else(|| DbErr::RecordNotFound("shipment insert returned no row".to_string()))?;
    let shipment_id: Uuid = inserted.try_get("", "id")?;

    tx.execute(stmt(
        "UPDATE orders SET status = 'fulfilled' WHERE id = $1",
        [order_id.into()],
    ))
    .await?;
    tx.commit().await?;
    Ok(Some(shipment_id))
}

/// Materialize every recurring order whose `next_run_at` is due. Each order is
/// processed in its own transaction guarded by a transaction-scoped advisory
/// lock on the order id, so two runners (or a runner and a bypassed leader
/// guard) can never fire the same subscription twice. Returns how many child
/// orders were created.
pub async fn run_due_recurring_orders(conn: &DatabaseConnection) -> Result<u64, DbErr> {
    // Provider-managed subscriptions (Stripe/PayPal/Square `mode=subscription`)
    // are driven by the provider, which charges the card and records a cycle
    // against the original order each period. Those orders must NOT also be
    // fired by this internal runner, or every cycle would mint an unpaid,
    // orphaned child order AND double-decrement stock. So skip any order that
    // has a provider subscription row; the runner only advances recurring
    // orders it owns (Net-30 / unpaid recurring with no provider subscription).
    let due = conn
        .query_all(stmt(
            "SELECT id FROM orders o \
             WHERE o.kind = 'recurring' AND o.status <> 'cancelled' \
             AND o.next_run_at IS NOT NULL AND o.next_run_at <= now() \
             AND NOT EXISTS ( \
                 SELECT 1 FROM payment_subscriptions ps WHERE ps.order_id = o.id \
             ) \
             ORDER BY o.next_run_at LIMIT 100",
            [],
        ))
        .await?;

    let mut created = 0u64;
    for row in due {
        let subscription_id: Uuid = row.try_get("", "id")?;
        let tx = conn.begin().await?;

        // hashtextextended gives a stable bigint key for the advisory lock;
        // xact-scoped so it releases with commit/rollback automatically.
        let got_row = tx
            .query_one(stmt(
                "SELECT pg_try_advisory_xact_lock(hashtextextended($1::text, 0)) AS got",
                [subscription_id.to_string().into()],
            ))
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("advisory lock returned no row".to_string()))?;
        let got: bool = got_row.try_get("", "got")?;
        if !got {
            tx.rollback().await?; // another runner owns this subscription
            continue;
        }

        // Re-read under the lock; skip if no longer due (a racing runner that
        // held the lock just before us already advanced it).
        let sub = tx
            .query_one(stmt(
                "SELECT user_id, frequency::text AS frequency, channel::text AS channel, \
                        ship_method::text AS ship_method FROM orders o \
                 WHERE o.id = $1 AND o.kind = 'recurring' AND o.status <> 'cancelled' \
                 AND o.next_run_at IS NOT NULL AND o.next_run_at <= now() \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM payment_subscriptions ps WHERE ps.order_id = o.id \
                 )",
                [subscription_id.into()],
            ))
            .await?;
        let Some(sub) = sub else {
            tx.rollback().await?;
            continue;
        };
        let user_id: Uuid = sub.try_get("", "user_id")?;
        let frequency: Option<String> = sub.try_get("", "frequency")?;
        let channel: String = sub.try_get("", "channel")?;
        let ship_method = ShipMethod::parse(&sub.try_get::<String>("", "ship_method")?)
            .unwrap_or(ShipMethod::Standard);

        let lines = tx
            .query_all(stmt(
                "SELECT product_id, qty, unit_price_cents FROM order_items WHERE order_id = $1",
                [subscription_id.into()],
            ))
            .await?
            .into_iter()
            .map(|line| {
                Ok((
                    line.try_get::<i64>("", "product_id")?,
                    line.try_get::<i32>("", "qty")?,
                    line.try_get::<i32>("", "unit_price_cents")?,
                ))
            })
            .collect::<Result<Vec<_>, DbErr>>()?;

        // Decrement stock for tracked products; if any line is short, skip the
        // child this cycle but still advance the cursor so the subscription
        // isn't wedged (a real system would backorder).
        let mut short = false;
        for (product_id, qty, _) in &lines {
            let on_hand = tx
                .query_one(stmt(
                    "SELECT on_hand FROM inventory WHERE product_id = $1 FOR UPDATE",
                    [(*product_id).into()],
                ))
                .await?;
            if let Some(on_hand_row) = on_hand {
                if on_hand_row.try_get::<i32>("", "on_hand")? < *qty {
                    short = true;
                    break;
                }
            }
        }

        if !short {
            for (product_id, qty, _) in &lines {
                tx.execute(stmt(
                    "UPDATE inventory SET on_hand = on_hand - $2, updated_at = now() \
                     WHERE product_id = $1",
                    [(*product_id).into(), (*qty).into()],
                ))
                .await?;
            }
            let subtotal: i64 = lines
                .iter()
                .map(|(_, qty, price)| i64::from(*price) * i64::from(*qty))
                .sum();
            let shipping = ship_method.shipping_cents();
            let child_row = tx
                .query_one(stmt(
                    "INSERT INTO orders \
                     (user_id, kind, channel, ship_method, subtotal_cents, shipping_cents, \
                      tax_cents, total_cents, recurs_from) \
                     VALUES ($1, 'one_time', $2::order_channel, $3::ship_method, $4, $5, 0, $6, $7) \
                     RETURNING id",
                    [
                        user_id.into(),
                        channel.into(),
                        ship_method.into(),
                        subtotal.into(),
                        shipping.into(),
                        (subtotal + shipping).into(),
                        subscription_id.into(),
                    ],
                ))
                .await?
                .ok_or_else(|| DbErr::RecordNotFound("child order insert returned no row".into()))?;
            let child_id: Uuid = child_row.try_get("", "id")?;
            for (product_id, qty, price) in &lines {
                tx.execute(stmt(
                    "INSERT INTO order_items (order_id, product_id, qty, unit_price_cents) \
                     VALUES ($1, $2, $3, $4)",
                    [
                        child_id.into(),
                        (*product_id).into(),
                        (*qty).into(),
                        (*price).into(),
                    ],
                ))
                .await?;
            }
            created += 1;
        } else {
            tracing::warn!(%subscription_id, "recurring order short on stock; skipping this cycle");
        }

        let interval = frequency
            .as_deref()
            .and_then(|f| match f {
                "weekly" => Some(7),
                "biweekly" => Some(14),
                "monthly" => Some(30),
                "quarterly" => Some(90),
                _ => None,
            })
            .unwrap_or(30);
        tx.execute(stmt(
            "UPDATE orders SET next_run_at = next_run_at + make_interval(days => $2) WHERE id = $1",
            [subscription_id.into(), interval.into()],
        ))
        .await?;
        tx.commit().await?;
    }
    Ok(created)
}

// ---------------------------------------------------------------------------
// Payments. These tables are new and therefore use SeaORM exclusively.

pub async fn set_order_payment(
    conn: &DatabaseConnection,
    order_id: Uuid,
    provider: PaymentProvider,
    payment_ref: &str,
    status: PaymentStatus,
) -> Result<(), DbErr> {
    let Some(row) = order::Entity::find_by_id(order_id).one(conn).await? else {
        return Ok(());
    };
    let mut active: order::ActiveModel = row.into();
    active.payment_provider = Set(Some(provider));
    active.payment_ref = Set(Some(payment_ref.to_string()));
    active.payment_status = Set(status);
    active.update(conn).await?;
    Ok(())
}

pub async fn set_order_payment_status(
    conn: &DatabaseConnection,
    order_id: Uuid,
    status: PaymentStatus,
) -> Result<(), DbErr> {
    let Some(row) = order::Entity::find_by_id(order_id).one(conn).await? else {
        return Ok(());
    };
    let stamp_paid = status == PaymentStatus::Paid && row.paid_at.is_none();
    let mut active: order::ActiveModel = row.into();
    active.payment_status = Set(status);
    if stamp_paid {
        active.paid_at = Set(Some(Utc::now()));
    }
    active.update(conn).await?;
    Ok(())
}

#[derive(Debug, Clone)]
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

impl From<order::Model> for OrderPaymentFacts {
    fn from(row: order::Model) -> Self {
        Self {
            id: row.id,
            user_id: row.user_id,
            total_cents: row.total_cents,
            kind: row.kind,
            frequency: row.frequency,
            payment_provider: row.payment_provider,
            payment_status: row.payment_status,
            payment_ref: row.payment_ref,
        }
    }
}

pub async fn order_payment_facts(
    conn: &DatabaseConnection,
    order_id: Uuid,
) -> Result<Option<OrderPaymentFacts>, DbErr> {
    Ok(order::Entity::find_by_id(order_id)
        .one(conn)
        .await?
        .map(OrderPaymentFacts::from))
}

pub async fn find_order_by_payment_ref(
    conn: &DatabaseConnection,
    provider: PaymentProvider,
    payment_ref: &str,
) -> Result<Option<Uuid>, DbErr> {
    Ok(order::Entity::find()
        .filter(order::Column::PaymentProvider.eq(provider))
        .filter(order::Column::PaymentRef.eq(payment_ref))
        .one(conn)
        .await?
        .map(|row| row.id))
}

#[allow(clippy::too_many_arguments)]
pub async fn record_payment(
    conn: &DatabaseConnection,
    order_id: Option<Uuid>,
    user_id: Uuid,
    provider: PaymentProvider,
    kind: PaymentKind,
    provider_ref: &str,
    amount_cents: i64,
    status: PaymentStatus,
) -> Result<bool, DbErr> {
    let existing = payment::Entity::find()
        .filter(payment::Column::Provider.eq(provider))
        .filter(payment::Column::ProviderRef.eq(provider_ref))
        .one(conn)
        .await?;
    match existing {
        Some(row) => {
            let mut active: payment::ActiveModel = row.into();
            active.status = Set(status);
            active.updated_at = Set(Utc::now());
            active.update(conn).await?;
            Ok(false)
        }
        None => {
            payment::ActiveModel {
                id: Set(Uuid::new_v4()),
                order_id: Set(order_id),
                user_id: Set(user_id),
                provider: Set(provider),
                kind: Set(kind),
                provider_ref: Set(provider_ref.to_string()),
                amount_cents: Set(amount_cents),
                currency: Set("USD".to_string()),
                status: Set(status),
                created_at: NotSet,
                updated_at: NotSet,
            }
            .insert(conn)
            .await?;
            Ok(true)
        }
    }
}

pub async fn upsert_subscription(
    conn: &DatabaseConnection,
    user_id: Uuid,
    order_id: Option<Uuid>,
    provider: PaymentProvider,
    provider_ref: &str,
    status: SubscriptionStatus,
    frequency: OrderFrequency,
) -> Result<(), DbErr> {
    let existing = payment_subscription::Entity::find()
        .filter(payment_subscription::Column::Provider.eq(provider))
        .filter(payment_subscription::Column::ProviderRef.eq(provider_ref))
        .one(conn)
        .await?;
    match existing {
        Some(row) => {
            let mut active: payment_subscription::ActiveModel = row.into();
            active.status = Set(status);
            active.updated_at = Set(Utc::now());
            active.update(conn).await?;
        }
        None => {
            payment_subscription::ActiveModel {
                id: Set(Uuid::new_v4()),
                user_id: Set(user_id),
                order_id: Set(order_id),
                provider: Set(provider),
                provider_ref: Set(provider_ref.to_string()),
                status: Set(status),
                frequency: Set(frequency),
                created_at: NotSet,
                updated_at: NotSet,
            }
            .insert(conn)
            .await?;
        }
    }
    Ok(())
}

pub async fn set_subscription_status(
    conn: &DatabaseConnection,
    provider: PaymentProvider,
    provider_ref: &str,
    status: SubscriptionStatus,
) -> Result<(), DbErr> {
    let Some(row) = payment_subscription::Entity::find()
        .filter(payment_subscription::Column::Provider.eq(provider))
        .filter(payment_subscription::Column::ProviderRef.eq(provider_ref))
        .one(conn)
        .await?
    else {
        return Ok(());
    };
    let mut active: payment_subscription::ActiveModel = row.into();
    active.status = Set(status);
    active.updated_at = Set(Utc::now());
    active.update(conn).await?;
    Ok(())
}

pub async fn subscription_owner(
    conn: &DatabaseConnection,
    provider: PaymentProvider,
    provider_ref: &str,
) -> Result<Option<(Uuid, Option<Uuid>)>, DbErr> {
    Ok(payment_subscription::Entity::find()
        .filter(payment_subscription::Column::Provider.eq(provider))
        .filter(payment_subscription::Column::ProviderRef.eq(provider_ref))
        .one(conn)
        .await?
        .map(|row| (row.user_id, row.order_id)))
}

pub async fn record_payment_event(
    conn: &DatabaseConnection,
    provider: PaymentProvider,
    event_id: &str,
    payload: &serde_json::Value,
) -> Result<bool, DbErr> {
    let outcome = payment_event::Entity::insert(payment_event::ActiveModel {
        provider: Set(provider),
        event_id: Set(event_id.to_string()),
        payload: Set(payload.clone()),
        received_at: NotSet,
    })
    .on_conflict(
        OnConflict::columns([
            payment_event::Column::Provider,
            payment_event::Column::EventId,
        ])
        .do_nothing()
        .to_owned(),
    )
    .do_nothing()
    .exec(conn)
    .await?;
    Ok(matches!(outcome, TryInsertResult::Inserted(_)))
}

pub async fn latest_email_for_user(
    conn: &DatabaseConnection,
    user_id: Uuid,
) -> Result<Option<String>, DbErr> {
    Ok(login_event::Entity::find()
        .filter(login_event::Column::UserId.eq(user_id))
        .order_by_desc(login_event::Column::CreatedAt)
        .one(conn)
        .await?
        .map(|row| row.email))
}

#[cfg(test)]
mod order_fulfillment_tests {
    use super::*;

    #[test]
    fn business_days_skip_weekends() {
        // Fri 2026-07-17 + 1 business day = Mon 2026-07-20 (skips Sat/Sun).
        let fri = chrono::NaiveDate::from_ymd_opt(2026, 7, 17).unwrap();
        assert_eq!(
            add_business_days(fri, 1),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 20).unwrap()
        );
        // + 5 business days = next Fri.
        assert_eq!(
            add_business_days(fri, 5),
            chrono::NaiveDate::from_ymd_opt(2026, 7, 24).unwrap()
        );
    }

    #[test]
    fn ship_method_windows_and_shipping_are_ordered() {
        assert_eq!(ShipMethod::Standard.shipping_cents(), 599);
        assert_eq!(ShipMethod::Freight.shipping_cents(), 0);
        assert_eq!(ShipMethod::Expedited.eta_business_days(), (1, 2));
        assert_eq!(ShipMethod::Standard.eta_business_days(), (3, 5));
        assert_eq!(ShipMethod::parse("expedited"), Some(ShipMethod::Expedited));
        assert_eq!(ShipMethod::parse("nonsense"), None);
    }

    fn shipment(carrier: &str, number: &str) -> Shipment {
        Shipment {
            id: Uuid::nil(),
            status: ShipmentStatus::Shipped,
            carrier: Some(carrier.into()),
            tracking_number: Some(number.into()),
            ship_date: None,
            eta_earliest: None,
            eta_latest: None,
            delivered_at: None,
        }
    }

    #[test]
    fn tracking_url_matches_major_carriers_only() {
        assert!(shipment("UPS", "1Z999")
            .tracking_url()
            .unwrap()
            .contains("ups.com/track?tracknum=1Z999"));
        assert!(shipment("FedEx", "7712")
            .tracking_url()
            .unwrap()
            .contains("fedex.com"));
        // Unknown carrier -> no deep link (UI falls back to plain text).
        assert!(shipment("Regional Freight Co", "ABC")
            .tracking_url()
            .is_none());
        // No number -> no link.
        let mut s = shipment("UPS", "x");
        s.tracking_number = None;
        assert!(s.tracking_url().is_none());
    }

    #[test]
    fn order_status_parse_round_trips() {
        for status in [
            OrderStatus::Placed,
            OrderStatus::Processing,
            OrderStatus::Fulfilled,
            OrderStatus::Cancelled,
        ] {
            assert_eq!(OrderStatus::parse(status.label()), Some(status));
        }
        assert_eq!(OrderStatus::parse("bogus"), None);
    }
}

#[cfg(test)]
mod payment_enum_tests {
    use sea_orm::ActiveEnum;

    use super::*;

    #[test]
    fn sea_orm_string_values_match_payment_migrations() {
        assert_eq!(PaymentProvider::Stripe.to_value(), "stripe");
        assert_eq!(PaymentProvider::Paypal.to_value(), "paypal");
        assert_eq!(PaymentProvider::Square.to_value(), "square");
        assert_eq!(PaymentProvider::Invoice.to_value(), "invoice");
        assert_eq!(PaymentStatus::Pending.to_value(), "pending");
        assert_eq!(PaymentStatus::Processing.to_value(), "processing");
        assert_eq!(PaymentStatus::Paid.to_value(), "paid");
        assert_eq!(PaymentStatus::Invoiced.to_value(), "invoiced");
        assert_eq!(PaymentStatus::Failed.to_value(), "failed");
        assert_eq!(PaymentStatus::Refunded.to_value(), "refunded");
        assert_eq!(PaymentKind::Charge.to_value(), "charge");
        assert_eq!(
            PaymentKind::SubscriptionCycle.to_value(),
            "subscription_cycle"
        );
        assert_eq!(PaymentKind::Refund.to_value(), "refund");
        assert_eq!(SubscriptionStatus::Pending.to_value(), "pending");
        assert_eq!(SubscriptionStatus::Active.to_value(), "active");
        assert_eq!(SubscriptionStatus::PastDue.to_value(), "past_due");
        assert_eq!(SubscriptionStatus::Cancelled.to_value(), "cancelled");
    }
}
