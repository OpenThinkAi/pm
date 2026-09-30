//! Admin subcommands (`pm-hub token create|list|bind|revoke`). They talk to the same database
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

/// Token names are labels like `studio` or `claude:pm-build`.
const MAX_TOKEN_NAME: usize = 128;

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
    if version < migrate::SCHEMA_VERSION {
        return Err(format!(
            "database schema is version {version}, older than this build expects ({}); \
             deploy (start) the current hub first so it migrates",
            migrate::SCHEMA_VERSION
        )
        .into());
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

/// `token create <name> --workspace <id> [--actor <pattern>...]`: mints a
/// token, stores its hash and prints the plaintext once on stdout. Creates
/// the workspace if this is its first token (nothing else creates
/// workspaces yet). With `--actor` the token may author ops only as the
/// matching actors (AGT-1450); without, it is recorded as `*` (any actor)
/// and a note says so — binding is opt-in, so existing mint scripts keep
/// working.
pub async fn token_create(
    db: &mut Client,
    name: &str,
    workspace: &str,
    actors: &[String],
) -> Result<(), Box<dyn Error>> {
    if !valid_workspace_id(workspace) {
        return Err(format!(
            "workspace id {workspace:?} must be 1-64 characters of a-z, 0-9, '-' or '_'"
        )
        .into());
    }
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_TOKEN_NAME {
        return Err(format!("token name must be 1-{MAX_TOKEN_NAME} characters").into());
    }
    let unbound = actors.is_empty();
    let actors = if unbound {
        vec!["*".to_string()]
    } else {
        auth::parse_actor_patterns(actors)?
    };
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
            "INSERT INTO tokens (workspace_id, label, token_hash, actors)
             VALUES ($1, $2, $3, $4)
             RETURNING id",
            &[&workspace, &name, &auth::hash_token(&token), &actors],
        )
        .await?
        .get(0);
    tx.commit().await?;
    if created_workspace {
        eprintln!("created workspace {workspace}");
    }
    if unbound {
        eprintln!(
            "note: token {id} may author ops as any actor; restrict it with \
             `pm-hub token bind {id} --actor <pattern>`"
        );
    } else {
        eprintln!("token {id} may author ops as: {}", actors.join(","));
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
            "SELECT id, workspace_id, label, actors,
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
    println!("ID\tWORKSPACE\tNAME\tACTORS\tCREATED\tREVOKED");
    for row in rows {
        let id: i64 = row.get(0);
        let workspace: String = row.get(1);
        let label: String = row.get(2);
        let actors = auth::ActorBinding::from_column(row.get(3)).describe();
        let created: String = row.get(4);
        let revoked: Option<String> = row.get(5);
        println!(
            "{id}\t{workspace}\t{label}\t{actors}\t{created}\t{}",
            revoked.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

/// `token bind <id> --actor <pattern>...`: replaces the actors a live
/// token may author ops as (AGT-1450). This is how a legacy token (minted
/// before bindings, unrestricted) is restricted after the fact; `--actor
/// '*'` makes a token explicitly unrestricted. Takes effect on the
/// token's next request.
pub async fn token_bind(db: &Client, id: i64, actors: &[String]) -> Result<(), Box<dyn Error>> {
    let actors = auth::parse_actor_patterns(actors)?;
    check_schema(db).await?;
    let bound = db
        .execute(
            "UPDATE tokens SET actors = $2 WHERE id = $1 AND revoked_at IS NULL",
            &[&id, &actors],
        )
        .await?;
    if bound == 1 {
        eprintln!("token {id} may author ops as: {}", actors.join(","));
        return Ok(());
    }
    let exists = db
        .query_opt("SELECT 1 FROM tokens WHERE id = $1", &[&id])
        .await?
        .is_some();
    if exists {
        Err(format!("token {id} is revoked").into())
    } else {
        Err(format!("no token {id}").into())
    }
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
