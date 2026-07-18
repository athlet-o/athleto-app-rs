//! Verification of the optional Turnstile proof required for self-signup.

use std::net::IpAddr;

use serde::Deserialize;

const TURNSTILE_VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
const MAX_TOKEN_LEN: usize = 4_096;

#[derive(Debug, Deserialize)]
struct TurnstileResponse {
    success: bool,
}

pub fn valid_turnstile_token(token: &str) -> bool {
    !token.trim().is_empty() && token.len() <= MAX_TOKEN_LEN
}

pub async fn verify_turnstile(
    http: &reqwest::Client,
    secret: &str,
    token: &str,
    remote_ip: Option<IpAddr>,
) -> Result<bool, &'static str> {
    if secret.trim().is_empty() || !valid_turnstile_token(token) {
        return Ok(false);
    }
    let remote_ip = remote_ip.map(|address| address.to_string()).unwrap_or_default();
    let response = http
        .post(TURNSTILE_VERIFY_URL)
        .form(&[
            ("secret", secret),
            ("response", token.trim()),
            ("remoteip", remote_ip.as_str()),
        ])
        .send()
        .await
        .map_err(|_| "verification service unavailable")?;
    if !response.status().is_success() {
        return Err("verification service rejected the request");
    }
    response
        .json::<TurnstileResponse>()
        .await
        .map(|body| body.success)
        .map_err(|_| "verification service returned an invalid response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turnstile_tokens_must_be_nonempty_and_bounded() {
        assert!(!valid_turnstile_token(""));
        assert!(!valid_turnstile_token("   "));
        assert!(valid_turnstile_token("token"));
        assert!(!valid_turnstile_token(&"x".repeat(MAX_TOKEN_LEN + 1)));
    }
}
