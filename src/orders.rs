//! Web checkout (B2C one-time/recurring, B2B with PO numbers), the order
//! history page, and the B2B quick-order grid.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use axum_extra::extract::cookie::CookieJar;
use maud::{html, Markup};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{self, Biz, MaybeUser};
use crate::db::{self, CartOwner, CustomerProfile, OrderFrequency, OrderKind};
use crate::{pages, payments, AppError, SharedState};

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
    #[serde(default)]
    ship_method: String,
    #[serde(default)]
    pay_method: String,
}

/// The payment methods this deployment can actually offer, per cohort. B2B
/// additionally gets ACH inside the Stripe option and Net-30 invoicing
/// (which rides Stripe hosted invoices, hence the stripe-config gate).
fn payment_method_options(
    config: &crate::Config,
    is_b2b: bool,
) -> Vec<(&'static str, &'static str)> {
    let mut options = Vec::new();
    if config.stripe.is_some() {
        options.push((
            "stripe",
            if is_b2b { "Card or ACH bank debit (Stripe)" } else { "Card (Stripe)" },
        ));
    }
    if config.paypal.is_some() {
        options.push(("paypal", "PayPal"));
    }
    if config.square.is_some() {
        options.push(("square", "Square"));
    }
    if is_b2b && config.stripe.is_some() {
        options.push(("invoice", "Invoice my account \u{2014} Net 30 (PO)"));
    }
    options
}

/// Kick off the chosen payment for a just-placed (or retried) order and say
/// where to send the browser.
async fn dispatch_payment(
    state: &SharedState,
    headers: &axum::http::HeaderMap,
    auth_user: &crate::auth::AuthUser,
    order_id: Uuid,
    method: payments::PayMethod,
    is_b2b: bool,
    po_number: Option<&str>,
) -> Redirect {
    let base = auth::request_base(headers, state);
    match payments::start_payment(
        state,
        &base,
        auth_user.id,
        auth_user.email.as_deref(),
        order_id,
        method,
        is_b2b,
        po_number,
    )
    .await
    {
        Ok(payments::StartOutcome::Redirect(url)) => Redirect::to(&url),
        Ok(payments::StartOutcome::Invoiced) => Redirect::to("/orders?invoiced=1"),
        Ok(payments::StartOutcome::NotConfigured) => Redirect::to("/orders?placed=1"),
        Err(err) => {
            tracing::error!(error = %err, %order_id, "payment start failed");
            // The order is placed; /orders offers a Pay-now retry.
            Redirect::to("/orders?payerror=1")
        }
    }
}

/// POST /checkout -- turn the cart into an order (stock + holds resolved in
/// one transaction by `place_order`).
pub async fn checkout(
    State(state): State<SharedState>,
    user: MaybeUser,
    jar: CookieJar,
    headers: axum::http::HeaderMap,
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

    let is_b2b = profile
        .as_ref()
        .map(CustomerProfile::is_b2b_approved)
        .unwrap_or(false);
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
    // B2B ships freight (billed on account); B2C picks standard/expedited.
    let ship_method = if is_b2b {
        db::ShipMethod::Freight
    } else {
        db::ShipMethod::parse(&request.ship_method).unwrap_or(db::ShipMethod::Standard)
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
        ship_method,
        po_number,
        &order_lines,
        Some(cart_id),
    )
    .await
    {
        Ok(order_id) => {
            let redirect = match payments::PayMethod::parse(request.pay_method.trim()) {
                Some(method) => {
                    dispatch_payment(&state, &headers, &auth_user, order_id, method, is_b2b, po_number)
                        .await
                }
                // No (or unknown) method chosen — order stays payment-pending
                // and /orders offers Pay now.
                None => Redirect::to("/orders?placed=1"),
            };
            Ok((jar, redirect).into_response())
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

/// CSS class for an order status badge.
fn status_class(status: db::OrderStatus) -> &'static str {
    match status {
        db::OrderStatus::Placed => "st-placed",
        db::OrderStatus::Processing => "st-processing",
        db::OrderStatus::Fulfilled => "st-fulfilled",
        db::OrderStatus::Cancelled => "st-cancelled",
    }
}

/// Delivery-estimate phrase for an order given its (optional) shipment.
fn delivery_estimate(order: &db::OrderRow, shipment: Option<&db::Shipment>) -> Markup {
    // A recorded shipment carries the authoritative window; otherwise estimate
    // from ship method + order date.
    if let Some(s) = shipment {
        if let Some(delivered) = s.delivered_at {
            return html! { span .ok-inline { "Delivered " (delivered.format("%b %-d")) } };
        }
        if let (Some(a), Some(b)) = (s.eta_earliest, s.eta_latest) {
            return html! { "Arrives " strong { (a.format("%b %-d")) "\u{2013}" (b.format("%b %-d")) } };
        }
    }
    if order.status == db::OrderStatus::Cancelled {
        return html! { span .muted-inline { "\u{2014}" } };
    }
    let (a, b) = order.delivery_window();
    html! { "Est. delivery " strong { (a.format("%b %-d")) "\u{2013}" (b.format("%b %-d")) } }
}

/// Tracking snippet: carrier + number, linked to the carrier when known.
fn tracking_snippet(shipment: &db::Shipment) -> Markup {
    let carrier = shipment.carrier.as_deref().unwrap_or("Carrier");
    match (&shipment.tracking_number, shipment.tracking_url()) {
        (Some(number), Some(url)) => html! {
            "Tracking: " (carrier) " " a .track-link href=(url) target="_blank" rel="noopener" { (number) }
        },
        (Some(number), None) => html! { "Tracking: " (carrier) " " code { (number) } },
        _ => html! {},
    }
}

/// GET /orders -- order history with status, delivery estimate, tracking,
/// receipt link and reorder; B2B additionally gets PO/status filters.
pub async fn orders_page(
    State(state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
    axum::extract::Query(filter): axum::extract::Query<OrderFilter>,
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

    let mut orders = db::list_orders(pool, auth_user.id).await?;
    let items = db::order_items_for_user(pool, auth_user.id).await?;
    let shipments = db::shipments_for_user(pool, auth_user.id).await?;
    let mut by_order: HashMap<Uuid, Vec<&db::OrderItemRow>> = HashMap::new();
    for item in &items {
        by_order.entry(item.order_id).or_default().push(item);
    }
    let mut ship_by_order: HashMap<Uuid, db::Shipment> = HashMap::new();
    for row in &shipments {
        ship_by_order.insert(row.order_id, row.shipment());
    }

    let is_b2b = profile
        .as_ref()
        .map(CustomerProfile::is_b2b_approved)
        .unwrap_or(false);

    // B2B order-management filters.
    let status_filter = filter.status.as_deref().and_then(db::OrderStatus::parse);
    let po_query = filter.po.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if is_b2b {
        if let Some(status) = status_filter {
            orders.retain(|o| o.status == status);
        }
        if let Some(po) = po_query {
            let needle = po.to_lowercase();
            orders.retain(|o| {
                o.po_number
                    .as_deref()
                    .map(|p| p.to_lowercase().contains(&needle))
                    .unwrap_or(false)
            });
        }
    }

    Ok(pages::layout_for(
        "Orders | AthletO",
        Some(&auth_user),
        biz,
        html! {
            section .section {
                div .orders-head {
                    h2 { @if is_b2b { "Order management" } @else { "Your orders" } }
                    @if is_b2b {
                        a .button .ghost href="/quick-order" { "Quick order by the case" }
                    }
                }

                @if is_b2b {
                    form .order-filters method="get" action="/orders" {
                        label {
                            "Status"
                            select name="status" {
                                option value="" selected[status_filter.is_none()] { "All statuses" }
                                @for st in [db::OrderStatus::Placed, db::OrderStatus::Processing, db::OrderStatus::Fulfilled, db::OrderStatus::Cancelled] {
                                    option value=(st.label()) selected[status_filter == Some(st)] { (st.label()) }
                                }
                            }
                        }
                        label {
                            "PO number"
                            input type="search" name="po" value=(po_query.unwrap_or("")) placeholder="PO-2026-...";
                        }
                        button .button type="submit" { "Filter" }
                        @if status_filter.is_some() || po_query.is_some() {
                            a .button .ghost href="/orders" { "Clear" }
                        }
                    }
                }

                @if orders.is_empty() {
                    div .notice { "No orders match. The lineup is waiting." }
                    p { a .button href="/" { "Shop the lineup" } }
                } @else {
                    @for order in &orders {
                        div .order-card {
                            div .order-head {
                                a .order-id href=(format!("/orders/{}", order.id)) { "Order " (order.short_id()) }
                                span .status-badge .(status_class(order.status)) { (order.status.label()) }
                                span .status-badge .(payment_class(order.payment_status)) { (order.payment_status.label()) }
                                @if order.kind == db::OrderKind::Recurring { span .status-badge .st-sub { "subscription" } }
                                span .muted-inline { (order.created_at.format("%b %-d, %Y")) }
                            }
                            p .order-meta {
                                (delivery_estimate(order, ship_by_order.get(&order.id)))
                                @if let Some(ship) = ship_by_order.get(&order.id) {
                                    @if ship.tracking_number.is_some() { " \u{00b7} " (tracking_snippet(ship)) }
                                }
                            }
                            p .auth-alt {
                                (order.kind.label())
                                @if let Some(freq) = order.frequency { ", " (freq.label()) }
                                @if let Some(next) = order.next_run_at { " \u{00b7} next run " (next.format("%b %-d")) }
                                @if let Some(po) = order.po_number.as_deref() { " \u{00b7} PO " code { (po) } }
                            }
                            ul .factor-list {
                                @for item in by_order.get(&order.id).map(|v| v.as_slice()).unwrap_or(&[]) {
                                    li {
                                        (item.qty) " \u{00d7} "
                                        @if let Some(subname) = item.subname.as_deref() { "AthletO " (subname) }
                                        @else { (item.name) }
                                        " (" (item.format.label()) ") \u{2014} "
                                        (pages::format_price(i64::from(item.unit_price_cents) * i64::from(item.qty)))
                                    }
                                }
                            }
                            div .order-foot {
                                p .cart-total { "Total: " strong { (pages::format_price(order.total_cents)) } }
                                div .order-actions {
                                    a .button .ghost href=(format!("/orders/{}", order.id)) { "View receipt" }
                                    form .inline-form method="post" action=(format!("/orders/{}/reorder", order.id)) {
                                        button .button type="submit" { "Reorder" }
                                    }
                                    @if payment_retryable(order) {
                                        @let retry_options = payment_method_options(&state.config, is_b2b);
                                        @if !retry_options.is_empty() {
                                            form .inline-form method="post" action=(format!("/orders/{}/pay", order.id)) {
                                                select name="pay_method" {
                                                    @for (value, label) in &retry_options {
                                                        option value=(value) { (label) }
                                                    }
                                                }
                                                button .button .primary type="submit" { "Pay now" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
    .into_response())
}

#[derive(Debug, Default, Deserialize)]
pub struct OrderFilter {
    status: Option<String>,
    po: Option<String>,
}

/// GET /orders/{id} -- order detail + printable receipt (both cohorts).
pub async fn order_detail_page(
    State(state): State<SharedState>,
    user: MaybeUser,
    biz: Biz,
    Path(order_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let (auth_user, _profile) = match auth::require_full(&state, &user).await {
        Ok(pair) => pair,
        Err(redirect) => return Ok(redirect),
    };
    let Some(pool) = &state.pool else {
        return Ok(Redirect::to("/orders").into_response());
    };

    let Some(order) = db::get_order(pool, auth_user.id, order_id).await? else {
        return Ok(Redirect::to("/orders").into_response());
    };
    let items = db::order_items(pool, order_id).await?;
    let shipments = db::shipments_for_order(pool, order_id).await?;
    let primary_shipment = shipments.first();

    Ok(pages::layout_for(
        &format!("Receipt {} | AthletO", order.short_id()),
        Some(&auth_user),
        biz,
        html! {
            section .section {
                div .receipt {
                    div .receipt-top {
                        div {
                            span .wordmark { "Athlet" span .o { "O" } }
                            p .muted-inline { "Order receipt" }
                        }
                        div .receipt-id {
                            strong { "Order " (order.short_id()) }
                            div .muted-inline { (order.created_at.format("%B %-d, %Y")) }
                            span .status-badge .(status_class(order.status)) { (order.status.label()) }
                        }
                    }

                    div .receipt-grid {
                        div {
                            h3 { "Fulfillment" }
                            p { (order.ship_method.label()) }
                            p { (delivery_estimate(&order, primary_shipment)) }
                            @if order.kind == db::OrderKind::Recurring {
                                p .auth-alt {
                                    "Subscription \u{00b7} " (order.frequency.map(|f| f.label()).unwrap_or("recurring"))
                                    @if let Some(next) = order.next_run_at { " \u{00b7} next " (next.format("%b %-d, %Y")) }
                                }
                            }
                        }
                        div {
                            h3 { "Details" }
                            @if let Some(po) = order.po_number.as_deref() { p { "PO number: " code { (po) } } }
                            p .muted-inline { "Channel: " (order.channel.as_str()) }
                        }
                    }

                    @if !shipments.is_empty() {
                        div .receipt-shipments {
                            h3 { "Shipments & tracking" }
                            @for s in &shipments {
                                div .shipment-row {
                                    span .status-badge .(shipment_status_class(s.status)) { (s.status.label()) }
                                    @if let Some(date) = s.ship_date { span .muted-inline { "Shipped " (date.format("%b %-d")) } }
                                    span { (tracking_snippet(s)) }
                                }
                            }
                        }
                    }

                    table .receipt-table {
                        thead { tr { th { "Item" } th { "Qty" } th { "Unit" } th .num { "Amount" } } }
                        tbody {
                            @for item in &items {
                                tr {
                                    td {
                                        @if let Some(subname) = item.subname.as_deref() { "AthletO " (subname) }
                                        @else { (item.name) }
                                        " " span .muted-inline { "(" (item.format.label()) ")" }
                                    }
                                    td { (item.qty) }
                                    td { (pages::format_price(item.unit_price_cents.into())) }
                                    td .num { (pages::format_price(i64::from(item.unit_price_cents) * i64::from(item.qty))) }
                                }
                            }
                        }
                        tfoot {
                            tr { td colspan="3" { "Subtotal" } td .num { (pages::format_price(order.subtotal_cents)) } }
                            tr { td colspan="3" { (order.ship_method.label()) } td .num {
                                @if order.shipping_cents == 0 { "billed on account" }
                                @else { (pages::format_price(order.shipping_cents)) }
                            } }
                            tr { td colspan="3" { "Tax" } td .num {
                                @if order.tax_cents == 0 { span .muted-inline { "at fulfillment" } }
                                @else { (pages::format_price(order.tax_cents)) }
                            } }
                            tr .receipt-total { td colspan="3" { "Total" } td .num { (pages::format_price(order.total_cents)) } }
                        }
                    }

                    div .receipt-actions {
                        button .button type="button" onclick="window.print()" { "Print / Save PDF" }
                        form .inline-form method="post" action=(format!("/orders/{}/reorder", order.id)) {
                            button .button .ghost type="submit" { "Reorder" }
                        }
                        a .button .ghost href="/orders" { "All orders" }
                    }
                }
            }
        },
    )
    .into_response())
}

fn shipment_status_class(status: db::ShipmentStatus) -> &'static str {
    match status {
        db::ShipmentStatus::Packing => "st-processing",
        db::ShipmentStatus::Shipped => "st-placed",
        db::ShipmentStatus::Delivered => "st-fulfilled",
    }
}

/// POST /orders/{id}/reorder -- re-add a past order's lines to the cart with
/// fresh stock holds, then send the shopper to the cart.
pub async fn reorder(
    State(state): State<SharedState>,
    user: MaybeUser,
    Path(order_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let (auth_user, profile) = match auth::require_full(&state, &user).await {
        Ok(pair) => pair,
        Err(redirect) => return Ok(redirect),
    };
    if let Err(redirect) = auth::require_b2b_ready(&auth_user, profile.as_ref()) {
        return Ok(redirect);
    }
    let Some(pool) = &state.pool else {
        return Ok(Redirect::to("/orders").into_response());
    };

    let lines = db::order_reorder_lines(pool, auth_user.id, order_id).await?;
    if lines.is_empty() {
        return Ok(Redirect::to("/orders").into_response());
    }
    let cart_id = db::find_or_create_cart(pool, &CartOwner::User(auth_user.id)).await?;
    for (product_id, qty) in lines {
        db::add_cart_item(pool, cart_id, product_id, qty).await?;
        let total = db::cart_lines(pool, cart_id)
            .await?
            .iter()
            .find(|l| l.product_id == product_id)
            .map(|l| l.qty)
            .unwrap_or(qty);
        let _ = db::ensure_hold(pool, cart_id, product_id, total).await;
    }
    Ok(Redirect::to("/cart").into_response())
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
    if !profile
        .as_ref()
        .map(CustomerProfile::is_b2b_approved)
        .unwrap_or(false)
    {
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
    let Some(pool) = &state.pool else {
        return Ok(Redirect::to("/cart").into_response());
    };

    let owner = CartOwner::User(auth_user.id);
    let cart_id = db::find_or_create_cart(pool, &owner).await?;
    for (key, value) in &form {
        let Some(product_id) = key.strip_prefix("qty_").and_then(|id| id.parse::<i64>().ok())
        else {
            continue;
        };
        let qty: i32 = value.trim().parse().unwrap_or(0);
        if qty <= 0 {
            continue;
        }
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
    Ok(Redirect::to("/cart").into_response())
}

/// Shared checkout form fragment rendered on the cart page.
pub fn checkout_form(
    config: &crate::Config,
    profile: Option<&CustomerProfile>,
    has_2fa: bool,
) -> Markup {
    let is_b2b_requested = profile.map(CustomerProfile::is_b2b).unwrap_or(false);
    let is_b2b = profile
        .map(CustomerProfile::is_b2b_approved)
        .unwrap_or(false);
    let pay_options = payment_method_options(config, is_b2b);
    if is_b2b_requested && !is_b2b {
        return html! {
            div .notice {
                strong { "Business account approval pending. " }
                "Ordering will unlock once operations approves your company."
            }
        };
    }
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
                    "Shipping"
                    select disabled { option selected { "Freight (LTL) \u{2014} billed on account" } }
                }
                label {
                    "PO number " span .muted-inline { "(optional)" }
                    input type="text" name="po_number" maxlength="60" placeholder="PO-2026-0417";
                }
            } @else {
                label {
                    "Shipping"
                    select name="ship_method" {
                        option value="standard" selected { "Standard \u{2014} $5.99 (3\u{2013}5 business days)" }
                        option value="expedited" { "Expedited \u{2014} $14.99 (1\u{2013}2 business days)" }
                    }
                }
            }
            @if pay_options.is_empty() {
                div .notice {
                    "Online payment isn't configured in this environment; the order is "
                    "placed as payment-pending."
                }
            } @else {
                fieldset .pay-methods {
                    legend { "Pay with" }
                    @for (index, (value, label)) in pay_options.iter().enumerate() {
                        label .pay-method {
                            input type="radio" name="pay_method" value=(value) checked[index == 0];
                            " " (label)
                        }
                    }
                    @if is_b2b {
                        p .muted-inline {
                            "Recurring orders bill automatically on your saved method; "
                            "Net-30 invoices arrive by email with card, ACH, and bank-transfer "
                            "payment options."
                        }
                    }
                }
            }
            button .primary type="submit" { "Place order" }
        }
    }
}

/// Map payment status onto the existing badge palette.
fn payment_class(status: db::PaymentStatus) -> &'static str {
    match status {
        db::PaymentStatus::Paid => "st-fulfilled",
        db::PaymentStatus::Invoiced | db::PaymentStatus::Processing => "st-processing",
        db::PaymentStatus::Pending => "st-placed",
        db::PaymentStatus::Failed | db::PaymentStatus::Refunded => "st-cancelled",
    }
}

/// Can the customer (re)start payment for this order from the orders page?
fn payment_retryable(order: &db::OrderRow) -> bool {
    order.status != db::OrderStatus::Cancelled
        && matches!(
            order.payment_status,
            db::PaymentStatus::Pending | db::PaymentStatus::Failed
        )
}

#[derive(Debug, Deserialize)]
pub struct PayNowRequest {
    #[serde(default)]
    pay_method: String,
}

/// POST /orders/{id}/pay -- (re)start payment for a pending or failed order.
pub async fn pay_now(
    State(state): State<SharedState>,
    user: MaybeUser,
    headers: axum::http::HeaderMap,
    Path(order_id): Path<Uuid>,
    Form(request): Form<PayNowRequest>,
) -> Result<Response, AppError> {
    let (auth_user, profile) = match auth::require_full(&state, &user).await {
        Ok(pair) => pair,
        Err(redirect) => return Ok(redirect),
    };
    if let Err(redirect) = auth::require_b2b_ready(&auth_user, profile.as_ref()) {
        return Ok(redirect);
    }
    let Some(pool) = &state.pool else {
        return Ok(Redirect::to("/orders").into_response());
    };
    // Scoped to the logged-in user: no paying (or probing) other people's
    // orders.
    let Some(order) = db::get_order(pool, auth_user.id, order_id).await? else {
        return Ok(Redirect::to("/orders").into_response());
    };
    if !payment_retryable(&order) {
        return Ok(Redirect::to("/orders").into_response());
    }
    let is_b2b = profile
        .as_ref()
        .map(CustomerProfile::is_b2b_approved)
        .unwrap_or(false);
    let Some(method) = payments::PayMethod::parse(request.pay_method.trim()) else {
        return Ok(Redirect::to("/orders").into_response());
    };
    let redirect = dispatch_payment(
        &state,
        &headers,
        &auth_user,
        order_id,
        method,
        is_b2b,
        order.po_number.as_deref(),
    )
    .await;
    Ok(redirect.into_response())
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

    fn config_with_stripe() -> crate::Config {
        crate::Config {
            stripe: Some(payments::StripeConfig {
                secret_key: "sk_test_x".into(),
                webhook_secret: None,
            }),
            ..crate::Config::default()
        }
    }

    #[test]
    fn b2b_checkout_form_blocks_until_2fa_then_shows_po_field() {
        let config = crate::Config::default();
        let profile = CustomerProfile {
            customer_type: db::CustomerType::B2b,
            company_name: Some("Wobble Co".into()),
        };
        // Business account without a verified factor: hard stop, no form.
        let blocked = checkout_form(&config, Some(&profile), false).into_string();
        assert!(blocked.contains("Two-factor authentication required"));
        assert!(!blocked.contains("Place order"));
        // With 2FA satisfied: the order form renders, including the PO field.
        let allowed = checkout_form(&config, Some(&profile), true).into_string();
        assert!(allowed.contains("Place order"));
        assert!(allowed.contains("PO number"));
    }

    #[test]
    fn b2c_checkout_form_has_no_po_field() {
        let config = crate::Config::default();
        let profile = CustomerProfile {
            customer_type: db::CustomerType::B2c,
            company_name: None,
        };
        let rendered = checkout_form(&config, Some(&profile), false).into_string();
        assert!(rendered.contains("Place order"));
        assert!(!rendered.contains("PO number"));
    }

    fn order_row(
        status: db::OrderStatus,
        payment_status: db::PaymentStatus,
    ) -> db::OrderRow {
        db::OrderRow {
            id: Uuid::nil(),
            kind: OrderKind::OneTime,
            frequency: None,
            status,
            channel: db::OrderChannel::D2cWeb,
            ship_method: db::ShipMethod::Standard,
            po_number: None,
            subtotal_cents: 1000,
            shipping_cents: 599,
            tax_cents: 0,
            total_cents: 1599,
            next_run_at: None,
            created_at: chrono::Utc::now(),
            payment_provider: None,
            payment_status,
            payment_ref: None,
            paid_at: None,
        }
    }

    #[test]
    fn payment_is_retryable_only_while_pending_or_failed_on_live_orders() {
        use db::{OrderStatus, PaymentStatus};
        assert!(payment_retryable(&order_row(OrderStatus::Placed, PaymentStatus::Pending)));
        assert!(payment_retryable(&order_row(OrderStatus::Processing, PaymentStatus::Failed)));
        // Settled, in-flight, or invoiced payments must not be re-payable.
        assert!(!payment_retryable(&order_row(OrderStatus::Placed, PaymentStatus::Paid)));
        assert!(!payment_retryable(&order_row(OrderStatus::Placed, PaymentStatus::Processing)));
        assert!(!payment_retryable(&order_row(OrderStatus::Placed, PaymentStatus::Invoiced)));
        // Cancelled orders take no money, whatever the payment state.
        assert!(!payment_retryable(&order_row(OrderStatus::Cancelled, PaymentStatus::Pending)));
    }

    #[test]
    fn payment_badges_reuse_the_status_palette() {
        use db::PaymentStatus;
        assert_eq!(payment_class(PaymentStatus::Paid), "st-fulfilled");
        assert_eq!(payment_class(PaymentStatus::Invoiced), "st-processing");
        assert_eq!(payment_class(PaymentStatus::Processing), "st-processing");
        assert_eq!(payment_class(PaymentStatus::Pending), "st-placed");
        assert_eq!(payment_class(PaymentStatus::Failed), "st-cancelled");
        assert_eq!(payment_class(PaymentStatus::Refunded), "st-cancelled");
    }

    #[test]
    fn b2b_checkout_form_offers_ach_and_net30_when_stripe_is_configured() {
        let config = config_with_stripe();
        let profile = CustomerProfile {
            customer_type: db::CustomerType::B2b,
            company_name: Some("Wobble Co".into()),
        };
        let rendered = checkout_form(&config, Some(&profile), true).into_string();
        assert!(rendered.contains("ACH bank debit"));
        assert!(rendered.contains("Net 30"));
        assert!(rendered.contains("name=\"pay_method\""));
        // Freight-only shipping for B2B: no ship_method selector.
        assert!(!rendered.contains("name=\"ship_method\""));

        // B2C with the same config: card only, ship-method picker present.
        let rendered = checkout_form(&config, None, false).into_string();
        assert!(!rendered.contains("Net 30"));
        assert!(!rendered.contains("ACH"));
        assert!(rendered.contains("name=\"ship_method\""));
    }

    #[test]
    fn payment_options_follow_configured_providers_and_cohort() {
        // Nothing configured: no options, and the form says orders go
        // payment-pending.
        let bare = crate::Config::default();
        assert!(payment_method_options(&bare, false).is_empty());
        let rendered = checkout_form(&bare, None, false).into_string();
        assert!(rendered.contains("payment-pending"));

        // Stripe configured: B2C gets cards; B2B additionally gets ACH
        // wording and the Net-30 invoice option.
        let config = config_with_stripe();
        let b2c: Vec<_> = payment_method_options(&config, false);
        assert_eq!(b2c, vec![("stripe", "Card (Stripe)")]);
        let b2b = payment_method_options(&config, true);
        assert!(b2b.iter().any(|(value, _)| *value == "invoice"));
        assert!(b2b.iter().any(|(_, label)| label.contains("ACH")));
    }
}
