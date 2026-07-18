//! Cross-cutting request hardening: CSRF double-submit tokens, security
//! headers (CSP with a per-request nonce), login rate limiting, and the
//! ALLOWED_HOSTS allowlist.
//!
//! CSRF model: a random token lives in a `SameSite=Lax` cookie that is *not*
//! HttpOnly (the auth-callback script must read it to authenticate its
//! token-forwarding POST to /auth/session). Every server-rendered form embeds
//! the same token as a hidden `csrf_token` field via `pages::csrf_field`, and
//! the layout stamps it on `<body hx-headers=...>` so every htmx request
//! carries it as an `x-csrf-token` header. The middleware rejects any
//! state-changing request whose field/header does not match the cookie. The
//! JSON API under /api/v1 authenticates with bearer API keys (no ambient
//! cookie credentials), so it is exempt.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use uuid::Uuid;

use crate::pages;

pub const CSRF_COOKIE: &str = "athleto_csrf";
pub const CSRF_FORM_FIELD: &str = "csrf_token";
pub const CSRF_HEADER: &str = "x-csrf-token";

/// Forms are small; anything bigger than this is not a legitimate storefront
/// POST and gets a 413 instead of being buffered.
const MAX_FORM_BYTES: usize = 256 * 1024;

/// Per-request values minted by the middleware and read back while rendering:
/// the CSRF token for forms/htmx headers and the CSP nonce for inline
/// script/style tags.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub csrf_token: String,
    pub csp_nonce: String,
}

tokio::task_local! {
    static REQUEST_CONTEXT: RequestContext;
}

/// The current request's CSRF token (empty outside a request scope, e.g. in
/// unit tests that render markup directly).
pub fn csrf_token() -> String {
    REQUEST_CONTEXT
        .try_with(|ctx| ctx.csrf_token.clone())
        .unwrap_or_default()
}

/// The current request's CSP nonce (empty outside a request scope).
pub fn csp_nonce() -> String {
    REQUEST_CONTEXT
        .try_with(|ctx| ctx.csp_nonce.clone())
        .unwrap_or_default()
}

/// 128 bits x2 of randomness, hex encoded -- same shape as the API keys.
fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// A stored token is always our own 64-char hex; anything else is treated as
/// absent so a tampered cookie regenerates instead of being echoed around.
fn valid_token(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Constant-time equality; token comparison should not leak prefix length.
fn tokens_match(a: &str, b: &str) -> bool {
    if a.len() != b.len() || a.is_empty() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Pull one field out of an `application/x-www-form-urlencoded` body. Only
/// handles the encodings our own token can appear in (it is plain hex), which
/// keeps this free of a full urlencoded parser.
fn form_value(body: &str, name: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then(|| value.replace('+', " "))
    })
}

fn is_form_content_type(request: &Request) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("application/x-www-form-urlencoded"))
        .unwrap_or(false)
}

fn csrf_rejection() -> Response {
    (
        StatusCode::FORBIDDEN,
        pages::error_page(
            "This form was missing its security token (it may have been open too long, \
             or the request came from another site). Go back, reload, and try again.",
        ),
    )
        .into_response()
}

/// Single security middleware: mints the per-request CSRF token + CSP nonce,
/// enforces CSRF on state-changing requests, and stamps security headers on
/// every response.
pub async fn apply(jar: CookieJar, request: Request, next: Next) -> Response {
    let cookie_token = jar
        .get(CSRF_COOKIE)
        .map(|cookie| cookie.value().to_string())
        .filter(|value| valid_token(value));
    let is_new_token = cookie_token.is_none();
    let token = cookie_token.unwrap_or_else(random_token);
    let nonce = Uuid::new_v4().simple().to_string();
    // Computed up front so the early CSRF/oversize rejections below carry the
    // same security headers as a normal response (they used to skip them).
    let hsts = wants_hsts(&request);

    let state_changing = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    // /api/v1/* authenticates with bearer API keys, never cookies, so CSRF
    // does not apply there.
    let path = request.uri().path();
    let csrf_exempt = path.starts_with("/api/v1/")
        // Provider webhooks authenticate with their signed raw payloads, not
        // ambient browser cookies. Requiring a browser CSRF token would make
        // legitimate callbacks impossible.
        || matches!(path, "/webhooks/stripe" | "/webhooks/paypal" | "/webhooks/square");

    let request = if state_changing && !csrf_exempt {
        let provided_header = request
            .headers()
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let (provided, request) = match provided_header {
            Some(header_token) => (Some(header_token), request),
            // No header: buffer the (small) form body to read the hidden
            // field, then hand the handler an identical request.
            None if is_form_content_type(&request) => {
                let (parts, body) = request.into_parts();
                let bytes = match axum::body::to_bytes(body, MAX_FORM_BYTES).await {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        return finish(
                            StatusCode::PAYLOAD_TOO_LARGE.into_response(),
                            &token,
                            is_new_token,
                            Some((nonce.clone(), hsts)),
                        )
                    }
                };
                let provided = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|body| form_value(body, CSRF_FORM_FIELD));
                (provided, Request::from_parts(parts, Body::from(bytes)))
            }
            None => (None, request),
        };
        let verified = !is_new_token
            && provided
                .as_deref()
                .map(|provided| tokens_match(provided, &token))
                .unwrap_or(false);
        if !verified {
            return finish(
                csrf_rejection(),
                &token,
                is_new_token,
                Some((nonce.clone(), hsts)),
            );
        }
        request
    } else {
        request
    };

    let context = RequestContext {
        csrf_token: token.clone(),
        csp_nonce: nonce.clone(),
    };
    let response = REQUEST_CONTEXT.scope(context, next.run(request)).await;
    finish(response, &token, is_new_token, Some((nonce, hsts)))
}

/// True when the request demonstrably arrived over TLS (we sit behind an
/// ingress, so the forwarded-proto header is the only signal).
fn wants_hsts(request: &Request) -> bool {
    request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|proto| proto.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

/// Attach the security headers (and the CSRF cookie when freshly minted).
fn finish(
    mut response: Response,
    token: &str,
    set_cookie: bool,
    headers: Option<(String, bool)>,
) -> Response {
    if let Some((nonce, hsts)) = headers {
        let csp = format!(
            "default-src 'self'; script-src 'self' 'nonce-{nonce}'; \
             style-src 'self' 'nonce-{nonce}'; img-src 'self' data:; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'self'; \
             form-action 'self'; object-src 'none'"
        );
        let headers = response.headers_mut();
        if let Ok(value) = HeaderValue::from_str(&csp) {
            headers.insert(header::CONTENT_SECURITY_POLICY, value);
        }
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        if hsts {
            headers.insert(
                header::STRICT_TRANSPORT_SECURITY,
                HeaderValue::from_static("max-age=63072000; includeSubDomains"),
            );
        }
    }
    if set_cookie {
        // Readable by the auth-callback script (see module docs), hence no
        // HttpOnly; the token authorizes nothing by itself.
        let cookie = Cookie::build((CSRF_COOKIE, token.to_string()))
            .path("/")
            .secure(true)
            .same_site(SameSite::Lax)
            .build();
        if let Ok(value) = HeaderValue::from_str(&cookie.to_string()) {
            response.headers_mut().append(header::SET_COOKIE, value);
        }
    }
    response
}

// ---------------------------------------------------------------------------
// Login rate limiting.

/// Sliding-window in-memory throttle keyed by arbitrary strings ("ip:..." /
/// "email:..."). Single-replica scope, same as the hold sweeper.
#[derive(Debug, Default)]
pub struct RateLimiter {
    entries: Mutex<HashMap<String, Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an attempt for `key`; returns false when the attempt exceeds
    /// `max` within `window` (the rejected attempt still counts).
    pub fn check(&self, key: &str, max: usize, window: Duration) -> bool {
        self.check_at(key, max, window, Instant::now())
    }

    fn check_at(&self, key: &str, max: usize, window: Duration, now: Instant) -> bool {
        let mut entries = self.entries.lock().expect("rate limiter lock");
        // Opportunistic pruning keeps the map from growing without bound.
        entries.retain(|_, attempts| {
            attempts.retain(|at| now.duration_since(*at) < window);
            !attempts.is_empty()
        });
        let attempts = entries.entry(key.to_string()).or_default();
        attempts.push(now);
        attempts.len() <= max
    }
}

/// Best-effort client address for throttling: the **last** (right-most) hop of
/// x-forwarded-for. Only the trusted proxy directly in front of us (the
/// ingress) can control the value it appends; a client can forge additional
/// left-most hops, so taking the first entry would let an attacker mint a fresh
/// throttle bucket per request. With a single trusted proxy the right-most
/// entry is the real client address.
pub fn client_ip(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next_back())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_tokens_are_valid_unique_hex() {
        let a = random_token();
        let b = random_token();
        assert!(valid_token(&a));
        assert!(valid_token(&b));
        assert_ne!(a, b);
        // Anything not exactly our shape is rejected.
        assert!(!valid_token(""));
        assert!(!valid_token("short"));
        assert!(!valid_token(&format!("{}g", &a[..63])));
    }

    #[test]
    fn tokens_match_requires_full_equality() {
        assert!(tokens_match("abc123", "abc123"));
        assert!(!tokens_match("abc123", "abc124"));
        assert!(!tokens_match("abc123", "abc1234"));
        assert!(!tokens_match("", ""));
    }

    #[test]
    fn form_value_finds_the_named_field() {
        let body = "email=a%40b.co&csrf_token=deadbeef&x=1";
        assert_eq!(form_value(body, "csrf_token").as_deref(), Some("deadbeef"));
        assert_eq!(form_value(body, "email").as_deref(), Some("a%40b.co"));
        assert_eq!(form_value(body, "missing"), None);
        assert_eq!(form_value("", "csrf_token"), None);
    }

    #[test]
    fn rate_limiter_blocks_after_max_and_recovers_after_window() {
        let limiter = RateLimiter::new();
        let window = Duration::from_secs(60);
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.check_at("ip:1.2.3.4", 3, window, start));
        }
        // Fourth attempt inside the window: blocked.
        assert!(!limiter.check_at("ip:1.2.3.4", 3, window, start));
        // Other keys are unaffected.
        assert!(limiter.check_at("ip:5.6.7.8", 3, window, start));
        // After the window has passed, the key is clean again.
        assert!(limiter.check_at("ip:1.2.3.4", 3, window, start + window * 2));
    }

    #[test]
    fn client_ip_uses_trusted_last_forwarded_hop() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(client_ip(&headers), "local");
        // The right-most hop is the one our trusted proxy appended; a client
        // that forges a left-most "1.2.3.4" cannot escape its real bucket.
        headers.insert(
            "x-forwarded-for",
            "1.2.3.4, 9.9.9.9, 10.0.0.1".parse().unwrap(),
        );
        assert_eq!(client_ip(&headers), "10.0.0.1");
    }
}
