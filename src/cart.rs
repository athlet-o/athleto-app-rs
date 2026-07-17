//! Cart pages and htmx fragments, plus the 90-minute stock holds.
//!
//! Carts are keyed by the Supabase user id for logged-in users, otherwise by
//! an anonymous cart cookie uuid. Adding to the cart claims (or refreshes) a
//! hold row per product; the cart page shows a countdown that re-syncs
//! against GET /cart/hold at random intervals. Without DATABASE_URL the
//! routes degrade to a "not configured" notice so the rest of the storefront
//! keeps working.

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum::Form;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use maud::{html, Markup, PreEscaped};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::auth::{self, Biz, MaybeUser};
use crate::db::{self, CartLine, CartOwner, CustomerProfile, HoldOutcome};
use crate::{orders, pages, AppError, SharedState};

pub const CART_COOKIE: &str = "athleto_cart";

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

fn anon_cart_id(jar: &CookieJar) -> Option<Uuid> {
    jar.get(CART_COOKIE)
        .and_then(|cookie| Uuid::parse_str(cookie.value()).ok())
}

fn cart_owner(user: &MaybeUser, jar: &CookieJar) -> Option<CartOwner> {
    match user.as_ref() {
        Some(user) => Some(CartOwner::User(user.id)),
        None => anon_cart_id(jar).map(CartOwner::Anon),
    }
}

fn anon_cart_cookie(id: Uuid) -> Cookie<'static> {
    Cookie::build((CART_COOKIE, id.to_string()))
        .path("/")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .permanent()
        .build()
}

/// The swappable cart contents block (`#cart-contents`), also returned as an
/// htmx fragment after item deletion.
fn cart_contents(lines: &[CartLine]) -> Markup {
    let total: i64 = lines.iter().map(CartLine::line_total_cents).sum();
    html! {
        div id="cart-contents" {
            @if lines.is_empty() {
                div .notice { "Your cart is empty. Time to stock the training bag." }
            } @else {
                table .cart-table {
                    thead {
                        tr {
                            th { "Product" }
                            th { "Format" }
                            th { "Calories" }
                            th { "Qty" }
                            th { "Price" }
                            th { "Total" }
                            th {}
                        }
                    }
                    tbody {
                        @for line in lines {
                            tr {
                                td .cart-product {
                                    span .wordmark { "Athlet" span .o { "O" } }
                                    @if let Some(subname) = line.subname.as_deref() {
                                        div .subname { (subname) }
                                    } @else {
                                        div .subname { (line.name) }
                                    }
                                }
                                td { (line.format.label()) }
                                td .cart-cal { (line.calories) " cal" }
                                td { (line.qty) }
                                td { (pages::format_price(line.price_cents.into())) }
                                td { (pages::format_price(line.line_total_cents())) }
                                td {
                                    button .danger
                                        hx-post=(format!("/cart/items/{}/delete", line.item_id))
                                        hx-target="#cart-contents"
                                        hx-swap="outerHTML" {
                                        "Remove"
                                    }
                                }
                            }
                        }
                    }
                }
                p .cart-total { "Cart total: " strong { (pages::format_price(total)) } }
            }
            div .cart-actions {
                a .button .ghost href="/" { "Keep shopping" }
            }
        }
    }
}

/// The countdown banner itself. With `oob` set the fragment carries
/// `hx-swap-oob` so the htmx ws extension swaps it in place by id when
/// pushed over /ws.
pub fn hold_banner_div(seconds_left: i64, oob: bool) -> Markup {
    html! {
        div #hold-banner .hold-banner .expired[seconds_left <= 0]
            data-seconds=(seconds_left.max(0))
            hx-swap-oob=[oob.then_some("true")] {
            span { "Items reserved for you: " }
            strong #hold-left {
                @if seconds_left > 0 {
                    (seconds_left / 60) "m " (seconds_left % 60) "s"
                } @else {
                    "expired - items may go back on sale"
                }
            }
            span .muted-inline { "(holds last " (db::HOLD_MINUTES) " minutes from your last cart change)" }
        }
    }
}

fn hold_banner(seconds_left: i64) -> Markup {
    html! {
        (hold_banner_div(seconds_left, false))
        script nonce=(crate::security::csp_nonce()) { (PreEscaped(pages::CART_HOLD_JS)) }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct CartPageParams {
    shortage: Option<String>,
    error: Option<String>,
}

fn cart_page_markup(
    config: &crate::Config,
    user: &MaybeUser,
    biz: Biz,
    profile: Option<&CustomerProfile>,
    lines: &[CartLine],
    hold_seconds: Option<i64>,
    params: &CartPageParams,
) -> Markup {
    pages::layout_for(
        "Your cart | AthletO",
        user.as_ref(),
        biz,
        html! {
            section .section {
                h2 { "Your cart" }
                @if let Some(shortage) = params.shortage.as_deref() {
                    div .notice .error {
                        strong { "Not enough stock to place that order. " }
                        (shortage)
                    }
                }
                @if params.error.is_some() {
                    div .notice .error { "Recurring orders need a repeat frequency -- pick one below." }
                }
                @if user.as_ref().is_none() {
                    p .auth-alt {
                        "You are shopping as a guest. "
                        a href="/login" { "Log in" }
                        " to keep your cart across devices and check out."
                    }
                }
                @if let Some(seconds) = hold_seconds {
                    @if !lines.is_empty() {
                        // Signed-in carts get live pushes over /ws (the
                        // banner script keeps polling as the fallback).
                        @if user.as_ref().is_some() {
                            div hx-ext="ws" ws-connect="/ws" { (hold_banner(seconds)) }
                        } @else {
                            (hold_banner(seconds))
                        }
                    }
                }
                (cart_contents(lines))
                @if user.as_ref().is_some() && !lines.is_empty() {
                    (orders::checkout_form(
                        config,
                        profile,
                        user.as_ref().map(|u| u.has_verified_factor()).unwrap_or(false),
                    ))
                }
            }
        },
    )
}

fn cart_not_configured(user: &MaybeUser, biz: Biz) -> Markup {
    pages::layout_for(
        "Your cart | AthletO",
        user.as_ref(),
        biz,
        html! {
            section .section {
                h2 { "Your cart" }
                (pages::not_configured_notice("The cart database"))
            }
        },
    )
}

/// GET /cart
pub async fn cart_page(
    State(state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
    jar: CookieJar,
    Query(params): Query<CartPageParams>,
) -> Result<Response, AppError> {
    let Some(pool) = &state.pool else {
        return Ok(cart_not_configured(&user, biz).into_response());
    };

    let (lines, hold_seconds) = match cart_owner(&user, &jar) {
        Some(owner) => match db::find_cart(pool, &owner).await? {
            Some(cart_id) => {
                let lines = db::cart_lines(pool, cart_id).await?;
                let hold_seconds = if lines.is_empty() {
                    None
                } else {
                    Some(
                        db::cart_hold_until(pool, cart_id)
                            .await?
                            .map(|until| (until - chrono::Utc::now()).num_seconds())
                            .unwrap_or(0),
                    )
                };
                (lines, hold_seconds)
            }
            None => (Vec::new(), None),
        },
        None => (Vec::new(), None),
    };

    let profile = match user.as_ref() {
        Some(auth_user) => auth::load_profile(&state, auth_user.id).await,
        None => None,
    };
    Ok(cart_page_markup(&state.config, &user, biz, profile.as_ref(), &lines, hold_seconds, &params)
        .into_response())
}

/// GET /cart/hold -- lease-status poll for the countdown banner.
pub async fn hold_status(
    State(state): State<SharedState>,
    user: MaybeUser,
    jar: CookieJar,
) -> Response {
    let Some(pool) = &state.pool else {
        return Json(json!({ "active": false, "seconds_left": 0 })).into_response();
    };
    let until = match cart_owner(&user, &jar) {
        Some(owner) => match db::find_cart(pool, &owner).await {
            Ok(Some(cart_id)) => db::cart_hold_until(pool, cart_id).await.ok().flatten(),
            _ => None,
        },
        None => None,
    };
    let seconds_left = until
        .map(|until| (until - chrono::Utc::now()).num_seconds().max(0))
        .unwrap_or(0);
    Json(json!({ "active": seconds_left > 0, "seconds_left": seconds_left })).into_response()
}

#[derive(Debug, Deserialize)]
pub struct AddItem {
    product_id: i64,
    #[serde(default = "default_qty")]
    qty: i32,
}

fn default_qty() -> i32 {
    1
}

/// POST /cart/items -- add an item and claim/refresh its stock hold. Returns
/// an htmx fragment for hx-post requests, or redirects to /cart otherwise.
pub async fn add_item(
    State(state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
    jar: CookieJar,
    headers: HeaderMap,
    Form(input): Form<AddItem>,
) -> Result<Response, AppError> {
    let Some(pool) = &state.pool else {
        if is_htmx(&headers) {
            return Ok(
                html! { span .added { "Cart is not configured on this deployment." } }
                    .into_response(),
            );
        }
        return Ok(cart_not_configured(&user, biz).into_response());
    };

    // Reuse the existing owner (user id or anon cookie), or mint a new
    // anonymous cart cookie on first add.
    let (owner, jar) = match cart_owner(&user, &jar) {
        Some(owner) => (owner, jar),
        None => {
            let anon_id = Uuid::new_v4();
            (CartOwner::Anon(anon_id), jar.add(anon_cart_cookie(anon_id)))
        }
    };

    let cart_id = db::find_or_create_cart(pool, &owner).await?;
    let requested = input.qty.max(1);
    let already_in_cart = db::cart_lines(pool, cart_id)
        .await?
        .iter()
        .find(|line| line.product_id == input.product_id)
        .map(|line| line.qty)
        .unwrap_or(0);

    // Claim the hold for the would-be total before mutating the cart, so a
    // sold-out product never lands in the cart unheld.
    let desired_total = already_in_cart + requested;
    match db::ensure_hold(pool, cart_id, input.product_id, desired_total).await? {
        HoldOutcome::Insufficient { available } => {
            let available_to_add = (available - already_in_cart).max(0);
            let message = if available_to_add == 0 {
                "Sold out (or fully reserved) right now -- check back soon.".to_string()
            } else {
                format!("Only {available_to_add} more available right now.")
            };
            if is_htmx(&headers) {
                return Ok((jar, html! { span .added { (message) } }).into_response());
            }
            return Ok((jar, Redirect::to("/cart")).into_response());
        }
        HoldOutcome::Held | HoldOutcome::Untracked => {}
    }

    db::add_cart_item(pool, cart_id, input.product_id, requested).await?;
    let count = db::cart_count(pool, cart_id).await?;
    // Nudge any open /ws connections to push the refreshed hold countdown.
    let _ = state.cart_events.send(cart_id);

    if is_htmx(&headers) {
        let fragment = html! {
            span .added {
                "Added! Reserved for " (db::HOLD_MINUTES) " min. "
                a href="/cart" { "View cart (" (count) ")" }
            }
        };
        Ok((jar, fragment).into_response())
    } else {
        Ok((jar, Redirect::to("/cart")).into_response())
    }
}

/// POST /cart/items/{id}/delete -- remove a line (and its hold). Returns the
/// refreshed #cart-contents fragment for htmx, or redirects for plain posts.
pub async fn delete_item(
    State(state): State<SharedState>,
    user: MaybeUser,
    jar: CookieJar,
    headers: HeaderMap,
    Path(item_id): Path<i64>,
) -> Result<Response, AppError> {
    let Some(pool) = &state.pool else {
        return Ok(Redirect::to("/cart").into_response());
    };

    let Some(owner) = cart_owner(&user, &jar) else {
        return Ok(Redirect::to("/cart").into_response());
    };
    let Some(cart_id) = db::find_cart(pool, &owner).await? else {
        return Ok(Redirect::to("/cart").into_response());
    };

    let removed_product = db::cart_lines(pool, cart_id)
        .await?
        .iter()
        .find(|line| line.item_id == item_id)
        .map(|line| line.product_id);
    db::delete_cart_item(pool, cart_id, item_id).await?;
    if let Some(product_id) = removed_product {
        db::release_hold(pool, cart_id, product_id).await?;
    }
    let _ = state.cart_events.send(cart_id);

    if is_htmx(&headers) {
        let lines = db::cart_lines(pool, cart_id).await?;
        Ok(cart_contents(&lines).into_response())
    } else {
        Ok(Redirect::to("/cart").into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cart_contents_preserve_subname_calories_and_totals() {
        let rendered = cart_contents(&[CartLine {
            item_id: 7,
            product_id: 3,
            name: "AthletO".to_string(),
            subname: Some("recover".to_string()),
            format: db::ProductFormat::Cup,
            calories: 90,
            price_cents: 499,
            qty: 2,
        }])
        .into_string();

        assert!(rendered.contains("recover"));
        assert!(rendered.contains("90 cal"));
        assert!(rendered.contains("$9.98"));
    }

    #[test]
    fn hold_banner_shows_countdown_and_expiry() {
        let active = hold_banner(125).into_string();
        assert!(active.contains("2m 5s"));
        assert!(!active.contains("hold-banner expired"));

        let expired = hold_banner(0).into_string();
        assert!(expired.contains("hold-banner expired"));
    }
}
