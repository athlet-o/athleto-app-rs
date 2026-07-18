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

use std::net::IpAddr;
use std::time::Duration;

use reqwest::{redirect::Policy, Url};
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
    match FiduciaClient::from_config(config) {
        Ok(Some(client)) => {
            let holder = format!("athleto:{}:{}", config.replica_id, job);
            let key = format!("athleto:cron:{job}");
            match client.acquire(&key, &holder, lease_secs * 1000).await {
                Ok(Some(fencing_token)) => {
                    return Some(Leadership::Fiducia {
                        client,
                        holder,
                        fencing_token,
                    });
                }
                // A competing replica has the lease. Do not fall back to a
                // different coordination plane or we could double-run jobs.
                Ok(None) => return None,
                Err(err) => {
                    tracing::warn!(error = %err, job, "fiducia lease acquisition failed; skipping tick");
                    return None;
                }
            }
        }
        // Only a fully unset Fiducia configuration may use the database-only
        // fallback. Partial or unsafe configuration is an operator error, not
        // permission to elect a second cross-cluster leader through Postgres.
        Ok(None) => {}
        Err(err) => {
            tracing::error!(error = %err, job, "invalid fiducia configuration; skipping singleton tick");
            return None;
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

/// Minimal async fiducia.cloud client for the protocol in
/// `fiducia-cloud/fiducia-clients`. The upstream Rust client is currently a
/// blocking, unreleased source checkout, so this keeps the app async while
/// matching the shared HTTP contract exactly. Used only for singleton-job
/// leadership and the encrypted config-KV overlay, never for cart holds.
#[derive(Clone)]
pub struct FiduciaClient {
    http: reqwest::Client,
    base: String,
    api_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FiduciaKvValue {
    pub value: String,
    pub at_rest: KvAtRest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvAtRest {
    Encrypted,
    Plaintext,
    /// Compatibility for an older Fiducia node that did not return protection
    /// metadata. Callers may accept a client-side envelope but not raw values.
    Unknown,
}

#[derive(Debug, thiserror::Error)]
pub enum FiduciaConfigError {
    #[error("FIDUCIA_URL and FIDUCIA_API_KEY must either both be set or both be unset")]
    PartialConfiguration,
    #[error("FIDUCIA_URL must be an absolute https URL or a trusted internal http URL")]
    UnsafeBaseUrl,
    #[error("FIDUCIA_API_KEY must not be empty")]
    EmptyApiKey,
    #[error("could not construct the fiducia HTTP client")]
    HttpClient,
}

#[derive(Debug, thiserror::Error)]
pub enum FiduciaRequestError {
    #[error("fiducia transport request failed")]
    Transport(#[source] reqwest::Error),
    #[error("fiducia returned HTTP {0}")]
    Rejected(u16),
    #[error("fiducia returned an invalid lock-acquire response: {0}")]
    InvalidGrant(&'static str),
}

impl FiduciaClient {
    pub fn new(base: String, api_key: String) -> Result<Self, FiduciaConfigError> {
        let base = normalize_base_url(&base)?;
        let api_key = api_key.trim().to_string();
        if api_key.is_empty() {
            return Err(FiduciaConfigError::EmptyApiKey);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .redirect(Policy::none())
            .build()
            .map_err(|_| FiduciaConfigError::HttpClient)?;
        Ok(Self {
            http,
            base,
            api_key,
        })
    }

    pub fn from_config(config: &Config) -> Result<Option<Self>, FiduciaConfigError> {
        Self::from_options(
            config.fiducia_url.as_deref(),
            config.fiducia_api_key.as_deref(),
        )
    }

    pub fn from_options(
        base: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<Option<Self>, FiduciaConfigError> {
        match (base, api_key) {
            (None, None) => Ok(None),
            (Some(base), Some(api_key)) => {
                Self::new(base.to_string(), api_key.to_string()).map(Some)
            }
            _ => Err(FiduciaConfigError::PartialConfiguration),
        }
    }

    /// Read one key from the fiducia config KV (`GET /v1/kv?key=K`); the org
    /// namespace comes from the API key on the fiducia side. Returns `None`
    /// for missing keys and for any transport/auth failure — callers treat
    /// both as "not configured". Used by `crate::secrets` at boot.
    pub async fn kv_get(&self, key: &str) -> Option<FiduciaKvValue> {
        let resp = self
            .http
            .get(format!("{}/v1/kv", self.base))
            .query(&[("key", key)])
            .bearer_auth(&self.api_key)
            .send()
            .await;
        match resp {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = match resp.json().await {
                    Ok(body) => body,
                    Err(err) => {
                        tracing::warn!(error = %err, key, "fiducia kv_get response was not JSON");
                        return None;
                    }
                };
                if body.get("found").and_then(|v| v.as_bool()) != Some(true) {
                    return None;
                }
                parse_kv_value(&body)
            }
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), key, "fiducia kv_get rejected");
                None
            }
            Err(err) => {
                tracing::warn!(error = %err, key, "fiducia kv_get unreachable");
                None
            }
        }
    }

    /// Acquire (non-blocking) a lease on `key` held by `holder` for `ttl_ms`.
    /// Returns the fencing token on success and `None` only when another holder
    /// already owns the lock. Transport and protocol failures stay distinct so
    /// callers can fail closed rather than mistake an outage for contention.
    pub async fn acquire(
        &self,
        key: &str,
        holder: &str,
        ttl_ms: u64,
    ) -> Result<Option<u64>, FiduciaRequestError> {
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
            .await
            .map_err(FiduciaRequestError::Transport)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FiduciaRequestError::Rejected(status.as_u16()));
        }
        let body = resp
            .json::<serde_json::Value>()
            .await
            .map_err(FiduciaRequestError::Transport)?;
        parse_lock_acquire(&body)
    }

    pub async fn release(&self, holder: &str, fencing_token: u64) {
        let result = self
            .http
            .post(format!("{}/v1/locks/release", self.base))
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({ "holder": holder, "fencing_token": fencing_token }))
            .send()
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(body)
                        if body.get("committed").and_then(|value| value.as_bool())
                            == Some(true) => {}
                    Ok(_) => tracing::warn!(
                        "fiducia release was not committed; lease will expire on TTL"
                    ),
                    Err(err) => {
                        tracing::warn!(error = %err, "fiducia release response was not JSON; lease will expire on TTL")
                    }
                }
            }
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), "fiducia release rejected; lease will expire on TTL")
            }
            Err(err) => {
                tracing::warn!(error = %err, "fiducia release failed; lease will expire on TTL")
            }
        }
    }
}

fn parse_kv_value(body: &serde_json::Value) -> Option<FiduciaKvValue> {
    if body.get("found").and_then(|value| value.as_bool()) != Some(true) {
        return None;
    }
    let value = body.pointer("/entry/value")?.as_str()?.to_string();
    let at_rest = match body
        .pointer("/protection/at_rest")
        .and_then(serde_json::Value::as_str)
    {
        Some("encrypted") => KvAtRest::Encrypted,
        Some("plaintext") => KvAtRest::Plaintext,
        _ => KvAtRest::Unknown,
    };
    Some(FiduciaKvValue { value, at_rest })
}

/// Normalize a coordination endpoint before attaching a bearer credential.
/// Public endpoints must use TLS; plaintext is allowed only for local or
/// cluster-internal addresses where the network is the explicit trust boundary.
fn normalize_base_url(raw: &str) -> Result<String, FiduciaConfigError> {
    let mut url = Url::parse(raw.trim()).map_err(|_| FiduciaConfigError::UnsafeBaseUrl)?;
    let host = url.host_str().ok_or(FiduciaConfigError::UnsafeBaseUrl)?;
    if !matches!(url.scheme(), "https" | "http")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
        || (url.scheme() == "http" && !cleartext_internal_host_allowed(host))
    {
        return Err(FiduciaConfigError::UnsafeBaseUrl);
    }
    url.set_path("");
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn cleartext_internal_host_allowed(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Ok(address) = host.parse::<IpAddr>() {
        return match address {
            IpAddr::V4(address) => {
                address.is_loopback() || address.is_private() || address.is_link_local()
            }
            IpAddr::V6(address) => {
                let first_segment = address.segments()[0];
                address.is_loopback()
                    || (first_segment & 0xfe00) == 0xfc00
                    || (first_segment & 0xffc0) == 0xfe80
            }
        };
    }
    !host.contains('.')
        || [
            ".svc",
            ".svc.cluster.local",
            ".cluster.local",
            ".internal",
            ".local",
        ]
        .iter()
        .any(|suffix| host.ends_with(suffix))
}

fn parse_lock_acquire(body: &serde_json::Value) -> Result<Option<u64>, FiduciaRequestError> {
    if body.get("committed").and_then(|value| value.as_bool()) != Some(true) {
        return Ok(None);
    }
    let output = body
        .pointer("/result/output")
        .ok_or(FiduciaRequestError::InvalidGrant("missing result.output"))?;
    match output.get("acquired").and_then(|value| value.as_bool()) {
        Some(false) => Ok(None),
        Some(true) => output
            .get("fencing_token")
            .and_then(|value| value.as_u64())
            .map(Some)
            .ok_or(FiduciaRequestError::InvalidGrant(
                "granted lease is missing fencing_token",
            )),
        None => Err(FiduciaRequestError::InvalidGrant(
            "result.output is missing acquired",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_key_is_stable_and_distinct_per_job() {
        assert_eq!(advisory_key("hold-sweeper"), advisory_key("hold-sweeper"));
        assert_ne!(
            advisory_key("hold-sweeper"),
            advisory_key("recurring-runner")
        );
    }

    #[test]
    fn fiducia_client_requires_both_url_and_key() {
        let mut config = Config::default();
        assert!(FiduciaClient::from_config(&config).unwrap().is_none());
        config.fiducia_url = Some("https://hetzner.lb.fiducia.cloud".into());
        assert!(matches!(
            FiduciaClient::from_config(&config),
            Err(FiduciaConfigError::PartialConfiguration)
        ));
        config.fiducia_api_key = Some("fdc_x.y".into());
        assert!(FiduciaClient::from_config(&config).unwrap().is_some());
    }

    #[test]
    fn fiducia_rejects_public_cleartext_but_allows_internal_cleartext() {
        assert!(matches!(
            FiduciaClient::from_options(Some("http://fiducia.cloud"), Some("fdc_x.y")),
            Err(FiduciaConfigError::UnsafeBaseUrl)
        ));
        assert!(FiduciaClient::from_options(
            Some("http://fiducia-node.default.svc.cluster.local:8090"),
            Some("fdc_x.y")
        )
        .unwrap()
        .is_some());
    }

    #[test]
    fn lock_acquire_requires_the_canonical_fenced_grant() {
        let granted = serde_json::json!({
            "committed": true,
            "result": { "output": { "acquired": true, "fencing_token": 42 } }
        });
        assert_eq!(parse_lock_acquire(&granted).unwrap(), Some(42));

        let contended = serde_json::json!({
            "committed": true,
            "result": { "output": { "acquired": false } }
        });
        assert_eq!(parse_lock_acquire(&contended).unwrap(), None);

        let unfenced = serde_json::json!({
            "committed": true,
            "result": { "output": { "acquired": true } }
        });
        assert!(matches!(
            parse_lock_acquire(&unfenced),
            Err(FiduciaRequestError::InvalidGrant(_))
        ));
    }

    #[test]
    fn kv_response_preserves_encrypted_plaintext_and_legacy_postures() {
        let encrypted = parse_kv_value(&serde_json::json!({
            "found": true,
            "entry": {"value": "secret"},
            "protection": {"at_rest": "encrypted", "provider": "vault_transit"}
        }))
        .unwrap();
        assert_eq!(encrypted.at_rest, KvAtRest::Encrypted);
        assert_eq!(encrypted.value, "secret");

        let plaintext = parse_kv_value(&serde_json::json!({
            "found": true,
            "entry": {"value": "public-config"},
            "protection": {"at_rest": "plaintext"}
        }))
        .unwrap();
        assert_eq!(plaintext.at_rest, KvAtRest::Plaintext);

        let legacy = parse_kv_value(&serde_json::json!({
            "found": true,
            "entry": {"value": "v1:client-envelope"}
        }))
        .unwrap();
        assert_eq!(legacy.at_rest, KvAtRest::Unknown);
        assert!(parse_kv_value(&serde_json::json!({"found": false})).is_none());
    }
}
