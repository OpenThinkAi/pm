//! Versioned SQL migrations, embedded at build time and applied on start.
//! Same shape as pm-store's (`schema_version` table, one row per applied
//! version), but in Postgres: every pending migration runs in one
//! transaction under an advisory lock, so two replicas starting at once
//! cannot both apply the same version. A migration may be followed by a
//! Rust backfill in the same transaction (version 3 rebuilds the
//! materialized views from the log).

use tokio_postgres::Client;

/// Embedded migrations, in order.
const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_schema_v1.sql")),
    (2, include_str!("../migrations/0002_numbers.sql")),
    (3, include_str!("../migrations/0003_views.sql")),
];

/// The newest schema version this build understands.
pub const SCHEMA_VERSION: i32 = 3;

/// The version whose migration is followed by a backfill of the
/// materialized views from the log (`views::rebuild`, AGT-1392): a hub
/// upgraded over an existing log starts with views that match it.
const VIEWS_VERSION: i32 = 3;

/// Arbitrary, fixed key for `pg_advisory_xact_lock` (the ticket number).
const MIGRATION_LOCK_KEY: i64 = 1387;

#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    #[error(transparent)]
    Postgres(#[from] tokio_postgres::Error),
    #[error("database schema is version {found}, newer than this build supports ({supported})")]
    SchemaTooNew { found: i32, supported: i32 },
}

/// Brings the database up to [`SCHEMA_VERSION`] and returns it.
pub async fn migrate(client: &mut Client) -> Result<i32, MigrateError> {
    let tx = client.transaction().await?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK_KEY])
        .await?;
    tx.batch_execute(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version    integer     PRIMARY KEY,
            applied_at timestamptz NOT NULL DEFAULT now()
        )",
    )
    .await?;
    let current: i32 = tx
        .query_one("SELECT COALESCE(MAX(version), 0) FROM schema_version", &[])
        .await?
        .get(0);
    if current > SCHEMA_VERSION {
        return Err(MigrateError::SchemaTooNew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for (version, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
        tx.batch_execute(sql).await?;
        if *version == VIEWS_VERSION {
            crate::views::rebuild(&tx).await?;
        }
        tx.execute(
            "INSERT INTO schema_version (version) VALUES ($1)",
            &[version],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(SCHEMA_VERSION)
}

/// The schema version recorded in the database.
pub async fn schema_version(client: &Client) -> Result<i32, tokio_postgres::Error> {
    Ok(client
        .query_one("SELECT COALESCE(MAX(version), 0) FROM schema_version", &[])
        .await?
        .get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_ordered_and_end_at_the_supported_version() {
        let versions: Vec<i32> = MIGRATIONS.iter().map(|(v, _)| *v).collect();
        assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
        assert_eq!(versions.last(), Some(&SCHEMA_VERSION));
    }
}
