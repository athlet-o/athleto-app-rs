//! Supabase GoTrue auth: signup, login, logout, and a current-user extractor.
//!
//! Sessions are stored as HttpOnly Secure SameSite=Lax cookies holding the
//! Supabase access and refresh tokens. The current user is resolved per
//! request by calling `{SUPABASE_URL}/auth/v1/user`. When SUPABASE_URL /
//! SUPABASE_ANON_KEY are unset every route degrades to a "not configured"
//! notice instead of failing.

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use maud::{html, Markup};
use serde::Deserialize;
use uuid::Uuid;

use crate::{pages, SharedState};

pub const ACCESS_COOKIE: &str = "sb_access_token";
pub const REFRESH_COOKIE: &str = "sb_refresh_token";

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: Uuid,
    pub email: Option<String>,
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
}

impl FromRequestParts<SharedState> for MaybeUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let jar = match CookieJar::from_request_parts(parts, state).await {
            Ok(jar) => jar,
        };
        let Some((base, key)) = state.config.supabase() else {
            return Ok(Self(None));
        };
        let Some(token) = jar.get(ACCESS_COOKIE).map(|cookie| cookie.value().to_string()) else {
            return Ok(Self(None));
        };

        let response = state
            .http
            .get(format!("{base}/auth/v1/user"))
            .header("apikey", key)
            .bearer_auth(&token)
            .send()
            .await;

        let user = match response {
            Ok(response) if response.status().is_success() => {
                match response.json::<GotrueUser>().await {
                    Ok(user) => Uuid::parse_str(&user.id).ok().map(|id| AuthUser {
                        id,
                        email: user.email,
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
        };
        Ok(Self(user))
    }
}

#[derive(Debug, Deserialize)]
pub struct Credentials {
    email: String,
    password: String,
}

#[derive(Debug, Default, Deserialize)]
struct Session {
    access_token: Option<String>,
    refresh_token: Option<String>,
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

fn auth_form_page(
    user: &MaybeUser,
    title: &str,
    heading: &str,
    action: &str,
    submit_label: &str,
    alt: Markup,
    notice: Option<Markup>,
) -> Markup {
    pages::layout(
        title,
        user.as_ref(),
        html! {
            section .section {
                h2 { (heading) }
                @if let Some(notice) = notice { (notice) }
                form .auth-card method="post" action=(action) {
                    label {
                        "Email"
                        input type="email" name="email" required placeholder="you@club.example";
                    }
                    label {
                        "Password"
                        input type="password" name="password" required minlength="6";
                    }
                    button .primary type="submit" { (submit_label) }
                    p .auth-alt { (alt) }
                }
            }
        },
    )
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

/// GET /signup
pub async fn signup_page(State(state): State<SharedState>, user: MaybeUser) -> Markup {
    if state.config.supabase().is_none() {
        return not_configured_page(&user, "Sign up | Athlet-O", "Sign up");
    }
    signup_form(&user, None)
}

fn signup_form(user: &MaybeUser, notice: Option<Markup>) -> Markup {
    auth_form_page(
        user,
        "Sign up | Athlet-O",
        "Join the squad",
        "/signup",
        "Sign up",
        html! { "Already wobbling? " a href="/login" { "Log in" } },
        notice,
    )
}

/// POST /signup -- proxies to `{SUPABASE_URL}/auth/v1/signup`.
pub async fn signup_submit(
    State(state): State<SharedState>,
    user: MaybeUser,
    jar: CookieJar,
    Form(credentials): Form<Credentials>,
) -> Response {
    let Some((base, key)) = state.config.supabase() else {
        return not_configured_page(&user, "Sign up | Athlet-O", "Sign up").into_response();
    };

    let response = state
        .http
        .post(format!("{base}/auth/v1/signup"))
        .header("apikey", key)
        .json(&serde_json::json!({
            "email": credentials.email,
            "password": credentials.password,
        }))
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => {
            let session: Session = response.json().await.unwrap_or_default();
            match (session.access_token, session.refresh_token) {
                (Some(access), Some(refresh)) => {
                    let jar = jar
                        .add(session_cookie(ACCESS_COOKIE, access))
                        .add(session_cookie(REFRESH_COOKIE, refresh));
                    (jar, Redirect::to("/")).into_response()
                }
                // Email confirmation is enabled on the Supabase project: no
                // session is returned until the address is confirmed.
                _ => signup_form(
                    &user,
                    Some(html! {
                        div .notice .success {
                            "Almost there -- check your inbox to confirm your email, then log in."
                        }
                    }),
                )
                .into_response(),
            }
        }
        Ok(response) => {
            let message = gotrue_error_message(response).await;
            signup_form(
                &user,
                Some(html! { div .notice .error { "Sign up failed: " (message) } }),
            )
            .into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "signup request to Supabase failed");
            signup_form(
                &user,
                Some(html! { div .notice .error { "Could not reach the auth service. Try again shortly." } }),
            )
            .into_response()
        }
    }
}

/// GET /login
pub async fn login_page(State(state): State<SharedState>, user: MaybeUser) -> Markup {
    if state.config.supabase().is_none() {
        return not_configured_page(&user, "Log in | Athlet-O", "Log in");
    }
    login_form(&user, None)
}

fn login_form(user: &MaybeUser, notice: Option<Markup>) -> Markup {
    auth_form_page(
        user,
        "Log in | Athlet-O",
        "Back for more wobble",
        "/login",
        "Log in",
        html! { "New here? " a href="/signup" { "Sign up" } },
        notice,
    )
}

/// POST /login -- proxies to `{SUPABASE_URL}/auth/v1/token?grant_type=password`.
pub async fn login_submit(
    State(state): State<SharedState>,
    user: MaybeUser,
    jar: CookieJar,
    Form(credentials): Form<Credentials>,
) -> Response {
    let Some((base, key)) = state.config.supabase() else {
        return not_configured_page(&user, "Log in | Athlet-O", "Log in").into_response();
    };

    let response = state
        .http
        .post(format!("{base}/auth/v1/token?grant_type=password"))
        .header("apikey", key)
        .json(&serde_json::json!({
            "email": credentials.email,
            "password": credentials.password,
        }))
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => {
            let session: Session = response.json().await.unwrap_or_default();
            match (session.access_token, session.refresh_token) {
                (Some(access), Some(refresh)) => {
                    let jar = jar
                        .add(session_cookie(ACCESS_COOKIE, access))
                        .add(session_cookie(REFRESH_COOKIE, refresh));
                    (jar, Redirect::to("/")).into_response()
                }
                _ => login_form(
                    &user,
                    Some(html! { div .notice .error { "The auth service returned no session. Try again." } }),
                )
                .into_response(),
            }
        }
        Ok(response) => {
            let message = gotrue_error_message(response).await;
            login_form(
                &user,
                Some(html! { div .notice .error { "Log in failed: " (message) } }),
            )
            .into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "login request to Supabase failed");
            login_form(
                &user,
                Some(html! { div .notice .error { "Could not reach the auth service. Try again shortly." } }),
            )
            .into_response()
        }
    }
}

/// POST /logout -- best-effort GoTrue logout, then clear session cookies.
pub async fn logout(State(state): State<SharedState>, jar: CookieJar) -> Response {
    if let (Some((base, key)), Some(token)) = (
        state.config.supabase(),
        jar.get(ACCESS_COOKIE).map(|cookie| cookie.value().to_string()),
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
