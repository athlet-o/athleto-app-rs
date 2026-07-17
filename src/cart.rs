//! Cart pages and htmx fragments.
//!
//! Carts are keyed by the Supabase user id for logged-in users, otherwise by
//! an anonymous cart cookie uuid. Without DATABASE_URL the routes degrade to
//! a "not configured" notice so the rest of the storefront keeps working.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use maud::{html, Markup};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::MaybeUser;
use crate::db::{self, CartLine, CartOwner};
use crate::{pages, AppError, SharedState};

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

fn cart_page_markup(user: &MaybeUser, lines: &[CartLine]) -> Markup {
    pages::layout(
        "Your cart | AthletO",
        user.as_ref(),
        html! {
            section .section {
                h2 { "Your cart" }
                @if user.as_ref().is_none() {
                    p .auth-alt {
                        "You are shopping as a guest. "
                        a href="/login" { "Log in" }
                        " to keep your cart across devices."
                    }
                }
                (cart_contents(lines))
            }
        },
    )
}

fn cart_not_configured(user: &MaybeUser) -> Markup {
    pages::layout(
        "Your cart | AthletO",
        user.as_ref(),
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
    jar: CookieJar,
) -> Result<Response, AppError> {
    let Some(pool) = &state.pool else {
        return Ok(cart_not_configured(&user).into_response());
    };

    let lines = match cart_owner(&user, &jar) {
        Some(owner) => match db::find_cart(pool, &owner).await? {
            Some(cart_id) => db::cart_lines(pool, cart_id).await?,
            None => Vec::new(),
        },
        None => Vec::new(),
    };
    Ok(cart_page_markup(&user, &lines).into_response())
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

/// POST /cart/items -- add an item. Returns an htmx fragment for hx-post
/// requests, or redirects to /cart for plain form posts.
pub async fn add_item(
    State(state): State<SharedState>,
    user: MaybeUser,
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
        return Ok(cart_not_configured(&user).into_response());
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
    db::add_cart_item(pool, cart_id, input.product_id, input.qty.max(1)).await?;
    let count = db::cart_count(pool, cart_id).await?;

    if is_htmx(&headers) {
        let fragment = html! {
            span .added {
                "Added! "
                a href="/cart" { "View cart (" (count) ")" }
            }
        };
        Ok((jar, fragment).into_response())
    } else {
        Ok((jar, Redirect::to("/cart")).into_response())
    }
}

/// POST /cart/items/{id}/delete -- remove a line. Returns the refreshed
/// #cart-contents fragment for htmx, or redirects for plain form posts.
pub async fn delete_item(
    State(state): State<SharedState>,
    user: MaybeUser,
    jar: CookieJar,
    headers: HeaderMap,
    Path(item_id): Path<i64>,
) -> Result<Response, AppError> {
    let Some(pool) = &state.pool else {
        return Ok(cart_not_configured(&user).into_response());
    };

    let Some(owner) = cart_owner(&user, &jar) else {
        return Ok(Redirect::to("/cart").into_response());
    };
    let Some(cart_id) = db::find_cart(pool, &owner).await? else {
        return Ok(Redirect::to("/cart").into_response());
    };

    db::delete_cart_item(pool, cart_id, item_id).await?;

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
}
