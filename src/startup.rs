//! Process composition and lifecycle for the `athleto-app-rs` binary.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use tracing::{field, Instrument};

use ipnet::IpNet;

use crate::{
    billing, coordinate, db, mfa_state, payments, router, secrets, telemetry, AppState, Config,
    SharedState,
};

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn trusted_proxy_networks() -> Vec<IpNet> {
    env_opt("ATHLETO_TRUSTED_PROXY_CIDRS")
        .into_iter()
        .flat_map(|cidrs| cidrs.split(',').map(str::to_owned).collect::<Vec<_>>())
        .filter_map(|cidr| match cidr.trim().parse::<IpNet>() {
            Ok(network) => Some(network),
            Err(error) => {
                tracing::error!(%cidr, %error, "ignoring invalid ATHLETO_TRUSTED_PROXY_CIDRS entry");
                None
            }
        })
        .collect()
}

/// Initialize telemetry, compose dependencies, and serve until termination.
pub async fn run() -> anyhow::Result<()> {
    let _telemetry = telemetry::init("athleto-app-rs", "athlet-o");
    let span = tracing::info_span!(
        "service.run",
        otel.kind = "internal",
        trace_id = field::Empty,
        span_id = field::Empty,
    );
    telemetry::record_trace_context(&span);
    run_inner().instrument(span).await
}

async fn run_inner() -> anyhow::Result<()> {
    let host = env_opt("HOST").unwrap_or_else(|| "0.0.0.0".to_string());
    let port: u16 = env_opt("PORT")
        .and_then(|value| value.parse().ok())
        .unwrap_or(8080);

    let fiducia_url = env_opt("FIDUCIA_URL");
    let fiducia_api_key = env_opt("FIDUCIA_API_KEY");
    let fiducia_client = match coordinate::FiduciaClient::from_options(
        fiducia_url.as_deref(),
        fiducia_api_key.as_deref(),
    ) {
        Ok(client) => client,
        Err(err) => {
            tracing::error!(error = %err, "invalid fiducia configuration; secret overlay disabled and singleton jobs will fail closed");
            None
        }
    };
    let secret = secrets::SecretSource::load(fiducia_client.as_ref()).await;
    let mfa_state_key = match secret.get("ATHLETO_MFA_STATE_KEY") {
        Some(value) => match mfa_state::decode_key(&value) {
            Some(key) => Some(key),
            None => {
                tracing::error!("ATHLETO_MFA_STATE_KEY must be base64 for exactly 32 bytes");
                None
            }
        },
        None => None,
    };

    let config = Config {
        supabase_url: secret
            .get("SUPABASE_URL")
            .map(|url| url.trim_end_matches('/').to_string()),
        supabase_anon_key: secret.get("SUPABASE_ANON_KEY"),
        shared_auth_base_url: env_opt("SHARED_AUTH_BASE_URL")
            .map(|url| url.trim_end_matches('/').to_string()),
        public_base_url: env_opt("ATHLETO_PUBLIC_BASE_URL")
            .unwrap_or_else(|| "https://app.athleto.store".to_string()),
        biz_public_base_url: env_opt("ATHLETO_BIZ_PUBLIC_BASE_URL")
            .unwrap_or_else(|| "https://biz.athleto.store".to_string()),
        allowed_hosts: env_opt("ALLOWED_HOSTS").map(|hosts| {
            hosts
                .split(',')
                .map(|host| host.trim().to_string())
                .filter(|host| !host.is_empty())
                .collect()
        }),
        sms_mfa_enabled: env_opt("ATHLETO_SMS_MFA_ENABLED").as_deref() == Some("1"),
        self_signup_enabled: env_opt("ATHLETO_ALLOW_SELF_SIGNUP").as_deref() == Some("1"),
        turnstile_site_key: env_opt("ATHLETO_TURNSTILE_SITE_KEY"),
        turnstile_secret: secret.get("ATHLETO_TURNSTILE_SECRET"),
        mfa_state_key,
        trusted_proxy_networks: trusted_proxy_networks(),
        fiducia_url,
        fiducia_api_key,
        // HOSTNAME is the pod name under Kubernetes; unique per replica.
        replica_id: env_opt("HOSTNAME").unwrap_or_else(|| "local".to_string()),
        operations_api_key: secret.get("ATHLETO_OPERATIONS_API_KEY"),
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
    if !config.auth_ready() {
        tracing::warn!(
            supabase.configured = config.supabase().is_some(),
            shared_auth.configured = config.shared_auth_base_url.is_some(),
            "Supabase/shared-auth stack incomplete; auth routes will show a 'not configured' notice"
        );
    }
    if config.allowed_hosts.is_none() {
        tracing::warn!(
            "ALLOWED_HOSTS not set; trusting any inbound Host header (fine for dev, set it in production)"
        );
    }
    if config.self_signup_enabled && !config.self_signup_ready() {
        tracing::error!(
            "ATHLETO_ALLOW_SELF_SIGNUP requires ATHLETO_TURNSTILE_SITE_KEY and ATHLETO_TURNSTILE_SECRET"
        );
    }
    if config.trusted_proxy_networks.is_empty() {
        tracing::warn!(
            "ATHLETO_TRUSTED_PROXY_CIDRS not set; forwarded client addresses are ignored"
        );
    }

    let pool = match secret.get("DATABASE_URL") {
        Some(url) => db::build_pool(&url).await,
        None => None,
    };
    match &pool {
        Some(pool) => spawn_background_jobs(pool, &config),
        None => {
            tracing::warn!(
                "DATABASE_URL not set; cart persistence disabled, storefront uses built-in catalog"
            );
        }
    }

    // One hardened outbound client shared by every provider call: a total
    // timeout so a stalled upstream can't accumulate detached tasks, and no
    // redirect-follow so a `bearer_auth(secret)` request can never forward the
    // merchant secret to a redirect target.
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let state: SharedState = Arc::new(AppState::new(pool, http, config));
    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "athleto-app-rs listening");
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

fn spawn_background_jobs(pool: &sea_orm::DatabaseConnection, config: &Config) {
    // Schema changes are owned by the k8s-cluster pg-defs/dpm release path;
    // the application never performs DDL or migrations at process startup.

    let sweep_pool = pool.clone();
    let sweep_config = config.clone();
    tokio::spawn(async move {
        let period = Duration::from_secs(15 * 60);
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        loop {
            ticker.tick().await;
            // run_singleton returns None when another replica holds this tick's
            // fiducia lease; Some(job_result) when we ran (fiducia leader, or
            // no fiducia and the DELETE self-guarded via its xact advisory lock).
            match coordinate::run_singleton(&sweep_config, "hold-sweeper", 120, || {
                db::sweep_expired_holds(&sweep_pool)
            })
            .await
            {
                None | Some(Ok(0)) => {}
                Some(Ok(swept)) => {
                    tracing::info!(swept, job.name = "hold-sweeper", "job completed")
                }
                Some(Err(err)) => {
                    tracing::warn!(error = %err, job.name = "hold-sweeper", "job failed")
                }
            }
        }
    });

    let recur_pool = pool.clone();
    let recur_config = config.clone();
    tokio::spawn(async move {
        let period = Duration::from_secs(10 * 60);
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        loop {
            ticker.tick().await;
            match coordinate::run_singleton(&recur_config, "recurring-runner", 120, || {
                db::run_due_recurring_orders(&recur_pool)
            })
            .await
            {
                None | Some(Ok(0)) => {}
                Some(Ok(materialized)) => {
                    tracing::info!(materialized, job.name = "recurring-runner", "job completed")
                }
                Some(Err(err)) => tracing::warn!(
                    error = %err,
                    job.name = "recurring-runner",
                    "job failed"
                ),
            }
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
