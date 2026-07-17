//! AthletO shop app library crate: configuration, shared state, and the
//! router live here so integration tests can drive the real router without
//! booting the binary.

pub mod account;
pub mod api;
pub mod assets;
pub mod auth;
pub mod cart;
pub mod db;
pub mod entities;
pub mod orders;
pub mod pages;
pub mod security;
pub mod ws;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// Hard ceiling on request bodies. Storefront forms and the JSON API are tiny;
/// anything larger is abuse, so cap it before a handler buffers it.
const MAX_BODY_BYTES: usize = 512 * 1024;
/// Whole-request timeout. Bounds slow-body / slow-handler resource holding.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

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
    /// Host-header allowlist (ALLOWED_HOSTS, comma-separated). `None` means
    /// permissive -- dev convenience -- with a warning logged at startup.
    pub allowed_hosts: Option<Vec<String>>,
    /// SMS second factors need the Supabase phone-MFA add-on plus a
    /// configured SMS provider; the UI stays hidden until this is set.
    pub sms_mfa_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            supabase_url: None,
            supabase_anon_key: None,
            public_base_url: "https://app.athleto.store".to_string(),
            allowed_hosts: None,
            sms_mfa_enabled: false,
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

    /// True when the inbound Host header may be trusted (for auth redirect
    /// bases and the biz.-host chrome). Ports are ignored so
    /// `localhost:8080` matches an allowlisted `localhost`.
    pub fn host_allowed(&self, host: &str) -> bool {
        let Some(allowed) = &self.allowed_hosts else {
            return true; // permissive when unset; warned about at startup
        };
        let bare = host.split(':').next().unwrap_or(host);
        allowed.iter().any(|entry| entry == bare)
    }
}

pub struct AppState {
    /// `None` when DATABASE_URL is unset; product pages fall back to the
    /// built-in catalog and cart routes show a "not configured" notice.
    pub pool: Option<sea_orm::DatabaseConnection>,
    pub http: reqwest::Client,
    pub config: Config,
    /// Per-IP / per-email throttle for the magic-link login endpoint.
    pub login_limiter: security::RateLimiter,
    /// Per-IP throttle for `POST /cart/items`, which reserves stock holds and
    /// is reachable anonymously; keeps a script from mass-reserving inventory.
    pub cart_limiter: security::RateLimiter,
    /// Cart mutations broadcast the affected cart id; /ws connections push a
    /// fresh hold-countdown fragment to their owner.
    pub cart_events: tokio::sync::broadcast::Sender<uuid::Uuid>,
}

impl AppState {
    pub fn new(
        pool: Option<sea_orm::DatabaseConnection>,
        http: reqwest::Client,
        config: Config,
    ) -> Self {
        let (cart_events, _) = tokio::sync::broadcast::channel(64);
        Self {
            pool,
            http,
            config,
            login_limiter: security::RateLimiter::new(),
            cart_limiter: security::RateLimiter::new(),
            cart_events,
        }
    }
}

pub type SharedState = Arc<AppState>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),
    #[error("upstream request error: {0}")]
    Upstream(#[from] reqwest::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Log the detail server-side only; users get a generic page with an
        // id they can quote, never the raw error (which may leak SQL/URLs).
        let error_id = uuid::Uuid::new_v4();
        tracing::error!(error = %self, %error_id, "request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            pages::error_page(&format!(
                "Something went wrong on our side. If it keeps happening, \
                 mention error id {error_id}."
            )),
        )
            .into_response()
    }
}

async fn healthz() -> &'static str {
    "ok"
}

pub fn router(state: SharedState) -> Router {
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
        // Cart + holds. /ws pushes the hold countdown as htmx OOB fragments;
        // GET /cart/hold polling stays as the fallback.
        .route("/ws", get(ws::upgrade))
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
        // Vendored assets (htmx + extensions), served same-origin for the CSP.
        .route(assets::HTMX_JS_PATH, get(assets::htmx_js))
        .route(assets::HTMX_WS_JS_PATH, get(assets::htmx_ws_js))
        .route("/healthz", get(healthz))
        // Innermost first: bound request bodies and total handler time. These
        // sit *inside* the security layer so their 413/408 responses still flow
        // back through `security::apply` and pick up the security headers.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        // CSRF enforcement + security headers on every route (incl. the 404
        // fallback); /api/v1 is CSRF-exempt inside the middleware.
        .layer(axum::middleware::from_fn(security::apply))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
