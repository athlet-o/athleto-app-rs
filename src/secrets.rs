//! Secrets sourcing: process env first, fiducia.cloud config KV as a
//! cross-provider overlay for anything the environment doesn't set.
//!
//! Precedence is strict and simple: **an explicit env var always wins**; the
//! overlay only fills gaps. That keeps every existing deployment story intact
//! (local `secrets.env`, cluster ESO -> env) while letting any machine on any
//! cloud provider bootstrap from just `FIDUCIA_URL` + `FIDUCIA_API_KEY`.
//!
//! Keys live in the fiducia KV under `secrets/athleto/<ENV_NAME>`, one KV key
//! per env var, org-scoped by the caller's API key on the fiducia side. The
//! fetch happens once at boot, per missing name, with a short timeout; any
//! failure degrades to "not configured" exactly like a missing env var —
//! fiducia being down must never stop the app from booting.
//!
//! Scope note (see docs/secrets-management.md): the fiducia KV is replicated
//! but not yet an encrypted secrets vault — production secrets-of-record stay
//! in AWS Secrets Manager (`dd/remote-dev/agent-secrets` -> ESO -> env, which
//! wins by precedence). This overlay is the client-side seam that a future
//! `/v1/secrets/*` (envelope-encrypted) fiducia API can slot into without the
//! app changing again.

use std::collections::HashMap;

use crate::coordinate::FiduciaClient;

/// Env vars the overlay may supply. Deliberately explicit: nothing outside
/// this list is ever read from the KV, so a compromised KV entry can't
/// redirect e.g. `PATH` or `LD_PRELOAD`.
pub const MANAGED_KEYS: &[&str] = &[
    "SUPABASE_URL",
    "SUPABASE_ANON_KEY",
    "DATABASE_URL",
    "ATHLETO_STRIPE_SECRET_KEY",
    "ATHLETO_STRIPE_PUBLISHABLE_KEY",
    "ATHLETO_STRIPE_WEBHOOK_SECRET",
    "ATHLETO_PAYPAL_CLIENT_ID",
    "ATHLETO_PAYPAL_CLIENT_SECRET",
    "ATHLETO_PAYPAL_WEBHOOK_ID",
    "ATHLETO_PAYPAL_ENV",
    "ATHLETO_SQUARE_ACCESS_TOKEN",
    "ATHLETO_SQUARE_LOCATION_ID",
    "ATHLETO_SQUARE_WEBHOOK_SIGNATURE_KEY",
    "ATHLETO_SQUARE_ENV",
    "ATHLETO_BILLING_URL",
    "ATHLETO_BILLING_API_KEY",
    "ATHLETO_BILLING_TENANT_ID",
];

/// KV key for a managed env var.
fn kv_key(name: &str) -> String {
    format!("secrets/athleto/{name}")
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Env-first configuration lookup with an optional fiducia-KV overlay behind
/// it. Built once at boot; `get` is then cheap and deterministic.
pub struct SecretSource {
    overlay: HashMap<String, String>,
}

impl SecretSource {
    /// No overlay: pure env passthrough.
    pub fn env_only() -> Self {
        Self { overlay: HashMap::new() }
    }

    #[cfg(test)]
    pub fn with_overlay(overlay: HashMap<String, String>) -> Self {
        Self { overlay }
    }

    /// Fetch every managed key that the environment does NOT already set from
    /// the fiducia config KV. Failures are logged and skipped.
    pub async fn load(client: Option<&FiduciaClient>) -> Self {
        let mut overlay = HashMap::new();
        let Some(client) = client else {
            return Self { overlay };
        };
        for name in MANAGED_KEYS {
            if env_opt(name).is_some() {
                continue; // env wins; don't even ask
            }
            if let Some(value) = client.kv_get(&kv_key(name)).await {
                let value = value.trim().to_string();
                if !value.is_empty() {
                    overlay.insert((*name).to_string(), value);
                }
            }
        }
        if !overlay.is_empty() {
            // Names only — never values.
            let mut names: Vec<&str> = overlay.keys().map(String::as_str).collect();
            names.sort_unstable();
            tracing::info!(keys = ?names, "config filled from fiducia KV overlay");
        }
        Self { overlay }
    }

    /// Env var if set (and non-empty), else the fiducia overlay, else `None`.
    pub fn get(&self, name: &str) -> Option<String> {
        env_opt(name).or_else(|| self.overlay.get(name).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_keys_are_namespaced_per_env_var() {
        assert_eq!(
            kv_key("ATHLETO_STRIPE_SECRET_KEY"),
            "secrets/athleto/ATHLETO_STRIPE_SECRET_KEY"
        );
    }

    #[test]
    fn overlay_fills_gaps_but_env_wins() {
        let mut map = HashMap::new();
        // A name that is never a real env var in tests.
        map.insert("ATHLETO_TEST_ONLY_SENTINEL".to_string(), "from-kv".to_string());
        // PATH is guaranteed set in any test environment.
        map.insert("PATH".to_string(), "kv-should-never-win".to_string());
        let source = SecretSource::with_overlay(map);

        assert_eq!(source.get("ATHLETO_TEST_ONLY_SENTINEL").as_deref(), Some("from-kv"));
        let path = source.get("PATH").expect("PATH is set");
        assert_ne!(path, "kv-should-never-win");
        assert!(source.get("ATHLETO_TOTALLY_UNSET").is_none());
    }

    #[test]
    fn managed_keys_cover_every_payment_and_billing_var() {
        for name in [
            "ATHLETO_STRIPE_SECRET_KEY",
            "ATHLETO_PAYPAL_CLIENT_SECRET",
            "ATHLETO_SQUARE_ACCESS_TOKEN",
            "ATHLETO_BILLING_TENANT_ID",
        ] {
            assert!(MANAGED_KEYS.contains(&name), "{name} missing from MANAGED_KEYS");
        }
        // And nothing dangerous snuck in.
        assert!(!MANAGED_KEYS.contains(&"PATH"));
        assert!(!MANAGED_KEYS.contains(&"FIDUCIA_API_KEY"));
    }
}
