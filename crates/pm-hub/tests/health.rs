//! End-to-end: run the real `pm-hub` binary against a throwaway Postgres
//! and check that it migrates on start and serves `GET /health` (AGT-1387).
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
