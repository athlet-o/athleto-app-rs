use axum::extract::State;
use axum::http::{header, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::SharedState;

fn browser_origin(request: &Request) -> Option<&str> {
    request.headers().get(header::ORIGIN)?.to_str().ok()
}

fn expected_origin(request: &Request, state: &SharedState) -> &str {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if host
        .split(':')
        .next()
        .is_some_and(|host| host.eq_ignore_ascii_case("biz.athleto.store"))
    {
        &state.config.biz_public_base_url
    } else {
        &state.config.public_base_url
    }
}

pub async fn require_same_origin(
    State(state): State<SharedState>,
    request: Request,
    next: Next,
) -> Response {
    if request.method() != Method::POST
        || request.uri().path().starts_with("/webhooks/")
        || request.uri().path().starts_with("/api/")
    {
        return next.run(request).await;
    }
    if browser_origin(&request) == Some(expected_origin(&request, &state)) {
        next.run(request).await
    } else {
        (StatusCode::FORBIDDEN, "cross-origin request rejected").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    #[test]
    fn browser_origin_reads_valid_origin_headers() {
        let request = Request::builder()
            .header(header::ORIGIN, "https://app.athleto.store")
            .body(())
            .unwrap();
        assert_eq!(browser_origin(&request), Some("https://app.athleto.store"));
    }
}
