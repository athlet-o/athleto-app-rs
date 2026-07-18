//! Signed, short-lived browser state for an in-progress MFA challenge.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

const VERSION: &str = "v1";
const MAX_AGE_SECS: u64 = 5 * 60;
const MAX_CLOCK_SKEW_SECS: u64 = 30;
type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingChallenge {
    pub user_id: Uuid,
    pub factor_id: String,
    pub challenge_id: String,
    pub issued_at: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("invalid MFA challenge state")]
    Invalid,
    #[error("expired MFA challenge state")]
    Expired,
    #[error("MFA challenge belongs to another user")]
    WrongUser,
}

pub fn decode_key(raw: &str) -> Option<[u8; 32]> {
    base64::engine::general_purpose::STANDARD
        .decode(raw.trim().as_bytes())
        .ok()?
        .try_into()
        .ok()
}

pub fn new_challenge(user_id: Uuid, factor_id: String, challenge_id: String) -> PendingChallenge {
    PendingChallenge {
        user_id,
        factor_id,
        challenge_id,
        issued_at: now_secs(),
    }
}

pub fn seal(key: &[u8; 32], state: &PendingChallenge) -> Result<String, StateError> {
    if state.factor_id.is_empty()
        || state.factor_id.len() > 256
        || state.challenge_id.is_empty()
        || state.challenge_id.len() > 256
    {
        return Err(StateError::Invalid);
    }
    let payload = serde_json::to_vec(state).map_err(|_| StateError::Invalid)?;
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| StateError::Invalid)?;
    mac.update(&payload);
    let signature = mac.finalize().into_bytes();
    let encoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    Ok(format!(
        "{VERSION}.{}.{}",
        encoder.encode(payload),
        encoder.encode(signature)
    ))
}

pub fn open(key: &[u8; 32], value: &str, expected_user: Uuid) -> Result<PendingChallenge, StateError> {
    open_at(key, value, expected_user, now_secs())
}

fn open_at(
    key: &[u8; 32],
    value: &str,
    expected_user: Uuid,
    now: u64,
) -> Result<PendingChallenge, StateError> {
    let mut parts = value.split('.');
    let (Some(version), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(StateError::Invalid);
    };
    if version != VERSION {
        return Err(StateError::Invalid);
    }
    let decoder = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = decoder
        .decode(payload.as_bytes())
        .map_err(|_| StateError::Invalid)?;
    let signature = decoder
        .decode(signature.as_bytes())
        .map_err(|_| StateError::Invalid)?;
    let mut mac = HmacSha256::new_from_slice(key).map_err(|_| StateError::Invalid)?;
    mac.update(&payload);
    mac.verify_slice(&signature).map_err(|_| StateError::Invalid)?;
    let state = serde_json::from_slice::<PendingChallenge>(&payload).map_err(|_| StateError::Invalid)?;
    if state.user_id != expected_user {
        return Err(StateError::WrongUser);
    }
    if state.issued_at > now.saturating_add(MAX_CLOCK_SKEW_SECS)
        || now.saturating_sub(state.issued_at) > MAX_AGE_SECS
    {
        return Err(StateError::Expired);
    }
    Ok(state)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> [u8; 32] {
        [7; 32]
    }

    fn state() -> PendingChallenge {
        PendingChallenge {
            user_id: Uuid::nil(),
            factor_id: "factor-1".to_string(),
            challenge_id: "challenge-1".to_string(),
            issued_at: 1_000,
        }
    }

    #[test]
    fn challenge_state_round_trips_only_for_the_bound_user() {
        let sealed = seal(&key(), &state()).unwrap();
        let opened = open_at(&key(), &sealed, Uuid::nil(), 1_100).unwrap();
        assert_eq!(opened.factor_id, "factor-1");
        assert!(matches!(
            open_at(&key(), &sealed, Uuid::new_v4(), 1_100),
            Err(StateError::WrongUser)
        ));
    }

    #[test]
    fn challenge_state_rejects_tampering_and_expiry() {
        let sealed = seal(&key(), &state()).unwrap();
        let tampered = format!("{sealed}x");
        assert!(matches!(
            open_at(&key(), &tampered, Uuid::nil(), 1_100),
            Err(StateError::Invalid)
        ));
        assert!(matches!(
            open_at(&key(), &sealed, Uuid::nil(), 1_301),
            Err(StateError::Expired)
        ));
    }

    #[test]
    fn state_key_requires_exactly_32_decoded_bytes() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([9; 32]);
        assert_eq!(decode_key(&encoded), Some([9; 32]));
        assert_eq!(decode_key("not-base64"), None);
    }
}
