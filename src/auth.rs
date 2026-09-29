//! API-key gate for HTTP mode: constant-time comparison, `X-API-Key` or `Authorization: Bearer`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use subtle::ConstantTimeEq;

/// Constant-time membership check; false when `keys` is empty.
pub fn key_matches(keys: &[String], presented: &str) -> bool {
    let presented = presented.as_bytes();
    keys.iter().fold(false, |acc, k| {
        let k = k.as_bytes();
        let eq = k.len() == presented.len() && bool::from(k.ct_eq(presented));
        acc | eq
    })
}

/// Reads `X-API-Key`, then `Authorization: Bearer <token>`.
pub fn presented_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(v) = headers.get("x-api-key") {
        return v.to_str().ok();
    }
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = auth.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then_some(token)
}

fn unauthorized() -> Response {
    let body = json!({
        "error": "unauthorized",
        "message": "missing or invalid API key; send it as `Authorization: Bearer <key>` or `X-API-Key`",
    });
    let mut resp = (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    resp.headers_mut()
        .insert("WWW-Authenticate", HeaderValue::from_static("Bearer"));
    resp
}

/// Middleware rejecting requests without a valid key. An empty key list rejects everything.
pub async fn require_api_key(
    State(keys): State<Arc<Vec<String>>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let valid = presented_key(req.headers()).is_some_and(|k| key_matches(&keys, k));
    if !valid {
        tracing::warn!(
            "rejected unauthenticated request: {} {}",
            req.method(),
            req.uri().path()
        );
        return unauthorized();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        assert!(!key_matches(&[], "x"));
        let keys = vec!["one".to_string(), "two".to_string()];
        assert!(key_matches(&keys, "two"));
        assert!(!key_matches(&keys, "tw"));
        assert!(!key_matches(&keys, "three"));
    }

    #[test]
    fn header_extraction() {
        let mut h = HeaderMap::new();
        assert_eq!(presented_key(&h), None);
        h.insert("authorization", HeaderValue::from_static("bearer k3"));
        assert_eq!(presented_key(&h), Some("k3"));
        h.insert("x-api-key", HeaderValue::from_static("k1"));
        assert_eq!(presented_key(&h), Some("k1"));
    }
}
