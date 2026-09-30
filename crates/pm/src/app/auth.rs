//! The three request guards (`docs/app-api.md` §Access): loopback `Host`,
//! allow-listed `Origin` (with the CORS headers a browser needs for it),
//! and the bearer token. Runs as one middleware ahead of every route, so
//! no handler can forget it.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_MAX_AGE, AUTHORIZATION, HOST, ORIGIN, VARY, WWW_AUTHENTICATE,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::routes::error_response;

/// Is `host` (the `Host` header) this server: `127.0.0.1:<port>` or
/// `localhost:<port>`? Anything else is a request that reached the port
/// under a name we never announced — the DNS-rebinding shape — and is
/// refused before auth so the token is never compared for it.
fn host_is_self(host: Option<&HeaderValue>, port: u16) -> bool {
    let Some(host) = host.and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let host = host.trim().to_ascii_lowercase();
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}

/// The token in `Authorization: Bearer <token>`, if the header has that
/// shape.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Equal without an early exit on the first differing byte, so a wrong
/// token's timing does not narrow the search. Length is not hidden: every
/// real token has the same length.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The headers that let `origin` (already allow-listed) read a response
/// and send `Authorization`/`Content-Type` on a cross-origin request.
/// Never `*`, never credentials.
fn cors_headers(response: &mut Response, origin: &str) {
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(origin) {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    headers.insert(VARY, HeaderValue::from_static("Origin"));
    headers.insert(
        ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    headers.insert(
        ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
}

/// The guard every route runs behind.
pub(crate) async fn guard(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    if !host_is_self(headers.get(HOST), state.port) {
        return error_response(
            StatusCode::FORBIDDEN,
            "the Host header is not this server's loopback address",
        );
    }
    let origin = headers
        .get(ORIGIN)
        .and_then(|o| o.to_str().ok())
        .map(|o| o.trim().to_ascii_lowercase());
    if let Some(origin) = &origin
        && !state.allowed_origins.iter().any(|a| a == origin)
    {
        return error_response(
            StatusCode::FORBIDDEN,
            "this origin is not allowed (start pm app with --allow-origin)",
        );
    }
    // A preflight carries no Authorization by design; it is answered
    // here, for an allowed origin only, and never reaches a handler.
    if request.method() == Method::OPTIONS {
        let mut response = StatusCode::NO_CONTENT.into_response();
        if let Some(origin) = &origin {
            cors_headers(&mut response, origin);
        }
        return response;
    }
    match bearer(headers) {
        Some(token) if constant_time_eq(token, &state.token) => {}
        _ => {
            let mut response = error_response(
                StatusCode::UNAUTHORIZED,
                "missing or wrong bearer token (Authorization: Bearer <token>)",
            );
            response
                .headers_mut()
                .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            if let Some(origin) = &origin {
                cors_headers(&mut response, origin);
            }
            return response;
        }
    }
    let mut response = next.run(request).await;
    if let Some(origin) = &origin {
        cors_headers(&mut response, origin);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_must_be_loopback_with_the_right_port() {
        let ok = |h: &str| host_is_self(Some(&HeaderValue::from_str(h).unwrap()), 4321);
        assert!(ok("127.0.0.1:4321"));
        assert!(ok("LOCALHOST:4321"));
        assert!(!ok("127.0.0.1:4322"));
        assert!(!ok("127.0.0.1"));
        assert!(!ok("evil.example:4321"));
        assert!(!ok("[::1]:4321"));
        assert!(!host_is_self(None, 4321));
    }

    #[test]
    fn bearer_parses_only_the_bearer_shape() {
        let map = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(AUTHORIZATION, HeaderValue::from_str(v).unwrap());
            h
        };
        assert_eq!(bearer(&map("Bearer abc")), Some("abc"));
        assert_eq!(bearer(&map("bearer  abc ")), Some("abc"));
        assert_eq!(bearer(&map("Basic abc")), None);
        assert_eq!(bearer(&map("Bearer ")), None);
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn constant_time_eq_compares_whole_strings() {
        assert!(constant_time_eq("pma_ab", "pma_ab"));
        assert!(!constant_time_eq("pma_ab", "pma_ac"));
        assert!(!constant_time_eq("pma_ab", "pma_abc"));
    }
}
