use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use athleto_app_rs::{coordinate, db, router, AppState, Config, SharedState};

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
        fiducia_url: env_opt("FIDUCIA_URL"),
        fiducia_api_key: env_opt("FIDUCIA_API_KEY"),
        // HOSTNAME is the pod name under Kubernetes; unique per replica.
        replica_id: env_opt("HOSTNAME").unwrap_or_else(|| "local".to_string()),
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
            // blocks on database availability. `sqlx::migrate!` runs on the
            // sqlx pool underneath the SeaORM connection.
            let migrate_pool = pool.get_postgres_connection_pool().clone();
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
            // configured, else a Postgres advisory lock). First ticks are
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
            // replenishment. Each due order is additionally claimed under a
            // transaction-scoped advisory lock inside
            // db::run_due_recurring_orders, so even if the leader guard were
            // bypassed no subscription could double-fire.
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
