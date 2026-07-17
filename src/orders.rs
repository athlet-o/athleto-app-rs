//! Web checkout (B2C one-time/recurring, B2B with PO numbers), the order
//! history page, and the B2B quick-order grid.

use std::collections::HashMap;

use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use axum_extra::extract::cookie::CookieJar;
use maud::{html, Markup};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{self, Biz, MaybeUser};
use crate::db::{self, CartOwner, CustomerProfile, OrderFrequency, OrderKind};
use crate::{pages, AppError, SharedState};

fn parse_kind(kind: &str) -> OrderKind {
    match kind {
        "recurring" => OrderKind::Recurring,
        _ => OrderKind::OneTime,
    }
}

fn parse_frequency(frequency: &str) -> Option<OrderFrequency> {
    match frequency {
        "weekly" => Some(OrderFrequency::Weekly),
        "biweekly" => Some(OrderFrequency::Biweekly),
        "monthly" => Some(OrderFrequency::Monthly),
        "quarterly" => Some(OrderFrequency::Quarterly),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
pub struct CheckoutRequest {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    frequency: String,
    #[serde(default)]
    po_number: String,
}

/// POST /checkout -- turn the cart into an order (stock + holds resolved in
/// one transaction by `place_order`).
pub async fn checkout(
    State(state): State<SharedState>,
    user: MaybeUser,
    jar: CookieJar,
    Form(request): Form<CheckoutRequest>,
) -> Result<Response, AppError> {
    let (auth_user, profile) = match auth::require_full(&state, &user).await {
        Ok(pair) => pair,
        Err(redirect) => return Ok(redirect),
    };
    if let Err(redirect) = auth::require_b2b_ready(&auth_user, profile.as_ref()) {
        return Ok(redirect);
    }
    let Some(pool) = &state.pool else {
        return Ok(Redirect::to("/cart").into_response());
    };

    let owner = CartOwner::User(auth_user.id);
    let Some(cart_id) = db::find_cart(pool, &owner).await? else {
        return Ok(Redirect::to("/cart").into_response());
    };
    let lines = db::cart_lines(pool, cart_id).await?;
    if lines.is_empty() {
        return Ok(Redirect::to("/cart").into_response());
    }

    let is_b2b = profile.as_ref().map(CustomerProfile::is_b2b).unwrap_or(false);
    let kind = parse_kind(&request.kind);
    let frequency = parse_frequency(&request.frequency);
    if kind == OrderKind::Recurring && frequency.is_none() {
        return Ok(Redirect::to("/cart?error=pick-a-frequency").into_response());
    }
    let po = request.po_number.trim();
    let po_number = (is_b2b && !po.is_empty()).then_some(po);
    let channel = if is_b2b {
        db::OrderChannel::B2bPortal
    } else {
        db::OrderChannel::D2cWeb
    };

    let order_lines: Vec<db::NewOrderLine> = lines
        .iter()
        .map(|line| db::NewOrderLine {
            product_id: line.product_id,
            qty: line.qty,
            unit_price_cents: line.price_cents,
        })
        .collect();

    match db::place_order(
        pool,
        auth_user.id,
        kind,
        frequency,
        channel,
        po_number,
        &order_lines,
        Some(cart_id),
    )
    .await
    {
        Ok(_) => {
            // Holds were consumed with the order; refresh any /ws listeners.
            let _ = state.cart_events.send(cart_id);
            Ok((jar, Redirect::to("/orders?placed=1")).into_response())
        }
        Err(db::OrderError::Insufficient(shortages)) => {
            let names: HashMap<i64, String> = lines
                .iter()
                .map(|line| {
                    (
                        line.product_id,
                        line.subname
                            .clone()
                            .map(|s| format!("AthletO {s} ({})", line.format.label()))
                            .unwrap_or_else(|| line.name.clone()),
                    )
                })
                .collect();
            let detail = shortages
                .iter()
                .map(|s| {
                    format!(
                        "{}: {} requested, {} available",
                        names.get(&s.product_id).cloned().unwrap_or_default(),
                        s.requested,
                        s.available
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            Ok(Redirect::to(&format!("/cart?shortage={}", pages::urlencode_component(&detail)))
                .into_response())
        }
        Err(db::OrderError::Db(err)) => Err(AppError::Db(err)),
    }
}

/// GET /orders
pub async fn orders_page(
    State(state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
) -> Result<Response, AppError> {
    let (auth_user, profile) = match auth::require_full(&state, &user).await {
        Ok(pair) => pair,
        Err(redirect) => return Ok(redirect),
    };
    let Some(pool) = &state.pool else {
        return Ok(pages::layout_for(
            "Orders | AthletO",
            Some(&auth_user),
            biz,
            html! { section .section { h2 { "Your orders" } (pages::not_configured_notice("The orders database")) } },
        )
        .into_response());
    };

    let orders = db::list_orders(pool, auth_user.id).await?;
    let items = db::order_items_for_user(pool, auth_user.id).await?;
    let mut by_order: HashMap<Uuid, Vec<&db::OrderItemRow>> = HashMap::new();
    for item in &items {
        by_order.entry(item.order_id).or_default().push(item);
    }

    let is_b2b = profile.as_ref().map(CustomerProfile::is_b2b).unwrap_or(false);
    Ok(pages::layout_for(
        "Orders | AthletO",
        Some(&auth_user),
        biz,
        html! {
            section .section {
                h2 { "Your orders" }
                @if orders.is_empty() {
                    div .notice { "No orders yet. The lineup is waiting." }
                    p { a .button href="/" { "Shop the lineup" } }
                } @else {
                    @for order in &orders {
                        div .order-card {
                            div .order-head {
                                strong { "Order " (order.id.simple().to_string()[..8].to_uppercase()) }
                                span .format-badge { (order.status.label()) }
                                span .muted-inline { (order.created_at.format("%b %-d, %Y")) }
                            }
                            p .auth-alt {
                                (order.kind.label())
                                @if let Some(freq) = order.frequency { ", " (freq.label()) }
                                @if let Some(next) = order.next_run_at {
                                    " -- next run " (next.format("%b %-d"))
                                }
                                @if let Some(po) = order.po_number.as_deref() { " -- PO " code { (po) } }
                            }
                            ul .factor-list {
                                @for item in by_order.get(&order.id).map(|v| v.as_slice()).unwrap_or(&[]) {
                                    li {
                                        (item.qty) " x "
                                        @if let Some(subname) = item.subname.as_deref() {
                                            "AthletO " (subname)
                                        } @else { (item.name) }
                                        " (" (item.format.label()) ") -- "
                                        (pages::format_price(i64::from(item.unit_price_cents) * i64::from(item.qty)))
                                    }
                                }
                            }
                            p .cart-total { "Total: " strong { (pages::format_price(order.total_cents)) } }
                        }
                    }
                }
                @if is_b2b {
                    p { a .button .ghost href="/quick-order" { "Quick order by the case" } }
                }
            }
        },
    )
    .into_response())
}

/// GET /quick-order -- B2B grid: every SKU with a qty box, one submit.
pub async fn quick_order_page(
    State(state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
) -> Result<Response, AppError> {
    let (auth_user, profile) = match auth::require_full(&state, &user).await {
        Ok(pair) => pair,
        Err(redirect) => return Ok(redirect),
    };
    if let Err(redirect) = auth::require_b2b_ready(&auth_user, profile.as_ref()) {
        return Ok(redirect);
    }
    if !profile.as_ref().map(CustomerProfile::is_b2b).unwrap_or(false) {
        return Ok(Redirect::to("/").into_response());
    }

    let products = match &state.pool {
        Some(pool) => db::list_products(pool).await.unwrap_or_else(|_| db::fallback_products()),
        None => db::fallback_products(),
    };

    Ok(pages::layout_for(
        "Quick order | AthletO Business",
        Some(&auth_user),
        biz,
        html! {
            section .section {
                h2 { "Quick order" }
                p .auth-alt {
                    "Case quantities land in your cart in one go: cups pack 12 per case, "
                    "powders 24. Prefer machines? Use the " a href="/account#api-keys" { "ERP API" } "."
                }
                form method="post" action="/quick-order" {
                    (pages::csrf_field())
                    table .cart-table {
                        thead {
                            tr { th { "Product" } th { "Format" } th { "Unit price" } th { "Quantity (units)" } }
                        }
                        tbody {
                            @for product in &products {
                                tr {
                                    td .cart-product {
                                        span .wordmark { "Athlet" span .o { "O" } }
                                        @if let Some(subname) = product.subname.as_deref() {
                                            div .subname { (subname) }
                                        }
                                    }
                                    td { (product.format.label()) }
                                    td { (pages::format_price(product.price_cents.into())) }
                                    td {
                                        input type="number" name=(format!("qty_{}", product.id))
                                            min="0" step="1" value="0" .qty-input;
                                    }
                                }
                            }
                        }
                    }
                    button .primary type="submit" { "Add all to cart" }
                }
            }
        },
    )
    .into_response())
}

/// POST /quick-order -- add every non-zero quantity to the cart with holds.
pub async fn quick_order_submit(
    State(state): State<SharedState>,
    user: MaybeUser,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Response, AppError> {
    let (auth_user, profile) = match auth::require_full(&state, &user).await {
        Ok(pair) => pair,
        Err(redirect) => return Ok(redirect),
    };
    if let Err(redirect) = auth::require_b2b_ready(&auth_user, profile.as_ref()) {
        return Ok(redirect);
    }
    // Quick order is the B2B bulk entry point; `require_b2b_ready` is a no-op
    // for B2C, so gate the endpoint on the profile explicitly rather than let a
    // personal account drive it.
    if !profile
        .as_ref()
        .map(CustomerProfile::is_b2b)
        .unwrap_or(false)
    {
        return Ok(Redirect::to("/").into_response());
    }
    let Some(pool) = &state.pool else {
        return Ok(Redirect::to("/cart").into_response());
    };

    // Validate product ids against the catalog up front: an unknown id would
    // otherwise hit the cart_items FK and surface as a 500.
    let valid_products: std::collections::HashSet<i64> = db::product_prices(pool)
        .await?
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();

    let owner = CartOwner::User(auth_user.id);
    let cart_id = db::find_or_create_cart(pool, &owner).await?;
    for (key, value) in &form {
        let Some(product_id) = key.strip_prefix("qty_").and_then(|id| id.parse::<i64>().ok())
        else {
            continue;
        };
        if !valid_products.contains(&product_id) {
            continue;
        }
        let qty: i32 = value.trim().parse().unwrap_or(0);
        if qty <= 0 {
            continue;
        }
        let qty = crate::cart::clamp_line_qty(qty);
        db::add_cart_item(pool, cart_id, product_id, qty).await?;
        let total_qty = db::cart_lines(pool, cart_id)
            .await?
            .iter()
            .find(|line| line.product_id == product_id)
            .map(|line| line.qty)
            .unwrap_or(qty);
        if let Err(err) = db::ensure_hold(pool, cart_id, product_id, total_qty).await {
            tracing::warn!(error = %err, "hold claim failed during quick order");
        }
    }
    let _ = state.cart_events.send(cart_id);
    Ok(Redirect::to("/cart").into_response())
}

/// Shared checkout form fragment rendered on the cart page.
pub fn checkout_form(profile: Option<&CustomerProfile>, has_2fa: bool) -> Markup {
    let is_b2b = profile.map(CustomerProfile::is_b2b).unwrap_or(false);
    if is_b2b && !has_2fa {
        return html! {
            div .notice .error {
                strong { "Two-factor authentication required. " }
                "Business accounts must " a href="/account?required2fa=1" { "set up 2FA" }
                " before placing orders."
            }
        };
    }
    html! {
        form .checkout-form method="post" action="/checkout" {
            (pages::csrf_field())
            h3 { "Place this order" }
            label {
                "Order type"
                select name="kind" {
                    option value="one_time" selected { "One-time order" }
                    option value="recurring" { "Recurring order" }
                }
            }
            label {
                "Repeat"
                select name="frequency" {
                    option value="" { "-- only for recurring --" }
                    option value="weekly" { "Weekly" }
                    option value="biweekly" { "Every 2 weeks" }
                    option value="monthly" { "Monthly" }
                    option value="quarterly" { "Quarterly" }
                }
            }
            @if is_b2b {
                label {
                    "PO number " span .muted-inline { "(optional)" }
                    input type="text" name="po_number" maxlength="60" placeholder="PO-2026-0417";
                }
            }
            button .primary type="submit" { "Place order" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kind_and_frequency_map_form_values() {
        assert_eq!(parse_kind("recurring"), OrderKind::Recurring);
        assert_eq!(parse_kind("one_time"), OrderKind::OneTime);
        assert_eq!(parse_kind("garbage"), OrderKind::OneTime);

        assert_eq!(parse_frequency("weekly"), Some(OrderFrequency::Weekly));
        assert_eq!(parse_frequency("quarterly"), Some(OrderFrequency::Quarterly));
        assert_eq!(parse_frequency(""), None);
    }

    #[test]
    fn b2b_checkout_form_blocks_until_2fa_then_shows_po_field() {
        let profile = CustomerProfile {
            customer_type: db::CustomerType::B2b,
            company_name: Some("Wobble Co".into()),
        };
        // Business account without a verified factor: hard stop, no form.
        let blocked = checkout_form(Some(&profile), false).into_string();
        assert!(blocked.contains("Two-factor authentication required"));
        assert!(!blocked.contains("Place order"));
        // With 2FA satisfied: the order form renders, including the PO field.
        let allowed = checkout_form(Some(&profile), true).into_string();
        assert!(allowed.contains("Place order"));
        assert!(allowed.contains("PO number"));
    }

    #[test]
    fn b2c_checkout_form_has_no_po_field() {
        let profile = CustomerProfile {
            customer_type: db::CustomerType::B2c,
            company_name: None,
        };
        let rendered = checkout_form(Some(&profile), false).into_string();
        assert!(rendered.contains("Place order"));
        assert!(!rendered.contains("PO number"));
    }
}
