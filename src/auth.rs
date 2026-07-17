//! Supabase GoTrue auth: passwordless magic-link login, MFA session
//! upgrades, session cookies, and the current-user extractor.
//!
//! There are no passwords. POST /login asks GoTrue to email a magic link
//! (`create_user: true`, so first sign-in doubles as signup). The default
//! email template points at GoTrue's own /verify, which 302s back to
//! /auth/callback with tokens in the URL fragment; inline JS forwards them to
//! POST /auth/session, which validates the token against /auth/v1/user before
//! setting HttpOnly cookies. /auth/confirm additionally supports the cleaner
//! `token_hash` server-side pattern for when a custom SMTP provider (and thus
//! a custom template) is configured.
//!
//! Users with a verified MFA factor are bounced to /login/2fa after the link
//! to upgrade the session to AAL2. B2B accounts are required to enroll a
//! factor before they can order (enforced by `require_full`).

use axum::extract::{FromRequestParts, Query, State};
use axum::http::header::HOST;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use base64::Engine;
use maud::{html, Markup, PreEscaped};
use serde::Deserialize;
use uuid::Uuid;

use crate::db::{self, CustomerProfile};
use crate::{pages, SharedState};

pub const ACCESS_COOKIE: &str = "sb_access_token";
pub const REFRESH_COOKIE: &str = "sb_refresh_token";
/// Pending SMS challenge, stored as "{factor_id}:{challenge_id}".
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

#[derive(Debug, Clone)]
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
#[derive(Debug, Clone, Default)]
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
    #[serde(default)]
    factors: Vec<Factor>,
}

/// Best-effort read of the `aal` claim from a JWT payload. The token is not
/// trusted on this alone -- every request still validates it against
/// /auth/v1/user -- so skipping signature verification here is fine.
fn jwt_aal(token: &str) -> String {
    let payload = token.split('.').nth(1).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|claims| claims.get("aal").and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_else(|| "aal1".to_string())
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
                Ok(user) => Uuid::parse_str(&user.id).ok().map(|id| AuthUser {
                    id,
                    email: user.email,
                    aal: jwt_aal(token),
                    factors: user.factors,
                    access_token: token.to_string(),
                }),
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

/// Host-aware flag: true when the request came in on the B2B storefront host
/// (biz.athleto.store or a `biz.` prefix in general).
#[derive(Debug, Clone, Copy, Default)]
pub struct Biz(pub bool);

impl<S: Send + Sync> FromRequestParts<S> for Biz {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let host = parts
            .headers
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        Ok(Self(host.starts_with("biz.")))
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

fn clear_session(jar: CookieJar) -> CookieJar {
    jar.remove(Cookie::build(ACCESS_COOKIE).path("/"))
        .remove(Cookie::build(REFRESH_COOKIE).path("/"))
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
/// header so app./biz./localhost all round-trip to themselves.
fn request_base(headers: &HeaderMap, state: &SharedState) -> String {
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if host.is_empty() {
        return state.config.public_base_url.clone();
    }
    let scheme = if host.starts_with("localhost") || host.starts_with("127.0.0.1") {
        "http"
    } else {
        "https"
    };
    format!("{scheme}://{host}")
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
pub async fn login_page(
    State(state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
) -> Response {
    if user.as_ref().is_some() {
        return Redirect::to("/").into_response();
    }
    if state.config.supabase().is_none() {
        return not_configured_page(&user, "Sign in | AthletO", "Sign in").into_response();
    }
    login_form(biz, None).into_response()
}

fn login_form(biz: Biz, notice: Option<Markup>) -> Markup {
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
                        "No passwords here. Type your email and we'll send you a one-time link -- "
                        "first-time emails get an account automatically."
                    }
                    @if let Some(notice) = notice { (notice) }
                    div #past-logins .past-logins {}
                    form method="post" action="/login" {
                        label {
                            "Email"
                            input #login-email type="email" name="email" required
                                autocomplete="email" placeholder="you@club.example";
                        }
                        button .primary type="submit" { "Email me a sign-in link" }
                    }
                    p .auth-alt {
                        @if biz.0 {
                            "Ordering for a team of one? " a href="https://app.athleto.store/login" { "Personal store" }
                        } @else {
                            "Buying for a retailer or club? " a href="https://biz.athleto.store/login" { "Business portal" }
                        }
                    }
                }
                script { (PreEscaped(pages::LOGIN_CHIPS_JS)) }
            }
        },
    )
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    email: String,
}

/// POST /login -- ask GoTrue to send the magic link.
pub async fn login_submit(
    State(state): State<SharedState>,
    biz: Biz,
    headers: HeaderMap,
    Form(request): Form<LoginRequest>,
) -> Response {
    let Some((base, key)) = state.config.supabase() else {
        return not_configured_page(&MaybeUser(None), "Sign in | AthletO", "Sign in")
            .into_response();
    };

    let email = request.email.trim().to_lowercase();
    let redirect_to = format!("{}/auth/callback", request_base(&headers, &state));
    let response = state
        .http
        .post(format!("{base}/auth/v1/otp"))
        .query(&[("redirect_to", redirect_to.as_str())])
        .header("apikey", key)
        .json(&serde_json::json!({ "email": email, "create_user": true }))
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => pages::layout_for(
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
        )
        .into_response(),
        Ok(response) => {
            let message = gotrue_error_message(response).await;
            login_form(
                biz,
                Some(html! { div .notice .error { "Could not send the link: " (message) } }),
            )
            .into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "magic link request to Supabase failed");
            login_form(
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
                script { (PreEscaped(pages::CALLBACK_JS)) }
            }
        },
    )
}

#[derive(Debug, Deserialize)]
pub struct SessionTokens {
    access_token: String,
    refresh_token: String,
}

/// POST /auth/session -- validate fragment-delivered tokens and set cookies.
pub async fn auth_session(
    State(state): State<SharedState>,
    biz: Biz,
    jar: CookieJar,
    Form(tokens): Form<SessionTokens>,
) -> Response {
    let Some(user) = fetch_user(&state, &tokens.access_token).await else {
        return login_form(
            biz,
            Some(html! { div .notice .error { "That sign-in link was invalid or expired. Request a fresh one." } }),
        )
        .into_response();
    };

    let jar = jar
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
            return login_form(
                biz,
                Some(html! { div .notice .error { "Sign-in link rejected: " (message) } }),
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
    let fragment = format!(
        "email={}&next={}",
        urlencode(email),
        urlencode(next)
    );
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
                script { (PreEscaped(pages::REMEMBER_JS)) }
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
                                input type="hidden" name="factor_id" value=(factor.id);
                                button type="submit" { "Text a code to my phone" }
                            }
                            @if sms_sent {
                                form method="post" action="/login/2fa" {
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

/// POST /login/2fa/send -- create a challenge for an SMS factor (GoTrue sends
/// the text) and stash the challenge id for the verify step.
pub async fn login_2fa_send(
    State(state): State<SharedState>,
    user: MaybeUser,
    jar: CookieJar,
    Form(request): Form<ChallengeRequest>,
) -> Response {
    let Some(user) = user.as_ref() else {
        return Redirect::to("/login").into_response();
    };
    match create_challenge(&state, &user.access_token, &request.factor_id).await {
        Ok(challenge_id) => {
            let cookie = Cookie::build((
                MFA_CHALLENGE_COOKIE,
                format!("{}:{}", request.factor_id, challenge_id),
            ))
            .path("/")
            .http_only(true)
            .secure(true)
            .same_site(SameSite::Lax)
            .build();
            (jar.add(cookie), Redirect::to("/login/2fa?sent=1")).into_response()
        }
        Err(message) => {
            two_fa_form(user, Biz(false), false, Some(html! { div .notice .error { (message) } }))
                .into_response()
        }
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

    // SMS challenges were created by /login/2fa/send; TOTP challenges can be
    // minted right before verification.
    let stored = jar
        .get(MFA_CHALLENGE_COOKIE)
        .and_then(|cookie| cookie.value().split_once(':').map(|(f, c)| (f.to_string(), c.to_string())));
    let challenge_id = match stored {
        Some((factor_id, challenge_id)) if factor_id == request.factor_id => challenge_id,
        _ => match create_challenge(&state, &user.access_token, &request.factor_id).await {
            Ok(challenge_id) => challenge_id,
            Err(message) => {
                return two_fa_form(user, biz, false, Some(html! { div .notice .error { (message) } }))
                    .into_response()
            }
        },
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
pub fn require_b2b_ready(user: &AuthUser, profile: Option<&CustomerProfile>) -> Result<(), Response> {
    if let Some(profile) = profile {
        if profile.is_b2b() && !user.has_verified_factor() {
            return Err(Redirect::to("/account?required2fa=1").into_response());
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
