//! Coordination for the singleton background jobs (hold sweeper, recurring-
//! order runner). With multiple app replicas, exactly one should do the work
//! each tick.
//!
//! Two layers, composed:
//!
//! * **fiducia.cloud leases (cross-cluster leadership).** When `FIDUCIA_URL` +
//!   `FIDUCIA_API_KEY` are set, `run_singleton` takes a seconds-long, fenced,
//!   crash-safe lease so exactly one replica *across clusters* runs the tick.
//!   Leadership lives in fiducia, so none of the Postgres-pooler hazards below
//!   apply to it. This is the production path. The lease is heartbeated for as
//!   long as the job runs: a lease is a *deadline*, not a mutex, so a job that
//!   outlives its TTL without renewing would keep working while fiducia reaps
//!   the grant and promotes another replica. When renewal stops proving we hold
//!   the grant, the in-flight job is cancelled rather than allowed to finish
//!   unguarded.
//! * **Transaction-scoped Postgres advisory locks (in-database guard).** Each
//!   job body self-guards with `pg_try_advisory_xact_lock`, acquired and
//!   released inside a single transaction (`db::sweep_expired_holds`,
//!   `db::run_due_recurring_orders`). That is the ONLY advisory-lock shape
//!   that survives the Supabase transaction pooler: a *session*-scoped
//!   `pg_advisory_lock` can have its acquire and release routed to different
//!   pooled backends, which leaks the lock forever and silently wedges the
//!   job. So when no fiducia lease is configured, `run_singleton` runs the job
//!   directly and lets these in-transaction locks provide the mutual exclusion.
//!
//! The previous design held a *session*-scoped advisory lock across the whole
//! tick from this module. Through the transaction pooler that was unsafe on
//! both counts — it leaked locks and did not actually exclude — which is why
//! it is gone.
//!
//! What is deliberately NOT here: the 90-minute cart hold. A customer hold is
//! business data with an expiry (`stock_holds.held_until`), not mutual
//! exclusion. It must survive the claiming process dying, so it is a row --
//! never an advisory lock and never a fiducia lease (liveness-coupled, wrong
//! layer). See docs/cart-holds.md.

use std::net::IpAddr;
use std::time::Duration;

use reqwest::{redirect::Policy, Url};

use crate::Config;

/// Run `work` as the single leader for this tick, then return its output.
///
/// - **Fiducia configured:** hold a fenced lease across `work` (cross-cluster
///   mutual exclusion), releasing it afterwards. Returns `None` without
///   running `work` when another replica already holds the lease this tick, or
///   when acquisition fails (fail closed — a fiducia outage must not let every
///   replica run).
/// - **Fiducia absent:** run `work` directly and return `Some(output)`. Both
///   callers self-guard with a transaction-scoped advisory lock, which is
///   pooler-safe, so concurrent replicas converge to a single effective run
///   without a session-scoped lock.
///
/// `work` owns its own database handle (captured in the closure), so this
/// function never touches Postgres itself and cannot reintroduce a
/// pooler-spanning lock.
pub async fn run_singleton<F, Fut, T>(
    config: &Config,
    job: &str,
    lease_secs: u64,
    work: F,
) -> Option<T>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    match FiduciaClient::from_config(config) {
        Ok(Some(client)) => {
            let holder = format!("athleto:{}:{}", config.replica_id, job);
            let key = format!("athleto:cron:{job}");
            let ttl_ms = lease_secs * 1000;
            match client.acquire(&key, &holder, ttl_ms).await {
                Ok(Some(fencing_token)) => {
                    // Heartbeat the lease for as long as `work` runs. Without
                    // this, a job that outlives `lease_secs` keeps running while
                    // fiducia reaps the grant and promotes another replica --
                    // two leaders, silently. `lost` fires when we can no longer
                    // prove we hold the grant, and cancels `work` by dropping
                    // its future mid-await.
                    let (lost_tx, mut lost_rx) = tokio::sync::oneshot::channel();
                    let heartbeat = tokio::spawn(renew_until_lost(
                        client.clone(),
                        key.clone(),
                        holder.clone(),
                        fencing_token,
                        ttl_ms,
                        lost_tx,
                    ));
                    let output = tokio::select! {
                        biased;
                        _ = &mut lost_rx => {
                            tracing::error!(
                                job,
                                "lost the fiducia lease while the job was still running; \
                                 abandoning this tick"
                            );
                            None
                        }
                        output = work() => Some(output),
                    };
                    heartbeat.abort();
                    // Release promptly; on any failure the lease still expires
                    // on its TTL, so leadership can never wedge permanently.
                    client.release(&holder, fencing_token).await;
                    output
                }
                // A competing replica holds the lease. Do NOT fall back to the
                // in-database guard: that would let a second cross-cluster
                // leader run. Skip the tick.
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!(error = %err, job, "fiducia lease acquisition failed; skipping tick");
                    None
                }
            }
        }
        // No fiducia: run directly. The job's own transaction-scoped advisory
        // lock is the mutual-exclusion mechanism, and it is pooler-safe.
        Ok(None) => Some(work().await),
        // Partial or unsafe fiducia configuration is an operator error, not
        // permission to run unguarded across clusters. Skip.
        Err(err) => {
            tracing::error!(error = %err, job, "invalid fiducia configuration; skipping singleton tick");
            None
        }
    }
}

/// Renew `fencing_token` every third of its TTL until we can no longer prove we
/// hold it, then signal `lost`.
///
/// A renew that comes back `renewed: false` is definitive -- fiducia reaped or
/// reassigned the grant -- so it reports loss immediately. Transport errors are
/// not definitive (the lease may well still be ours), so they are retried, but
/// only until the lease we last confirmed would have expired: past that point we
/// cannot distinguish "fiducia is unreachable" from "another replica is already
/// leading", and the safe assumption is that we lost.
async fn renew_until_lost(
    client: FiduciaClient,
    key: String,
    holder: String,
    fencing_token: u64,
    ttl_ms: u64,
    lost: tokio::sync::oneshot::Sender<()>,
) {
    let period = Duration::from_millis((ttl_ms / 3).max(1_000));
    let lease = Duration::from_millis(ttl_ms);
    let mut confirmed = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(period).await;
        match client.renew(&key, &holder, fencing_token, ttl_ms).await {
            Ok(true) => confirmed = tokio::time::Instant::now(),
            Ok(false) => {
                tracing::warn!(key, "fiducia no longer recognises our lease");
                let _ = lost.send(());
                return;
            }
            Err(err) if confirmed.elapsed() < lease => {
                tracing::warn!(error = %err, key, "fiducia lease renewal failed; will retry");
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    key,
                    "fiducia lease renewal has failed for longer than the lease; \
                     assuming leadership is lost"
                );
                let _ = lost.send(());
                return;
            }
        }
    }
}

/// Minimal async fiducia.cloud client for the protocol in
/// `fiducia-cloud/fiducia-clients`. The upstream Rust client is currently a
/// blocking, unreleased source checkout, so this keeps the app async while
/// matching the shared HTTP contract exactly. Used for singleton-job
/// leadership, distributed abuse throttles, and the encrypted config-KV
/// overlay, never for cart holds.
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
    #[error("fiducia had no reachable leader after retrying")]
    NoLeader,
    #[error("fiducia returned an invalid lock-acquire response: {0}")]
    InvalidGrant(&'static str),
    #[error("fiducia returned an invalid rate-limit response: {0}")]
    InvalidRateLimit(&'static str),
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

    /// POST a mutating fiducia command, retrying a bounded number of times when
    /// the node answers `307 not_leader`.
    ///
    /// Fiducia returns 307 whenever the request reaches a follower -- routine
    /// during an election or a deliberate leadership transfer, and entirely
    /// retryable. Treating it as a hard rejection (as this client used to) makes
    /// every fail-closed caller deny traffic for the duration of an election:
    /// logins and cart adds would 429 and singleton ticks would be skipped, for
    /// a condition that resolves itself in milliseconds.
    ///
    /// We deliberately do NOT follow the `Location` header fiducia sends with
    /// the 307. This client attaches a bearer credential to every request, and
    /// following a server-named redirect target would forward that credential to
    /// whatever host the response points at. Retrying the *configured* base URL
    /// is safe and sufficient: athleto talks to the fiducia load balancer, which
    /// routes the retry to the new leader.
    async fn post_command(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, FiduciaRequestError> {
        const ATTEMPTS: u32 = 3;
        let url = format!("{}{path}", self.base);
        let mut backoff = Duration::from_millis(50);
        for attempt in 1..=ATTEMPTS {
            let response = self
                .http
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await
                .map_err(FiduciaRequestError::Transport)?;
            let status = response.status();
            if status.as_u16() == 307 {
                if attempt == ATTEMPTS {
                    return Err(FiduciaRequestError::NoLeader);
                }
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                continue;
            }
            if !status.is_success() {
                return Err(FiduciaRequestError::Rejected(status.as_u16()));
            }
            return response
                .json::<serde_json::Value>()
                .await
                .map_err(FiduciaRequestError::Transport);
        }
        Err(FiduciaRequestError::NoLeader)
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
        let body = self
            .post_command(
                "/v1/locks/acquire",
                &serde_json::json!({
                    "key": key,
                    "holder": holder,
                    "ttl_ms": ttl_ms,
                    "wait": false,
                }),
            )
            .await?;
        parse_lock_acquire(&body)
    }

    /// Extend an already-held lease (`POST /v1/locks/renew`) without minting a
    /// new fencing token. `Ok(true)` means the lease now runs for another
    /// `ttl_ms`; `Ok(false)` means fiducia no longer recognises this grant --
    /// it expired and was reaped, the holder does not match, or the key set
    /// differs -- so the caller has *lost leadership* and must stop working.
    ///
    /// The key set must match the grant exactly; fiducia canonicalizes it, so a
    /// renew must pass the same key that `acquire` did.
    pub async fn renew(
        &self,
        key: &str,
        holder: &str,
        fencing_token: u64,
        ttl_ms: u64,
    ) -> Result<bool, FiduciaRequestError> {
        let body = self
            .post_command(
                "/v1/locks/renew",
                &serde_json::json!({
                    "key": key,
                    "holder": holder,
                    "fencing_token": fencing_token,
                    "ttl_ms": ttl_ms,
                }),
            )
            .await?;
        parse_lock_renew(&body)
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
                // `committed: true` only means the command reached the log; a
                // release that found no matching grant is a committed no-op, so
                // the real outcome is `result.output.released`.
                match resp.json::<serde_json::Value>().await {
                    Ok(body)
                        if body
                            .pointer("/result/output/released")
                            .and_then(|value| value.as_bool())
                            == Some(true) => {}
                    Ok(body) => tracing::warn!(
                        reason = body
                            .pointer("/result/output/reason")
                            .and_then(|value| value.as_str())
                            .unwrap_or("unknown"),
                        "fiducia did not release the lease; it will expire on TTL"
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

    /// Atomically consume one token from Fiducia's canonical distributed rate
    /// limiter. Callers fail closed on errors so a backend outage cannot turn
    /// a protected endpoint into an unbounded one.
    pub async fn rate_limit_check(
        &self,
        tenant: &str,
        key: &str,
        limit: u64,
        window_ms: u64,
    ) -> Result<bool, FiduciaRequestError> {
        let tenant = path_segment(tenant);
        let key = path_segment(key);
        let body = self
            .post_command(
                &format!("/v1/rate-limit/{tenant}/{key}/check"),
                &serde_json::json!({
                    "algorithm": "sliding_window",
                    "limit": limit,
                    "window_ms": window_ms,
                }),
            )
            .await?;
        parse_rate_limit(&body)
    }
}

fn path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
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

/// Whether `host` is local/private/cluster-internal, i.e. a network where
/// cleartext is an acceptable trust boundary. Shared with `db::build_pool`,
/// which uses the same rule to decide when a Postgres connection may skip TLS
/// (dev/CI on localhost) versus when it must enforce it (a public host).
pub(crate) fn cleartext_internal_host_allowed(host: &str) -> bool {
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

/// `renewed: false` carries a closed set of reasons (`not_found`,
/// `not_holder`, `key_mismatch`); all of them mean the same thing to us -- this
/// process no longer holds the grant -- so they collapse to `Ok(false)`.
fn parse_lock_renew(body: &serde_json::Value) -> Result<bool, FiduciaRequestError> {
    if body.get("committed").and_then(|value| value.as_bool()) != Some(true) {
        return Err(FiduciaRequestError::InvalidGrant("renew was not committed"));
    }
    let output = body
        .pointer("/result/output")
        .ok_or(FiduciaRequestError::InvalidGrant("missing result.output"))?;
    output
        .get("renewed")
        .and_then(|value| value.as_bool())
        .ok_or(FiduciaRequestError::InvalidGrant(
            "result.output is missing renewed",
        ))
}

fn parse_rate_limit(body: &serde_json::Value) -> Result<bool, FiduciaRequestError> {
    if body.get("committed").and_then(|value| value.as_bool()) != Some(true) {
        return Err(FiduciaRequestError::InvalidRateLimit(
            "request was not committed",
        ));
    }
    let output = body
        .pointer("/result/output")
        .ok_or(FiduciaRequestError::InvalidRateLimit(
            "missing result.output",
        ))?;
    output
        .get("allowed")
        .and_then(|value| value.as_bool())
        .ok_or(FiduciaRequestError::InvalidRateLimit(
            "result.output is missing allowed",
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_singleton_runs_work_when_fiducia_is_unset() {
        // No fiducia config -> the job runs directly (its own transaction-
        // scoped advisory lock is the guard) and its output is returned.
        let config = Config::default();
        let ran = std::sync::atomic::AtomicBool::new(false);
        let out = run_singleton(&config, "hold-sweeper", 120, || async {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
            7u64
        })
        .await;
        assert_eq!(out, Some(7));
        assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn run_singleton_skips_work_on_invalid_fiducia_config() {
        // Partial fiducia config is an operator error: skip the tick, and do
        // NOT run the work unguarded.
        // fiducia_url set, api key intentionally unset -> PartialConfiguration
        let config = Config {
            fiducia_url: Some("https://hetzner.lb.fiducia.cloud".into()),
            ..Config::default()
        };
        let ran = std::sync::atomic::AtomicBool::new(false);
        let out: Option<()> = run_singleton(&config, "hold-sweeper", 120, || async {
            ran.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await;
        assert_eq!(out, None);
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "work must not run when fiducia config is invalid"
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
    fn renew_reports_loss_for_every_rejection_reason() {
        let renewed = serde_json::json!({
            "committed": true,
            "result": { "output": { "renewed": true, "lease_expires_ms": 1_737_000_000_000u64 } }
        });
        assert!(parse_lock_renew(&renewed).unwrap());

        // not_found / not_holder / key_mismatch all mean the same thing to us:
        // we can no longer prove we hold the grant, so we must stop working.
        for reason in ["not_found", "not_holder", "key_mismatch"] {
            let lost = serde_json::json!({
                "committed": true,
                "result": { "output": { "renewed": false, "reason": reason } }
            });
            assert!(
                !parse_lock_renew(&lost).unwrap(),
                "{reason} must report lost leadership"
            );
        }

        // A malformed renew is NOT permission to keep running.
        assert!(matches!(
            parse_lock_renew(&serde_json::json!({
                "committed": true,
                "result": { "output": {} }
            })),
            Err(FiduciaRequestError::InvalidGrant(_))
        ));
        assert!(matches!(
            parse_lock_renew(&serde_json::json!({ "committed": false })),
            Err(FiduciaRequestError::InvalidGrant(_))
        ));
    }

    #[test]
    fn rate_limit_requires_a_committed_canonical_response() {
        let allowed = serde_json::json!({
            "committed": true,
            "result": { "output": { "allowed": true, "remaining": 2 } }
        });
        assert!(parse_rate_limit(&allowed).unwrap());

        let denied = serde_json::json!({
            "committed": true,
            "result": { "output": { "allowed": false, "remaining": 0 } }
        });
        assert!(!parse_rate_limit(&denied).unwrap());

        assert!(matches!(
            parse_rate_limit(&serde_json::json!({ "committed": false })),
            Err(FiduciaRequestError::InvalidRateLimit(_))
        ));
    }

    #[test]
    fn path_segments_are_percent_encoded() {
        assert_eq!(path_segment("tenant/key value"), "tenant%2Fkey%20value");
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
