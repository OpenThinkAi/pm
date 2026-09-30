//! End-to-end: run the real `pm-hub` binary against a throwaway Postgres
//! and check that it migrates on start and serves `GET /health` (AGT-1387),
//! that `pm-hub healthcheck` probes it, and that a separate migration role
//! (`MIGRATION_DATABASE_URL`, AGT-1463) leaves the serving role DML-only.
//! See `common` for where Postgres comes from.

mod common;

use std::process::Command;

use common::*;

#[test]
fn migrates_on_start_and_serves_health() {
    let Some((_container, url)) = postgres_for("migrates_on_start_and_serves_health") else {
        return;
    };

    // First start applies every migration.
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    let (status, health) = wait_for_health(&mut hub, port);
    assert_eq!(status, 200, "{health}");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["schema_version"], expected_schema_version());
    assert_eq!(health["op_version"], pm_core::OP_VERSION);
    // "unknown" unless PM_HUB_BUILD_SHA was set when this test built the hub.
    assert!(
        health["build"].as_str().is_some_and(|b| !b.is_empty()),
        "{health}"
    );
    drop(hub);

    let tables = query_rows(
        &url,
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public' ORDER BY table_name",
    )
    .unwrap();
    let tables: Vec<&str> = tables.iter().filter_map(|r| r[0].as_deref()).collect();
    assert_eq!(
        tables,
        [
            "numbers",
            "ops",
            "schema_version",
            "ticket_views",
            "tokens",
            "workspace_views",
            "workspaces"
        ]
    );
    let seq = query_rows(
        &url,
        "SELECT data_type, column_default FROM information_schema.columns
         WHERE table_name = 'ops' AND column_name = 'seq'",
    )
    .unwrap();
    assert_eq!(seq[0][0].as_deref(), Some("bigint"));
    assert!(
        seq[0][1]
            .as_deref()
            .is_some_and(|d| d.starts_with("nextval(")),
        "ops.seq is not a bigserial: {seq:?}"
    );

    // A restart against the migrated database is a no-op migration.
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    let (status, health) = wait_for_health(&mut hub, port);
    assert_eq!(status, 200, "{health}");
    assert_eq!(health["schema_version"], expected_schema_version());
    let applied = query_rows(&url, "SELECT count(*) FROM schema_version").unwrap();
    assert_eq!(
        applied[0][0].as_deref(),
        Some(expected_schema_version().to_string().as_str())
    );
}

#[test]
fn refuses_to_start_without_database_url() {
    let out = Command::new(env!("CARGO_BIN_EXE_pm-hub"))
        .env_remove("DATABASE_URL")
        .output()
        .expect("running pm-hub");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("DATABASE_URL is not set"), "{stderr}");
}

fn healthcheck(port: u16) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_pm-hub"))
        .arg("healthcheck")
        .env("PORT", port.to_string())
        .env_remove("DATABASE_URL")
        .output()
        .expect("running pm-hub healthcheck");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn healthcheck_subcommand_probes_the_local_hub() {
    // Nothing listening: a failure, not a hang.
    let (ok, stderr) = healthcheck(free_port());
    assert!(!ok);
    assert!(stderr.contains("nothing listening"), "{stderr}");

    let Some((_container, url)) = postgres_for("healthcheck_subcommand_probes_the_local_hub")
    else {
        return;
    };
    let port = free_port();
    let mut hub = spawn_hub(&url, port);
    wait_for_health(&mut hub, port);
    let (ok, stderr) = healthcheck(port);
    assert!(ok, "{stderr}");
}

/// `url` with its user and password replaced.
fn as_role(url: &str, user: &str, password: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap();
    let (_, host) = rest.split_once('@').unwrap();
    format!("{scheme}://{user}:{password}@{host}")
}

#[test]
fn a_separate_migration_role_leaves_the_server_dml_only() {
    let Some((_container, url)) =
        postgres_for("a_separate_migration_role_leaves_the_server_dml_only")
    else {
        return;
    };
    // The least-privilege setup docs/hub-api.md §Deployment describes: the
    // owner (here the superuser) runs migrations; the serving role may
    // read and write rows but not change the schema.
    query_rows(
        &url,
        "CREATE ROLE pm_hub_app LOGIN PASSWORD 'app';
         GRANT CONNECT ON DATABASE postgres TO pm_hub_app;
         GRANT USAGE ON SCHEMA public TO pm_hub_app;
         REVOKE CREATE ON SCHEMA public FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES IN SCHEMA public
             GRANT SELECT, INSERT, UPDATE ON TABLES TO pm_hub_app;
         ALTER DEFAULT PRIVILEGES IN SCHEMA public
             GRANT USAGE, SELECT ON SEQUENCES TO pm_hub_app;",
    )
    .unwrap();
    let app = as_role(&url, "pm_hub_app", "app");

    // Serving role alone cannot migrate a fresh database: the hub exits.
    let port = free_port();
    let mut hub = spawn_hub(&app, port);
    let status = hub.0.wait().unwrap();
    assert!(!status.success(), "migrated without DDL rights");

    // With the migration role it migrates, then serves as the app role.
    let port = free_port();
    let mut hub = Hub(Command::new(env!("CARGO_BIN_EXE_pm-hub"))
        .env("DATABASE_URL", &app)
        .env("MIGRATION_DATABASE_URL", &url)
        .env("PORT", port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap());
    let (status, health) = wait_for_health(&mut hub, port);
    assert_eq!(status, 200, "{health}");
    assert_eq!(health["schema_version"], expected_schema_version());
    let owner = query_rows(
        &url,
        "SELECT DISTINCT tableowner FROM pg_tables WHERE schemaname = 'public'",
    )
    .unwrap();
    assert_eq!(owner, [[Some("postgres".to_string())]]);

    // The app role mints tokens and pushes (row writes and sequences) ...
    let (ok, stdout, stderr) = admin(
        &app,
        &[
            "token",
            "create",
            "studio",
            "--workspace",
            "saltline",
            "--any",
        ],
    );
    assert!(ok, "{stderr}");
    let token = stdout.trim();
    let op = serde_json::json!({"op_id": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        "hlc": {"wall_ms": 1, "counter": 0}, "actor": "matt",
        "entity": "01ARZ3NDEKTSV4RRFFQ69G5FAW", "kind": "hold.clear", "version": 1});
    let resp = request_body(
        port,
        "POST",
        "/w/saltline/ops",
        &[&format!("Authorization: Bearer {token}")],
        serde_json::json!({ "ops": [op] }).to_string().as_bytes(),
    );
    assert_eq!(resp.status, 200, "{resp:?}");
    // ... but cannot change the schema.
    let err = query_rows(&app, "CREATE TABLE sneaky (x int)").unwrap_err();
    assert!(
        err.as_db_error()
            .is_some_and(|e| e.message().contains("permission denied")),
        "{err}"
    );
    let err = query_rows(&app, "DROP TABLE ops").unwrap_err();
    assert!(
        err.as_db_error()
            .is_some_and(|e| e.message().contains("must be owner")),
        "{err}"
    );
}
