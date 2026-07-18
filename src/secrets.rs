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
//! **Confidentiality (see docs/secrets-management.md).** Fiducia now protects
//! KV values before they enter Raft using either a versioned local keyring or
//! cloud-neutral HashiCorp Vault Transit, and reports `protection.at_rest` on
//! reads. This overlay accepts those decrypted-at-the-API values, including an
//! explicitly plaintext entry when an operator intentionally chose that mode.
//! Legacy client-side AES-256-GCM `v1:` envelopes remain supported for staged
//! migration; they still require `ATHLETO_SECRETS_KEY` from an external source.
//! Raw values from an old node with no protection metadata are rejected.

use std::collections::HashMap;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use base64::Engine;

use crate::coordinate::{FiduciaClient, FiduciaKvValue, KvAtRest};

/// Env vars the overlay may supply. Deliberately explicit: nothing outside
/// this list is ever read from the KV, so a compromised KV entry can't
/// redirect e.g. `PATH` or `LD_PRELOAD`.
pub const MANAGED_KEYS: &[&str] = &[
    "SUPABASE_URL",
    "SUPABASE_ANON_KEY",
    "DATABASE_URL",
    "ATHLETO_OPERATIONS_API_KEY",
    "ATHLETO_TURNSTILE_SECRET",
    "ATHLETO_MFA_STATE_KEY",
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

/// The 32-byte AES-256 key that unwraps legacy client-side KV envelopes,
/// base64 in `ATHLETO_SECRETS_KEY`. Sourced from env only; new deployments can
/// instead let Fiducia's Vault Transit or local-keyring backend own encryption.
fn secrets_key() -> Option<[u8; 32]> {
    let raw = env_opt("ATHLETO_SECRETS_KEY")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(raw.as_bytes())
        .ok()?;
    bytes.try_into().ok()
}

/// Wrap plaintext into a `v1:` envelope: `v1:` + base64(nonce[12] ‖ ciphertext
/// ‖ tag), AES-256-GCM. The seal side of the overlay: operators (or a future
/// `athleto secrets put` subcommand) call this to produce the ciphertext they
/// PUT into `secrets/athleto/*`. Exercised by the round-trip tests.
#[allow(dead_code)] // publish-side helper / tooling seam; consumed by tests + ops
pub fn seal_envelope(key: &[u8; 32], plaintext: &str) -> Option<String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ciphertext = cipher.encrypt(&nonce, plaintext.as_bytes()).ok()?;
    let mut blob = nonce.to_vec();
    blob.extend_from_slice(&ciphertext);
    Some(format!(
        "v1:{}",
        base64::engine::general_purpose::STANDARD.encode(blob)
    ))
}

/// Unwrap a `v1:` envelope. Returns `None` on any format/auth failure — an
/// unparseable, unauthenticated, or plaintext value is treated as *absent*,
/// never accepted as a secret. This is what stops a plaintext (or tampered)
/// KV value from ever reaching the app as config.
fn decrypt_envelope(key: &[u8; 32], value: &str) -> Option<String> {
    let b64 = value.strip_prefix("v1:")?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .ok()?;
    if raw.len() < 12 + 16 {
        return None; // need at least a nonce and a GCM tag
    }
    let (nonce, ciphertext) = raw.split_at(12);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let plaintext = cipher.decrypt(Nonce::from_slice(nonce), ciphertext).ok()?;
    String::from_utf8(plaintext).ok()
}

fn decode_fiducia_value(key: Option<&[u8; 32]>, entry: &FiduciaKvValue) -> Option<String> {
    let value = entry.value.trim();
    if value.starts_with("v1:") {
        return key
            .and_then(|key| decrypt_envelope(key, value))
            .filter(|value| !value.is_empty());
    }
    match entry.at_rest {
        KvAtRest::Encrypted | KvAtRest::Plaintext if !value.is_empty() => Some(value.to_string()),
        KvAtRest::Encrypted | KvAtRest::Plaintext | KvAtRest::Unknown => None,
    }
}

/// Env-first configuration lookup with an optional fiducia-KV overlay behind
/// it. Built once at boot; `get` is then cheap and deterministic.
pub struct SecretSource {
    overlay: HashMap<String, String>,
}

impl SecretSource {
    /// No overlay: pure env passthrough.
    pub fn env_only() -> Self {
        Self {
            overlay: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub fn with_overlay(overlay: HashMap<String, String>) -> Self {
        Self { overlay }
    }

    /// Fetch every managed key the environment does NOT set from Fiducia KV.
    /// Node-protected encrypted and explicitly plaintext entries are accepted;
    /// legacy `v1:` envelopes require `ATHLETO_SECRETS_KEY`. All failures
    /// degrade to "unset" so the storefront keeps its zero-secret boot posture.
    pub async fn load(client: Option<&FiduciaClient>) -> Self {
        let mut overlay = HashMap::new();
        let Some(client) = client else {
            return Self { overlay };
        };
        let key = secrets_key();
        for name in MANAGED_KEYS {
            if env_opt(name).is_some() {
                continue; // env wins; don't even ask
            }
            let Some(value) = client.kv_get(&kv_key(name)).await else {
                continue;
            };
            match decode_fiducia_value(key.as_ref(), &value) {
                Some(plaintext) => {
                    overlay.insert((*name).to_string(), plaintext);
                }
                _ => tracing::warn!(
                    key = *name,
                    protection = ?value.at_rest,
                    "fiducia KV value lacks trusted protection metadata or a decryptable legacy \
                     envelope; ignoring"
                ),
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
        map.insert(
            "ATHLETO_TEST_ONLY_SENTINEL".to_string(),
            "from-kv".to_string(),
        );
        // PATH is guaranteed set in any test environment.
        map.insert("PATH".to_string(), "kv-should-never-win".to_string());
        let source = SecretSource::with_overlay(map);

        assert_eq!(
            source.get("ATHLETO_TEST_ONLY_SENTINEL").as_deref(),
            Some("from-kv")
        );
        let path = source.get("PATH").expect("PATH is set");
        assert_ne!(path, "kv-should-never-win");
        assert!(source.get("ATHLETO_TOTALLY_UNSET").is_none());
    }

    #[test]
    fn envelope_seals_and_unwraps_round_trip() {
        let key = [7u8; 32];
        let sealed = seal_envelope(&key, "sk_test_secret_value").expect("seal");
        assert!(sealed.starts_with("v1:"));
        // The ciphertext must not leak the plaintext.
        assert!(!sealed.contains("sk_test_secret_value"));
        assert_eq!(
            decrypt_envelope(&key, &sealed).as_deref(),
            Some("sk_test_secret_value")
        );
    }

    #[test]
    fn envelope_rejects_wrong_key_tamper_and_plaintext() {
        let key = [7u8; 32];
        let other = [9u8; 32];
        let sealed = seal_envelope(&key, "top-secret").unwrap();
        // Wrong key: GCM auth fails → None (treated as absent, not accepted).
        assert!(decrypt_envelope(&other, &sealed).is_none());
        // Plaintext value (no v1: envelope) is never accepted as a secret.
        assert!(decrypt_envelope(&key, "sk_live_plaintext").is_none());
        // Tampered ciphertext fails the GCM tag.
        let mut tampered = sealed.clone();
        tampered.push('A');
        assert!(decrypt_envelope(&key, &tampered).is_none());
        // Too-short blob (no room for nonce+tag) is rejected.
        let short = format!(
            "v1:{}",
            base64::engine::general_purpose::STANDARD.encode([0u8; 8])
        );
        assert!(decrypt_envelope(&key, &short).is_none());
    }

    #[test]
    fn fiducia_values_support_node_encryption_plaintext_and_legacy_envelopes() {
        let direct_encrypted = FiduciaKvValue {
            value: "node-decrypted-secret".into(),
            at_rest: KvAtRest::Encrypted,
        };
        assert_eq!(
            decode_fiducia_value(None, &direct_encrypted).as_deref(),
            Some("node-decrypted-secret")
        );

        let explicit_plaintext = FiduciaKvValue {
            value: "non-sensitive-config".into(),
            at_rest: KvAtRest::Plaintext,
        };
        assert_eq!(
            decode_fiducia_value(None, &explicit_plaintext).as_deref(),
            Some("non-sensitive-config")
        );

        let unknown_raw = FiduciaKvValue {
            value: "untrusted-legacy-raw".into(),
            at_rest: KvAtRest::Unknown,
        };
        assert!(decode_fiducia_value(None, &unknown_raw).is_none());

        let key = [7u8; 32];
        let legacy = FiduciaKvValue {
            value: seal_envelope(&key, "legacy-secret").unwrap(),
            at_rest: KvAtRest::Unknown,
        };
        assert!(decode_fiducia_value(None, &legacy).is_none());
        assert_eq!(
            decode_fiducia_value(Some(&key), &legacy).as_deref(),
            Some("legacy-secret")
        );
    }

    #[test]
    fn managed_keys_cover_every_payment_billing_and_auth_secret() {
        for name in [
            "ATHLETO_STRIPE_SECRET_KEY",
            "ATHLETO_PAYPAL_CLIENT_SECRET",
            "ATHLETO_SQUARE_ACCESS_TOKEN",
            "ATHLETO_BILLING_TENANT_ID",
            "ATHLETO_TURNSTILE_SECRET",
            "ATHLETO_MFA_STATE_KEY",
        ] {
            assert!(
                MANAGED_KEYS.contains(&name),
                "{name} missing from MANAGED_KEYS"
            );
        }
        // And nothing dangerous snuck in.
        assert!(!MANAGED_KEYS.contains(&"PATH"));
        assert!(!MANAGED_KEYS.contains(&"FIDUCIA_API_KEY"));
    }
}
