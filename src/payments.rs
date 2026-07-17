//! Online payment acceptance for app.athleto.store (B2C) and
//! biz.athleto.store (B2B).
//!
//! Three processors, all via hosted/redirect flows (no card data ever touches
//! this server, so PCI scope stays SAQ-A):
//!
//! * **Stripe** — Checkout Sessions for one-time and subscription orders.
//!   B2B sessions also offer ACH bank debit (`us_bank_account`); B2B
//!   open-account orders settle through hosted Net-30 Stripe invoices
//!   (card / ACH / bank transfer) instead of a checkout session.
//! * **PayPal** — Orders v2 for one-time, Billing Plans + Subscriptions for
//!   recurring.
//! * **Square** — hosted payment links for one-time, catalog subscription
//!   plans + payment links for recurring.
//!
//! Every provider confirms twice: once on the browser return URL
//! (`/pay/success`, verified server-side against the provider API) and once
//! via signed webhooks (`/webhooks/{stripe,paypal,square}`), deduplicated
//! through `payment_events`. Settled money is mirrored into the Quaestor
//! billing-server ledger (see `crate::billing`) — that service observes and
//! reconciles but never moves money.

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use uuid::Uuid;

use crate::db::{
    self, OrderFrequency, OrderKind, PaymentKind, PaymentProvider, PaymentStatus,
    SubscriptionStatus,
};
use crate::SharedState;

// ---------------------------------------------------------------------------
// Configuration (all optional; a missing provider degrades to "not offered").

#[derive(Clone, Debug)]
pub struct StripeConfig {
    pub secret_key: String,
    pub webhook_secret: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PayPalConfig {
    pub client_id: String,
    pub client_secret: String,
    pub webhook_id: Option<String>,
    /// `https://api-m.sandbox.paypal.com` or `https://api-m.paypal.com`.
    pub api_base: String,
}

#[derive(Clone, Debug)]
pub struct SquareConfig {
    pub access_token: String,
    pub location_id: String,
    pub webhook_signature_key: Option<String>,
    /// `https://connect.squareupsandbox.com` or `https://connect.squareup.com`.
    pub api_base: String,
}

/// How the customer chose to pay at checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayMethod {
    Stripe,
    Paypal,
    Square,
    /// B2B only: ship on open account against the PO, settle by Net-30
    /// hosted invoice.
    Invoice,
}

impl PayMethod {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stripe" => Some(Self::Stripe),
            "paypal" => Some(Self::Paypal),
            "square" => Some(Self::Square),
            "invoice" => Some(Self::Invoice),
            _ => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PaymentError {
    #[error("provider not configured")]
    NotConfigured,
    #[error("provider rejected request: {0}")]
    Provider(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Orm(#[from] sea_orm::DbErr),
}

/// What `start_payment` decided; the checkout handler turns this into a
/// redirect.
pub enum StartOutcome {
    /// Send the browser to the provider's hosted page.
    Redirect(String),
    /// Net-30 invoice issued and emailed; nothing to redirect to.
    Invoiced,
    /// The chosen provider has no keys in this environment. The order stays
    /// `payment_status = pending` and can be paid later from /orders.
    NotConfigured,
}

// ---------------------------------------------------------------------------
// Small shared helpers.

fn dollars(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}

/// Stripe `price_data[recurring]` mapping.
fn stripe_interval(freq: OrderFrequency) -> (&'static str, u32) {
    match freq {
        OrderFrequency::Weekly => ("week", 1),
        OrderFrequency::Biweekly => ("week", 2),
        OrderFrequency::Monthly => ("month", 1),
        OrderFrequency::Quarterly => ("month", 3),
    }
}

/// PayPal billing-cycle `frequency` mapping.
fn paypal_interval(freq: OrderFrequency) -> (&'static str, u32) {
    match freq {
        OrderFrequency::Weekly => ("WEEK", 1),
        OrderFrequency::Biweekly => ("WEEK", 2),
        OrderFrequency::Monthly => ("MONTH", 1),
        OrderFrequency::Quarterly => ("MONTH", 3),
    }
}

/// Square subscription-plan cadence mapping.
fn square_cadence(freq: OrderFrequency) -> &'static str {
    match freq {
        OrderFrequency::Weekly => "WEEKLY",
        OrderFrequency::Biweekly => "EVERY_TWO_WEEKS",
        OrderFrequency::Monthly => "MONTHLY",
        OrderFrequency::Quarterly => "QUARTERLY",
    }
}

fn item_display_name(item: &db::OrderItemRow) -> String {
    match item.subname.as_deref() {
        Some(subname) => format!("AthletO {subname} ({})", item.format.label()),
        None => format!("{} ({})", item.name, item.format.label()),
    }
}

fn success_url(base: &str, provider: &str, order_id: Uuid) -> String {
    format!("{base}/pay/success?provider={provider}&order={order_id}")
}

fn cancel_url(base: &str, order_id: Uuid) -> String {
    format!("{base}/pay/cancel?order={order_id}")
}

async fn provider_error(response: reqwest::Response) -> PaymentError {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let snippet: String = body.chars().take(400).collect();
    PaymentError::Provider(format!("{status}: {snippet}"))
}

// ---------------------------------------------------------------------------
// Checkout entry point.

/// Create the provider-side checkout artifact for an already-placed order and
/// say where to send the customer. The order keeps `payment_status =
/// pending` until a verified return or webhook settles it.
pub async fn start_payment(
    state: &SharedState,
    base_url: &str,
    user_id: Uuid,
    email: Option<&str>,
    order_id: Uuid,
    method: PayMethod,
    is_b2b: bool,
    po_number: Option<&str>,
) -> Result<StartOutcome, PaymentError> {
    let (Some(pool), Some(orm)) = (&state.pool, &state.orm) else {
        return Ok(StartOutcome::NotConfigured);
    };
    let Some(facts) = db::order_payment_facts(orm, order_id).await? else {
        return Err(PaymentError::Provider("order not found".into()));
    };
    let items = db::order_items(pool, order_id).await?;
    let shipping_cents = facts.total_cents
        - items
            .iter()
            .map(|item| i64::from(item.unit_price_cents) * i64::from(item.qty))
            .sum::<i64>();

    match method {
        PayMethod::Stripe => {
            let Some(cfg) = &state.config.stripe else {
                return Ok(StartOutcome::NotConfigured);
            };
            let url = stripe_checkout_session(
                state, cfg, base_url, &facts, &items, shipping_cents, email, is_b2b,
            )
            .await?;
            Ok(StartOutcome::Redirect(url))
        }
        PayMethod::Paypal => {
            let Some(cfg) = &state.config.paypal else {
                return Ok(StartOutcome::NotConfigured);
            };
            let url = match (facts.kind, facts.frequency) {
                (OrderKind::Recurring, Some(freq)) => {
                    paypal_subscription(state, cfg, base_url, &facts, freq).await?
                }
                _ => paypal_order(state, cfg, base_url, &facts).await?,
            };
            Ok(StartOutcome::Redirect(url))
        }
        PayMethod::Square => {
            let Some(cfg) = &state.config.square else {
                return Ok(StartOutcome::NotConfigured);
            };
            let url = square_payment_link(
                state, cfg, base_url, &facts, &items, shipping_cents, email,
            )
            .await?;
            Ok(StartOutcome::Redirect(url))
        }
        PayMethod::Invoice => {
            let Some(cfg) = &state.config.stripe else {
                return Ok(StartOutcome::NotConfigured);
            };
            stripe_net30_invoice(state, cfg, &facts, &items, shipping_cents, email, po_number)
                .await?;
            // Open AR in the observer ledger right away: the invoice is the
            // billable event, payment lands later via invoice.paid.
            crate::billing::spawn_order_invoice(state, user_id, order_id, facts.total_cents);
            Ok(StartOutcome::Invoiced)
        }
    }
}

// ---------------------------------------------------------------------------
// Stripe.

const STRIPE_API: &str = "https://api.stripe.com";

async fn stripe_checkout_session(
    state: &SharedState,
    cfg: &StripeConfig,
    base_url: &str,
    facts: &db::OrderPaymentFacts,
    items: &[db::OrderItemRow],
    shipping_cents: i64,
    email: Option<&str>,
    is_b2b: bool,
) -> Result<String, PaymentError> {
    let recurring = match (facts.kind, facts.frequency) {
        (OrderKind::Recurring, Some(freq)) => Some(stripe_interval(freq)),
        _ => None,
    };

    let mut form: Vec<(String, String)> = vec![
        (
            "mode".into(),
            if recurring.is_some() { "subscription" } else { "payment" }.into(),
        ),
        ("client_reference_id".into(), facts.id.to_string()),
        (
            "success_url".into(),
            format!(
                "{}&session_id={{CHECKOUT_SESSION_ID}}",
                success_url(base_url, "stripe", facts.id)
            ),
        ),
        ("cancel_url".into(), cancel_url(base_url, facts.id)),
        ("metadata[order_id]".into(), facts.id.to_string()),
    ];
    if let Some(email) = email {
        form.push(("customer_email".into(), email.into()));
    }
    if recurring.is_some() {
        form.push((
            "subscription_data[metadata][order_id]".into(),
            facts.id.to_string(),
        ));
    } else {
        form.push((
            "payment_intent_data[metadata][order_id]".into(),
            facts.id.to_string(),
        ));
    }
    // B2B gets modern bank-debit rails alongside cards. (ACH settles in days;
    // the webhook's async_payment_succeeded finishes the order.)
    if is_b2b {
        form.push(("payment_method_types[0]".into(), "card".into()));
        form.push(("payment_method_types[1]".into(), "us_bank_account".into()));
    }

    let mut line = 0usize;
    let mut push_line = |form: &mut Vec<(String, String)>, name: &str, unit_cents: i64, qty: i64| {
        form.push((format!("line_items[{line}][quantity]"), qty.to_string()));
        form.push((
            format!("line_items[{line}][price_data][currency]"),
            "usd".into(),
        ));
        form.push((
            format!("line_items[{line}][price_data][unit_amount]"),
            unit_cents.to_string(),
        ));
        form.push((
            format!("line_items[{line}][price_data][product_data][name]"),
            name.into(),
        ));
        if let Some((interval, count)) = recurring {
            form.push((
                format!("line_items[{line}][price_data][recurring][interval]"),
                interval.into(),
            ));
            form.push((
                format!("line_items[{line}][price_data][recurring][interval_count]"),
                count.to_string(),
            ));
        }
        line += 1;
    };
    for item in items {
        push_line(
            &mut form,
            &item_display_name(item),
            i64::from(item.unit_price_cents),
            i64::from(item.qty),
        );
    }
    if shipping_cents > 0 {
        push_line(&mut form, "Shipping", shipping_cents, 1);
    }

    let response = state
        .http
        .post(format!("{STRIPE_API}/v1/checkout/sessions"))
        .bearer_auth(&cfg.secret_key)
        .form(&form)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let session: Value = response.json().await?;
    let session_id = session["id"].as_str().unwrap_or_default().to_string();
    let url = session["url"]
        .as_str()
        .ok_or_else(|| PaymentError::Provider("checkout session has no url".into()))?
        .to_string();

    if let Some(orm) = &state.orm {
        db::set_order_payment(orm, facts.id, PaymentProvider::Stripe, &session_id, PaymentStatus::Pending)
            .await?;
    }
    Ok(url)
}

/// B2B open account: hosted Stripe invoice on Net-30 terms. The customer gets
/// an email with a payment page offering card, ACH debit, and bank transfer.
async fn stripe_net30_invoice(
    state: &SharedState,
    cfg: &StripeConfig,
    facts: &db::OrderPaymentFacts,
    items: &[db::OrderItemRow],
    shipping_cents: i64,
    email: Option<&str>,
    po_number: Option<&str>,
) -> Result<(), PaymentError> {
    let Some(email) = email else {
        return Err(PaymentError::Provider(
            "invoice billing needs a customer email".into(),
        ));
    };

    // Find-or-create the Stripe customer by email.
    let found: Value = state
        .http
        .get(format!("{STRIPE_API}/v1/customers"))
        .bearer_auth(&cfg.secret_key)
        .query(&[("email", email), ("limit", "1")])
        .send()
        .await?
        .json()
        .await?;
    let customer_id = match found["data"][0]["id"].as_str() {
        Some(id) => id.to_string(),
        None => {
            let response = state
                .http
                .post(format!("{STRIPE_API}/v1/customers"))
                .bearer_auth(&cfg.secret_key)
                .form(&[
                    ("email", email.to_string()),
                    ("metadata[athleto_user_id]", facts.user_id.to_string()),
                ])
                .send()
                .await?;
            if !response.status().is_success() {
                return Err(provider_error(response).await);
            }
            let customer: Value = response.json().await?;
            customer["id"].as_str().unwrap_or_default().to_string()
        }
    };

    let description = match po_number {
        Some(po) => format!("AthletO order {} — PO {po}", facts.id.simple()),
        None => format!("AthletO order {}", facts.id.simple()),
    };
    let response = state
        .http
        .post(format!("{STRIPE_API}/v1/invoices"))
        .bearer_auth(&cfg.secret_key)
        .form(&[
            ("customer", customer_id.clone()),
            ("collection_method", "send_invoice".into()),
            ("days_until_due", "30".into()),
            ("description", description),
            ("metadata[order_id]", facts.id.to_string()),
        ])
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let invoice: Value = response.json().await?;
    let invoice_id = invoice["id"].as_str().unwrap_or_default().to_string();

    for item in items {
        let amount = i64::from(item.unit_price_cents) * i64::from(item.qty);
        let response = state
            .http
            .post(format!("{STRIPE_API}/v1/invoiceitems"))
            .bearer_auth(&cfg.secret_key)
            .form(&[
                ("customer", customer_id.clone()),
                ("invoice", invoice_id.clone()),
                ("amount", amount.to_string()),
                ("currency", "usd".into()),
                (
                    "description",
                    format!("{} x {}", item.qty, item_display_name(item)),
                ),
            ])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(provider_error(response).await);
        }
    }
    if shipping_cents > 0 {
        state
            .http
            .post(format!("{STRIPE_API}/v1/invoiceitems"))
            .bearer_auth(&cfg.secret_key)
            .form(&[
                ("customer", customer_id.clone()),
                ("invoice", invoice_id.clone()),
                ("amount", shipping_cents.to_string()),
                ("currency", "usd".into()),
                ("description", "Shipping".into()),
            ])
            .send()
            .await?;
    }

    for action in ["finalize", "send"] {
        let response = state
            .http
            .post(format!("{STRIPE_API}/v1/invoices/{invoice_id}/{action}"))
            .bearer_auth(&cfg.secret_key)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(provider_error(response).await);
        }
    }

    if let Some(orm) = &state.orm {
        db::set_order_payment(
            pool,
            facts.id,
            PaymentProvider::Invoice,
            &invoice_id,
            PaymentStatus::Invoiced,
        )
        .await?;
    }
    Ok(())
}

/// Verify a `Stripe-Signature` header: HMAC-SHA256 over `"{t}.{body}"`.
fn stripe_signature_valid(secret: &str, header: &str, body: &[u8]) -> bool {
    let mut timestamp = None;
    let mut signatures = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", value)) => timestamp = Some(value),
            Some(("v1", value)) => signatures.push(value),
            _ => {}
        }
    }
    let Some(timestamp) = timestamp else { return false };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    let expected = hex::encode(mac.finalize().into_bytes());
    signatures.iter().any(|signature| {
        signature.len() == expected.len()
            && signature
                .bytes()
                .zip(expected.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    })
}

/// POST /webhooks/stripe
pub async fn stripe_webhook(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(cfg) = &state.config.stripe else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if let Some(secret) = &cfg.webhook_secret {
        let header = headers
            .get("stripe-signature")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !stripe_signature_valid(secret, header, &body) {
            return StatusCode::BAD_REQUEST.into_response();
        }
    }
    let Some(orm) = state.orm.clone() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let event_id = event["id"].as_str().unwrap_or_default().to_string();
    let event_type = event["type"].as_str().unwrap_or_default().to_string();
    match db::record_payment_event(&orm, PaymentProvider::Stripe, &event_id, &event).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::OK.into_response(), // replay
        Err(err) => {
            tracing::error!(error = %err, "stripe webhook bookkeeping failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let object = &event["data"]["object"];

    let result: Result<(), PaymentError> = async {
        match event_type.as_str() {
            "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
                let order_id = object["metadata"]["order_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Uuid>().ok());
                let Some(order_id) = order_id else { return Ok(()) };
                if let Some(subscription) = object["subscription"].as_str() {
                    if let Ok(Some(facts)) = db::order_payment_facts(&orm, order_id).await {
                        if let Some(freq) = facts.frequency {
                            db::upsert_subscription(
                                &orm,
                                facts.user_id,
                                Some(order_id),
                                PaymentProvider::Stripe,
                                subscription,
                                SubscriptionStatus::Active,
                                freq,
                            )
                            .await?;
                        }
                    }
                }
                if object["payment_status"].as_str() == Some("paid") {
                    let reference = object["payment_intent"]
                        .as_str()
                        .or_else(|| object["subscription"].as_str())
                        .or_else(|| object["id"].as_str())
                        .unwrap_or_default();
                    settle_order(&state, &orm, order_id, PaymentProvider::Stripe, reference, PaymentKind::Charge)
                        .await?;
                } else {
                    // ACH debit initiated; settles via async_payment_succeeded.
                    db::set_order_payment_status(&orm, order_id, PaymentStatus::Processing).await?;
                }
            }
            "checkout.session.async_payment_failed" => {
                if let Some(order_id) = object["metadata"]["order_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Uuid>().ok())
                {
                    db::set_order_payment_status(&orm, order_id, PaymentStatus::Failed).await?;
                }
            }
            "invoice.paid" => {
                let invoice_id = object["id"].as_str().unwrap_or_default();
                let amount = object["amount_paid"].as_i64().unwrap_or_default();
                if let Some(order_id) = object["metadata"]["order_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Uuid>().ok())
                {
                    // Our hosted Net-30 invoice.
                    settle_order(&state, &orm, order_id, PaymentProvider::Invoice, invoice_id, PaymentKind::Charge)
                        .await?;
                } else if let Some(subscription) = object["subscription"].as_str() {
                    record_subscription_cycle(
                        &state,
                        &orm,
                        PaymentProvider::Stripe,
                        subscription,
                        invoice_id,
                        amount,
                    )
                    .await?;
                }
            }
            "invoice.payment_failed" => {
                if let Some(subscription) = object["subscription"].as_str() {
                    db::set_subscription_status(
                        &orm,
                        PaymentProvider::Stripe,
                        subscription,
                        SubscriptionStatus::PastDue,
                    )
                    .await?;
                }
            }
            "customer.subscription.deleted" => {
                if let Some(subscription) = object["id"].as_str() {
                    db::set_subscription_status(
                        &orm,
                        PaymentProvider::Stripe,
                        subscription,
                        SubscriptionStatus::Cancelled,
                    )
                    .await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => {
            tracing::error!(error = %err, event = %event_type, "stripe webhook handling failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// PayPal.

async fn paypal_token(
    state: &SharedState,
    cfg: &PayPalConfig,
) -> Result<String, PaymentError> {
    let response = state
        .http
        .post(format!("{}/v1/oauth2/token", cfg.api_base))
        .basic_auth(&cfg.client_id, Some(&cfg.client_secret))
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let token: Value = response.json().await?;
    token["access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| PaymentError::Provider("paypal token response had no access_token".into()))
}

fn paypal_approval_link(value: &Value) -> Option<String> {
    value["links"].as_array()?.iter().find_map(|link| {
        let rel = link["rel"].as_str()?;
        if rel == "approve" || rel == "payer-action" {
            link["href"].as_str().map(str::to_string)
        } else {
            None
        }
    })
}

async fn paypal_order(
    state: &SharedState,
    cfg: &PayPalConfig,
    base_url: &str,
    facts: &db::OrderPaymentFacts,
) -> Result<String, PaymentError> {
    let token = paypal_token(state, cfg).await?;
    let body = json!({
        "intent": "CAPTURE",
        "purchase_units": [{
            "reference_id": facts.id.to_string(),
            "custom_id": facts.id.to_string(),
            "amount": {"currency_code": "USD", "value": dollars(facts.total_cents)},
        }],
        "application_context": {
            "brand_name": "AthletO",
            "user_action": "PAY_NOW",
            "return_url": success_url(base_url, "paypal", facts.id),
            "cancel_url": cancel_url(base_url, facts.id),
        },
    });
    let response = state
        .http
        .post(format!("{}/v2/checkout/orders", cfg.api_base))
        .bearer_auth(&token)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let order: Value = response.json().await?;
    let paypal_order_id = order["id"].as_str().unwrap_or_default();
    if let Some(orm) = &state.orm {
        db::set_order_payment(orm, facts.id, PaymentProvider::Paypal, paypal_order_id, PaymentStatus::Pending)
            .await?;
    }
    paypal_approval_link(&order)
        .ok_or_else(|| PaymentError::Provider("paypal order had no approval link".into()))
}

/// Recurring PayPal orders: catalog product -> billing plan -> subscription,
/// all created on the fly for this order's amount and cadence.
async fn paypal_subscription(
    state: &SharedState,
    cfg: &PayPalConfig,
    base_url: &str,
    facts: &db::OrderPaymentFacts,
    freq: OrderFrequency,
) -> Result<String, PaymentError> {
    let token = paypal_token(state, cfg).await?;
    let (unit, count) = paypal_interval(freq);

    let response = state
        .http
        .post(format!("{}/v1/catalogs/products", cfg.api_base))
        .bearer_auth(&token)
        .json(&json!({"name": "AthletO recurring order", "type": "PHYSICAL"}))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let product: Value = response.json().await?;
    let product_id = product["id"].as_str().unwrap_or_default();

    let response = state
        .http
        .post(format!("{}/v1/billing/plans", cfg.api_base))
        .bearer_auth(&token)
        .json(&json!({
            "product_id": product_id,
            "name": format!("AthletO {} order", freq.label()),
            "billing_cycles": [{
                "frequency": {"interval_unit": unit, "interval_count": count},
                "tenure_type": "REGULAR",
                "sequence": 1,
                "total_cycles": 0,
                "pricing_scheme": {
                    "fixed_price": {"value": dollars(facts.total_cents), "currency_code": "USD"}
                },
            }],
            "payment_preferences": {"auto_bill_outstanding": true},
        }))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let plan: Value = response.json().await?;
    let plan_id = plan["id"].as_str().unwrap_or_default();

    let response = state
        .http
        .post(format!("{}/v1/billing/subscriptions", cfg.api_base))
        .bearer_auth(&token)
        .json(&json!({
            "plan_id": plan_id,
            "custom_id": facts.id.to_string(),
            "application_context": {
                "brand_name": "AthletO",
                "return_url": success_url(base_url, "paypal", facts.id),
                "cancel_url": cancel_url(base_url, facts.id),
            },
        }))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let subscription: Value = response.json().await?;
    let subscription_id = subscription["id"].as_str().unwrap_or_default();

    if let Some(orm) = &state.orm {
        db::set_order_payment(orm, facts.id, PaymentProvider::Paypal, subscription_id, PaymentStatus::Pending)
            .await?;
        db::upsert_subscription(
            orm,
            facts.user_id,
            Some(facts.id),
            PaymentProvider::Paypal,
            subscription_id,
            SubscriptionStatus::Pending,
            freq,
        )
        .await?;
    }
    paypal_approval_link(&subscription)
        .ok_or_else(|| PaymentError::Provider("paypal subscription had no approval link".into()))
}

/// POST /webhooks/paypal — verified via PayPal's verify-webhook-signature API.
pub async fn paypal_webhook(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(cfg) = &state.config.paypal else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some(orm) = state.orm.clone() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };

    if let Some(webhook_id) = &cfg.webhook_id {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
        };
        let verified: Result<bool, PaymentError> = async {
            let token = paypal_token(&state, cfg).await?;
            let response = state
                .http
                .post(format!(
                    "{}/v1/notifications/verify-webhook-signature",
                    cfg.api_base
                ))
                .bearer_auth(&token)
                .json(&json!({
                    "auth_algo": header("paypal-auth-algo"),
                    "cert_url": header("paypal-cert-url"),
                    "transmission_id": header("paypal-transmission-id"),
                    "transmission_sig": header("paypal-transmission-sig"),
                    "transmission_time": header("paypal-transmission-time"),
                    "webhook_id": webhook_id,
                    "webhook_event": event,
                }))
                .send()
                .await?;
            let verdict: Value = response.json().await?;
            Ok(verdict["verification_status"].as_str() == Some("SUCCESS"))
        }
        .await;
        match verified {
            Ok(true) => {}
            Ok(false) => return StatusCode::BAD_REQUEST.into_response(),
            Err(err) => {
                tracing::error!(error = %err, "paypal webhook verification errored");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }

    let event_id = event["id"].as_str().unwrap_or_default().to_string();
    let event_type = event["event_type"].as_str().unwrap_or_default().to_string();
    match db::record_payment_event(&orm, PaymentProvider::Paypal, &event_id, &event).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::OK.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "paypal webhook bookkeeping failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let resource = &event["resource"];

    let result: Result<(), PaymentError> = async {
        match event_type.as_str() {
            "PAYMENT.CAPTURE.COMPLETED" => {
                let capture_id = resource["id"].as_str().unwrap_or_default();
                if let Some(order_id) = resource["custom_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Uuid>().ok())
                {
                    settle_order(&state, &orm, order_id, PaymentProvider::Paypal, capture_id, PaymentKind::Charge)
                        .await?;
                }
            }
            "PAYMENT.SALE.COMPLETED" => {
                // Subscription cycle payments arrive as sales tied to the
                // billing agreement (= subscription id).
                let sale_id = resource["id"].as_str().unwrap_or_default();
                let amount = resource["amount"]["total"]
                    .as_str()
                    .and_then(|value| value.replace('.', "").parse::<i64>().ok())
                    .unwrap_or_default();
                if let Some(subscription) = resource["billing_agreement_id"].as_str() {
                    record_subscription_cycle(&state, &orm, PaymentProvider::Paypal, subscription, sale_id, amount)
                        .await?;
                }
            }
            "BILLING.SUBSCRIPTION.ACTIVATED" => {
                let subscription = resource["id"].as_str().unwrap_or_default();
                db::set_subscription_status(&orm, PaymentProvider::Paypal, subscription, SubscriptionStatus::Active)
                    .await?;
                if let Some(order_id) = resource["custom_id"]
                    .as_str()
                    .and_then(|id| id.parse::<Uuid>().ok())
                {
                    settle_order(&state, &orm, order_id, PaymentProvider::Paypal, subscription, PaymentKind::Charge)
                        .await?;
                }
            }
            "BILLING.SUBSCRIPTION.CANCELLED" | "BILLING.SUBSCRIPTION.SUSPENDED" => {
                let subscription = resource["id"].as_str().unwrap_or_default();
                db::set_subscription_status(&orm, PaymentProvider::Paypal, subscription, SubscriptionStatus::Cancelled)
                    .await?;
            }
            "BILLING.SUBSCRIPTION.PAYMENT.FAILED" => {
                let subscription = resource["id"].as_str().unwrap_or_default();
                db::set_subscription_status(&orm, PaymentProvider::Paypal, subscription, SubscriptionStatus::PastDue)
                    .await?;
            }
            _ => {}
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => {
            tracing::error!(error = %err, event = %event_type, "paypal webhook handling failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Square.

async fn square_payment_link(
    state: &SharedState,
    cfg: &SquareConfig,
    base_url: &str,
    facts: &db::OrderPaymentFacts,
    items: &[db::OrderItemRow],
    shipping_cents: i64,
    email: Option<&str>,
) -> Result<String, PaymentError> {
    let mut line_items: Vec<Value> = items
        .iter()
        .map(|item| {
            json!({
                "name": item_display_name(item),
                "quantity": item.qty.to_string(),
                "base_price_money": {"amount": i64::from(item.unit_price_cents), "currency": "USD"},
            })
        })
        .collect();
    if shipping_cents > 0 {
        line_items.push(json!({
            "name": "Shipping",
            "quantity": "1",
            "base_price_money": {"amount": shipping_cents, "currency": "USD"},
        }));
    }

    let mut checkout_options = json!({
        "redirect_url": success_url(base_url, "square", facts.id),
    });
    // Recurring orders ride a catalog subscription plan pinned to this
    // order's total and cadence.
    if let (OrderKind::Recurring, Some(freq)) = (facts.kind, facts.frequency) {
        let variation_id = square_subscription_plan(state, cfg, facts, freq).await?;
        checkout_options["subscription_plan_id"] = json!(variation_id);
    }

    let mut body = json!({
        "idempotency_key": facts.id.to_string(),
        "order": {
            "location_id": cfg.location_id,
            "reference_id": facts.id.to_string(),
            "line_items": line_items,
        },
        "checkout_options": checkout_options,
    });
    if let Some(email) = email {
        body["pre_populated_data"] = json!({"buyer_email": email});
    }

    let response = state
        .http
        .post(format!("{}/v2/online-checkout/payment-links", cfg.api_base))
        .bearer_auth(&cfg.access_token)
        .json(&body)
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let link: Value = response.json().await?;
    let square_order_id = link["payment_link"]["order_id"]
        .as_str()
        .or_else(|| link["payment_link"]["id"].as_str())
        .unwrap_or_default();
    let url = link["payment_link"]["url"]
        .as_str()
        .ok_or_else(|| PaymentError::Provider("square payment link had no url".into()))?
        .to_string();

    if let Some(orm) = &state.orm {
        db::set_order_payment(orm, facts.id, PaymentProvider::Square, square_order_id, PaymentStatus::Pending)
            .await?;
    }
    Ok(url)
}

/// Create a Square catalog SUBSCRIPTION_PLAN + variation for this order's
/// total and cadence; returns the plan-variation id for checkout_options.
async fn square_subscription_plan(
    state: &SharedState,
    cfg: &SquareConfig,
    facts: &db::OrderPaymentFacts,
    freq: OrderFrequency,
) -> Result<String, PaymentError> {
    let response = state
        .http
        .post(format!("{}/v2/catalog/object", cfg.api_base))
        .bearer_auth(&cfg.access_token)
        .json(&json!({
            "idempotency_key": format!("athleto-plan-{}", facts.id),
            "object": {
                "type": "SUBSCRIPTION_PLAN",
                "id": "#athleto-plan",
                "subscription_plan_data": {"name": "AthletO recurring order", "all_items": true},
            },
        }))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let plan: Value = response.json().await?;
    let plan_id = plan["catalog_object"]["id"].as_str().unwrap_or_default();

    let response = state
        .http
        .post(format!("{}/v2/catalog/object", cfg.api_base))
        .bearer_auth(&cfg.access_token)
        .json(&json!({
            "idempotency_key": format!("athleto-planvar-{}", facts.id),
            "object": {
                "type": "SUBSCRIPTION_PLAN_VARIATION",
                "id": "#athleto-planvar",
                "subscription_plan_variation_data": {
                    "name": format!("AthletO {} order", freq.label()),
                    "subscription_plan_id": plan_id,
                    "phases": [{
                        "cadence": square_cadence(freq),
                        "ordinal": 0,
                        "pricing": {
                            "type": "STATIC",
                            "price_money": {"amount": facts.total_cents, "currency": "USD"},
                        },
                    }],
                },
            },
        }))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(provider_error(response).await);
    }
    let variation: Value = response.json().await?;
    variation["catalog_object"]["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| PaymentError::Provider("square plan variation had no id".into()))
}

/// Verify Square's `x-square-hmacsha256-signature`: base64(HMAC-SHA256(key,
/// notification_url + body)).
fn square_signature_valid(key: &str, notification_url: &str, header: &str, body: &[u8]) -> bool {
    let mut mac = match Hmac::<Sha256>::new_from_slice(key.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(notification_url.as_bytes());
    mac.update(body);
    let expected = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
    header.len() == expected.len()
        && header
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// POST /webhooks/square
pub async fn square_webhook(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(cfg) = &state.config.square else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if let Some(key) = &cfg.webhook_signature_key {
        let notification_url = format!("{}/webhooks/square", state.config.public_base_url);
        let header = headers
            .get("x-square-hmacsha256-signature")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !square_signature_valid(key, &notification_url, header, &body) {
            return StatusCode::BAD_REQUEST.into_response();
        }
    }
    let Some(orm) = state.orm.clone() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(event) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let event_id = event["event_id"].as_str().unwrap_or_default().to_string();
    let event_type = event["type"].as_str().unwrap_or_default().to_string();
    match db::record_payment_event(&orm, PaymentProvider::Square, &event_id, &event).await {
        Ok(true) => {}
        Ok(false) => return StatusCode::OK.into_response(),
        Err(err) => {
            tracing::error!(error = %err, "square webhook bookkeeping failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    let result: Result<(), PaymentError> = async {
        match event_type.as_str() {
            "payment.updated" => {
                let payment = &event["data"]["object"]["payment"];
                if payment["status"].as_str() != Some("COMPLETED") {
                    return Ok(());
                }
                let payment_id = payment["id"].as_str().unwrap_or_default();
                let Some(square_order) = payment["order_id"].as_str() else {
                    return Ok(());
                };
                if let Some(order_id) =
                    db::find_order_by_payment_ref(&orm, PaymentProvider::Square, square_order).await?
                {
                    settle_order(&state, &orm, order_id, PaymentProvider::Square, payment_id, PaymentKind::Charge)
                        .await?;
                }
            }
            "subscription.created" | "subscription.updated" => {
                let subscription = &event["data"]["object"]["subscription"];
                let subscription_id = subscription["id"].as_str().unwrap_or_default();
                let status = match subscription["status"].as_str() {
                    Some("ACTIVE") => SubscriptionStatus::Active,
                    Some("CANCELED") | Some("DEACTIVATED") => SubscriptionStatus::Cancelled,
                    Some("DELINQUENT") => SubscriptionStatus::PastDue,
                    _ => SubscriptionStatus::Pending,
                };
                // Square subscriptions are born from our payment link's order;
                // tie back to the shop order via the checkout order id.
                let order_id = match subscription["order_id"].as_str() {
                    Some(square_order) => {
                        db::find_order_by_payment_ref(&orm, PaymentProvider::Square, square_order)
                            .await?
                    }
                    None => None,
                };
                if let Some(order_id) = order_id {
                    if let Ok(Some(facts)) = db::order_payment_facts(&orm, order_id).await {
                        if let Some(freq) = facts.frequency {
                            db::upsert_subscription(
                                &orm,
                                facts.user_id,
                                Some(order_id),
                                PaymentProvider::Square,
                                subscription_id,
                                status,
                                freq,
                            )
                            .await?;
                        }
                    }
                } else {
                    db::set_subscription_status(&orm, PaymentProvider::Square, subscription_id, status)
                        .await?;
                }
            }
            "invoice.payment_made" => {
                let invoice = &event["data"]["object"]["invoice"];
                let invoice_id = invoice["id"].as_str().unwrap_or_default();
                let amount = invoice["payment_requests"][0]["computed_amount_money"]["amount"]
                    .as_i64()
                    .unwrap_or_default();
                if let Some(subscription) = invoice["subscription_id"].as_str() {
                    record_subscription_cycle(&state, &orm, PaymentProvider::Square, subscription, invoice_id, amount)
                        .await?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    .await;

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => {
            tracing::error!(error = %err, event = %event_type, "square webhook handling failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Settlement plumbing shared by webhooks and return URLs.

/// Mark an order paid, record the money movement once, and mirror it into
/// the Quaestor ledger (invoice + payment postings) on first sight.
async fn settle_order(
    state: &SharedState,
    pool: &sqlx::PgPool,
    order_id: Uuid,
    provider: PaymentProvider,
    provider_ref: &str,
    kind: PaymentKind,
) -> Result<(), PaymentError> {
    let Some(facts) = db::order_payment_facts(pool, order_id).await? else {
        return Ok(());
    };
    db::set_order_payment_status(pool, order_id, PaymentStatus::Paid).await?;
    let newly_recorded = db::record_payment(
        pool,
        Some(order_id),
        facts.user_id,
        provider,
        kind,
        provider_ref,
        facts.total_cents,
        PaymentStatus::Paid,
    )
    .await?;
    if newly_recorded {
        crate::billing::spawn_order_invoice(state, facts.user_id, order_id, facts.total_cents);
        crate::billing::spawn_payment(
            state,
            facts.user_id,
            Some(order_id),
            provider,
            provider_ref,
            facts.total_cents,
        );
    }
    Ok(())
}

/// A recurring charge landed for a known subscription: record it and mirror
/// invoice+payment postings for the cycle into the ledger.
async fn record_subscription_cycle(
    state: &SharedState,
    pool: &sqlx::PgPool,
    provider: PaymentProvider,
    subscription_ref: &str,
    payment_ref: &str,
    amount_cents: i64,
) -> Result<(), PaymentError> {
    let Some((user_id, order_id)) = db::subscription_owner(pool, provider, subscription_ref).await?
    else {
        return Ok(());
    };
    db::set_subscription_status(pool, provider, subscription_ref, SubscriptionStatus::Active).await?;
    let amount = if amount_cents > 0 {
        amount_cents
    } else if let Some(order_id) = order_id {
        db::order_payment_facts(pool, order_id)
            .await?
            .map(|facts| facts.total_cents)
            .unwrap_or_default()
    } else {
        0
    };
    let newly_recorded = db::record_payment(
        pool,
        order_id,
        user_id,
        provider,
        PaymentKind::SubscriptionCycle,
        payment_ref,
        amount,
        PaymentStatus::Paid,
    )
    .await?;
    if newly_recorded {
        crate::billing::spawn_subscription_cycle(state, user_id, provider, payment_ref, amount);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Browser return URLs.

#[derive(Debug, Deserialize)]
pub struct ReturnParams {
    #[serde(default)]
    provider: String,
    order: Uuid,
    /// Stripe: `session_id={CHECKOUT_SESSION_ID}`.
    session_id: Option<String>,
    /// PayPal one-time: the PayPal order id.
    token: Option<String>,
    /// PayPal subscriptions.
    subscription_id: Option<String>,
}

/// GET /pay/success — the customer came back from the hosted page. Verify
/// with the provider before believing anything in the query string.
pub async fn pay_success(
    State(state): State<SharedState>,
    Query(params): Query<ReturnParams>,
) -> Response {
    let Some(orm) = state.orm.clone() else {
        return Redirect::to("/orders").into_response();
    };
    let order_id = params.order;

    let outcome: Result<PaymentStatus, PaymentError> = async {
        match params.provider.as_str() {
            "stripe" => {
                let Some(cfg) = &state.config.stripe else {
                    return Ok(PaymentStatus::Pending);
                };
                let Some(session_id) = params.session_id.as_deref() else {
                    return Ok(PaymentStatus::Pending);
                };
                let session: Value = state
                    .http
                    .get(format!("{STRIPE_API}/v1/checkout/sessions/{session_id}"))
                    .bearer_auth(&cfg.secret_key)
                    .send()
                    .await?
                    .json()
                    .await?;
                if session["metadata"]["order_id"].as_str() != Some(order_id.to_string().as_str()) {
                    return Ok(PaymentStatus::Pending);
                }
                if let Some(subscription) = session["subscription"].as_str() {
                    if let Ok(Some(facts)) = db::order_payment_facts(&orm, order_id).await {
                        if let Some(freq) = facts.frequency {
                            db::upsert_subscription(
                                &orm,
                                facts.user_id,
                                Some(order_id),
                                PaymentProvider::Stripe,
                                subscription,
                                SubscriptionStatus::Active,
                                freq,
                            )
                            .await?;
                        }
                    }
                }
                match session["payment_status"].as_str() {
                    Some("paid") | Some("no_payment_required") => {
                        let reference = session["payment_intent"]
                            .as_str()
                            .or_else(|| session["subscription"].as_str())
                            .unwrap_or(session_id);
                        settle_order(&state, &orm, order_id, PaymentProvider::Stripe, reference, PaymentKind::Charge)
                            .await?;
                        Ok(PaymentStatus::Paid)
                    }
                    _ => {
                        // ACH debit still clearing.
                        db::set_order_payment_status(&orm, order_id, PaymentStatus::Processing).await?;
                        Ok(PaymentStatus::Processing)
                    }
                }
            }
            "paypal" => {
                let Some(cfg) = &state.config.paypal else {
                    return Ok(PaymentStatus::Pending);
                };
                if let Some(subscription_id) = params.subscription_id.as_deref() {
                    let token = paypal_token(&state, cfg).await?;
                    let subscription: Value = state
                        .http
                        .get(format!("{}/v1/billing/subscriptions/{subscription_id}", cfg.api_base))
                        .bearer_auth(&token)
                        .send()
                        .await?
                        .json()
                        .await?;
                    if matches!(subscription["status"].as_str(), Some("ACTIVE") | Some("APPROVED")) {
                        settle_order(&state, &orm, order_id, PaymentProvider::Paypal, subscription_id, PaymentKind::Charge)
                            .await?;
                        return Ok(PaymentStatus::Paid);
                    }
                    db::set_order_payment_status(&orm, order_id, PaymentStatus::Processing).await?;
                    return Ok(PaymentStatus::Processing);
                }
                let Some(paypal_order) = params.token.as_deref() else {
                    return Ok(PaymentStatus::Pending);
                };
                let token = paypal_token(&state, cfg).await?;
                let response = state
                    .http
                    .post(format!("{}/v2/checkout/orders/{paypal_order}/capture", cfg.api_base))
                    .bearer_auth(&token)
                    .header("content-type", "application/json")
                    .send()
                    .await?;
                let capture: Value = response.json().await?;
                if capture["status"].as_str() == Some("COMPLETED") {
                    let capture_id = capture["purchase_units"][0]["payments"]["captures"][0]["id"]
                        .as_str()
                        .unwrap_or(paypal_order);
                    settle_order(&state, &orm, order_id, PaymentProvider::Paypal, capture_id, PaymentKind::Charge)
                        .await?;
                    Ok(PaymentStatus::Paid)
                } else {
                    db::set_order_payment_status(&orm, order_id, PaymentStatus::Processing).await?;
                    Ok(PaymentStatus::Processing)
                }
            }
            "square" => {
                let Some(cfg) = &state.config.square else {
                    return Ok(PaymentStatus::Pending);
                };
                let Some(facts) = db::order_payment_facts(&orm, order_id).await? else {
                    return Ok(PaymentStatus::Pending);
                };
                let Some(square_order) = facts.payment_ref.as_deref() else {
                    return Ok(PaymentStatus::Pending);
                };
                let order: Value = state
                    .http
                    .get(format!("{}/v2/orders/{square_order}", cfg.api_base))
                    .bearer_auth(&cfg.access_token)
                    .send()
                    .await?
                    .json()
                    .await?;
                let tender_id = order["order"]["tenders"][0]["id"].as_str();
                if order["order"]["state"].as_str() == Some("COMPLETED") || tender_id.is_some() {
                    let reference = tender_id.unwrap_or(square_order);
                    settle_order(&state, &orm, order_id, PaymentProvider::Square, reference, PaymentKind::Charge)
                        .await?;
                    Ok(PaymentStatus::Paid)
                } else {
                    // The webhook will finish this once Square marks it paid.
                    db::set_order_payment_status(&orm, order_id, PaymentStatus::Processing).await?;
                    Ok(PaymentStatus::Processing)
                }
            }
            _ => Ok(PaymentStatus::Pending),
        }
    }
    .await;

    match outcome {
        Ok(PaymentStatus::Paid) => Redirect::to("/orders?paid=1").into_response(),
        Ok(PaymentStatus::Processing) => Redirect::to("/orders?processing=1").into_response(),
        Ok(_) => Redirect::to("/orders").into_response(),
        Err(err) => {
            tracing::error!(error = %err, %order_id, "payment return verification failed");
            Redirect::to("/orders?payerror=1").into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CancelParams {
    #[allow(dead_code)]
    order: Option<Uuid>,
}

/// GET /pay/cancel — the customer backed out of the hosted page. The order
/// stays `payment_status = pending` and /orders offers a "Pay now" retry.
pub async fn pay_cancel(Query(_params): Query<CancelParams>) -> Response {
    Redirect::to("/orders?paycancel=1").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pay_method_parses_form_values() {
        assert_eq!(PayMethod::parse("stripe"), Some(PayMethod::Stripe));
        assert_eq!(PayMethod::parse("paypal"), Some(PayMethod::Paypal));
        assert_eq!(PayMethod::parse("square"), Some(PayMethod::Square));
        assert_eq!(PayMethod::parse("invoice"), Some(PayMethod::Invoice));
        assert_eq!(PayMethod::parse("cash-under-the-door"), None);
    }

    #[test]
    fn frequency_maps_to_provider_intervals() {
        assert_eq!(stripe_interval(OrderFrequency::Biweekly), ("week", 2));
        assert_eq!(stripe_interval(OrderFrequency::Quarterly), ("month", 3));
        assert_eq!(paypal_interval(OrderFrequency::Weekly), ("WEEK", 1));
        assert_eq!(paypal_interval(OrderFrequency::Quarterly), ("MONTH", 3));
        assert_eq!(square_cadence(OrderFrequency::Biweekly), "EVERY_TWO_WEEKS");
        assert_eq!(square_cadence(OrderFrequency::Monthly), "MONTHLY");
    }

    #[test]
    fn dollars_formats_cents() {
        assert_eq!(dollars(0), "0.00");
        assert_eq!(dollars(5), "0.05");
        assert_eq!(dollars(599), "5.99");
        assert_eq!(dollars(123400), "1234.00");
    }

    #[test]
    fn stripe_signature_round_trips() {
        let secret = "whsec_test_secret";
        let body = br#"{"id":"evt_1","type":"checkout.session.completed"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(b"1700000000.");
        mac.update(body);
        let signature = hex::encode(mac.finalize().into_bytes());
        let header = format!("t=1700000000,v1={signature}");
        assert!(stripe_signature_valid(secret, &header, body));
        assert!(!stripe_signature_valid(secret, &header, b"tampered"));
        assert!(!stripe_signature_valid("whsec_other", &header, body));
    }

    #[test]
    fn square_signature_round_trips() {
        let key = "sq_sig_key";
        let url = "https://app.athleto.store/webhooks/square";
        let body = br#"{"event_id":"abc","type":"payment.updated"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).unwrap();
        mac.update(url.as_bytes());
        mac.update(body);
        let expected = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        assert!(square_signature_valid(key, url, &expected, body));
        assert!(!square_signature_valid(key, url, &expected, b"tampered"));
    }
}
