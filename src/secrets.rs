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
//! **Confidentiality (see docs/secrets-management.md).** A 2026-07 security
//! audit confirmed the fiducia KV stores values in *cleartext* on every
//! node's disk (Raft log + snapshots) and over the plain-HTTP peer network —
//! it is not an encrypted vault. So this overlay never trusts the KV with a
//! usable plaintext secret: values are **client-side envelope-encrypted**
//! (AES-256-GCM, `v1:` prefix) and decrypted here with a key
//! (`ATHLETO_SECRETS_KEY`) that lives *outside* fiducia (AWS Secrets Manager
//! / secrets.env). A KV-disk or peer-network compromise therefore yields only
//! opaque ciphertext. Without that key the overlay is disabled (env-only), so
//! we can never accidentally read a plaintext secret out of the KV. AWS
//! Secrets Manager remains production secrets-of-record; this is a
//! cross-provider bootstrap convenience, not a replacement.
//!
//! When a real encrypted `/v1/secrets/*` fiducia API ships, the seam is
//! `FiduciaClient::kv_get` + `decrypt_envelope` — swap those two, nothing
//! else changes.

use std::collections::HashMap;

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use base64::Engine;

use crate::coordinate::FiduciaClient;

/// Env vars the overlay may supply. Deliberately explicit: nothing outside
/// this list is ever read from the KV, so a compromised KV entry can't
/// redirect e.g. `PATH` or `LD_PRELOAD`.
pub const MANAGED_KEYS: &[&str] = &[
    "SUPABASE_URL",
    "SUPABASE_ANON_KEY",
    "DATABASE_URL",
    "ATHLETO_OPERATIONS_API_KEY",
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

/// The 32-byte AES-256 key that unwraps KV envelopes, base64 in
/// `ATHLETO_SECRETS_KEY`. Sourced from env only (AWS SM / secrets.env) — the
/// root of trust must never live in fiducia, since fiducia is what we're
/// protecting the values from.
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
    Some(format!("v1:{}", base64::engine::general_purpose::STANDARD.encode(blob)))
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

    /// Fetch every managed key the environment does NOT set from the fiducia
    /// KV, decrypting each `v1:` envelope with `ATHLETO_SECRETS_KEY`. Disabled
    /// (env-only) when that key is absent, because the KV holds ciphertext we
    /// couldn't read — and must never hold usable plaintext. All failures
    /// degrade to "unset".
    pub async fn load(client: Option<&FiduciaClient>) -> Self {
        let mut overlay = HashMap::new();
        let Some(client) = client else {
            return Self { overlay };
        };
        let Some(key) = secrets_key() else {
            tracing::warn!(
                "fiducia KV secret overlay disabled: ATHLETO_SECRETS_KEY is unset. The fiducia \
                 KV stores values in cleartext, so secrets must be envelope-encrypted and this \
                 app refuses to read plaintext from it; using environment only. See \
                 docs/secrets-management.md"
            );
            return Self { overlay };
        };
        for name in MANAGED_KEYS {
            if env_opt(name).is_some() {
                continue; // env wins; don't even ask
            }
            let Some(value) = client.kv_get(&kv_key(name)).await else {
                continue;
            };
            match decrypt_envelope(&key, value.trim()) {
                Some(plaintext) if !plaintext.is_empty() => {
                    overlay.insert((*name).to_string(), plaintext);
                }
                _ => tracing::warn!(
                    key = *name,
                    "fiducia KV value is not a decryptable v1 envelope; ignoring (plaintext \
                     values are never accepted as secrets)"
                ),
            }
        }
        if !overlay.is_empty() {
            // Names only — never values.
            let mut names: Vec<&str> = overlay.keys().map(String::as_str).collect();
            names.sort_unstable();
            tracing::info!(keys = ?names, "config filled from fiducia KV overlay (decrypted)");
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
