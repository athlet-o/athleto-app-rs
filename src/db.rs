//! Pool setup plus product and cart queries.
//!
//! All queries are runtime `sqlx::query` / `query_as` calls (no compile-time
//! `query!` macros) so the crate builds without a live DATABASE_URL.

use std::time::Duration;

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
    let row: Option<(Uuid,)> = sqlx::query_as(&sql).bind(owner.id()).fetch_optional(pool).await?;
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
    let (id,): (Uuid,) = sqlx::query_as(&sql).bind(owner.id()).fetch_one(pool).await?;
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
        "SELECT ci.id AS item_id, p.name, p.subname, p.format, p.calories, p.price_cents, ci.qty \
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
            "Recover-O",
            "Berry-orange recovery wobble for the ride home. Gelatin protein plus magnesium, potassium, vitamin C, fiber, and live cultures in a ready cup.",
            ProductFormat::Cup,
            90,
            22,
            499,
        ),
        product(
            4,
            "recover-o-powder",
            "Recover-O",
            "Berry-orange recovery wobble for the ride home. Gelatin protein plus magnesium, potassium, vitamin C, fiber, and live cultures -- just add water and chill.",
            ProductFormat::Powder,
            80,
            22,
            329,
        ),
        product(
            5,
            "pre-game-o-cup",
            "Pre-Game-O",
            "Citrus-punch prep cup for pre-game rituals. Sodium, potassium, and vitamin C with gelatin protein and no sugar rush, ready to eat.",
            ProductFormat::Cup,
            85,
            15,
            399,
        ),
        product(
            6,
            "pre-game-o-powder",
            "Pre-Game-O",
            "Citrus-punch prep for pre-game rituals. Sodium, potassium, and vitamin C with gelatin protein and no sugar rush -- just add water and chill.",
            ProductFormat::Powder,
            75,
            15,
            249,
        ),
    ]
}
