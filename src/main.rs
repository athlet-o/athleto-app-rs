use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use athleto_app_rs::{
    billing, coordinate, db, payments, router, secrets, AppState, Config, SharedState,
};

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

    // The Fiducia bootstrap credentials must be available before the encrypted
    // overlay can be read. `SecretSource` keeps ordinary environment variables
    // authoritative and treats an unavailable overlay as optional.
    let fiducia_url = env_opt("FIDUCIA_URL");
    let fiducia_api_key = env_opt("FIDUCIA_API_KEY");
    let fiducia_client = match (fiducia_url.clone(), fiducia_api_key.clone()) {
        (Some(url), Some(key)) => Some(coordinate::FiduciaClient::new(url, key)),
        _ => None,
    };
    let secret = secrets::SecretSource::load(fiducia_client.as_ref()).await;

    let config = Config {
        supabase_url: secret
            .get("SUPABASE_URL")
            .map(|url| url.trim_end_matches('/').to_string()),
        supabase_anon_key: secret.get("SUPABASE_ANON_KEY"),
        public_base_url: secret
            .get("ATHLETO_PUBLIC_BASE_URL")
            .unwrap_or_else(|| "https://app.athleto.store".to_string()),
        biz_public_base_url: secret
            .get("ATHLETO_BIZ_PUBLIC_BASE_URL")
            .unwrap_or_else(|| "https://biz.athleto.store".to_string()),
        allowed_hosts: secret.get("ALLOWED_HOSTS").map(|hosts| {
            hosts
                .split(',')
                .map(|host| host.trim().to_string())
                .filter(|host| !host.is_empty())
                .collect()
        }),
        operations_api_key: secret.get("ATHLETO_OPERATIONS_API_KEY"),
        sms_mfa_enabled: secret.get("ATHLETO_SMS_MFA_ENABLED").as_deref() == Some("1"),
        fiducia_url,
        fiducia_api_key,
        replica_id: env_opt("HOSTNAME").unwrap_or_else(|| "local".to_string()),
        stripe: secret
            .get("ATHLETO_STRIPE_SECRET_KEY")
            .map(|secret_key| payments::StripeConfig {
                secret_key,
                webhook_secret: secret.get("ATHLETO_STRIPE_WEBHOOK_SECRET"),
            }),
        paypal: match (
            secret.get("ATHLETO_PAYPAL_CLIENT_ID"),
            secret.get("ATHLETO_PAYPAL_CLIENT_SECRET"),
        ) {
            (Some(client_id), Some(client_secret)) => Some(payments::PayPalConfig {
                client_id,
                client_secret,
                webhook_id: secret.get("ATHLETO_PAYPAL_WEBHOOK_ID"),
                api_base: match secret.get("ATHLETO_PAYPAL_ENV").as_deref() {
                    Some("live") => "https://api-m.paypal.com".to_string(),
                    _ => "https://api-m.sandbox.paypal.com".to_string(),
                },
            }),
            _ => None,
        },
        square: match (
            secret.get("ATHLETO_SQUARE_ACCESS_TOKEN"),
            secret.get("ATHLETO_SQUARE_LOCATION_ID"),
        ) {
            (Some(access_token), Some(location_id)) => Some(payments::SquareConfig {
                access_token,
                location_id,
                webhook_signature_key: secret.get("ATHLETO_SQUARE_WEBHOOK_SIGNATURE_KEY"),
                api_base: match secret.get("ATHLETO_SQUARE_ENV").as_deref() {
                    Some("production") => "https://connect.squareup.com".to_string(),
                    _ => "https://connect.squareupsandbox.com".to_string(),
                },
            }),
            _ => None,
        },
        billing: match (
            secret.get("ATHLETO_BILLING_URL"),
            secret
                .get("ATHLETO_BILLING_TENANT_ID")
                .and_then(|id| id.parse().ok()),
        ) {
            (Some(url), Some(tenant_id)) => Some(billing::BillingConfig {
                url: url.trim_end_matches('/').to_string(),
                api_key: secret.get("ATHLETO_BILLING_API_KEY"),
                tenant_id,
            }),
            _ => None,
        },
    };
    if config.supabase().is_none() {
        tracing::warn!("SUPABASE_URL / SUPABASE_ANON_KEY not set; auth routes will show a 'not configured' notice");
    }
    if config.allowed_hosts.is_none() {
        tracing::warn!(
            "ALLOWED_HOSTS not set; trusting any inbound Host header (fine for dev, set it in production)"
        );
    }

    let pool = secret
        .get("DATABASE_URL")
        .and_then(|url| db::build_pool(&url));
    match &pool {
        Some(pool) => {
            // Migrations stay asynchronous so process startup and /healthz do
            // not depend on a reachable database.
            let migrate_pool = pool.get_postgres_connection_pool().clone();
            tokio::spawn(async move {
                match sqlx::migrate!().run(&migrate_pool).await {
                    Ok(()) => tracing::info!("database migrations applied"),
                    Err(err) => {
                        tracing::error!(error = %err, "database migrations failed; continuing")
                    }
                }
            });

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
                        continue;
                    };
                    match db::sweep_expired_holds(&sweep_pool).await {
                        Ok(0) => {}
                        Ok(swept) => tracing::info!(swept, "cleared expired stock holds"),
                        Err(err) => tracing::warn!(error = %err, "hold sweep failed"),
                    }
                    lead.release().await;
                }
            });

            let recurring_pool = pool.clone();
            let recurring_config = config.clone();
            tokio::spawn(async move {
                let period = Duration::from_secs(10 * 60);
                let mut ticker =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                loop {
                    ticker.tick().await;
                    let Some(lead) = coordinate::try_lead(
                        &recurring_pool,
                        &recurring_config,
                        "recurring-runner",
                        120,
                    )
                    .await
                    else {
                        continue;
                    };
                    match db::run_due_recurring_orders(&recurring_pool).await {
                        Ok(0) => {}
                        Ok(count) => tracing::info!(count, "recurring orders advanced"),
                        Err(err) => tracing::warn!(error = %err, "recurring runner failed"),
                    }
                    lead.release().await;
                }
            });
        }
        None => tracing::warn!(
            "DATABASE_URL not set; cart persistence disabled, storefront uses built-in catalog"
        ),
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
