//! End-to-end bearer-token auth (AGT-1388): `pm-hub token create|list|revoke`
//! against a throwaway Postgres, and the running hub answering every auth
//! failure exactly like an unknown route. See `common` for where Postgres
//! comes from.

mod common;

use common::*;

/// The whoami probe: `/w/<workspace>/whoami` with an optional bearer token.
fn whoami(port: u16, workspace: &str, token: Option<&str>) -> Response {
    let auth = token.map(|t| format!("Authorization: Bearer {t}"));
    let headers: Vec<&str> = auth.iter().map(String::as_str).collect();
    request(port, "GET", &format!("/w/{workspace}/whoami"), &headers)
}

/// Mints a token and returns `(plaintext, id)`.
fn create_token(url: &str, name: &str, workspace: &str) -> (String, String) {
    let (ok, stdout, stderr) = admin(url, &["token", "create", name, "--workspace", workspace]);
    assert!(ok, "token create failed: {stderr}");
    let token = stdout.trim().to_string();
    assert!(
        token.starts_with("pmh_") && !token.contains(char::is_whitespace),
        "{stdout:?}"
    );
    let id = stderr
        .lines()
        .find_map(|l| l.strip_prefix("token "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or_else(|| panic!("no token id in {stderr:?}"))
        .to_string();
    (token, id)
}

#[test]
fn tokens_create_use_revoke_and_every_failure_is_a_plain_404() {
    let Some((_container, url)) =
        postgres_for("tokens_create_use_revoke_and_every_failure_is_a_plain_404")
    else {
        return;
    };

    // Admin commands never migrate: on a fresh database they refuse.
    let (ok, _, stderr) = admin(&url, &["token", "list"]);
    assert!(!ok);
    assert!(stderr.contains("start the hub once"), "{stderr}");

    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    let (status, _) = wait_for_health(&mut hub, port);
    assert_eq!(status, 200);

    // Create: plaintext once on stdout, only its SHA-256 at rest.
    let (ok, stdout, stderr) = admin(
        &url,
        &["token", "create", "studio", "--workspace", "saltline"],
    );
    assert!(ok, "{stderr}");
    assert!(stderr.contains("created workspace saltline"), "{stderr}");
    let studio = stdout.trim().to_string();
    let studio_id = stderr
        .lines()
        .find_map(|l| l.strip_prefix("token "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string();
    let (other, _) = create_token(&url, "elsewhere", "other");
    let (laptop, _) = create_token(&url, "laptop", "saltline");
    let stored = query_rows(
        &url,
        &format!(
            "SELECT count(*) FROM tokens
             WHERE token_hash = sha256(convert_to('{studio}', 'UTF8'))
               AND workspace_id = 'saltline' AND label = 'studio'"
        ),
    )
    .unwrap();
    assert_eq!(stored[0][0].as_deref(), Some("1"));
    let dump = query_rows(&url, "SELECT t::text FROM tokens t").unwrap();
    for row in &dump {
        let row = row[0].as_deref().unwrap();
        for t in [&studio, &other, &laptop] {
            assert!(!row.contains(t.as_str()), "plaintext token stored: {row}");
        }
    }
    let (ok, _, stderr) = admin(&url, &["token", "create", "x", "--workspace", "Bad Id"]);
    assert!(!ok);
    assert!(stderr.contains("workspace id"), "{stderr}");
    let long = "n".repeat(129);
    let (ok, _, stderr) = admin(&url, &["token", "create", &long, "--workspace", "saltline"]);
    assert!(!ok);
    assert!(stderr.contains("token name must be"), "{stderr}");

    // Use.
    let ok_resp = whoami(port, "saltline", Some(&studio));
    assert_eq!(ok_resp.status, 200, "{ok_resp:?}");
    let json: serde_json::Value = serde_json::from_str(&ok_resp.body).unwrap();
    assert_eq!(json["workspace"], "saltline");
    assert_eq!(json["name"], "studio");
    assert!(json.get("token").is_none(), "{json}");
    assert_eq!(json["token_id"].to_string(), studio_id);
    let json: serde_json::Value =
        serde_json::from_str(&whoami(port, "other", Some(&other)).body).unwrap();
    assert_eq!(json["workspace"], "other");

    // Every failure is byte-identical to an unknown route.
    let unknown = request(port, "GET", "/no/such/route", &[]);
    assert_eq!(unknown.status, 404, "{unknown:?}");
    assert_eq!(request(port, "GET", "/w/saltline/nope", &[]), unknown);
    let forged = format!("pmh_{}", "A".repeat(43));
    let cases: Vec<(&str, Response)> = vec![
        ("missing header", whoami(port, "saltline", None)),
        ("unknown token", whoami(port, "saltline", Some(&forged))),
        ("garbage token", whoami(port, "saltline", Some("hunter2"))),
        (
            "wrong scheme",
            request(
                port,
                "GET",
                "/w/saltline/whoami",
                &[&format!("Authorization: Basic {studio}")],
            ),
        ),
        (
            "other workspace's token",
            whoami(port, "saltline", Some(&other)),
        ),
        (
            "token for a workspace that does not exist",
            whoami(port, "nosuch", Some(&studio)),
        ),
        (
            "wrong method",
            request(
                port,
                "POST",
                "/w/saltline/whoami",
                &[&format!("Authorization: Bearer {studio}")],
            ),
        ),
    ];
    for (case, resp) in cases {
        assert_eq!(resp, unknown, "{case}");
    }

    // /health stays open.
    assert_eq!(request(port, "GET", "/health", &[]).status, 200);

    // List shows metadata, never secrets.
    let (ok, stdout, stderr) = admin(&url, &["token", "list"]);
    assert!(ok, "{stderr}");
    assert_eq!(stdout.lines().count(), 4, "{stdout}");
    for t in [&studio, &other, &laptop] {
        assert!(!stdout.contains(t.as_str()));
    }
    let (_, stdout, _) = admin(&url, &["token", "list", "--workspace", "other"]);
    assert_eq!(stdout.lines().count(), 2, "{stdout}");
    assert!(stdout.contains("elsewhere"), "{stdout}");

    // Revoke → 404, immediately; other tokens unaffected.
    let (ok, _, stderr) = admin(&url, &["token", "revoke", &studio_id]);
    assert!(ok, "{stderr}");
    assert_eq!(whoami(port, "saltline", Some(&studio)), unknown);
    assert_eq!(whoami(port, "saltline", Some(&laptop)).status, 200);
    assert_eq!(whoami(port, "other", Some(&other)).status, 200);
    let (ok, _, stderr) = admin(&url, &["token", "revoke", &studio_id]);
    assert!(!ok);
    assert!(stderr.contains("already revoked"), "{stderr}");
    let (ok, _, stderr) = admin(&url, &["token", "revoke", "999999"]);
    assert!(!ok);
    assert!(stderr.contains("no token 999999"), "{stderr}");
    let (_, stdout, _) = admin(&url, &["token", "list", "--workspace", "saltline"]);
    let studio_row = stdout
        .lines()
        .find(|l| l.starts_with(&format!("{studio_id}\t")))
        .unwrap();
    assert!(
        !studio_row.ends_with("\t-"),
        "not marked revoked: {studio_row}"
    );

    // Revocation survives a restart (it is in the database, not memory).
    drop(hub);
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    assert_eq!(whoami(port, "saltline", Some(&studio)).status, 404);
    assert_eq!(whoami(port, "saltline", Some(&laptop)).status, 200);
}
