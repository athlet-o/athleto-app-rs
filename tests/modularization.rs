use std::time::Duration;

const MAIN_RS: &str = include_str!("../src/main.rs");
const LIB_RS: &str = include_str!("../src/lib.rs");
const DB_RS: &str = include_str!("../src/db.rs");
const STARTUP_RS: &str = include_str!("../src/startup.rs");
const TELEMETRY_RS: &str = include_str!("../src/telemetry.rs");
const CARGO_TOML: &str = include_str!("../Cargo.toml");

fn code_line_count(source: &str) -> usize {
    source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .count()
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
