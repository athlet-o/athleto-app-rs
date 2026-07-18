//! Trusted-proxy handling for client-address security controls.

use std::net::{IpAddr, SocketAddr};

use axum::http::HeaderMap;
use ipnet::IpNet;

/// Return a client address only from a configured immediate proxy. Direct
/// callers cannot steer a rate-limit bucket with a forged forwarding header.
pub fn client_ip(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxy_networks: &[IpNet],
) -> String {
    let Some(peer) = peer else {
        return "unknown".to_string();
    };
    if !trusted_proxy_networks.iter().any(|network| network.contains(&peer.ip())) {
        return peer.ip().to_string();
    }
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .split(',')
                .rev()
                .find_map(|candidate| candidate.trim().parse::<IpAddr>().ok())
        })
        .unwrap_or_else(|| peer.ip())
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_peer_ignores_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        let peer = "198.51.100.10:443".parse().unwrap();
        assert_eq!(client_ip(&headers, Some(peer), &[]), "198.51.100.10");
    }

    #[test]
    fn trusted_proxy_uses_rightmost_valid_forwarded_hop() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "forged, 203.0.113.9, 198.51.100.7".parse().unwrap(),
        );
        let peer = "10.0.0.7:443".parse().unwrap();
        let trusted = ["10.0.0.0/8".parse().unwrap()];
        assert_eq!(
            client_ip(&headers, Some(peer), &trusted),
            "198.51.100.7"
        );
    }

    #[test]
    fn missing_peer_does_not_trust_the_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        assert_eq!(client_ip(&headers, None, &[]), "unknown");
    }
}
