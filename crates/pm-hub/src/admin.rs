//! Admin subcommands (`pm-hub token ...`). They talk to the same database
//! as the server, found through `DATABASE_URL`: on Railway, run them inside
//! the hub service (`railway ssh` then `pm-hub token ...`), or with
//! `railway run pm-hub token ...` from a machine that can reach Postgres.
//!
//! They never migrate: a local binary newer than the deployed hub must not
//! move the schema ahead of it. They only run against a schema this build
//! understands, which the deployed hub creates on start.

use std::error::Error;

use tokio_postgres::Client;

use crate::auth;
use crate::migrate;

/// Workspace ids are short slugs (today: `saltline`).
fn valid_workspace_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

async fn check_schema(db: &Client) -> Result<(), Box<dyn Error>> {
    let version = migrate::schema_version(db).await.map_err(|e| {
        let why = e
            .as_db_error()
            .map_or_else(|| e.to_string(), |d| d.message().to_string());
        format!("cannot read the hub's schema version ({why}); start the hub once to migrate")
    })?;
    if version < 1 {
        return Err("the database is not migrated; start the hub once to migrate".into());
    }
    if version > migrate::SCHEMA_VERSION {
        return Err(format!(
            "database schema is version {version}, newer than this build supports ({}); \
             use a current pm-hub",
            migrate::SCHEMA_VERSION
        )
        .into());
    }
    Ok(())
}

/// `token create <name> --workspace <id>`: mints a token, stores its hash
/// and prints the plaintext once on stdout. Creates the workspace if this
/// is its first token (nothing else creates workspaces yet).
pub async fn token_create(
    db: &mut Client,
    name: &str,
    workspace: &str,
) -> Result<(), Box<dyn Error>> {
    if !valid_workspace_id(workspace) {
        return Err(format!(
            "workspace id {workspace:?} must be 1-64 characters of a-z, 0-9, '-' or '_'"
        )
        .into());
    }
    let name = name.trim();
    if name.is_empty() {
        return Err("token name must not be empty".into());
    }
    check_schema(db).await?;
    let token = auth::generate_token()?;
    let tx = db.transaction().await?;
    let created_workspace = tx
        .execute(
            "INSERT INTO workspaces (id) VALUES ($1) ON CONFLICT (id) DO NOTHING",
            &[&workspace],
        )
        .await?
        == 1;
    let id: i64 = tx
        .query_one(
            "INSERT INTO tokens (workspace_id, label, token_hash) VALUES ($1, $2, $3)
             RETURNING id",
            &[&workspace, &name, &auth::hash_token(&token)],
        )
        .await?
        .get(0);
    tx.commit().await?;
    if created_workspace {
        eprintln!("created workspace {workspace}");
    }
    eprintln!(
        "token {id} ({name}) for workspace {workspace}; it is shown once and cannot be recovered:"
    );
    println!("{token}");
    Ok(())
}

/// `token list [--workspace <id>]`: every token's metadata, never a secret.
pub async fn token_list(db: &Client, workspace: Option<&str>) -> Result<(), Box<dyn Error>> {
    check_schema(db).await?;
    let rows = db
        .query(
            "SELECT id, workspace_id, label,
                    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),
                    to_char(revoked_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
             FROM tokens
             WHERE $1::text IS NULL OR workspace_id = $1
             ORDER BY id",
            &[&workspace],
        )
        .await?;
    if rows.is_empty() {
        eprintln!("no tokens");
        return Ok(());
    }
    println!("ID\tWORKSPACE\tNAME\tCREATED\tREVOKED");
    for row in rows {
        let id: i64 = row.get(0);
        let workspace: String = row.get(1);
        let label: String = row.get(2);
        let created: String = row.get(3);
        let revoked: Option<String> = row.get(4);
        println!(
            "{id}\t{workspace}\t{label}\t{created}\t{}",
            revoked.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

/// `token revoke <id>`: the token stops authenticating immediately.
pub async fn token_revoke(db: &Client, id: i64) -> Result<(), Box<dyn Error>> {
    check_schema(db).await?;
    let revoked = db
        .execute(
            "UPDATE tokens SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL",
            &[&id],
        )
        .await?;
    if revoked == 1 {
        eprintln!("revoked token {id}");
        return Ok(());
    }
    let exists = db
        .query_opt("SELECT 1 FROM tokens WHERE id = $1", &[&id])
        .await?
        .is_some();
    if exists {
        Err(format!("token {id} is already revoked").into())
    } else {
        Err(format!("no token {id}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_ids() {
        for ok in ["saltline", "a", "ws-2", "w_s"] {
            assert!(valid_workspace_id(ok), "{ok}");
        }
        for bad in ["", "Saltline", "a b", "a/b", &"x".repeat(65)] {
            assert!(!valid_workspace_id(bad), "{bad}");
        }
    }
}
