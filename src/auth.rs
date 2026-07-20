//! Supabase GoTrue auth: passwordless magic-link login, MFA session
//! upgrades, session cookies, and the current-user extractor.
//!
//! There are no passwords. POST /login asks GoTrue to email a magic link;
//! only explicitly enabled, Turnstile-verified self-signup may create a new
//! user. The default email template points at GoTrue's own /verify, which 302s back to
//! /auth/callback with tokens in the URL fragment; inline JS forwards them to
//! POST /auth/session, which validates the token against /auth/v1/user before
//! setting HttpOnly cookies. /auth/confirm additionally supports the cleaner
//! `token_hash` server-side pattern for when a custom SMTP provider (and thus
//! a custom template) is configured.
//!
//! Users with a verified MFA factor are bounced to /login/2fa after the link
//! to upgrade the session to AAL2. B2B accounts are required to enroll a
//! factor before they can order (enforced by `require_full`).

use std::time::Duration;

use axum::extract::{FromRequestParts, Query, State};
use axum::http::header::HOST;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use maud::{html, Markup, PreEscaped};
use serde::Deserialize;
use uuid::Uuid;

use crate::db::{self, CustomerProfile};
use crate::{
    anti_abuse, mfa_state, pages,
    request_trust::{self, PeerAddress},
    security, Config, SharedState,
};

pub const ACCESS_COOKIE: &str = "sb_access_token";
pub const REFRESH_COOKIE: &str = "sb_refresh_token";
/// Short-lived browser binding for a magic-link request. The high-entropy
/// nonce is HttpOnly and must accompany the callback before session cookies
/// are minted, preventing a link initiated in another browser from logging a
/// recipient in unexpectedly.
const LOGIN_FLOW_COOKIE: &str = "athleto_login_flow";
/// Signed, short-lived binding for a pending SMS MFA challenge.
const MFA_CHALLENGE_COOKIE: &str = "sb_mfa_challenge";

#[derive(Debug, Clone, Deserialize)]
pub struct Factor {
    pub id: String,
    pub factor_type: String,
    pub status: String,
    #[serde(default)]
    pub friendly_name: Option<String>,
}

impl Factor {
    pub fn is_verified(&self) -> bool {
        self.status == "verified"
    }
}

#[derive(Clone)]
pub struct AuthUser {
    pub id: Uuid,
    pub email: Option<String>,
    /// Authenticator assurance level claimed by the access token JWT
    /// ("aal1" or "aal2").
    pub aal: String,
    pub factors: Vec<Factor>,
    pub access_token: String,
}

impl AuthUser {
    pub fn verified_factors(&self) -> impl Iterator<Item = &Factor> {
        self.factors.iter().filter(|factor| factor.is_verified())
    }

    pub fn has_verified_factor(&self) -> bool {
        self.verified_factors().next().is_some()
    }

    /// True when the account has MFA enrolled but this session is still AAL1.
    pub fn needs_aal2(&self) -> bool {
        self.has_verified_factor() && self.aal != "aal2"
    }

    pub fn email_str(&self) -> &str {
        self.email.as_deref().unwrap_or("(no email)")
    }
}

/// Current-user extractor. Resolves to `MaybeUser(None)` (never rejects) when
/// there is no session cookie, Supabase is not configured, or the token is
/// invalid/expired.
#[derive(Clone, Default)]
pub struct MaybeUser(pub Option<AuthUser>);

impl MaybeUser {
    pub fn as_ref(&self) -> Option<&AuthUser> {
        self.0.as_ref()
    }
}

#[derive(Debug, Deserialize)]
struct GotrueUser {
    id: String,
    email: Option<String>,
    // GoTrue's user object carries the enrolled MFA factors directly. This is
    // the AUTHORITATIVE factor list and it is parsed here so AAL2 enforcement
    // does not depend on the shape of the separate /auth/v1/factors response.
    // A user with no factors yields an empty array, never a missing key, so
    // defaulting here cannot hide an enrolled factor — it only tolerates the
    // legitimate no-MFA case.
    #[serde(default)]
    factors: Vec<Factor>,
}

#[derive(Debug, Default, Deserialize)]
struct GotrueMfaState {
    #[serde(default)]
    factors: Vec<Factor>,
    #[serde(default)]
    current_level: Option<String>,
}

async fn fetch_mfa_state(state: &SharedState, token: &str) -> Option<GotrueMfaState> {
    let (base, key) = state.config.supabase()?;
    let response = state
        .http
        .get(format!("{base}/auth/v1/factors"))
        .header("apikey", key)
        .bearer_auth(token)
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => response.json().await.ok(),
        Ok(response) => {
            tracing::warn!(status = %response.status(), "GoTrue MFA state request rejected");
            None
        }
        Err(err) => {
            tracing::warn!(error = %err, "GoTrue MFA state request failed");
            None
        }
    }
}

/// Combine the factor lists from `/auth/v1/user` and `/auth/v1/factors`,
/// deduplicating by factor id and preferring whichever copy reports the
/// stronger (verified) status. A factor known to either endpoint counts, so a
/// verified factor must be absent from BOTH to escape AAL2 enforcement.
fn merge_factors(primary: Vec<Factor>, secondary: Vec<Factor>) -> Vec<Factor> {
    let mut by_id: std::collections::HashMap<String, Factor> = std::collections::HashMap::new();
    for factor in primary.into_iter().chain(secondary) {
        match by_id.get(&factor.id) {
            // Keep the entry that is verified if the two disagree, so a stale
            // "unverified" copy can never mask a verified one.
            Some(existing) if existing.is_verified() && !factor.is_verified() => {}
            _ => {
                by_id.insert(factor.id.clone(), factor);
            }
        }
    }
    by_id.into_values().collect()
}

async fn fetch_user(state: &SharedState, token: &str) -> Option<AuthUser> {
    let (base, key) = state.config.supabase()?;
    let response = state
        .http
        .get(format!("{base}/auth/v1/user"))
        .header("apikey", key)
        .bearer_auth(token)
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => {
            match response.json::<GotrueUser>().await {
                Ok(user) => {
                    let mfa = fetch_mfa_state(state, token).await?;
                    // Union the two factor sources by id. Previously the
                    // factor list came ONLY from /auth/v1/factors, so any 200
                    // response missing/renaming its `factors` key silently
                    // emptied the list and needs_aal2() went false for EVERY
                    // enrolled user (a fail-open 2FA bypass). Trusting either
                    // source means a verified factor has to disappear from
                    // BOTH to be missed, while a genuine no-MFA user (empty in
                    // both) still needs no step-up — so this cannot lock anyone
                    // out. AAL still comes from current_level, defaulting to
                    // the conservative "aal1" (assume NOT stepped up) when the
                    // level is absent.
                    let factors = merge_factors(user.factors, mfa.factors);
                    Uuid::parse_str(&user.id).ok().map(|id| AuthUser {
                        id,
                        email: user.email,
                        aal: mfa.current_level.unwrap_or_else(|| "aal1".to_string()),
                        factors,
                        access_token: token.to_string(),
                    })
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to decode GoTrue user payload");
                    None
                }
            }
        }
        Ok(_) => None, // expired or revoked token
        Err(err) => {
            tracing::warn!(error = %err, "GoTrue /auth/v1/user request failed");
            None
        }
    }
}

impl FromRequestParts<SharedState> for MaybeUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let Ok(jar) = CookieJar::from_request_parts(parts, state).await;
        let Some(token) = jar
            .get(ACCESS_COOKIE)
            .map(|cookie| cookie.value().to_string())
        else {
            return Ok(Self(None));
        };
        Ok(Self(fetch_user(state, &token).await))
    }
}

/// Host-aware flag for the configured canonical B2B storefront. A prefix in
/// an arbitrary Host header must not select a more privileged-looking UI.
#[derive(Debug, Clone, Copy, Default)]
pub struct Biz(pub bool);

impl FromRequestParts<SharedState> for Biz {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let host = parts
            .headers
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        Ok(Self(state.config.is_biz_host(host)))
    }
}

fn session_cookie(name: &'static str, value: String) -> Cookie<'static> {
    Cookie::build((name, value))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .build()
}

fn login_flow_cookie(value: String) -> Cookie<'static> {
    Cookie::build((LOGIN_FLOW_COOKIE, value))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .build()
}

pub fn auth_session_cookie(value: String) -> Cookie<'static> {
    session_cookie(ACCESS_COOKIE, value)
}

pub fn refresh_session_cookie(value: String) -> Cookie<'static> {
    session_cookie(REFRESH_COOKIE, value)
}

fn clear_session(jar: CookieJar) -> CookieJar {
    jar.remove(Cookie::build(ACCESS_COOKIE).path("/"))
        .remove(Cookie::build(REFRESH_COOKIE).path("/"))
        .remove(Cookie::build(LOGIN_FLOW_COOKIE).path("/"))
        .remove(Cookie::build(MFA_CHALLENGE_COOKIE).path("/"))
}

async fn gotrue_error_message(response: reqwest::Response) -> String {
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    let message = body
        .get("msg")
        .or_else(|| body.get("error_description"))
        .or_else(|| body.get("message"))
        .or_else(|| body.get("error"))
        .and_then(|value| value.as_str())
        .unwrap_or("authentication request failed");
    format!("{message} (status {status})")
}

/// Scheme + host to build same-site redirect URLs from, derived from the Host
/// header so app./biz./localhost all round-trip to themselves. Hosts outside
/// the ALLOWED_HOSTS allowlist fall back to the configured public base so a
/// spoofed Host header cannot steer auth redirects off-site.
pub(crate) fn request_base(headers: &HeaderMap, state: &SharedState) -> String {
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if state.config.is_biz_host(host) {
        state.config.biz_public_base_url.clone()
    } else {
        state.config.public_base_url.clone()
    }
}

fn not_configured_page(user: &MaybeUser, title: &str, heading: &str) -> Markup {
    pages::layout(
        title,
        user.as_ref(),
        html! {
            section .section {
                h2 { (heading) }
                (pages::not_configured_notice("Supabase auth"))
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Login: magic link request.

/// GET /login
pub async fn login_page(State(state): State<SharedState>, user: MaybeUser, biz: Biz) -> Response {
    if user.as_ref().is_some() {
        return Redirect::to("/").into_response();
    }
    if state.config.supabase().is_none() {
        return not_configured_page(&user, "Sign in | AthletO", "Sign in").into_response();
    }
    login_form_for(&state.config, biz, None).into_response()
}

fn login_form(biz: Biz, notice: Option<Markup>) -> Markup {
    login_form_with_turnstile(biz, notice, None)
}

fn login_form_for(config: &Config, biz: Biz, notice: Option<Markup>) -> Markup {
    let site_key = config
        .self_signup_ready()
        .then_some(config.turnstile_site_key.as_deref())
        .flatten();
    login_form_with_turnstile(biz, notice, site_key)
}

fn login_form_with_turnstile(
    biz: Biz,
    notice: Option<Markup>,
    turnstile_site_key: Option<&str>,
) -> Markup {
    pages::layout_for(
        "Sign in | AthletO",
        None,
        biz,
        html! {
            section .section .auth-section {
                div .auth-card {
                    p .eyebrow { @if biz.0 { "Business portal" } @else { "Athletes only (and everyone else)" } }
                    h2 { "Sign in with a magic link" }
                    p .auth-lede {
                        "No passwords here. Type the email associated with your account and we'll send a one-time link."
                    }
                    @if let Some(notice) = notice { (notice) }
                    div #past-logins .past-logins {}
                    form method="post" action="/login" {
                        (pages::csrf_field())
                        label {
                            "Email"
                            input #login-email type="email" name="email" required
                                autocomplete="email" placeholder="you@club.example";
                        }
                        @if let Some(site_key) = turnstile_site_key {
                            div .cf-turnstile data-sitekey=(site_key) {}
                            script src="https://challenges.cloudflare.com/turnstile/v0/api.js" async defer {}
                        }
                        button .primary type="submit" { "Email me a sign-in link" }
                    }
                    @if turnstile_site_key.is_none() {
                        p .auth-alt { "New accounts are created by invitation or an approved business onboarding flow." }
                    }
                    p .auth-alt {
                        @if biz.0 {
                            "Ordering for a team of one? " a href="https://app.athleto.store/login" { "Personal store" }
                        } @else {
                            "Buying for a retailer or club? " a href="https://biz.athleto.store/login" { "Business portal" }
                        }
                    }
                }
                script nonce=(security::csp_nonce()) { (PreEscaped(pages::LOGIN_CHIPS_JS)) }
            }
        },
    )
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    email: String,
    #[serde(rename = "cf-turnstile-response", default)]
    turnstile_response: String,
}

/// POST /login -- ask GoTrue to send the magic link.
pub async fn login_submit(
    State(state): State<SharedState>,
    biz: Biz,
    headers: HeaderMap,
    peer: PeerAddress,
    jar: CookieJar,
    Form(request): Form<LoginRequest>,
) -> Response {
    let email = request.email.trim().to_lowercase();

    // Throttle before anything touches GoTrue: each accepted submit sends an
    // email, so this endpoint must not be free to hammer. The only forwarded
    // address accepted here is one supplied by an explicitly trusted proxy.
    let peer_ip = peer.0.map(|peer| peer.ip());
    let ip = request_trust::client_ip(&headers, peer.0, &state.config.trusted_proxy_networks);
    let ip_ok = state
        .rate_limits
        .check("login-ip", &ip, 5, Duration::from_secs(60))
        .await;
    let email_ok = state
        .rate_limits
        .check("login-email", &email, 3, Duration::from_secs(5 * 60))
        .await;
    if !ip_ok || !email_ok {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            login_form_for(
                &state.config,
                biz,
                Some(html! { div .notice .error {
                    "Too many sign-in attempts. Wait a few minutes and try again."
                } }),
            ),
        )
            .into_response();
    }

    let Some((base, key)) = state.config.supabase() else {
        return not_configured_page(&MaybeUser(None), "Sign in | AthletO", "Sign in")
            .into_response();
    };

    let create_user = if state.config.self_signup_enabled {
        if !state.config.self_signup_ready() {
            tracing::error!("self-signup enabled without complete Turnstile configuration");
            return login_form_for(
                &state.config,
                biz,
                Some(html! { div .notice .error { "Sign-up is temporarily unavailable. Please try again later." } }),
            )
            .into_response();
        }
        let Some(turnstile_secret) = state.config.turnstile_secret.as_deref() else {
            tracing::error!("self-signup enabled without ATHLETO_TURNSTILE_SECRET");
            return login_form_for(
                &state.config,
                biz,
                Some(html! { div .notice .error { "Sign-up is temporarily unavailable. Please try again later." } }),
            )
            .into_response();
        };
        match anti_abuse::verify_turnstile(
            &state.http,
            turnstile_secret,
            &request.turnstile_response,
            peer_ip,
        )
        .await
        {
            Ok(true) => true,
            Ok(false) => {
                return login_form_for(
                    &state.config,
                    biz,
                    Some(html! { div .notice .error { "Please complete the anti-abuse check and try again." } }),
                )
                .into_response()
            }
            Err(err) => {
                tracing::warn!(error = err, "Turnstile verification failed");
                return login_form_for(
                    &state.config,
                    biz,
                    Some(html! { div .notice .error { "Unable to verify the anti-abuse check. Try again shortly." } }),
                )
                .into_response()
            }
        }
    } else {
        false
    };

    let flow = Uuid::new_v4().to_string();
    let redirect_to = format!(
        "{}/auth/callback?flow={flow}",
        request_base(&headers, &state)
    );
    let response = state
        .http
        .post(format!("{base}/auth/v1/otp"))
        .query(&[("redirect_to", redirect_to.as_str())])
        .header("apikey", key)
        .json(&serde_json::json!({ "email": email, "create_user": create_user }))
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => (
            jar.add(login_flow_cookie(flow)),
            pages::layout_for(
                "Check your email | AthletO",
                None,
                biz,
                html! {
                    section .section .auth-section {
                        div .auth-card {
                            h2 { "Check your inbox" }
                            div .notice .success {
                                "We sent a sign-in link to " strong { (email) } ". "
                                "It expires in about an hour and works once."
                            }
                            p .auth-lede {
                                "Open the link on this device to land right back here, signed in. "
                                "Nothing arriving? Check spam, or "
                                a href="/login" { "try again" } "."
                            }
                        }
                    }
                },
            ),
        )
            .into_response(),
        Ok(response) => {
            let message = gotrue_error_message(response).await;
            tracing::warn!(%message, "magic link request rejected by Supabase");
            login_form_for(
                &state.config,
                biz,
                Some(html! { div .notice .error { "Could not send a sign-in link. Check your email and try again shortly." } }),
            )
            .into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "magic link request to Supabase failed");
            login_form_for(
                &state.config,
                biz,
                Some(html! { div .notice .error { "Could not reach the auth service. Try again shortly." } }),
            )
            .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Login: completing the link.

/// GET /auth/callback -- landing page for GoTrue's redirect. Tokens arrive in
/// the URL fragment (invisible to the server), so a small script forwards
/// them to POST /auth/session.
pub async fn auth_callback(biz: Biz) -> Markup {
    pages::layout_for(
        "Signing you in | AthletO",
        None,
        biz,
        html! {
            section .section .auth-section {
                div .auth-card {
                    h2 { "Signing you in..." }
                    p #callback-status .auth-lede { "One moment while we finish the handshake." }
                    noscript {
                        div .notice .error {
                            "JavaScript is required to finish signing in. Enable it and reload this page."
                        }
                    }
                }
                script nonce=(security::csp_nonce()) { (PreEscaped(pages::CALLBACK_JS)) }
            }
        },
    )
}

#[derive(Debug, Deserialize)]
pub struct SessionTokens {
    access_token: String,
    refresh_token: String,
    flow: String,
}

fn flow_matches(jar: &CookieJar, flow: &str) -> bool {
    Uuid::parse_str(flow).is_ok()
        && jar
            .get(LOGIN_FLOW_COOKIE)
            .is_some_and(|cookie| cookie.value() == flow)
}

/// POST /auth/session -- validate fragment-delivered tokens and set cookies.
pub async fn auth_session(
    State(state): State<SharedState>,
    biz: Biz,
    jar: CookieJar,
    Form(tokens): Form<SessionTokens>,
) -> Response {
    if !flow_matches(&jar, &tokens.flow) {
        return login_form(
            biz,
            Some(html! { div .notice .error { "This sign-in attempt did not start in this browser. Request a fresh link." } }),
        )
        .into_response();
    }
    let Some(user) = fetch_user(&state, &tokens.access_token).await else {
        return login_form(
            biz,
            Some(html! { div .notice .error { "That sign-in link was invalid or expired. Request a fresh one." } }),
        )
        .into_response();
    };

    let jar = jar
        .remove(Cookie::build(LOGIN_FLOW_COOKIE).path("/"))
        .add(session_cookie(ACCESS_COOKIE, tokens.access_token.clone()))
        .add(session_cookie(REFRESH_COOKIE, tokens.refresh_token.clone()));

    if user.needs_aal2() {
        return (jar, Redirect::to("/login/2fa")).into_response();
    }
    finalize_login(&state, &user, jar).await
}

#[derive(Debug, Deserialize)]
pub struct ConfirmParams {
    token_hash: String,
    flow: String,
    #[serde(rename = "type", default = "default_confirm_type")]
    verify_type: String,
}

fn default_confirm_type() -> String {
    "magiclink".to_string()
}

#[derive(Debug, Default, Deserialize)]
struct GotrueSession {
    access_token: Option<String>,
    refresh_token: Option<String>,
}

/// GET /auth/confirm?token_hash=...&type=magiclink -- server-side verify
/// path, used once a custom SMTP provider + email template is configured.
pub async fn auth_confirm(
    State(state): State<SharedState>,
    biz: Biz,
    jar: CookieJar,
    Query(params): Query<ConfirmParams>,
) -> Response {
    let Some((base, key)) = state.config.supabase() else {
        return not_configured_page(&MaybeUser(None), "Sign in | AthletO", "Sign in")
            .into_response();
    };
    if !flow_matches(&jar, &params.flow) {
        return login_form(
            biz,
            Some(html! { div .notice .error { "This sign-in attempt did not start in this browser. Request a fresh link." } }),
        )
        .into_response();
    }

    let response = state
        .http
        .post(format!("{base}/auth/v1/verify"))
        .header("apikey", key)
        .json(&serde_json::json!({
            "type": params.verify_type,
            "token_hash": params.token_hash,
        }))
        .send()
        .await;

    let session: GotrueSession = match response {
        Ok(response) if response.status().is_success() => response.json().await.unwrap_or_default(),
        Ok(response) => {
            let message = gotrue_error_message(response).await;
            tracing::warn!(%message, "magic link verification rejected by Supabase");
            return login_form(
                biz,
                Some(html! { div .notice .error { "That sign-in link was invalid or expired. Request a fresh one." } }),
            )
            .into_response();
        }
        Err(err) => {
            tracing::error!(error = %err, "verify request to Supabase failed");
            return login_form(
                biz,
                Some(html! { div .notice .error { "Could not reach the auth service. Try again shortly." } }),
            )
            .into_response();
        }
    };

    let (Some(access), Some(refresh)) = (session.access_token, session.refresh_token) else {
        return login_form(
            biz,
            Some(html! { div .notice .error { "The auth service returned no session. Request a fresh link." } }),
        )
        .into_response();
    };

    let Some(user) = fetch_user(&state, &access).await else {
        return login_form(
            biz,
            Some(html! { div .notice .error { "That sign-in link was invalid or expired. Request a fresh one." } }),
        )
        .into_response();
    };

    let jar = jar
        .remove(Cookie::build(LOGIN_FLOW_COOKIE).path("/"))
        .add(session_cookie(ACCESS_COOKIE, access))
        .add(session_cookie(REFRESH_COOKIE, refresh));

    if user.needs_aal2() {
        return (jar, Redirect::to("/login/2fa")).into_response();
    }
    finalize_login(&state, &user, jar).await
}

/// After the session is fully established (AAL2 done if applicable): record
/// the successful login, pick where to send the user, and detour through the
/// /login/remembered interstitial so the browser can cache the email in
/// IndexedDB for the login page's "welcome back" chips.
pub async fn finalize_login(state: &SharedState, user: &AuthUser, jar: CookieJar) -> Response {
    let profile = load_profile(state, user.id).await;

    if let (Some(pool), Some(email)) = (&state.pool, user.email.as_deref()) {
        if let Err(err) = db::record_login_event(pool, user.id, email, &user.aal).await {
            tracing::warn!(error = %err, "failed to record login event");
        }
    }

    let next = match &profile {
        None if state.pool.is_some() => "/account/setup",
        Some(profile) if profile.is_b2b() && !user.has_verified_factor() => {
            "/account?required2fa=1"
        }
        _ => "/",
    };

    let email = user.email.as_deref().unwrap_or_default();
    let fragment = format!("email={}&next={}", urlencode(email), urlencode(next));
    (jar, Redirect::to(&format!("/login/remembered#{fragment}"))).into_response()
}

/// Minimal percent-encoding for values placed in the interstitial fragment.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// GET /login/remembered -- stores the just-used email in IndexedDB (capped
/// at the three most recent) and forwards to the real destination.
pub async fn remembered_page(biz: Biz) -> Markup {
    pages::layout_for(
        "Welcome back | AthletO",
        None,
        biz,
        html! {
            section .section .auth-section {
                div .auth-card {
                    h2 { "You're in" }
                    p .auth-lede { "Taking you to the store..." }
                }
                script nonce=(security::csp_nonce()) { (PreEscaped(pages::REMEMBER_JS)) }
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Login: second factor.

/// GET /login/2fa
pub async fn login_2fa_page(
    State(_state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
    Query(params): Query<TwoFaPageParams>,
) -> Response {
    let Some(user) = user.as_ref() else {
        return Redirect::to("/login").into_response();
    };
    if !user.needs_aal2() {
        return Redirect::to("/").into_response();
    }
    two_fa_form(user, biz, params.sent.is_some(), None).into_response()
}

#[derive(Debug, Deserialize)]
pub struct TwoFaPageParams {
    sent: Option<String>,
}

fn two_fa_form(user: &AuthUser, biz: Biz, sms_sent: bool, notice: Option<Markup>) -> Markup {
    let totp: Vec<&Factor> = user
        .verified_factors()
        .filter(|factor| factor.factor_type == "totp")
        .collect();
    let phone: Vec<&Factor> = user
        .verified_factors()
        .filter(|factor| factor.factor_type == "phone")
        .collect();

    pages::layout_for(
        "Two-factor check | AthletO",
        None,
        biz,
        html! {
            section .section .auth-section {
                div .auth-card {
                    p .eyebrow { "Second factor" }
                    h2 { "Prove it's really you" }
                    p .auth-lede { "Signed in as " strong { (user.email_str()) } "." }
                    @if let Some(notice) = notice { (notice) }
                    @if sms_sent {
                        div .notice .success { "Code sent by text. Enter it below." }
                    }
                    @for factor in &totp {
                        form method="post" action="/login/2fa" {
                            (pages::csrf_field())
                            input type="hidden" name="factor_id" value=(factor.id);
                            label {
                                "Authenticator app code"
                                input .code-input type="text" name="code" inputmode="numeric"
                                    pattern="[0-9]*" minlength="6" maxlength="8" required
                                    autocomplete="one-time-code" placeholder="123456";
                            }
                            button .primary type="submit" { "Verify" }
                        }
                    }
                    @for factor in &phone {
                        div .factor-alt {
                            form method="post" action="/login/2fa/send" {
                                (pages::csrf_field())
                                input type="hidden" name="factor_id" value=(factor.id);
                                button type="submit" { "Text a code to my phone" }
                            }
                            @if sms_sent {
                                form method="post" action="/login/2fa" {
                                    (pages::csrf_field())
                                    input type="hidden" name="factor_id" value=(factor.id);
                                    label {
                                        "SMS code"
                                        input .code-input type="text" name="code" inputmode="numeric"
                                            pattern="[0-9]*" minlength="6" maxlength="8" required
                                            autocomplete="one-time-code" placeholder="123456";
                                    }
                                    button .primary type="submit" { "Verify" }
                                }
                            }
                        }
                    }
                    form method="post" action="/logout" {
                        (pages::csrf_field())
                        button .linklike type="submit" { "Cancel and sign out" }
                    }
                }
            }
        },
    )
}

#[derive(Debug, Deserialize)]
pub struct ChallengeRequest {
    factor_id: String,
}

fn verified_factor<'a>(user: &'a AuthUser, factor_id: &str) -> Option<&'a Factor> {
    user.verified_factors()
        .find(|factor| factor.id == factor_id)
}

/// POST /login/2fa/send -- create a challenge for an SMS factor (GoTrue sends
/// the text) and stash the challenge id for the verify step.
pub async fn login_2fa_send(
    State(state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
    jar: CookieJar,
    Form(request): Form<ChallengeRequest>,
) -> Response {
    let Some(user) = user.as_ref() else {
        return Redirect::to("/login").into_response();
    };
    let Some(factor) = verified_factor(user, &request.factor_id) else {
        return two_fa_form(
            user,
            biz,
            false,
            Some(html! { div .notice .error { "Choose one of the verified factors on this account." } }),
        )
        .into_response();
    };
    if factor.factor_type != "phone" {
        return two_fa_form(
            user,
            biz,
            false,
            Some(html! { div .notice .error { "Authenticator-app codes do not need a text message." } }),
        )
        .into_response();
    }
    let Some(state_key) = state.config.mfa_state_key.as_ref() else {
        tracing::error!("ATHLETO_MFA_STATE_KEY is required for SMS MFA");
        return two_fa_form(
            user,
            biz,
            false,
            Some(html! { div .notice .error { "Text-message verification is temporarily unavailable." } }),
        )
        .into_response();
    };
    // Each send triggers an SMS; throttle per user so a live AAL1 session can't
    // be used to spam the victim's phone (toll/SMS-bomb).
    if !state
        .rate_limits
        .check(
            "mfa-send",
            &user.id.to_string(),
            3,
            Duration::from_secs(5 * 60),
        )
        .await
    {
        return two_fa_form(
            user,
            biz,
            false,
            Some(html! { div .notice .error {
                "Too many code requests. Wait a few minutes and try again."
            } }),
        )
        .into_response();
    }
    match create_challenge(&state, &user.access_token, &request.factor_id).await {
        Ok(challenge_id) => {
            let state = mfa_state::new_challenge(user.id, request.factor_id, challenge_id);
            let value = match mfa_state::seal(state_key, &state) {
                Ok(value) => value,
                Err(error) => {
                    tracing::error!(error = %error, "could not seal SMS MFA challenge state");
                    return two_fa_form(
                        user,
                        biz,
                        false,
                        Some(html! { div .notice .error { "Text-message verification is temporarily unavailable." } }),
                    )
                    .into_response();
                }
            };
            let cookie = Cookie::build((MFA_CHALLENGE_COOKIE, value))
                .path("/")
                .http_only(true)
                .secure(true)
                .same_site(SameSite::Lax)
                .build();
            (jar.add(cookie), Redirect::to("/login/2fa?sent=1")).into_response()
        }
        Err(message) => two_fa_form(
            user,
            biz,
            false,
            Some(html! { div .notice .error { (message) } }),
        )
        .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct VerifyRequest {
    factor_id: String,
    code: String,
}

/// POST /login/2fa -- verify a factor code and upgrade the session to AAL2.
pub async fn login_2fa_submit(
    State(state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
    jar: CookieJar,
    Form(request): Form<VerifyRequest>,
) -> Response {
    let Some(user) = user.as_ref() else {
        return Redirect::to("/login").into_response();
    };
    let Some(factor) = verified_factor(user, &request.factor_id) else {
        return two_fa_form(
            user,
            biz,
            false,
            Some(html! { div .notice .error { "Choose one of the verified factors on this account." } }),
        )
        .into_response();
    };

    // SMS challenges were created by /login/2fa/send; TOTP challenges can be
    // minted right before verification.
    let stored = state.config.mfa_state_key.as_ref().and_then(|state_key| {
        jar.get(MFA_CHALLENGE_COOKIE)
            .and_then(|cookie| mfa_state::open(state_key, cookie.value(), user.id).ok())
    });
    let challenge_id = match stored {
        Some(stored) if stored.factor_id == request.factor_id => stored.challenge_id,
        _ if factor.factor_type == "totp" => {
            match create_challenge(&state, &user.access_token, &request.factor_id).await {
            Ok(challenge_id) => challenge_id,
            Err(message) => {
                return two_fa_form(
                    user,
                    biz,
                    false,
                    Some(html! { div .notice .error { (message) } }),
                )
                .into_response()
            }
            }
        }
        _ => {
            return two_fa_form(
                user,
                biz,
                false,
                Some(html! { div .notice .error { "Request a new text-message code before verifying." } }),
            )
            .into_response()
        }
    };

    match verify_challenge(
        &state,
        &user.access_token,
        &request.factor_id,
        &challenge_id,
        request.code.trim(),
    )
    .await
    {
        Ok((access, refresh)) => {
            let jar = clear_challenge(jar)
                .add(session_cookie(ACCESS_COOKIE, access.clone()))
                .add(session_cookie(REFRESH_COOKIE, refresh));
            match fetch_user(&state, &access).await {
                Some(upgraded) => finalize_login(&state, &upgraded, jar).await,
                None => (jar, Redirect::to("/")).into_response(),
            }
        }
        Err(message) => two_fa_form(
            user,
            biz,
            false,
            Some(html! { div .notice .error { "Verification failed: " (message) } }),
        )
        .into_response(),
    }
}

fn clear_challenge(jar: CookieJar) -> CookieJar {
    jar.remove(Cookie::build(MFA_CHALLENGE_COOKIE).path("/"))
}

// ---------------------------------------------------------------------------
// GoTrue MFA helpers (shared with the account pages).

/// POST /factors/{id}/challenge -- returns the challenge id. For phone
/// factors this also sends the SMS.
pub async fn create_challenge(
    state: &SharedState,
    token: &str,
    factor_id: &str,
) -> Result<String, String> {
    let Some((base, key)) = state.config.supabase() else {
        return Err("Supabase is not configured".to_string());
    };
    let response = state
        .http
        .post(format!("{base}/auth/v1/factors/{factor_id}/challenge"))
        .header("apikey", key)
        .bearer_auth(token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|err| format!("auth service unreachable: {err}"))?;
    if !response.status().is_success() {
        return Err(gotrue_error_message(response).await);
    }
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    body.get("id")
        .and_then(|value| value.as_str())
        .map(String::from)
        .ok_or_else(|| "challenge response had no id".to_string())
}

/// POST /factors/{id}/verify -- returns the upgraded (AAL2) session tokens.
pub async fn verify_challenge(
    state: &SharedState,
    token: &str,
    factor_id: &str,
    challenge_id: &str,
    code: &str,
) -> Result<(String, String), String> {
    let Some((base, key)) = state.config.supabase() else {
        return Err("Supabase is not configured".to_string());
    };
    let response = state
        .http
        .post(format!("{base}/auth/v1/factors/{factor_id}/verify"))
        .header("apikey", key)
        .bearer_auth(token)
        .json(&serde_json::json!({ "challenge_id": challenge_id, "code": code }))
        .send()
        .await
        .map_err(|err| format!("auth service unreachable: {err}"))?;
    if !response.status().is_success() {
        return Err(gotrue_error_message(response).await);
    }
    let session: GotrueSession = response.json().await.unwrap_or_default();
    match (session.access_token, session.refresh_token) {
        (Some(access), Some(refresh)) => Ok((access, refresh)),
        _ => Err("verify response had no session".to_string()),
    }
}

/// POST /factors -- enroll a new factor. Returns the raw GoTrue JSON (the
/// TOTP variant carries qr_code/secret/uri the account page renders).
pub async fn enroll_factor(
    state: &SharedState,
    token: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let Some((base, key)) = state.config.supabase() else {
        return Err("Supabase is not configured".to_string());
    };
    let response = state
        .http
        .post(format!("{base}/auth/v1/factors"))
        .header("apikey", key)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .map_err(|err| format!("auth service unreachable: {err}"))?;
    if !response.status().is_success() {
        return Err(gotrue_error_message(response).await);
    }
    response
        .json()
        .await
        .map_err(|err| format!("bad enroll response: {err}"))
}

/// DELETE /factors/{id}.
pub async fn unenroll_factor(
    state: &SharedState,
    token: &str,
    factor_id: &str,
) -> Result<(), String> {
    let Some((base, key)) = state.config.supabase() else {
        return Err("Supabase is not configured".to_string());
    };
    let response = state
        .http
        .delete(format!("{base}/auth/v1/factors/{factor_id}"))
        .header("apikey", key)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|err| format!("auth service unreachable: {err}"))?;
    if !response.status().is_success() {
        return Err(gotrue_error_message(response).await);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Route guards.

pub async fn load_profile(state: &SharedState, user_id: Uuid) -> Option<CustomerProfile> {
    let pool = state.pool.as_ref()?;
    match db::get_profile(pool, user_id).await {
        Ok(profile) => profile,
        Err(err) => {
            tracing::warn!(error = %err, "profile lookup failed");
            None
        }
    }
}

/// Gate for signed-in pages: bounce anonymous visitors to /login and
/// MFA-enrolled users still at AAL1 to /login/2fa. Returns the user plus
/// their profile (None until /account/setup has run).
pub async fn require_full(
    state: &SharedState,
    user: &MaybeUser,
) -> Result<(AuthUser, Option<CustomerProfile>), Response> {
    let Some(user) = user.as_ref() else {
        return Err(Redirect::to("/login").into_response());
    };
    if user.needs_aal2() {
        return Err(Redirect::to("/login/2fa").into_response());
    }
    let profile = load_profile(state, user.id).await;
    Ok((user.clone(), profile))
}

/// Additional gate for order placement and other B2B-sensitive actions:
/// business accounts must have a verified second factor.
#[allow(clippy::result_large_err)]
pub fn require_b2b_ready(
    user: &AuthUser,
    profile: Option<&CustomerProfile>,
) -> Result<(), Response> {
    if let Some(profile) = profile {
        if profile.is_b2b() {
            if !profile.is_b2b_approved() {
                return Err(Redirect::to("/account?approval=pending").into_response());
            }
            if !user.has_verified_factor() {
                return Err(Redirect::to("/account?required2fa=1").into_response());
            }
        }
    }
    Ok(())
}

/// POST /logout -- best-effort GoTrue logout, then clear session cookies.
pub async fn logout(State(state): State<SharedState>, jar: CookieJar) -> Response {
    if let (Some((base, key)), Some(token)) = (
        state.config.supabase(),
        jar.get(ACCESS_COOKIE)
            .map(|cookie| cookie.value().to_string()),
    ) {
        let result = state
            .http
            .post(format!("{base}/auth/v1/logout"))
            .header("apikey", key)
            .bearer_auth(&token)
            .send()
            .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "GoTrue logout call failed; clearing cookies anyway");
        }
    }
    (clear_session(jar), Redirect::to("/")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn factor(status: &str, kind: &str) -> Factor {
        Factor {
            id: "f-1".into(),
            factor_type: kind.into(),
            status: status.into(),
            friendly_name: None,
        }
    }

    fn user(aal: &str, factors: Vec<Factor>) -> AuthUser {
        AuthUser {
            id: Uuid::nil(),
            email: Some("a@b.co".into()),
            aal: aal.into(),
            factors,
            access_token: "t".into(),
        }
    }

    fn factor_id(id: &str, status: &str) -> Factor {
        Factor {
            id: id.into(),
            factor_type: "totp".into(),
            status: status.into(),
            friendly_name: None,
        }
    }

    #[test]
    fn merge_factors_catches_a_verified_factor_present_in_only_one_source() {
        // The fail-open this closes: /auth/v1/factors returned a 200 whose
        // `factors` was empty/renamed, so the enrolled factor was invisible.
        // The user object still carries it, and that must be enough to enforce
        // AAL2.
        let from_user = vec![factor_id("f-a", "verified")];
        let from_factors_endpoint = vec![]; // the endpoint dropped it

        let merged = merge_factors(from_user, from_factors_endpoint);
        assert!(
            merged.iter().any(Factor::is_verified),
            "a verified factor seen by either source must survive the merge"
        );

        // ...and symmetrically when only the factors endpoint has it.
        let merged = merge_factors(vec![], vec![factor_id("f-a", "verified")]);
        assert!(merged.iter().any(Factor::is_verified));
    }

    #[test]
    fn merge_factors_does_not_invent_factors_for_a_no_mfa_user() {
        // Both sources empty (a genuine non-MFA user) -> no factors, no
        // step-up, no lockout.
        assert!(merge_factors(vec![], vec![]).is_empty());
    }

    #[test]
    fn merge_factors_dedupes_by_id_and_prefers_verified() {
        // Same factor id reported unverified by one source and verified by the
        // other must resolve to verified, never masked by the stale copy.
        let merged = merge_factors(
            vec![factor_id("f-a", "unverified")],
            vec![factor_id("f-a", "verified")],
        );
        assert_eq!(merged.len(), 1, "same id must not duplicate");
        assert!(merged[0].is_verified());

        // Order-independent: verified first, unverified second.
        let merged = merge_factors(
            vec![factor_id("f-a", "verified")],
            vec![factor_id("f-a", "unverified")],
        );
        assert_eq!(merged.len(), 1);
        assert!(merged[0].is_verified());
    }

    #[test]
    fn needs_aal2_only_when_enrolled_and_not_yet_upgraded() {
        // Enrolled + AAL1 -> must step up.
        assert!(user("aal1", vec![factor("verified", "totp")]).needs_aal2());
        // Enrolled + already AAL2 -> satisfied.
        assert!(!user("aal2", vec![factor("verified", "totp")]).needs_aal2());
        // Unverified factor doesn't count as enrolled.
        assert!(!user("aal1", vec![factor("unverified", "totp")]).needs_aal2());
        // No factors at all -> nothing to step up to.
        assert!(!user("aal1", vec![]).needs_aal2());
    }

    #[test]
    fn urlencode_escapes_reserved_but_keeps_path_and_unreserved() {
        assert_eq!(urlencode("a@b.co"), "a%40b.co");
        assert_eq!(urlencode("/account/setup"), "/account/setup");
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
    }
}
