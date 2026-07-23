//! Session adapter for the canonical `github.com/shared-auth` authority.
//!
//! Supabase remains the identity provider for magic links and MFA, while
//! shared-auth owns the cross-provider session and revocation decision. Tokens
//! are never included in logs or span fields.

use std::{sync::Arc, time::Duration};

use opentelemetry::{global, trace::TraceContextExt};
use opentelemetry_http::HeaderInjector;
use reqwest::{header::HeaderMap, redirect::Policy, Url};
use serde::{Deserialize, Serialize};
use tracing::{field, Instrument, Span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

const TOKEN_MAX_BYTES: usize = 16 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(1_500);

#[derive(Clone)]
pub(crate) struct Client {
    base_url: Arc<str>,
    http: reqwest::Client,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub shared_user_id: String,
    pub provider: String,
    pub provider_subject: String,
    pub roles: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Session {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub identity: Identity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthError {
    Invalid,
    Unavailable,
}

#[derive(Serialize)]
struct IntrospectRequest<'a> {
    token: &'a str,
}

#[derive(Deserialize)]
struct IntrospectResponse {
    active: bool,
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    provider_subject: Option<String>,
    #[serde(default)]
    roles: Vec<String>,
}

#[derive(Deserialize)]
struct ExchangeResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    shared_user_id: String,
    provider: String,
    provider_subject: String,
    #[serde(default)]
    roles: Vec<String>,
}

impl Client {
    pub(crate) fn new(raw_base_url: &str) -> Result<Self, &'static str> {
        let base_url = normalized_base_url(raw_base_url)?;
        let http = reqwest::Client::builder()
            .redirect(Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| "could not build shared-auth HTTP client")?;
        Ok(Self {
            base_url: Arc::from(base_url),
            http,
        })
    }

    #[tracing::instrument(
        name = "athleto.auth.exchange",
        skip_all,
        fields(
            auth.system = "shared-auth",
            auth.provider = "supabase",
            auth.outcome = field::Empty,
            otel.status_code = field::Empty,
            trace_id = field::Empty,
            span_id = field::Empty,
        )
    )]
    pub(crate) async fn exchange(&self, provider_token: &str) -> Result<Session, AuthError> {
        record_trace_context(&Span::current());
        validate_token(provider_token)?;
        let response = self
            .request(self.http.post(format!("{}/auth/exchange", self.base_url)))
            .bearer_auth(provider_token)
            .send()
            .instrument(tracing::debug_span!(
                "shared_auth.http",
                http.request.method = "POST",
                server.address = "shared-auth",
            ))
            .await
            .map_err(|_| unavailable())?;
        if matches!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            record_outcome("invalid", "ERROR");
            return Err(AuthError::Invalid);
        }
        if !response.status().is_success() {
            return Err(unavailable());
        }
        let body: ExchangeResponse = response.json().await.map_err(|_| unavailable())?;
        if body.access_token.is_empty()
            || body.shared_user_id.is_empty()
            || body.provider.is_empty()
            || body.provider_subject.is_empty()
        {
            return Err(unavailable());
        }
        record_outcome("authenticated", "OK");
        Ok(Session {
            access_token: body.access_token,
            refresh_token: body.refresh_token,
            identity: Identity {
                shared_user_id: body.shared_user_id,
                provider: body.provider,
                provider_subject: body.provider_subject,
                roles: body.roles,
            },
        })
    }

    #[tracing::instrument(
        name = "athleto.auth.introspect",
        skip_all,
        fields(
            auth.system = "shared-auth",
            auth.outcome = field::Empty,
            otel.status_code = field::Empty,
            trace_id = field::Empty,
            span_id = field::Empty,
        )
    )]
    pub(crate) async fn introspect(&self, token: &str) -> Result<Identity, AuthError> {
        record_trace_context(&Span::current());
        validate_token(token)?;
        let response = self
            .request(self.http.post(format!("{}/auth/introspect", self.base_url)))
            .json(&IntrospectRequest { token })
            .send()
            .instrument(tracing::debug_span!(
                "shared_auth.http",
                http.request.method = "POST",
                server.address = "shared-auth",
            ))
            .await
            .map_err(|_| unavailable())?;
        if !response.status().is_success() {
            return Err(unavailable());
        }
        let body: IntrospectResponse = response.json().await.map_err(|_| unavailable())?;
        if !body.active {
            record_outcome("invalid", "ERROR");
            return Err(AuthError::Invalid);
        }
        let identity = Identity {
            shared_user_id: required(body.sub)?,
            provider: required(body.provider)?,
            provider_subject: required(body.provider_subject)?,
            roles: body.roles,
        };
        record_outcome("authenticated", "OK");
        Ok(identity)
    }

    #[tracing::instrument(
        name = "athleto.auth.logout",
        skip_all,
        fields(
            auth.system = "shared-auth",
            auth.outcome = field::Empty,
            otel.status_code = field::Empty,
            trace_id = field::Empty,
            span_id = field::Empty,
        )
    )]
    pub(crate) async fn logout(&self, refresh_token: &str) -> Result<(), AuthError> {
        record_trace_context(&Span::current());
        validate_token(refresh_token)?;
        let response = self
            .request(self.http.post(format!("{}/auth/logout", self.base_url)))
            .json(&serde_json::json!({ "refresh_token": refresh_token }))
            .send()
            .instrument(tracing::debug_span!(
                "shared_auth.http",
                http.request.method = "POST",
                server.address = "shared-auth",
            ))
            .await
            .map_err(|_| unavailable())?;
        if response.status().is_success() {
            record_outcome("logged_out", "OK");
            Ok(())
        } else {
            Err(unavailable())
        }
    }

    fn request(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut headers = HeaderMap::new();
        let context = Span::current().context();
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&context, &mut HeaderInjector(&mut headers));
        });
        request.headers(headers)
    }
}

fn required(value: Option<String>) -> Result<String, AuthError> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(unavailable)
}

fn validate_token(token: &str) -> Result<(), AuthError> {
    if token.is_empty() || token.len() > TOKEN_MAX_BYTES {
        record_outcome("invalid", "ERROR");
        Err(AuthError::Invalid)
    } else {
        Ok(())
    }
}

fn unavailable() -> AuthError {
    record_outcome("degraded", "ERROR");
    tracing::warn!(auth.system = "shared-auth", "auth authority unavailable");
    AuthError::Unavailable
}

fn record_outcome(outcome: &'static str, otel_status: &'static str) {
    let span = Span::current();
    span.record("auth.outcome", outcome);
    span.record("otel.status_code", otel_status);
}

fn record_trace_context(span: &Span) {
    let context = span.context();
    let span_context = context.span().span_context().clone();
    if span_context.is_valid() {
        span.record("trace_id", span_context.trace_id().to_string());
        span.record("span_id", span_context.span_id().to_string());
    }
}

fn normalized_base_url(raw: &str) -> Result<String, &'static str> {
    let url = Url::parse(raw.trim()).map_err(|_| "URL is not valid")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("URL must not contain credentials, a query, or a fragment");
    }
    let host = url.host_str().ok_or("URL must have a host")?;
    let trusted_http = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
        || host.ends_with(".svc")
        || host.ends_with(".svc.cluster.local");
    if url.scheme() != "https" && !(url.scheme() == "http" && trusted_http) {
        return Err("URL must use HTTPS outside loopback or the cluster network");
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use axum::{http::StatusCode, routing::post, Json, Router};
    use serde_json::{json, Value};

    use super::*;

    async fn server(exchange: Value, introspect: Value) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route(
                "/auth/exchange",
                post(move || {
                    let body = exchange.clone();
                    async move { (StatusCode::OK, Json(body)) }
                }),
            )
            .route(
                "/auth/introspect",
                post(move || {
                    let body = introspect.clone();
                    async move { (StatusCode::OK, Json(body)) }
                }),
            )
            .route("/auth/logout", post(|| async { StatusCode::NO_CONTENT }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn exchange_and_introspection_preserve_provider_identity() {
        let base = server(
            json!({
                "access_token": "shared-access",
                "refresh_token": "ore_rt_refresh",
                "shared_user_id": "shared-1",
                "provider": "supabase",
                "provider_subject": "00000000-0000-0000-0000-000000000001",
                "roles": ["buyer"]
            }),
            json!({
                "active": true,
                "sub": "shared-1",
                "provider": "supabase",
                "provider_subject": "00000000-0000-0000-0000-000000000001",
                "roles": ["buyer"]
            }),
        )
        .await;
        let client = Client::new(&base).unwrap();
        let session = client.exchange("supabase-access").await.unwrap();
        assert_eq!(session.access_token, "shared-access");
        assert_eq!(session.identity.provider, "supabase");

        let identity = client.introspect("shared-access").await.unwrap();
        assert_eq!(identity, session.identity);
        client.logout("ore_rt_refresh").await.unwrap();
    }

    #[tokio::test]
    async fn inactive_session_is_invalid_not_an_authority_outage() {
        let base = server(json!({}), json!({"active": false})).await;
        assert_eq!(
            Client::new(&base).unwrap().introspect("expired").await,
            Err(AuthError::Invalid)
        );
    }

    #[test]
    fn shared_auth_url_rejects_public_cleartext_and_credentials() {
        assert!(Client::new("http://auth.example.com").is_err());
        assert!(Client::new("https://user:secret@auth.example.com").is_err());
        assert!(Client::new("http://shared-auth.default.svc.cluster.local").is_ok());
        assert!(Client::new("https://auth.oresoftware.dev/shared-auth").is_ok());
    }
}
