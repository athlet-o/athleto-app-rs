//! Vendored static assets, embedded in the binary so the app stays
//! self-contained (no CDN dependency, and the CSP can stay 'self'). The
//! filenames carry the upstream version so the immutable cache header is
//! safe: an upgrade changes the URL.

use axum::http::header;
use axum::response::IntoResponse;

/// htmx 2.0.4, dist/htmx.min.js from the htmx.org npm package.
const HTMX_JS: &str = include_str!("../static/htmx-2.0.4.min.js");
/// htmx websocket extension 2.0.2, ws.js from the htmx-ext-ws npm package.
const HTMX_WS_JS: &str = include_str!("../static/htmx-ext-ws-2.0.2.js");

pub const HTMX_JS_PATH: &str = "/static/htmx-2.0.4.min.js";
pub const HTMX_WS_JS_PATH: &str = "/static/htmx-ext-ws-2.0.2.js";

fn js_response(body: &'static str) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        body,
    )
}

/// GET /static/htmx-2.0.4.min.js
pub async fn htmx_js() -> impl IntoResponse {
    js_response(HTMX_JS)
}

/// GET /static/htmx-ext-ws-2.0.2.js
pub async fn htmx_ws_js() -> impl IntoResponse {
    js_response(HTMX_WS_JS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_htmx_looks_like_the_real_library() {
        assert!(HTMX_JS.contains("htmx"));
        assert!(HTMX_JS.len() > 10_000, "vendored htmx should not be a stub");
        assert!(HTMX_WS_JS.contains("WebSocket"));
    }
}
