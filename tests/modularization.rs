use std::{sync::Arc, time::Duration};

use athleto_app_rs::{router, AppState, Config};
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

const MAIN_RS: &str = include_str!("../src/main.rs");
const LIB_RS: &str = include_str!("../src/lib.rs");
const DB_RS: &str = include_str!("../src/db.rs");
const AUTH_RS: &str = include_str!("../src/auth.rs");
const SHARED_AUTH_RS: &str = include_str!("../src/shared_auth.rs");
const STARTUP_RS: &str = include_str!("../src/startup.rs");
const TELEMETRY_RS: &str = include_str!("../src/telemetry.rs");
const CARGO_TOML: &str = include_str!("../Cargo.toml");
const K8S_DEPLOYMENT: &str = include_str!("../deploy/k8s/deployment.yaml");
const K8S_README: &str = include_str!("../deploy/k8s/README.md");

fn code_line_count(source: &str) -> usize {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .count()
}

fn degraded_state() -> Arc<AppState> {
    Arc::new(AppState::new(
        None,
        reqwest::Client::new(),
        Config::default(),
    ))
}

async fn get(path: &str) -> axum::response::Response {
    router(degraded_state())
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn body_text(response: axum::response::Response) -> String {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(body.to_vec()).unwrap()
}

#[test]
fn binary_entrypoint_stays_a_thin_startup_adapter() {
    assert!(
        code_line_count(MAIN_RS) <= 5,
        "main.rs grew beyond its adapter role"
    );
    assert!(MAIN_RS.contains("athleto_app_rs::startup::run().await"));
    for responsibility in ["Router", "TcpListener", "Database", "opentelemetry"] {
        assert!(
            !MAIN_RS.contains(responsibility),
            "main.rs must not own {responsibility}"
        );
    }
}

#[test]
fn startup_routing_and_telemetry_have_separate_owners() {
    for module in ["db", "startup", "telemetry"] {
        assert!(LIB_RS.contains(&format!("pub mod {module};")));
    }
    assert!(LIB_RS.contains("pub fn router"));
    assert!(LIB_RS.contains("telemetry::track_http_request"));

    assert!(STARTUP_RS.contains("telemetry::init"));
    assert!(STARTUP_RS.contains("tokio::net::TcpListener"));
    assert!(STARTUP_RS.contains("spawn_background_jobs"));
    assert!(!STARTUP_RS.contains("Router::new()"));

    assert!(TELEMETRY_RS.contains("OTEL_EXPORTER_OTLP_ENDPOINT"));
    assert!(TELEMETRY_RS.contains("with_batch_exporter"));
    assert!(TELEMETRY_RS.contains("with_writer(std::io::stderr)"));
}

#[test]
fn library_router_and_startup_keep_distinct_composition_roles() {
    assert!(!LIB_RS.contains("TcpListener::bind"));
    assert!(!LIB_RS.contains("telemetry::init("));

    assert!(STARTUP_RS.contains("AppState::new"));
    assert!(STARTUP_RS.contains("router(state)"));
    assert!(STARTUP_RS.contains("into_make_service_with_connect_info"));
    assert!(!STARTUP_RS.contains("Router::new"));
    assert!(!STARTUP_RS.contains(".route("));
}

#[test]
fn shared_auth_is_a_focused_traced_authority_adapter() {
    assert!(LIB_RS.contains("mod shared_auth;"));
    assert!(LIB_RS.contains("pub(crate) shared_auth: Option<shared_auth::Client>"));
    assert!(STARTUP_RS.contains("SHARED_AUTH_BASE_URL"));
    assert!(AUTH_RS.contains("exchange_shared_session"));
    assert!(AUTH_RS.contains("fetch_authenticated_user"));
    assert!(AUTH_RS.contains("shared_identity_matches"));

    for endpoint in ["/auth/exchange", "/auth/introspect", "/auth/logout"] {
        assert!(
            SHARED_AUTH_RS.contains(endpoint),
            "shared-auth adapter lost {endpoint}"
        );
    }
    for span in [
        "athleto.auth.exchange",
        "athleto.auth.introspect",
        "athleto.auth.logout",
    ] {
        assert!(SHARED_AUTH_RS.contains(span), "missing auth span {span}");
    }
    assert!(SHARED_AUTH_RS.contains("HeaderInjector"));
    assert!(!MAIN_RS.contains("shared_auth"));
    assert!(!STARTUP_RS.contains("/auth/introspect"));
}

#[test]
fn shared_auth_replaces_local_auth_authority_without_leaking_tokens_to_logs() {
    assert!(!CARGO_TOML.contains("jsonwebtoken"));
    assert!(!STARTUP_RS.contains("SUPABASE_JWT_SECRET"));
    assert!(!AUTH_RS.contains("SUPABASE_JWT_SECRET"));
    assert!(AUTH_RS.contains("__Host-ore_access_token"));
    assert!(AUTH_RS.contains("__Host-ore_refresh_token"));

    for unsafe_field in ["%provider_token", "%shared_token", "%refresh_token"] {
        assert!(
            !SHARED_AUTH_RS.contains(unsafe_field),
            "auth token must never be recorded in telemetry: {unsafe_field}"
        );
    }
}

#[test]
fn kubernetes_runtime_uses_the_same_auth_and_otel_contract() {
    assert!(K8S_DEPLOYMENT.contains("name: HOST"));
    assert!(K8S_DEPLOYMENT.contains("name: PORT"));
    assert!(!K8S_DEPLOYMENT.contains("name: ATHLETO_HOST"));
    assert!(!K8S_DEPLOYMENT.contains("name: ATHLETO_PORT"));
    assert!(K8S_DEPLOYMENT.contains("OTEL_EXPORTER_OTLP_ENDPOINT"));
    assert!(K8S_DEPLOYMENT.contains("dd-otel-collector.observability.svc.cluster.local:4317"));
    assert!(K8S_DEPLOYMENT.contains("athleto-app-rs-secrets"));
    assert!(K8S_README.contains("SHARED_AUTH_BASE_URL"));
}

#[test]
fn database_boundary_is_seaorm_only_and_never_runs_migrations() {
    assert!(CARGO_TOML.contains("sea-orm ="));
    assert!(DB_RS.contains("ConnectOptions"));
    assert!(DB_RS.contains("Database::connect(options)"));
    assert!(DB_RS.contains("connect_lazy(true)"));

    for source in [CARGO_TOML, DB_RS, STARTUP_RS] {
        assert!(!source.contains("sqlx::"));
        assert!(!source.contains("sqlx::migrate!"));
    }
    assert!(
        !CARGO_TOML
            .lines()
            .any(|line| line.trim_start().starts_with("sqlx =")),
        "database access must go through SeaORM"
    );
}

#[test]
fn startup_and_database_modules_contain_no_runtime_ddl() {
    for statement in ["CREATE TABLE", "ALTER TABLE", "DROP TABLE", "CREATE TYPE"] {
        assert!(
            !STARTUP_RS.contains(statement),
            "startup.rs must not execute {statement}"
        );
        assert!(
            !DB_RS.contains(statement),
            "db.rs must not execute {statement}"
        );
    }
}

#[tokio::test]
async fn seaorm_connection_creation_is_lazy_and_offline_safe() {
    let connection = tokio::time::timeout(
        Duration::from_secs(1),
        athleto_app_rs::db::build_pool(
            "postgres://athleto:athleto@127.0.0.1:1/athleto_modularization_test",
        ),
    )
    .await
    .expect("lazy SeaORM setup unexpectedly attempted a network connection");

    assert!(connection.is_some());
}

#[tokio::test]
async fn public_router_preserves_degraded_storefront_behavior_after_extraction() {
    let health = tokio::time::timeout(Duration::from_secs(1), get("/healthz"))
        .await
        .expect("health route attempted external I/O");
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(body_text(health).await, "ok");

    let home = tokio::time::timeout(Duration::from_secs(1), get("/"))
        .await
        .expect("degraded storefront attempted external I/O");
    assert_eq!(home.status(), StatusCode::OK);
    assert!(body_text(home).await.contains("The lineup"));
}

#[tokio::test]
async fn public_router_keeps_security_middleware_outside_the_route_tree() {
    let response = get("/not-a-real-route").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        response
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY),
        "the extracted router's fallback bypassed the security middleware"
    );
    assert_eq!(
        response.headers().get(header::X_FRAME_OPTIONS).unwrap(),
        "DENY"
    );
}
