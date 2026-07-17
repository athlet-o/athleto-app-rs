use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use athleto_app_rs::{db, router, AppState, Config, SharedState};

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
        allowed_hosts: env_opt("ALLOWED_HOSTS").map(|hosts| {
            hosts
                .split(',')
                .map(|host| host.trim().to_string())
                .filter(|host| !host.is_empty())
                .collect()
        }),
        sms_mfa_enabled: env_opt("ATHLETO_SMS_MFA_ENABLED").as_deref() == Some("1"),
    };
    if config.supabase().is_none() {
        tracing::warn!("SUPABASE_URL / SUPABASE_ANON_KEY not set; auth routes will show a 'not configured' notice");
    }
    if config.allowed_hosts.is_none() {
        tracing::warn!(
            "ALLOWED_HOSTS not set; trusting any inbound Host header (fine for dev, set it in production)"
        );
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

    let state: SharedState = Arc::new(AppState::new(pool, reqwest::Client::new(), config));

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
