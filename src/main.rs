mod account;
mod api;
mod auth;
mod cart;
mod coordinate;
mod db;
mod orders;
mod pages;

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
        .route("/quick-order", get(orders::quick_order_page).post(orders::quick_order_submit))
        // B2B ERP API.
        .route("/api/v1/products", get(api::products))
        .route("/api/v1/orders", get(api::orders_list).post(api::orders_create))
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

            // Expired-hold sweeper. Hygiene only: claims and availability
            // already treat stale holds as free (lazy expiry). Runs in-process
            // because the app is single-replica; with more replicas this wants
            // leader election so exactly one runs it.
            let sweep_pool = pool.clone();
            tokio::spawn(async move {
                // First tick is delayed so the sweep never races the
                // migrations that create stock_holds on a fresh database.
                let period = Duration::from_secs(15 * 60);
                let mut ticker =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                loop {
                    ticker.tick().await;
                    match db::sweep_expired_holds(&sweep_pool).await {
                        Ok(0) => {}
                        Ok(swept) => tracing::info!(swept, "cleared expired stock holds"),
                        Err(err) => tracing::warn!(error = %err, "hold sweep failed"),
                    }
                }
            });
        }
        None => {
            tracing::warn!(
                "DATABASE_URL not set; cart persistence disabled, storefront uses built-in catalog"
            );
        }
    }

    let state: SharedState = Arc::new(AppState {
        pool,
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
