//! Local-development and Fiducia-backed rate-limit checks.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::coordinate::FiduciaClient;
use crate::Config;

const FIDUCIA_TENANT: &str = "athleto";

#[derive(Default)]
pub struct LocalRateLimiter {
    entries: Mutex<HashMap<String, Vec<Instant>>>,
}

impl LocalRateLimiter {
    fn check(&self, key: &str, max: usize, window: Duration) -> bool {
        self.check_at(key, max, window, Instant::now())
    }

    fn check_at(&self, key: &str, max: usize, window: Duration, now: Instant) -> bool {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.retain(|_, attempts| {
            attempts.retain(|attempt| now.duration_since(*attempt) < window);
            !attempts.is_empty()
        });
        let attempts = entries.entry(key.to_string()).or_default();
        attempts.push(now);
        attempts.len() <= max
    }
}

pub struct RateLimits {
    local: LocalRateLimiter,
    remote: Option<FiduciaClient>,
    remote_required: bool,
}

impl RateLimits {
    pub fn from_config(config: &Config) -> Self {
        match FiduciaClient::from_config(config) {
            Ok(remote) => Self {
                local: LocalRateLimiter::default(),
                remote,
                remote_required: config.fiducia_url.is_some() || config.fiducia_api_key.is_some(),
            },
            Err(error) => {
                tracing::error!(error = %error, "invalid Fiducia configuration; distributed rate limits fail closed");
                Self {
                    local: LocalRateLimiter::default(),
                    remote: None,
                    remote_required: true,
                }
            }
        }
    }

    pub async fn check(&self, scope: &str, subject: &str, max: usize, window: Duration) -> bool {
        let key = bucket_key(scope, subject);
        if let Some(remote) = &self.remote {
            return match remote
                .rate_limit_check(FIDUCIA_TENANT, &key, max as u64, window.as_millis() as u64)
                .await
            {
                Ok(allowed) => allowed,
                Err(error) => {
                    tracing::warn!(error = %error, scope, "distributed rate limit failed closed");
                    false
                }
            };
        }
        if self.remote_required {
            return false;
        }
        self.local.check(&key, max, window)
    }
}

fn bucket_key(scope: &str, subject: &str) -> String {
    let digest = Sha256::digest(subject.as_bytes());
    format!("{scope}:{}", hex::encode(digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_limiter_blocks_and_recovers_without_panicking_on_poison() {
        let limiter = LocalRateLimiter::default();
        let window = Duration::from_secs(60);
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.check_at("login:1", 3, window, start));
        }
        assert!(!limiter.check_at("login:1", 3, window, start));
        assert!(limiter.check_at("login:1", 3, window, start + window * 2));
    }

    #[test]
    fn remote_bucket_keys_do_not_disclose_the_subject() {
        let key = bucket_key("login-email", "athlete@example.com");
        assert!(key.starts_with("login-email:"));
        assert!(!key.contains("athlete@example.com"));
        assert_eq!(key, bucket_key("login-email", "athlete@example.com"));
    }
}
