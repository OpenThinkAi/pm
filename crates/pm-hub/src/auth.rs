//! Bearer-token auth (README decision A7: one owner, one token per machine
//! or agent, hashed at rest).
//!
//! A token is `pmh_` followed by 32 random bytes from the OS CSPRNG,
//! base64url-encoded. The hub stores only its SHA-256: a 256-bit random
//! secret needs no salt or slow hash (there is nothing to brute-force), and
//! looking the digest up by its unique index means no secret-dependent
//! comparison ever happens in this process.
//!
//! Every route except `/health` sits behind [`require_auth`]. Any failure
//! (no `Authorization` header, a malformed one, an unknown or revoked
//! token, or a token minted for a different workspace) answers exactly what
//! an unknown route answers ([`not_found`]), so callers cannot tell a real
//! workspace or route from a missing one (think-hub precedent: 404, never
//! 401/403).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{FromRequestParts, Path, Request};
use axum::http::StatusCode;
use axum::http::header::{ALLOW, AUTHORIZATION};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use tokio_postgres::Client;

/// Every token starts with this, so a leaked one is recognisable in logs
/// and secret scanners.
pub const TOKEN_PREFIX: &str = "pmh_";
const TOKEN_BYTES: usize = 32;
/// `TOKEN_BYTES` in unpadded base64.
const TOKEN_BODY_LEN: usize = 43;

/// A fresh plaintext token. Shown once at mint time and never stored.
pub fn generate_token() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes)?;
    Ok(format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

/// The value stored in `tokens.token_hash`.
pub fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// The token in an `Authorization: Bearer <token>` header value, if the
/// value has that shape and the token looks like one we mint.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    let body = token.strip_prefix(TOKEN_PREFIX)?;
    let well_formed = body.len() == TOKEN_BODY_LEN
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    well_formed.then_some(token)
}

/// The one response for "nothing here": unknown routes, wrong methods and
/// every auth failure.
pub async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// Axum adds `Allow` to a wrong-method answer even when the fallback is
/// [`not_found`]; dropping it on every 404 keeps a wrong method on a real
/// route indistinguishable from a route that does not exist.
pub async fn strip_allow(mut resp: Response) -> Response {
    if resp.status() == StatusCode::NOT_FOUND {
        resp.headers_mut().remove(ALLOW);
    }
    resp
}

/// The caller of a `/w/{workspace}/...` route, authenticated by a live
/// token minted for that workspace.
#[derive(Clone, Debug)]
pub struct Authed {
    pub workspace: String,
    pub token_id: i64,
    pub token_label: String,
}

impl FromRequestParts<Arc<Client>> for Authed {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        db: &Arc<Client>,
    ) -> Result<Self, Self::Rejection> {
        // Already authenticated by `require_auth` on this request.
        if let Some(authed) = parts.extensions.get::<Authed>() {
            return Ok(authed.clone());
        }
        let Ok(Path(params)) = Path::<HashMap<String, String>>::from_request_parts(parts, db).await
        else {
            return Err(not_found().await);
        };
        let Some(workspace) = params.get("workspace") else {
            return Err(not_found().await);
        };
        let Some(token) = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(bearer_token)
        else {
            return Err(not_found().await);
        };
        // Workspace and token in one lookup, so "no such workspace" and
        // "wrong workspace for this token" are the same miss.
        let row = db
            .query_opt(
                "SELECT id, label FROM tokens
                 WHERE token_hash = $1 AND workspace_id = $2 AND revoked_at IS NULL",
                &[&hash_token(token), workspace],
            )
            .await;
        match row {
            Ok(Some(row)) => Ok(Authed {
                workspace: workspace.clone(),
                token_id: row.get(0),
                token_label: row.get(1),
            }),
            Ok(None) => Err(not_found().await),
            Err(e) => {
                eprintln!("pm-hub: auth lookup: {e}");
                Err(StatusCode::SERVICE_UNAVAILABLE.into_response())
            }
        }
    }
}

/// Route layer for every authenticated route: rejects with [`not_found`]
/// and otherwise stashes the [`Authed`] caller for the handler.
pub async fn require_auth(authed: Authed, mut req: Request, next: Next) -> Response {
    req.extensions_mut().insert(authed);
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_well_formed_and_distinct() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert_eq!(bearer_token(&format!("Bearer {t}")), Some(t.as_str()));
        }
    }

    #[test]
    fn hash_is_sha256_of_the_token() {
        // SHA-256("abc"), FIPS 180-2 test vector.
        assert_eq!(
            hash_token("abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad
            ]
        );
    }

    #[test]
    fn bearer_parsing() {
        let t = generate_token().unwrap();
        assert_eq!(bearer_token(&format!("bearer {t}")), Some(t.as_str()));
        assert_eq!(bearer_token(&format!("BEARER  {t} ")), Some(t.as_str()));
        assert_eq!(bearer_token(&format!("Basic {t}")), None);
        assert_eq!(bearer_token(&t), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Bearer pmh_short"), None);
        assert_eq!(
            bearer_token(&format!("Bearer {}", &t[TOKEN_PREFIX.len()..])),
            None
        );
        let bad = format!("Bearer {}!", &t[..t.len() - 1]);
        assert_eq!(bearer_token(&bad), None);
    }
}
