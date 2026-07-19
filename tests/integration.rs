//! Integration tests driving the real router in degraded mode (no database,
//! no Supabase, no network): route smoke tests, authz guards, CSRF
//! enforcement, security headers, login rate limiting, 404s, and the /ws
//! auth gate. This is exactly the no-secrets boot mode the README promises.

use athleto_app_rs::{router, AppState, Config, SharedState};
use axum::body::Body;
use axum::http::{header, Request, Response, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

fn test_state() -> SharedState {
    Arc::new(AppState::new(
        None,
        reqwest::Client::new(),
        Config::default(),
    ))
}

async fn send(state: &SharedState, request: Request<Body>) -> Response<Body> {
    router(state.clone())
        .oneshot(request)
        .await
        .expect("router is infallible")
}

async fn get(state: &SharedState, path: &str) -> Response<Body> {
    send(state, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn body_string(response: Response<Body>) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

/// Mint a CSRF token the way a browser receives it: request any page and
/// retain the HttpOnly cookie value for a subsequent form submission.
async fn mint_csrf(state: &SharedState) -> String {
    let response = get(state, "/").await;
    let set_cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("athleto_csrf="))
        .expect("csrf cookie set on first response")
        .to_string();
    set_cookie
        .trim_start_matches("athleto_csrf=")
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

fn form_post(path: &str, token: Option<&str>, body: &str) -> Request<Body> {
    let mut builder =
        Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    let body = match token {
        Some(token) => {
            builder = builder.header(header::COOKIE, format!("athleto_csrf={token}"));
            if body.is_empty() {
                format!("csrf_token={token}")
            } else {
                format!("csrf_token={token}&{body}")
            }
        }
        None => body.to_string(),
    };
    builder.body(Body::from(body)).unwrap()
}

// ---------------------------------------------------------------------------
// Route smoke tests (degraded mode: built-in catalog, no auth).

#[tokio::test]
async fn home_healthz_and_login_render() {
    let state = test_state();

    let home = get(&state, "/").await;
    assert_eq!(home.status(), StatusCode::OK);
    assert!(body_string(home).await.contains("The lineup"));

    let health = get(&state, "/healthz").await;
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(body_string(health).await, "ok");

    // Supabase unset: the login page still renders (not-configured notice).
    let login = get(&state, "/login").await;
    assert_eq!(login.status(), StatusCode::OK);
    assert!(body_string(login).await.contains("Sign in"));
}

#[tokio::test]
async fn product_pages_serve_fallback_catalog_and_404_unknown_slugs() {
    let state = test_state();

    let found = get(&state, "/product/recover-o-cup").await;
    assert_eq!(found.status(), StatusCode::OK);
    assert!(body_string(found).await.contains("recover"));

    let missing = get(&state, "/product/does-not-exist").await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_routes_404_with_security_headers() {
    let state = test_state();
    let response = get(&state, "/definitely/not/a/route").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // The middleware wraps the fallback too.
    assert!(response
        .headers()
        .contains_key(header::CONTENT_SECURITY_POLICY));
}

// ---------------------------------------------------------------------------
// Authz guards.

#[tokio::test]
async fn signed_in_pages_bounce_anonymous_visitors_to_login() {
    let state = test_state();
    for path in ["/account", "/orders", "/quick-order", "/account/setup"] {
        let response = get(&state, path).await;
        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "{path} should redirect anonymous users"
        );
        assert_eq!(
            response.headers().get(header::LOCATION).unwrap(),
            "/login",
            "{path} should redirect to /login"
        );
    }
}

#[tokio::test]
async fn api_rejects_requests_without_a_key() {
    let state = test_state();
    // Degraded mode has no key store at all; either way the request must
    // fail closed, never serve data.
    for path in ["/api/v1/products", "/api/v1/orders"] {
        let response = get(&state, path).await;
        assert!(
            response.status().is_client_error() || response.status().is_server_error(),
            "{path} must not succeed unauthenticated (got {})",
            response.status()
        );
    }
}

#[tokio::test]
async fn ws_upgrade_rejects_unauthenticated_sessions() {
    let state = test_state();
    let request = Request::get("/ws")
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(header::SEC_WEBSOCKET_KEY, "x3JJHMbDL1EzLkh9GBhXDw==")
        .body(Body::empty())
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// CSRF.

#[tokio::test]
async fn state_changing_post_without_token_is_rejected() {
    let state = test_state();
    for path in ["/logout", "/cart/items", "/checkout", "/account/api-keys"] {
        let response = send(&state, form_post(path, None, "")).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{path} must reject a POST without a CSRF token"
        );
        // Regression: the CSRF-reject path used to skip the security headers.
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY),
            "{path} CSRF rejection must still carry the CSP header"
        );
        assert_eq!(
            response.headers().get(header::X_FRAME_OPTIONS).unwrap(),
            "DENY",
            "{path} CSRF rejection must still carry X-Frame-Options"
        );
    }
}

#[tokio::test]
async fn remembered_interstitial_rejects_off_origin_next_targets() {
    let state = test_state();
    let response = get(&state, "/login/remembered").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    // The client-side guard must reject "//host" and the "/\host" backslash
    // bypass (browsers normalise "\" to "/"), so an attacker-controlled
    // fragment cannot turn this into an open redirect.
    assert!(
        body.contains(r"next.charAt(1)==='\\'"),
        "remembered-page redirect guard must reject a leading backslash"
    );
    assert!(body.contains("next.charAt(1)==='/'"));
}

#[tokio::test]
async fn post_with_matching_token_passes_the_csrf_gate() {
    let state = test_state();
    let token = mint_csrf(&state).await;

    // Hidden-field variant (regular form submit): passes CSRF, and /logout
    // proceeds to its normal redirect.
    let response = send(&state, form_post("/logout", Some(&token), "")).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // Header variant (htmx request): equally accepted.
    let request = Request::post("/logout")
        .header(header::COOKIE, format!("athleto_csrf={token}"))
        .header("x-csrf-token", &token)
        .body(Body::empty())
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn post_with_mismatched_token_is_rejected() {
    let state = test_state();
    let token = mint_csrf(&state).await;
    let forged = "0".repeat(64);
    let request = Request::post("/logout")
        .header(header::COOKIE, format!("athleto_csrf={token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("csrf_token={forged}")))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn json_api_is_exempt_from_csrf() {
    let state = test_state();
    // No token anywhere; must not be a CSRF 403 (it fails on auth instead).
    let request = Request::post("/api/v1/orders")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{\"items\":[]}"))
        .unwrap();
    let response = send(&state, request).await;
    assert_ne!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response.status().is_success());
}

#[tokio::test]
async fn forms_embed_the_csrf_cookie_token() {
    let state = test_state();
    let token = mint_csrf(&state).await;
    // Same cookie sent back: the rendered add-to-cart forms must embed that
    // exact token as their hidden field.
    let request = Request::get("/")
        .header(header::COOKIE, format!("athleto_csrf={token}"))
        .body(Body::empty())
        .unwrap();
    let response = send(&state, request).await;
    let body = body_string(response).await;
    assert!(body.contains(&format!("name=\"csrf_token\" value=\"{token}\"")));
    assert!(body.contains(&format!("data-csrf-token=\"{token}\"")));
    // And the layout hands it to htmx via hx-headers on <body>.
    assert!(body.contains("hx-headers"));
    assert!(body.contains("x-csrf-token"));
}

#[tokio::test]
async fn csrf_cookie_is_httponly_and_callback_uses_rendered_token() {
    let state = test_state();
    let response = get(&state, "/").await;
    let set_cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("athleto_csrf="))
        .expect("csrf cookie set on first response");
    assert!(set_cookie.contains("HttpOnly"));
    assert!(!athleto_app_rs::pages::CALLBACK_JS.contains("document.cookie"));
    assert!(athleto_app_rs::pages::CALLBACK_JS.contains("data-csrf-token"));
}

// ---------------------------------------------------------------------------
// Security headers.

#[tokio::test]
async fn html_responses_carry_security_headers() {
    let state = test_state();
    let response = get(&state, "/").await;
    let headers = response.headers();

    let csp = headers
        .get(header::CONTENT_SECURITY_POLICY)
        .expect("CSP header present")
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("'nonce-"), "CSP should carry the inline nonce");
    assert!(csp.contains("frame-ancestors 'none'"));

    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    assert_eq!(headers.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
    assert_eq!(headers.get(header::REFERRER_POLICY).unwrap(), "no-referrer");
    // Plain http (no proxy header): no HSTS.
    assert!(!headers.contains_key(header::STRICT_TRANSPORT_SECURITY));

    // The nonce in the CSP matches the one stamped on the inline style tag.
    let nonce = csp
        .split("'nonce-")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap()
        .to_string();
    assert!(body_string(response)
        .await
        .contains(&format!("nonce=\"{nonce}\"")));
}

#[tokio::test]
async fn hsts_only_when_terminated_tls_is_signalled() {
    let state = test_state();
    let request = Request::get("/")
        .header("x-forwarded-proto", "https")
        .body(Body::empty())
        .unwrap();
    let response = send(&state, request).await;
    let hsts = response
        .headers()
        .get(header::STRICT_TRANSPORT_SECURITY)
        .expect("HSTS behind https proxy")
        .to_str()
        .unwrap();
    assert!(hsts.contains("max-age="));
}

#[tokio::test]
async fn csp_nonce_is_fresh_per_request() {
    let state = test_state();
    let csp = |response: &Response<Body>| {
        response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    };
    let first = get(&state, "/").await;
    let second = get(&state, "/").await;
    assert_ne!(csp(&first), csp(&second));
}

// ---------------------------------------------------------------------------
// Login rate limiting.

#[tokio::test]
async fn login_throttles_per_email_after_three_attempts() {
    let state = test_state();
    let token = mint_csrf(&state).await;
    for attempt in 1..=3 {
        let response = send(
            &state,
            form_post("/login", Some(&token), "email=runner%40club.example"),
        )
        .await;
        assert_ne!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "attempt {attempt} should pass"
        );
    }
    let response = send(
        &state,
        form_post("/login", Some(&token), "email=runner%40club.example"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn login_throttles_per_ip_after_five_attempts() {
    let state = test_state();
    let token = mint_csrf(&state).await;
    let post = |n: usize| {
        let mut request = form_post(
            "/login",
            Some(&token),
            &format!("email=u{n}%40club.example"),
        );
        request
            .headers_mut()
            .insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        request
    };
    for attempt in 1..=5 {
        let response = send(&state, post(attempt)).await;
        assert_ne!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "attempt {attempt} should pass"
        );
    }
    let response = send(&state, post(6)).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

    // The test router has no trusted socket peer, so forwarding headers are
    // deliberately ignored. An attacker cannot evade the bucket by spoofing
    // a new X-Forwarded-For value.
    let mut request = form_post("/login", Some(&token), "email=other%40club.example");
    request
        .headers_mut()
        .insert("x-forwarded-for", "198.51.100.7".parse().unwrap());
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ---------------------------------------------------------------------------
// Static assets + host allowlist.

#[tokio::test]
async fn vendored_htmx_is_served_with_immutable_caching() {
    let state = test_state();
    for path in ["/static/htmx-2.0.4.min.js", "/static/htmx-ext-ws-2.0.2.js"] {
        let response = get(&state, path).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
        assert!(response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/javascript"));
    }
}

// ---------------------------------------------------------------------------
// Commerce / payment / order-management handlers in degraded mode: every
// SeaORM-backed query path must gracefully fall back to a "not configured"
// notice or a fail-closed status instead of crashing when the pool is absent.

#[tokio::test]
async fn cart_page_renders_not_configured_notice_without_a_database() {
    let state = test_state();
    // The cart handler takes the `state.pool.is_none()` branch and renders the
    // storefront shell with the not-configured notice rather than 500ing.
    let response = get(&state, "/cart").await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_string(response).await;
    assert!(
        body.contains("is not configured on this deployment"),
        "degraded cart page should carry the not-configured notice"
    );
    assert!(body.contains("cart database"));
}

#[tokio::test]
async fn cart_hold_poll_returns_inactive_json_in_degraded_mode() {
    let state = test_state();
    // The htmx countdown poll is reachable anonymously; with no pool the
    // SeaORM find_cart path is skipped and it reports an inactive hold.
    let response = get(&state, "/cart/hold").await;
    assert_eq!(response.status(), StatusCode::OK);
    let ctype = response
        .headers()
        .get(header::CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ctype.starts_with("application/json"), "got {ctype}");
    let body = body_string(response).await;
    assert!(body.contains("\"active\":false"));
    assert!(body.contains("\"seconds_left\":0"));
}

#[tokio::test]
async fn hosted_payment_landing_pages_redirect_in_degraded_mode() {
    let state = test_state();
    // /pay/success with no database can't reconcile an order, so it bounces to
    // the order list rather than erroring; /pay/cancel always retries there.
    let success = get(
        &state,
        "/pay/success?provider=stripe&order=00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert!(
        success.status().is_redirection(),
        "pay/success should redirect in degraded mode (got {})",
        success.status()
    );
    assert_eq!(success.headers().get(header::LOCATION).unwrap(), "/orders");

    let cancel = get(
        &state,
        "/pay/cancel?order=00000000-0000-0000-0000-000000000000",
    )
    .await;
    assert!(cancel.status().is_redirection());
    assert_eq!(
        cancel.headers().get(header::LOCATION).unwrap(),
        "/orders?paycancel=1"
    );
}

#[tokio::test]
async fn provider_webhooks_are_csrf_exempt_and_fail_closed_unconfigured() {
    let state = test_state();
    // Webhooks authenticate with signed raw payloads, not browser cookies, so
    // they must NOT be blocked by the CSRF gate (no 403). With no provider
    // configured they fail closed with 503 -- never process an order.
    for path in ["/webhooks/stripe", "/webhooks/paypal", "/webhooks/square"] {
        let request = Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{\"id\":\"evt_test\",\"type\":\"noop\"}"))
            .unwrap();
        let response = send(&state, request).await;
        assert_ne!(
            response.status(),
            StatusCode::FORBIDDEN,
            "{path} must be exempt from CSRF"
        );
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{path} must fail closed when the provider is unconfigured"
        );
        // Even the fail-closed path flows back through the security layer.
        assert!(response
            .headers()
            .contains_key(header::CONTENT_SECURITY_POLICY));
    }
}

// ---------------------------------------------------------------------------
// CSRF edge cases: the double-submit gate must reject every partial state
// (cookie-without-field, field-without-cookie) as firmly as a full mismatch.

#[tokio::test]
async fn csrf_cookie_without_matching_field_is_rejected() {
    let state = test_state();
    let token = mint_csrf(&state).await;
    // Valid cookie present, but the form omits the hidden csrf_token field:
    // there is nothing to compare against, so the request is forbidden.
    let request = Request::post("/logout")
        .header(header::COOKIE, format!("athleto_csrf={token}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("intent=logout"))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn csrf_field_without_a_cookie_is_rejected() {
    let state = test_state();
    // A form field with no backing cookie is the classic double-submit bypass
    // attempt: the server minted no prior token, so `is_new_token` is true and
    // verification fails closed even though field and (fresh) token could look
    // alike. Send a plausible 64-hex value with no cookie at all.
    let forged = "a".repeat(64);
    let request = Request::post("/logout")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("csrf_token={forged}")))
        .unwrap();
    let response = send(&state, request).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    // A brand-new token cookie is issued so the reloaded form can succeed.
    assert!(response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.starts_with("athleto_csrf=")));
}

#[test]
fn host_allowlist_is_permissive_only_when_unset() {
    let permissive = Config::default();
    assert!(permissive.host_allowed("evil.example"));

    let locked = Config {
        allowed_hosts: Some(vec![
            "app.athleto.store".to_string(),
            "biz.athleto.store".to_string(),
            "localhost".to_string(),
        ]),
        ..Config::default()
    };
    assert!(locked.host_allowed("app.athleto.store"));
    assert!(locked.host_allowed("biz.athleto.store"));
    // Ports are ignored for matching.
    assert!(locked.host_allowed("localhost:8080"));
    assert!(!locked.host_allowed("evil.example"));
    assert!(!locked.host_allowed("app.athleto.store.evil.example"));
}
