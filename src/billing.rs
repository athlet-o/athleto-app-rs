//! Client for the Quaestor billing-server
//! (github.com/quaestor-ledger/billing-server.rs) — the multi-tenant AR/AP
//! observer ledger that tracks customer balances and credits.
//!
//! Posture matches the ledger's Model A: it records and reconciles, it never
//! moves money. The shop posts two double-entry transactions per settled
//! order — an invoice (debit `ar/<user>`, credit `revenue/athleto`) and a
//! payment (debit `cash/<provider>`, credit `ar/<user>`) — plus one payment
//! posting per provider-billed subscription cycle. Balances and credit memos
//! come back from `GET .../customers/by-email/{email}/billing-state`.
//!
//! Every write is fire-and-forget (`tokio::spawn` + warn on failure): the
//! ledger being down must never block a checkout. Idempotency keys make
//! retries and webhook replays safe on the ledger side.

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::db::{self, PaymentProvider};
use crate::SharedState;

#[derive(Clone)]
pub struct BillingConfig {
    /// e.g. `https://billing.dev.datadamn.com` (no trailing slash).
    pub url: String,
    /// Bearer for the ledger's API auth middleware; optional in dev.
    pub api_key: Option<String>,
    /// The AthletO tenant in the multi-tenant ledger.
    pub tenant_id: Uuid,
}

fn request(
    state: &SharedState,
    cfg: &BillingConfig,
    method: reqwest::Method,
    path: &str,
) -> reqwest::RequestBuilder {
    let mut builder = state.http.request(method, format!("{}{}", cfg.url, path));
    if let Some(key) = &cfg.api_key {
        builder = builder.bearer_auth(key);
    }
    builder
}

/// Ledger identity for a shop user: the ledger keys customers by email
/// within the tenant. Falls back to the latest login email.
async fn user_email(state: &SharedState, user_id: Uuid) -> Option<String> {
    let pool = state.pool.as_ref()?;
    db::latest_email_for_user(pool, user_id)
        .await
        .ok()
        .flatten()
}

async fn ensure_customer(
    state: &SharedState,
    cfg: &BillingConfig,
    user_id: Uuid,
    email: &str,
) -> anyhow::Result<()> {
    let response = request(
        state,
        cfg,
        reqwest::Method::POST,
        &format!("/v1/tenants/{}/users", cfg.tenant_id),
    )
    .json(&json!({
        "email": email,
        "is_customer": true,
        "external_refs": {"athleto_user_id": user_id},
    }))
    .send()
    .await?;
    if !response.status().is_success() {
        anyhow::bail!("ensure_customer: {}", response.status());
    }
    Ok(())
}

async fn ensure_account(
    state: &SharedState,
    cfg: &BillingConfig,
    kind: &str,
    code: &str,
    user_id: Option<Uuid>,
) -> anyhow::Result<()> {
    let response = request(
        state,
        cfg,
        reqwest::Method::POST,
        &format!("/v1/tenants/{}/accounts", cfg.tenant_id),
    )
    .json(&json!({
        "kind": kind,
        "code": code,
        "currency": "USD",
        "user_id": user_id,
    }))
    .send()
    .await?;
    if !response.status().is_success() {
        anyhow::bail!("ensure_account {code}: {}", response.status());
    }
    Ok(())
}

struct Posting<'a> {
    account_code: &'a str,
    direction: &'a str,
    amount_cents: i64,
}

/// The ledger's `DraftTransaction` wire shape, built pure so it can be
/// tested: every transaction must balance (debits == credits) per currency —
/// the ledger enforces it with a deferred trigger, we assert it in tests.
fn transaction_body(
    kind: &str,
    idempotency_key: &str,
    description: &str,
    source_event_id: &str,
    postings: &[Posting<'_>],
) -> serde_json::Value {
    let postings: Vec<_> = postings
        .iter()
        .map(|posting| {
            json!({
                "account_code": posting.account_code,
                "direction": posting.direction,
                "amount_minor": posting.amount_cents,
                "currency": "USD",
                "source": "athleto-app",
                "source_event_id": source_event_id,
            })
        })
        .collect();
    json!({
        "kind": kind,
        "idempotency_key": idempotency_key,
        "description": description,
        "postings": postings,
    })
}

async fn post_transaction(
    state: &SharedState,
    cfg: &BillingConfig,
    kind: &str,
    idempotency_key: &str,
    description: &str,
    source_event_id: &str,
    postings: &[Posting<'_>],
) -> anyhow::Result<()> {
    let response = request(
        state,
        cfg,
        reqwest::Method::POST,
        &format!("/v1/tenants/{}/transactions", cfg.tenant_id),
    )
    .json(&transaction_body(
        kind,
        idempotency_key,
        description,
        source_event_id,
        postings,
    ))
    .send()
    .await?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        // The ledger replays idempotency keys as 2xx/409 depending on
        // version; a conflict means "already recorded", which is fine.
        if status == reqwest::StatusCode::CONFLICT {
            return Ok(());
        }
        anyhow::bail!("post_transaction {kind}: {status}: {body}");
    }
    Ok(())
}

/// AR opens when an order becomes billable (paid checkout or Net-30 invoice
/// issued): debit the customer's receivable, credit revenue.
async fn order_invoice_tx(
    state: &SharedState,
    cfg: &BillingConfig,
    user_id: Uuid,
    order_id: Uuid,
    amount_cents: i64,
) -> anyhow::Result<()> {
    let Some(email) = user_email(state, user_id).await else {
        anyhow::bail!("no email on record for user {user_id}");
    };
    ensure_customer(state, cfg, user_id, &email).await?;
    let ar_code = format!("ar/{user_id}");
    ensure_account(state, cfg, "receivable", &ar_code, Some(user_id)).await?;
    ensure_account(state, cfg, "income", "revenue/athleto", None).await?;
    post_transaction(
        state,
        cfg,
        "invoice",
        &format!("athleto:order:{order_id}:invoice"),
        &format!("AthletO order {order_id}"),
        &order_id.to_string(),
        &[
            Posting {
                account_code: &ar_code,
                direction: "debit",
                amount_cents,
            },
            Posting {
                account_code: "revenue/athleto",
                direction: "credit",
                amount_cents,
            },
        ],
    )
    .await
}

/// Cash lands: debit the provider clearing account, credit the customer's
/// receivable.
async fn payment_tx(
    state: &SharedState,
    cfg: &BillingConfig,
    user_id: Uuid,
    provider: PaymentProvider,
    provider_ref: &str,
    amount_cents: i64,
    order_id: Option<Uuid>,
) -> anyhow::Result<()> {
    let Some(email) = user_email(state, user_id).await else {
        anyhow::bail!("no email on record for user {user_id}");
    };
    ensure_customer(state, cfg, user_id, &email).await?;
    let ar_code = format!("ar/{user_id}");
    let cash_code = format!("cash/{}", provider.as_str());
    ensure_account(state, cfg, "receivable", &ar_code, Some(user_id)).await?;
    ensure_account(state, cfg, "asset", &cash_code, None).await?;
    let description = match order_id {
        Some(order_id) => format!("AthletO payment for order {order_id}"),
        None => "AthletO subscription cycle payment".to_string(),
    };
    post_transaction(
        state,
        cfg,
        "payment",
        &format!("athleto:payment:{}:{provider_ref}", provider.as_str()),
        &description,
        provider_ref,
        &[
            Posting {
                account_code: &cash_code,
                direction: "debit",
                amount_cents,
            },
            Posting {
                account_code: &ar_code,
                direction: "credit",
                amount_cents,
            },
        ],
    )
    .await
}

// ---------------------------------------------------------------------------
// Fire-and-forget entry points used by the payment flow.

/// Post the AR invoice for an order (billable event). Safe to call more than
/// once — the idempotency key collapses replays.
pub fn spawn_order_invoice(state: &SharedState, user_id: Uuid, order_id: Uuid, amount_cents: i64) {
    let Some(cfg) = state.config.billing.clone() else {
        return;
    };
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = order_invoice_tx(&state, &cfg, user_id, order_id, amount_cents).await {
            tracing::warn!(error = %err, %order_id, "ledger invoice posting failed");
        }
    });
}

/// Post a settled payment against the customer's receivable.
pub fn spawn_payment(
    state: &SharedState,
    user_id: Uuid,
    order_id: Option<Uuid>,
    provider: PaymentProvider,
    provider_ref: &str,
    amount_cents: i64,
) {
    let Some(cfg) = state.config.billing.clone() else {
        return;
    };
    let state = state.clone();
    let provider_ref = provider_ref.to_string();
    tokio::spawn(async move {
        if let Err(err) = payment_tx(
            &state,
            &cfg,
            user_id,
            provider,
            &provider_ref,
            amount_cents,
            order_id,
        )
        .await
        {
            tracing::warn!(error = %err, "ledger payment posting failed");
        }
    });
}

/// A provider-billed subscription cycle: invoice + payment in one shot so
/// AR nets to zero per cycle.
pub fn spawn_subscription_cycle(
    state: &SharedState,
    user_id: Uuid,
    provider: PaymentProvider,
    provider_ref: &str,
    amount_cents: i64,
) {
    let Some(cfg) = state.config.billing.clone() else {
        return;
    };
    let state = state.clone();
    let provider_ref = provider_ref.to_string();
    tokio::spawn(async move {
        let invoice_key = format!("athleto:cycle:{}:{provider_ref}:invoice", provider.as_str());
        let result = async {
            let Some(email) = user_email(&state, user_id).await else {
                anyhow::bail!("no email on record for user {user_id}");
            };
            ensure_customer(&state, &cfg, user_id, &email).await?;
            let ar_code = format!("ar/{user_id}");
            ensure_account(&state, &cfg, "receivable", &ar_code, Some(user_id)).await?;
            ensure_account(&state, &cfg, "income", "revenue/athleto", None).await?;
            post_transaction(
                &state,
                &cfg,
                "invoice",
                &invoice_key,
                "AthletO subscription cycle",
                &provider_ref,
                &[
                    Posting {
                        account_code: &ar_code,
                        direction: "debit",
                        amount_cents,
                    },
                    Posting {
                        account_code: "revenue/athleto",
                        direction: "credit",
                        amount_cents,
                    },
                ],
            )
            .await?;
            payment_tx(
                &state,
                &cfg,
                user_id,
                provider,
                &provider_ref,
                amount_cents,
                None,
            )
            .await
        }
        .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "ledger subscription-cycle posting failed");
        }
    });
}

// ---------------------------------------------------------------------------
// Balance / credits read path (account page).

/// The slice of the ledger's billing-state the account page shows. Amounts
/// are minor units (cents).
#[derive(Debug, Clone, Deserialize)]
pub struct BillingSummary {
    pub outstanding_balance_minor: i64,
    pub credit_memos_minor: i64,
    pub unallocated_cash_minor: i64,
    #[serde(default)]
    pub last_payment: Option<LastPayment>,
    pub currency: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LastPayment {
    pub amount_minor: i64,
    pub via: String,
}

/// Best-effort read of the customer's balance and credits; `None` when the
/// ledger is unreachable, unconfigured, or the customer is unknown to it.
pub async fn billing_summary(state: &SharedState, email: &str) -> Option<BillingSummary> {
    let cfg = state.config.billing.as_ref()?;
    let response = request(
        state,
        cfg,
        reqwest::Method::GET,
        &format!(
            "/v1/tenants/{}/customers/by-email/{}/billing-state",
            cfg.tenant_id,
            urlencoding_component(email),
        ),
    )
    .send()
    .await
    .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// Minimal percent-encoding for a path segment (emails: `+` and `@` and
/// friends). Mirrors pages::urlencode_component but stays dependency-free
/// here to avoid a maud import in this module.
fn urlencoding_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_path_segment_is_percent_encoded() {
        assert_eq!(
            urlencoding_component("dev+athleto@example.com"),
            "dev%2Bathleto%40example.com"
        );
        assert_eq!(urlencoding_component("plain-user_99"), "plain-user_99");
    }

    /// Sum debits minus credits across a body's postings; the ledger rejects
    /// anything non-zero, so our builders must always produce zero.
    fn imbalance(body: &serde_json::Value) -> i64 {
        body["postings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|posting| {
                let amount = posting["amount_minor"].as_i64().unwrap();
                match posting["direction"].as_str().unwrap() {
                    "debit" => amount,
                    "credit" => -amount,
                    other => panic!("unexpected direction {other}"),
                }
            })
            .sum()
    }

    #[test]
    fn invoice_and_payment_transactions_balance_to_zero() {
        let user = Uuid::nil();
        let ar = format!("ar/{user}");
        let invoice = transaction_body(
            "invoice",
            "athleto:order:x:invoice",
            "AthletO order x",
            "x",
            &[
                Posting {
                    account_code: &ar,
                    direction: "debit",
                    amount_cents: 12345,
                },
                Posting {
                    account_code: "revenue/athleto",
                    direction: "credit",
                    amount_cents: 12345,
                },
            ],
        );
        assert_eq!(imbalance(&invoice), 0);
        assert_eq!(invoice["kind"], "invoice");
        assert_eq!(invoice["idempotency_key"], "athleto:order:x:invoice");

        let payment = transaction_body(
            "payment",
            "athleto:payment:stripe:pi_1",
            "AthletO payment",
            "pi_1",
            &[
                Posting {
                    account_code: "cash/stripe",
                    direction: "debit",
                    amount_cents: 12345,
                },
                Posting {
                    account_code: &ar,
                    direction: "credit",
                    amount_cents: 12345,
                },
            ],
        );
        assert_eq!(imbalance(&payment), 0);
        // Ledger contract: every posting names its source event for replay
        // tracing.
        for posting in payment["postings"].as_array().unwrap() {
            assert_eq!(posting["source"], "athleto-app");
            assert_eq!(posting["source_event_id"], "pi_1");
            assert_eq!(posting["currency"], "USD");
        }
    }

    #[test]
    fn billing_summary_deserializes_ledger_billing_state() {
        // A trimmed real-shape billing-state payload; unknown fields (aging,
        // snapshot_lock, ...) must be ignored, last_payment may be absent.
        let full: BillingSummary = serde_json::from_str(
            r#"{
                "user_id": "6dd8ec81-0000-0000-0000-000000000000",
                "email": "buyer@example.com",
                "currency": "USD",
                "outstanding_balance_minor": 4500,
                "credit_memos_minor": 1000,
                "unallocated_cash_minor": 250,
                "aging": {"current_minor": 4500, "d1_30_minor": 0},
                "last_payment": {"amount_minor": 8999, "via": "stripe", "external_id": "pi_9"}
            }"#,
        )
        .expect("full payload");
        assert_eq!(full.outstanding_balance_minor, 4500);
        assert_eq!(full.credit_memos_minor + full.unallocated_cash_minor, 1250);
        assert_eq!(
            full.last_payment.as_ref().map(|p| p.amount_minor),
            Some(8999)
        );

        let minimal: BillingSummary = serde_json::from_str(
            r#"{
                "currency": "USD",
                "outstanding_balance_minor": 0,
                "credit_memos_minor": 0,
                "unallocated_cash_minor": 0
            }"#,
        )
        .expect("minimal payload");
        assert!(minimal.last_payment.is_none());
    }
}
