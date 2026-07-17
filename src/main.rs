mod account;
mod api;
mod auth;
mod billing;
mod cart;
mod coordinate;
mod db;
mod entities;
mod orders;
mod pages;
mod payments;
mod secrets;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

pub use auth::{auth_session_cookie, refresh_session_cookie};

/// Environment-derived configuration. Every field is optional so the app can
/// boot (and pass health checks) with no secrets present; features that need
/// a missing value degrade to a "not configured" notice instead of failing.
#[derive(Clone, Debug)]
pub struct Config {
    pub supabase_url: Option<String>,
    pub supabase_anon_key: Option<String>,
    /// Fallback origin for auth redirects when the Host header is absent.
    pub public_base_url: String,
    /// SMS second factors need the Supabase phone-MFA add-on plus a
    /// configured SMS provider; the UI stays hidden until this is set.
    pub sms_mfa_enabled: bool,
    /// fiducia.cloud lock service, used only for singleton-job leadership
    /// leases (never for cart holds). Both must be set to activate; otherwise
    /// leadership falls back to a Postgres advisory lock.
    pub fiducia_url: Option<String>,
    pub fiducia_api_key: Option<String>,
    /// Identifies this replica in fiducia lease holder strings.
    pub replica_id: String,
    /// Payment processors; each is independently optional and the checkout
    /// only offers the ones with keys present.
    pub stripe: Option<payments::StripeConfig>,
    pub paypal: Option<payments::PayPalConfig>,
    pub square: Option<payments::SquareConfig>,
    /// Quaestor billing-server (observer AR/AP ledger for balances/credits).
    pub billing: Option<billing::BillingConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            supabase_url: None,
            supabase_anon_key: None,
            public_base_url: "https://app.athleto.store".to_string(),
            sms_mfa_enabled: false,
            fiducia_url: None,
            fiducia_api_key: None,
            replica_id: "local".to_string(),
            stripe: None,
            paypal: None,
            square: None,
            billing: None,
        }
    }
}

impl Config {
    /// Returns `(base_url, anon_key)` when Supabase auth is fully configured.
    pub fn supabase(&self) -> Option<(&str, &str)> {
        match (
            self.supabase_url.as_deref(),
            self.supabase_anon_key.as_deref(),
        ) {
            (Some(url), Some(key)) => Some((url, key)),
            _ => None,
        }
    }
}

pub struct AppState {
    /// `None` when DATABASE_URL is unset; product pages fall back to the
    /// built-in catalog and cart routes show a "not configured" notice.
    pub pool: Option<sqlx::PgPool>,
    /// SeaORM handle over the same pool — the data-access layer for all new
    /// code (payments, billing); see src/entities.rs.
    pub orm: Option<sea_orm::DatabaseConnection>,
    pub http: reqwest::Client,
    pub config: Config,
}

pub type SharedState = Arc<AppState>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("upstream request error: {0}")]
    Upstream(#[from] reqwest::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self, "request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::error_page(&self.to_string()),
        )
            .into_response()
    }
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn healthz() -> &'static str {
    "ok"
}

fn router(state: SharedState) -> Router {
    Router::new()
        .route("/", get(pages::home))
        .route("/product/{slug}", get(pages::product_page))
        // Passwordless auth: magic links in, MFA upgrade, remembered emails.
        .route("/login", get(auth::login_page).post(auth::login_submit))
        .route("/login/2fa", get(auth::login_2fa_page).post(auth::login_2fa_submit))
        .route("/login/2fa/send", post(auth::login_2fa_send))
        .route("/login/remembered", get(auth::remembered_page))
        .route("/auth/callback", get(auth::auth_callback))
        .route("/auth/confirm", get(auth::auth_confirm))
        .route("/auth/session", post(auth::auth_session))
        .route("/logout", post(auth::logout))
        // The old password signup page; magic-link login signs new users up.
        .route("/signup", get(|| async { Redirect::permanent("/login") }))
        // Account: B2C/B2B profile, 2FA, API keys.
        .route("/account", get(account::account_page))
        .route("/account/setup", get(account::setup_page).post(account::setup_submit))
        .route("/account/2fa/totp", post(account::totp_enroll))
        .route("/account/2fa/totp/verify", post(account::totp_verify))
        .route("/account/2fa/phone", post(account::phone_enroll))
        .route("/account/2fa/phone/verify", post(account::phone_verify))
        .route("/account/2fa/{factor_id}/unenroll", post(account::factor_unenroll))
        .route("/account/api-keys", post(account::api_key_create))
        .route("/account/api-keys/{key_id}/revoke", post(account::api_key_revoke))
        // Cart + holds.
        .route("/cart", get(cart::cart_page))
        .route("/cart/hold", get(cart::hold_status))
        .route("/cart/items", post(cart::add_item))
        .route("/cart/items/{id}/delete", post(cart::delete_item))
        // Orders.
        .route("/checkout", post(orders::checkout))
        .route("/orders", get(orders::orders_page))
        .route("/orders/{id}", get(orders::order_detail_page))
        .route("/orders/{id}/reorder", post(orders::reorder))
        .route("/quick-order", get(orders::quick_order_page).post(orders::quick_order_submit))
        // B2B ERP API.
        .route("/api/v1/products", get(api::products))
        .route("/api/v1/orders", get(api::orders_list).post(api::orders_create))
        .route("/api/v1/orders/{id}/fulfillment", post(api::order_fulfill))
        .route("/healthz", get(healthz))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let host = env_opt("HOST").unwrap_or_else(|| "0.0.0.0".to_string());
    let port: u16 = env_opt("PORT")
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);

    let config = Config {
        supabase_url: env_opt("SUPABASE_URL").map(|url| url.trim_end_matches('/').to_string()),
        supabase_anon_key: env_opt("SUPABASE_ANON_KEY"),
        public_base_url: env_opt("ATHLETO_PUBLIC_BASE_URL")
            .unwrap_or_else(|| "https://app.athleto.store".to_string()),
        sms_mfa_enabled: env_opt("ATHLETO_SMS_MFA_ENABLED").as_deref() == Some("1"),
        fiducia_url: env_opt("FIDUCIA_URL"),
        fiducia_api_key: env_opt("FIDUCIA_API_KEY"),
        // HOSTNAME is the pod name under Kubernetes; unique per replica.
        replica_id: env_opt("HOSTNAME").unwrap_or_else(|| "local".to_string()),
        stripe: env_opt("ATHLETO_STRIPE_SECRET_KEY").map(|secret_key| payments::StripeConfig {
            secret_key,
            webhook_secret: env_opt("ATHLETO_STRIPE_WEBHOOK_SECRET"),
        }),
        paypal: match (env_opt("ATHLETO_PAYPAL_CLIENT_ID"), env_opt("ATHLETO_PAYPAL_CLIENT_SECRET")) {
            (Some(client_id), Some(client_secret)) => Some(payments::PayPalConfig {
                client_id,
                client_secret,
                webhook_id: env_opt("ATHLETO_PAYPAL_WEBHOOK_ID"),
                api_base: match env_opt("ATHLETO_PAYPAL_ENV").as_deref() {
                    Some("live") => "https://api-m.paypal.com".to_string(),
                    _ => "https://api-m.sandbox.paypal.com".to_string(),
                },
            }),
            _ => None,
        },
        square: match (env_opt("ATHLETO_SQUARE_ACCESS_TOKEN"), env_opt("ATHLETO_SQUARE_LOCATION_ID")) {
            (Some(access_token), Some(location_id)) => Some(payments::SquareConfig {
                access_token,
                location_id,
                webhook_signature_key: env_opt("ATHLETO_SQUARE_WEBHOOK_SIGNATURE_KEY"),
                api_base: match env_opt("ATHLETO_SQUARE_ENV").as_deref() {
                    Some("production") => "https://connect.squareup.com".to_string(),
                    _ => "https://connect.squareupsandbox.com".to_string(),
                },
            }),
            _ => None,
        },
        billing: match (
            env_opt("ATHLETO_BILLING_URL"),
            env_opt("ATHLETO_BILLING_TENANT_ID").and_then(|id| id.parse().ok()),
        ) {
            (Some(url), Some(tenant_id)) => Some(billing::BillingConfig {
                url: url.trim_end_matches('/').to_string(),
                api_key: env_opt("ATHLETO_BILLING_API_KEY"),
                tenant_id,
            }),
            _ => None,
        },
    };
    if config.supabase().is_none() {
        tracing::warn!("SUPABASE_URL / SUPABASE_ANON_KEY not set; auth routes will show a 'not configured' notice");
    }

    let pool = env_opt("DATABASE_URL").and_then(|url| db::build_pool(&url));
    match &pool {
        Some(pool) => {
            // Run migrations in the background so startup (and /healthz) never
            // blocks on database availability.
            let migrate_pool = pool.clone();
            tokio::spawn(async move {
                match sqlx::migrate!().run(&migrate_pool).await {
                    Ok(()) => tracing::info!("database migrations applied"),
                    Err(err) => {
                        tracing::error!(error = %err, "database migrations failed; continuing")
                    }
                }
            });

            // Singleton background jobs. Across replicas exactly one runs each
            // tick, chosen by coordinate::try_lead (a fiducia lease when
            // configured, else a Postgres advisory lock). The first tick is
            // delayed so nothing races the migrations on a fresh database.

            // Expired-hold sweeper -- hygiene only (claims/availability already
            // treat stale holds as free via lazy expiry).
            let sweep_pool = pool.clone();
            let sweep_config = config.clone();
            tokio::spawn(async move {
                let period = Duration::from_secs(15 * 60);
                let mut ticker =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                loop {
                    ticker.tick().await;
                    let Some(lead) =
                        coordinate::try_lead(&sweep_pool, &sweep_config, "hold-sweeper", 120).await
                    else {
                        continue; // another replica is sweeping this tick
                    };
                    match db::sweep_expired_holds(&sweep_pool).await {
                        Ok(0) => {}
                        Ok(swept) => tracing::info!(swept, "cleared expired stock holds"),
                        Err(err) => tracing::warn!(error = %err, "hold sweep failed"),
                    }
                    lead.release().await;
                }
            });

            // Recurring-order runner -- materializes due subscriptions / B2B
            // replenishment. Each due order is claimed and advanced inside one
            // transaction (see db::run_due_recurring_orders), so even if the
            // leader guard were bypassed no order could double-fire.
            let recur_pool = pool.clone();
            let recur_config = config.clone();
            tokio::spawn(async move {
                let period = Duration::from_secs(10 * 60);
                let mut ticker =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                loop {
                    ticker.tick().await;
                    let Some(lead) = coordinate::try_lead(
                        &recur_pool,
                        &recur_config,
                        "recurring-runner",
                        120,
                    )
                    .await
                    else {
                        continue;
                    };
                    match db::run_due_recurring_orders(&recur_pool).await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(materialized = n, "recurring orders advanced"),
                        Err(err) => tracing::warn!(error = %err, "recurring runner failed"),
                    }
                    lead.release().await;
                }
            });
        }
        None => {
            tracing::warn!(
                "DATABASE_URL not set; cart persistence disabled, storefront uses built-in catalog"
            );
        }
    }

    let orm = pool
        .clone()
        .map(sea_orm::SqlxPostgresConnector::from_sqlx_postgres_pool);
    let state: SharedState = Arc::new(AppState {
        pool,
        orm,
        http: reqwest::Client::new(),
        config,
    });

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "athleto-app-rs listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
