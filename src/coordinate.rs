//! Coordination for the singleton background jobs (hold sweeper, recurring-
//! order runner). With multiple app replicas, exactly one must run each tick.
//!
//! Three tools, each for the job it is actually good at:
//!
//! * **Postgres transactions** own the money-and-stock invariants
//!   (`db::place_order`, `db::ensure_hold`): a short `FOR UPDATE` row lock plus
//!   an atomic commit. That is the default and it never spans a network round
//!   trip to another service.
//! * **Postgres advisory locks** give cross-replica mutual exclusion for a
//!   *short critical section* without a table: `pg_try_advisory_lock` picks the
//!   one replica that runs a periodic job this tick, then unlocks. This is the
//!   built-in, dependency-free leader mechanism. (Session-scoped, so it runs on
//!   the SeaORM connection exactly like every other query.)
//! * **fiducia.cloud leases** are the same idea at the infrastructure layer:
//!   a seconds-long, fenced, crash-safe lease, useful when leadership must be
//!   coordinated across clusters (not just replicas in one Postgres). When
//!   `FIDUCIA_URL` + `FIDUCIA_API_KEY` are set we take a lease there; otherwise
//!   we fall back to the advisory lock.
//!
//! What is deliberately NOT here: the 90-minute cart hold. A customer hold is
//! business data with an expiry (`stock_holds.held_until`), not mutual
//! exclusion. It must survive the claiming process dying, so it is a row --
//! never an advisory lock (session-scoped, breaks through poolers) and never a
//! fiducia lease (liveness-coupled, wrong layer). See docs/cart-holds.md.

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, Value};

use crate::Config;

fn stmt<I>(sql: &str, values: I) -> Statement
where
    I: IntoIterator<Item = Value>,
{
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// Stable 64-bit key for a named advisory lock. FNV-1a over the job name keeps
/// it deterministic across replicas and releases.
fn advisory_key(name: &str) -> i64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash as i64
}

/// A held coordination grant. Dropping it is not enough -- call `release`
/// (async) so the advisory lock or fiducia lease is returned promptly.
pub enum Leadership {
    /// Won via `pg_try_advisory_lock`; released with `pg_advisory_unlock`.
    Advisory { conn: DatabaseConnection, key: i64 },
    /// Won via a fiducia lease; released with POST /v1/locks/release.
    Fiducia {
        client: FiduciaClient,
        holder: String,
        fencing_token: u64,
    },
}

impl Leadership {
    pub async fn release(self) {
        match self {
            Leadership::Advisory { conn, key } => {
                if let Err(err) = conn
                    .execute(stmt("SELECT pg_advisory_unlock($1)", [key.into()]))
                    .await
                {
                    tracing::warn!(error = %err, "pg_advisory_unlock failed");
                }
            }
            Leadership::Fiducia {
                client,
                holder,
                fencing_token,
            } => client.release(&holder, fencing_token).await,
        }
    }
}

/// Try to become the single runner for `job` this tick. Returns `Some` if we
/// hold leadership (run the job, then `release`), `None` if another replica
/// holds it (skip this tick).
pub async fn try_lead(
    conn: &DatabaseConnection,
    config: &Config,
    job: &str,
    lease_secs: u64,
) -> Option<Leadership> {
    if let Some(client) = FiduciaClient::from_config(config) {
        let holder = format!("athleto:{}:{}", config.replica_id, job);
        let key = format!("athleto:cron:{job}");
        match client.acquire(&key, &holder, lease_secs * 1000).await {
            Some(fencing_token) => {
                return Some(Leadership::Fiducia {
                    client,
                    holder,
                    fencing_token,
                })
            }
            // Fiducia is configured but someone else holds it (or it is
            // unreachable): skip this tick rather than double-run.
            None => return None,
        }
    }

    let key = advisory_key(job);
    let row = conn
        .query_one(stmt("SELECT pg_try_advisory_lock($1) AS got", [key.into()]))
        .await;
    match row {
        Ok(Some(row)) => match row.try_get::<bool>("", "got") {
            Ok(true) => Some(Leadership::Advisory {
                conn: conn.clone(),
                key,
            }),
            Ok(false) => None, // another replica holds it
            Err(err) => {
                tracing::warn!(error = %err, "advisory lock result unreadable; skipping tick");
                None
            }
        },
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(error = %err, "pg_try_advisory_lock failed; skipping tick");
            None
        }
    }
}

/// Minimal fiducia.cloud lock/lease client. The lock API is
/// `POST /v1/locks/acquire` and `POST /v1/locks/release`, authed with a
/// `Bearer` API key; the edge injects the org identity. Used only for the
/// seconds-long leadership lease, never for cart holds.
#[derive(Clone)]
pub struct FiduciaClient {
    http: reqwest::Client,
    base: String,
    api_key: String,
}

impl FiduciaClient {
    pub fn from_config(config: &Config) -> Option<Self> {
        let base = config.fiducia_url.clone()?;
        let api_key = config.fiducia_api_key.clone()?;
        Some(Self {
            http: reqwest::Client::new(),
            base: base.trim_end_matches('/').to_string(),
            api_key,
        })
    }

    /// Acquire (non-blocking) a lease on `key` held by `holder` for `ttl_ms`.
    /// Returns the fencing token on success, `None` if held or unreachable.
    pub async fn acquire(&self, key: &str, holder: &str, ttl_ms: u64) -> Option<u64> {
        let resp = self
            .http
            .post(format!("{}/v1/locks/acquire", self.base))
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({
                "key": key,
                "holder": holder,
                "ttl_ms": ttl_ms,
                "wait": false,
            }))
            .send()
            .await;
        match resp {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.ok()?;
                if body.get("acquired").and_then(|v| v.as_bool()) == Some(true) {
                    body.get("fencing_token").and_then(|v| v.as_u64())
                } else {
                    None // queued/blocked -> someone else leads
                }
            }
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), "fiducia acquire rejected");
                None
            }
            Err(err) => {
                tracing::warn!(error = %err, "fiducia acquire unreachable");
                None
            }
        }
    }

    pub async fn release(&self, holder: &str, fencing_token: u64) {
        let result = self
            .http
            .post(format!("{}/v1/locks/release", self.base))
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({ "holder": holder, "fencing_token": fencing_token }))
            .send()
            .await;
        if let Err(err) = result {
            tracing::warn!(error = %err, "fiducia release failed; lease will expire on TTL");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_key_is_stable_and_distinct_per_job() {
        assert_eq!(advisory_key("hold-sweeper"), advisory_key("hold-sweeper"));
        assert_ne!(advisory_key("hold-sweeper"), advisory_key("recurring-runner"));
    }

    #[test]
    fn fiducia_client_requires_both_url_and_key() {
        let mut config = Config::default();
        assert!(FiduciaClient::from_config(&config).is_none());
        config.fiducia_url = Some("https://hetzner.lb.fiducia.cloud".into());
        assert!(FiduciaClient::from_config(&config).is_none());
        config.fiducia_api_key = Some("fdc_x.y".into());
        assert!(FiduciaClient::from_config(&config).is_some());
    }
}
