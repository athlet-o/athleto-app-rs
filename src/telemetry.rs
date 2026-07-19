//! OpenTelemetry bootstrap plus explicit HTTP spans and metrics.
//!
//! JSON logs go to stderr for Kubernetes/Promtail/Loki collection. When
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set, traces and metrics are exported over
//! OTLP to the cluster collector; Prometheus scrapes the collector's metrics
//! endpoint rather than this process.

use std::{sync::OnceLock, time::Instant};

use axum::{
    extract::{MatchedPath, Request},
    middleware::Next,
    response::Response,
};
use opentelemetry::{
    global,
    metrics::{Counter, Histogram},
    trace::TraceContextExt,
    KeyValue,
};
use opentelemetry_http::HeaderExtractor;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    metrics::{PeriodicReader, SdkMeterProvider},
    propagation::TraceContextPropagator,
    runtime,
    trace::{Tracer, TracerProvider},
    Resource,
};
use tracing::{field, Instrument, Span};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

const EXPORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Owns the SDK providers so pending batches are flushed during shutdown.
pub struct TelemetryGuard {
    tracer_provider: Option<TracerProvider>,
    meter_provider: Option<SdkMeterProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        let tracer_provider = self.tracer_provider.take();
        let meter_provider = self.meter_provider.take();
        if tracer_provider.is_none() && meter_provider.is_none() {
            return;
        }

        if std::thread::spawn(move || {
            if let Some(provider) = meter_provider {
                let _ = provider.shutdown();
            }
            if let Some(provider) = tracer_provider {
                let _ = provider.shutdown();
            }
        })
        .join()
        .is_err()
        {
            eprintln!("telemetry: shutdown flush panicked; final batches may be incomplete");
        }
    }
}

/// Install Loki-friendly structured logs and optional OTLP exporters.
pub fn init(service_name: &'static str, service_namespace: &'static str) -> TelemetryGuard {
    global::set_text_map_propagator(TraceContextPropagator::new());

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,tower_http=info"));
    let resource = resource(service_name, service_namespace);
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|value| !value.trim().is_empty());

    let (tracer_provider, tracer) = endpoint
        .as_deref()
        .and_then(|endpoint| build_tracer_provider(endpoint, resource.clone()).ok())
        .map_or((None, None), |(provider, tracer)| {
            global::set_tracer_provider(provider.clone());
            (Some(provider), Some(tracer))
        });

    let meter_provider = endpoint
        .as_deref()
        .and_then(|endpoint| build_meter_provider(endpoint, resource).ok());
    if let Some(provider) = meter_provider.as_ref() {
        global::set_meter_provider(provider.clone());
    }

    install_subscriber(filter, tracer);
    tracing::info!(
        service.name = service_name,
        service.namespace = service_namespace,
        otel.trace_exporter = tracer_provider.is_some(),
        otel.metric_exporter = meter_provider.is_some(),
        log.format = "json",
        log.destination = "stderr",
        "telemetry initialized"
    );

    TelemetryGuard {
        tracer_provider,
        meter_provider,
    }
}

fn build_tracer_provider(
    endpoint: &str,
    resource: Resource,
) -> Result<(TracerProvider, Tracer), ()> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|_| ())?;
    let provider = TracerProvider::builder()
        .with_batch_exporter(exporter, runtime::Tokio)
        .with_resource(resource)
        .build();
    use opentelemetry::trace::TracerProvider as _;
    let tracer = provider.tracer("athleto-http");
    Ok((provider, tracer))
}

fn build_meter_provider(endpoint: &str, resource: Resource) -> Result<SdkMeterProvider, ()> {
    let exporter = opentelemetry_otlp::MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|_| ())?;
    let reader = PeriodicReader::builder(exporter, runtime::Tokio).build();
    Ok(SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build())
}

fn install_subscriber(filter: EnvFilter, tracer: Option<Tracer>) {
    let result = match tracer {
        Some(tracer) => tracing_subscriber::registry()
            .with(filter)
            .with(json_log_layer())
            .with(tracing_opentelemetry::layer().with_tracer(tracer))
            .try_init(),
        None => tracing_subscriber::registry()
            .with(filter)
            .with(json_log_layer())
            .try_init(),
    };
    if result.is_err() {
        eprintln!("telemetry: subscriber already initialized; keeping existing subscriber");
    }
}

fn json_log_layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_ansi(false)
        .with_current_span(true)
        .with_span_list(true)
        .with_target(true)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr)
}

fn resource(service_name: &str, service_namespace: &str) -> Resource {
    let mut attributes = vec![
        KeyValue::new("service.name", service_name.to_string()),
        KeyValue::new("service.namespace", service_namespace.to_string()),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
    ];
    push_env_attribute(&mut attributes, "DEPLOYMENT_ENV", "deployment.environment");
    push_env_attribute(&mut attributes, "POD_NAMESPACE", "k8s.namespace.name");
    push_env_attribute(&mut attributes, "POD_NAME", "k8s.pod.name");
    push_env_attribute(&mut attributes, "NODE_NAME", "k8s.node.name");
    push_env_attribute(&mut attributes, "HOSTNAME", "host.name");

    if let Ok(raw) = std::env::var("OTEL_RESOURCE_ATTRIBUTES") {
        attributes
            .extend(resource_attribute_pairs(&raw).map(|(key, value)| KeyValue::new(key, value)));
    }
    Resource::new(attributes)
}

fn push_env_attribute(attributes: &mut Vec<KeyValue>, env_name: &str, key: &'static str) {
    if let Ok(value) = std::env::var(env_name) {
        let value = value.trim();
        if valid_attribute_value(value) {
            attributes.push(KeyValue::new(key, value.to_string()));
        }
    }
}

fn resource_attribute_pairs(raw: &str) -> impl Iterator<Item = (String, String)> + '_ {
    raw.split(',').filter_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        let key = key.trim();
        let value = value.trim();
        if valid_attribute_key(key)
            && valid_attribute_value(value)
            && !sensitive_attribute_key(key)
            && !matches!(key, "service.name" | "service.namespace")
        {
            Some((key.to_string(), value.to_string()))
        } else {
            None
        }
    })
}

fn valid_attribute_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_attribute_value(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn sensitive_attribute_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', '.'], "_");
    [
        "authorization",
        "bearer",
        "cookie",
        "credential",
        "email",
        "jwt",
        "passphrase",
        "passwd",
        "password",
        "private_key",
        "pwd",
        "secret",
        "session",
        "signing_key",
        "token",
        "api_key",
        "apikey",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

struct HttpMetrics {
    requests: Counter<u64>,
    duration: Histogram<f64>,
}

fn http_metrics() -> &'static HttpMetrics {
    static METRICS: OnceLock<HttpMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("athleto-http");
        HttpMetrics {
            requests: meter
                .u64_counter("http.server.request.count")
                .with_description("Number of completed HTTP requests")
                .with_unit("{request}")
                .build(),
            duration: meter
                .f64_histogram("http.server.request.duration")
                .with_description("HTTP request duration")
                .with_unit("s")
                .build(),
        }
    })
}

/// Axum middleware that extracts W3C trace context and records bounded HTTP
/// telemetry. Query strings, headers, request bodies, and user identifiers are
/// deliberately excluded.
pub async fn track_http_request(request: Request, next: Next) -> Response {
    let method = request.method().as_str().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched")
        .to_string();
    let parent = global::get_text_map_propagator(|propagator| {
        propagator.extract(&HeaderExtractor(request.headers()))
    });
    let started = Instant::now();
    let span = tracing::info_span!(
        "http.server.request",
        otel.kind = "server",
        http.request.method = %method,
        http.route = %route,
        http.response.status_code = field::Empty,
        otel.status_code = field::Empty,
        trace_id = field::Empty,
        span_id = field::Empty,
    );
    span.set_parent(parent);
    record_trace_context(&span);

    async move {
        let response = next.run(request).await;
        let status = response.status();
        let attributes = [
            KeyValue::new("http.request.method", method),
            KeyValue::new("http.route", route),
            KeyValue::new("http.response.status_code", i64::from(status.as_u16())),
        ];
        let metrics = http_metrics();
        metrics.requests.add(1, &attributes);
        metrics
            .duration
            .record(started.elapsed().as_secs_f64(), &attributes);

        Span::current().record("http.response.status_code", status.as_u16());
        Span::current().record(
            "otel.status_code",
            if status.is_server_error() {
                "ERROR"
            } else {
                "OK"
            },
        );
        response
    }
    .instrument(span)
    .await
}

/// Record IDs on a tracing span so Loki log lines can link to the same trace.
pub fn record_trace_context(span: &Span) {
    let context = span.context();
    let otel_span = context.span();
    let span_context = otel_span.span_context();
    if span_context.is_valid() {
        span.record("trace_id", span_context.trace_id().to_string());
        span.record("span_id", span_context.span_id().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_attributes_reject_secrets_and_identity_overrides() {
        let attributes = resource_attribute_pairs(
            "team=commerce,api.token=nope,service.name=spoof,cloud.region=us-east-1",
        )
        .collect::<Vec<_>>();
        assert_eq!(
            attributes,
            vec![
                ("team".to_string(), "commerce".to_string()),
                ("cloud.region".to_string(), "us-east-1".to_string()),
            ]
        );
    }
}
