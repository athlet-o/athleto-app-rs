//! Trusted-proxy handling for client-address security controls.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::{request::Parts, HeaderMap};
use ipnet::IpNet;

/// Direct TCP peer when the server was built with connection info. Unit and
/// router tests do not have a socket, so absence deliberately maps to the
/// conservative `unknown` bucket instead of rejecting the request.
#[derive(Clone, Copy)]
pub struct PeerAddress(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddress
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|peer| peer.0),
        ))
    }
}

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
    if !trusted_proxy_networks
        .iter()
        .any(|network| network.contains(&peer.ip()))
    {
        return peer.ip().to_string();
    }
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value
                .split(',')
                .rev()
                .filter_map(|candidate| candidate.trim().parse::<IpAddr>().ok())
                .find(|address| {
                    !trusted_proxy_networks
                        .iter()
                        .any(|network| network.contains(address))
                })
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
        assert_eq!(client_ip(&headers, Some(peer), &trusted), "198.51.100.7");
    }

    #[test]
    fn trusted_proxy_chain_skips_internal_hops() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, 10.0.0.6".parse().unwrap());
        let peer = "10.0.0.7:443".parse().unwrap();
        let trusted = ["10.0.0.0/8".parse().unwrap()];
        assert_eq!(client_ip(&headers, Some(peer), &trusted), "203.0.113.9");
    }

    #[test]
    fn missing_peer_does_not_trust_the_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        assert_eq!(client_ip(&headers, None, &[]), "unknown");
    }
}
