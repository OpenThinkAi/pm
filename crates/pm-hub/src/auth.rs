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

use axum::extract::{FromRef, FromRequestParts, Path, Request};
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
    /// Which actors this token may author ops as (AGT-1450).
    pub actors: ActorBinding,
}

/// Most patterns one token may be bound to.
pub const MAX_ACTOR_PATTERNS: usize = 32;
/// Longest actor pattern, in characters (actor ids are short handles).
pub const MAX_ACTOR_PATTERN: usize = 128;

/// The actors a token may author ops as (AGT-1450, oaudit 2026-09-30),
/// from `tokens.actors`. README decision A7 mints one token per machine,
/// and one machine pushes for several actors (the owner, `pm-sync`, its
/// `claude:*` agents), so a binding is a set of patterns, not one actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActorBinding {
    /// `tokens.actors IS NULL`: minted before bindings existed. Any actor,
    /// exactly as before, until `pm-hub token bind` restricts it.
    Legacy,
    /// Actor patterns: `matt` (exactly that actor), `claude:*` (any actor
    /// starting `claude:`), or `*` (any actor).
    Patterns(Vec<String>),
}

impl ActorBinding {
    pub fn from_column(actors: Option<Vec<String>>) -> Self {
        match actors {
            None => ActorBinding::Legacy,
            Some(patterns) => ActorBinding::Patterns(patterns),
        }
    }

    /// Whether any actor at all is allowed (a legacy token, or `*`).
    pub fn unrestricted(&self) -> bool {
        match self {
            ActorBinding::Legacy => true,
            ActorBinding::Patterns(p) => p.iter().any(|p| p == "*"),
        }
    }

    /// Whether this binding lets the token author an op as `actor`.
    pub fn permits(&self, actor: &str) -> bool {
        match self {
            ActorBinding::Legacy => true,
            ActorBinding::Patterns(patterns) => {
                patterns.iter().any(|p| match p.strip_suffix('*') {
                    Some(prefix) => actor.starts_with(prefix),
                    None => actor == p,
                })
            }
        }
    }

    /// How `token list` and error messages show it.
    pub fn describe(&self) -> String {
        match self {
            ActorBinding::Legacy => "any (legacy, unbound)".to_string(),
            ActorBinding::Patterns(p) => p.join(","),
        }
    }
}

/// Checks and normalises `--actor` patterns for `token create` / `token
/// bind`: each is `*`, an actor id, or an actor-id prefix followed by one
/// trailing `*`; no whitespace or control characters. Comma-separated
/// values are split, and duplicates dropped (first occurrence kept).
pub fn parse_actor_patterns(values: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for pattern in values.iter().flat_map(|v| v.split(',')).map(str::trim) {
        if pattern.is_empty() {
            return Err("an actor pattern is empty".to_string());
        }
        if pattern.chars().count() > MAX_ACTOR_PATTERN {
            return Err(format!(
                "actor pattern {pattern:?} is longer than {MAX_ACTOR_PATTERN} characters"
            ));
        }
        if pattern.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!(
                "actor pattern {pattern:?} contains whitespace or a control character"
            ));
        }
        if pattern.trim_end_matches('*').contains('*') || pattern.ends_with("**") {
            return Err(format!(
                "actor pattern {pattern:?}: '*' may only end a pattern (e.g. claude:*), or be the whole pattern"
            ));
        }
        if !out.iter().any(|p| p == pattern) {
            out.push(pattern.to_string());
        }
    }
    if out.is_empty() {
        return Err("give at least one --actor pattern".to_string());
    }
    if out.len() > MAX_ACTOR_PATTERNS {
        return Err(format!(
            "{} actor patterns; the limit is {MAX_ACTOR_PATTERNS}",
            out.len()
        ));
    }
    Ok(out)
}

/// Extractable from any state that lends the reader connection (the
/// auth layer's `Arc<Client>` and the server's `Db`).
impl<S> FromRequestParts<S> for Authed
where
    S: Send + Sync,
    Arc<Client>: FromRef<S>,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        // Already authenticated by `require_auth` on this request (every
        // routed handler). The lookup below runs for the layer itself, and
        // would also guard a handler wired outside the layer by mistake.
        if let Some(authed) = parts.extensions.get::<Authed>() {
            return Ok(authed.clone());
        }
        let Ok(Path(params)) =
            Path::<HashMap<String, String>>::from_request_parts(parts, state).await
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
        let row = Arc::<Client>::from_ref(state)
            .query_opt(
                "SELECT id, label, actors FROM tokens
                 WHERE token_hash = $1 AND workspace_id = $2 AND revoked_at IS NULL",
                &[&hash_token(token), workspace],
            )
            .await;
        match row {
            Ok(Some(row)) => Ok(Authed {
                workspace: workspace.clone(),
                token_id: row.get(0),
                token_label: row.get(1),
                actors: ActorBinding::from_column(row.get(2)),
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

    fn patterns(p: &[&str]) -> ActorBinding {
        ActorBinding::Patterns(p.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn bindings_match_exact_actors_prefixes_and_everything() {
        let legacy = ActorBinding::Legacy;
        assert!(legacy.unrestricted());
        assert!(legacy.permits("matt") && legacy.permits("hub"));
        assert_eq!(legacy.describe(), "any (legacy, unbound)");

        let studio = patterns(&["matt", "pm-sync", "claude:*"]);
        assert!(!studio.unrestricted());
        for ok in ["matt", "pm-sync", "claude:pm-build", "claude:"] {
            assert!(studio.permits(ok), "{ok}");
        }
        for no in ["mat", "matt2", "pm-sync:x", "claude", "Claude:x", "hub", ""] {
            assert!(!studio.permits(no), "{no}");
        }
        assert_eq!(studio.describe(), "matt,pm-sync,claude:*");

        let any = patterns(&["*"]);
        assert!(any.unrestricted());
        assert!(any.permits("anyone"));
        assert!(!patterns(&[]).permits("matt"));
    }

    #[test]
    fn actor_patterns_are_checked_and_normalised() {
        let ok =
            |v: &[&str]| parse_actor_patterns(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            ok(&["matt,claude:*", " pm-sync ", "matt"]).unwrap(),
            ["matt", "claude:*", "pm-sync"]
        );
        assert_eq!(ok(&["*"]).unwrap(), ["*"]);
        for bad in [
            &[][..],
            &[""],
            &["a,,b"],
            &["cl*ude"],
            &["*x"],
            &["x**"],
            &["a b"],
            &["a\tb"],
        ] {
            assert!(ok(bad).is_err(), "{bad:?}");
        }
        assert!(ok(&[&"x".repeat(MAX_ACTOR_PATTERN + 1)]).is_err());
        let many: Vec<String> = (0..=MAX_ACTOR_PATTERNS).map(|i| format!("a{i}")).collect();
        assert!(parse_actor_patterns(&many).is_err());
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
