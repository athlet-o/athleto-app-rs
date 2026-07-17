mod auth;
mod cart;
mod db;
mod pages;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tower_http::trace::TraceLayer;

/// Environment-derived configuration. Every field is optional so the app can
/// boot (and pass health checks) with no secrets present; features that need
/// a missing value degrade to a "not configured" notice instead of failing.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub supabase_url: Option<String>,
    pub supabase_anon_key: Option<String>,
}

impl Config {
    /// Returns `(base_url, anon_key)` when Supabase auth is fully configured.
    pub fn supabase(&self) -> Option<(&str, &str)> {
        match (self.supabase_url.as_deref(), self.supabase_anon_key.as_deref()) {
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
        .route("/signup", get(auth::signup_page).post(auth::signup_submit))
        .route("/login", get(auth::login_page).post(auth::login_submit))
        .route("/logout", post(auth::logout))
        .route("/cart", get(cart::cart_page))
        .route("/cart/items", post(cart::add_item))
        .route("/cart/items/{id}/delete", post(cart::delete_item))
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
    };
    if config.supabase().is_none() {
        tracing::warn!("SUPABASE_URL / SUPABASE_ANON_KEY not set; auth routes will show a 'not configured' notice");
    }

    let pool = env_opt("DATABASE_URL").and_then(|url| db::build_pool(&url));
    match &pool {
        Some(pool) => {
            // Run migrations in the background so startup (and /healthz) never
            // blocks on database availability.
            let pool = pool.clone();
            tokio::spawn(async move {
                match sqlx::migrate!().run(&pool).await {
                    Ok(()) => tracing::info!("database migrations applied"),
                    Err(err) => {
                        tracing::error!(error = %err, "database migrations failed; continuing")
                    }
                }
            });
        }
        None => {
            tracing::warn!("DATABASE_URL not set; cart persistence disabled, storefront uses built-in catalog");
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
