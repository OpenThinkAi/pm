//! `pm hub login|status|logout` (AGT-1394): where this machine keeps the
//! hub URL and the bearer token for one workspace (projects/pm/README.md
//! §Sync & hub).
//!
//! - The **URL** is `hub = "<url>"` in config.toml (not a secret). Login and
//!   logout edit only that key; the rest of the file is preserved, though
//!   comments are not (the file is re-serialized from its TOML table).
//! - The **token** never touches config.toml. On macOS it lives in the login
//!   keychain under service `<prefix>.<workspace-id>` and account `pm`, where
//!   `<prefix>` is `pm-hub` (override with `PM_HUB_KEYCHAIN_SERVICE_PREFIX`,
//!   which tests use to keep to throwaway items) and `<workspace-id>` is the
//!   workspace's ULID from its database — the same id the hub keys a workspace
//!   by (`/w/{workspace}/…`), stable across prefix renames, distinct for two
//!   workspaces on one machine. Elsewhere there is no keychain: the token is
//!   only ever read from the `PM_HUB_TOKEN` environment variable.
//! - `PM_HUB_KEYCHAIN=<file>` makes every keychain call use that keychain
//!   file instead of the default one (tests use a throwaway keychain).
//! - `PM_HUB_TOKEN`, when set, wins over the keychain on every platform.
//! - The keychain is driven through `security -i`, which reads its commands
//!   from stdin. `security add-generic-password -w <secret>` would put the
//!   secret on argv (visible in `ps`), and the argv-free `-w` form prompts on
//!   /dev/tty rather than reading stdin, so stdin of `security -i` is the one
//!   scripted route that keeps the secret off argv. Tokens are therefore
//!   restricted to `[A-Za-z0-9_-]`, the hub's own token alphabet, so no
//!   quoting is needed.
//! - Only `pm hub status` (`GET /health`, and `GET /w/<id>/whoami` when a
//!   token is present) and `pm sync` (`crate::sync`, AGT-1395) talk to the
//!   hub. Login and logout are local, and every other verb — the reads
//!   above all — never constructs an HTTP client.
//! - The id in `/w/{workspace}/…` is [`hub_workspace_id`]: the workspace
//!   ULID in lowercase. The hub's workspace-id alphabet (`pm-hub token
//!   create --workspace`) is `[a-z0-9_-]`, and a ULID's canonical spelling
//!   is uppercase, which the hub refuses; lowercase is the same ULID
//!   (Crockford base32 parses case-insensitively). The keychain service
//!   name keeps the canonical uppercase form.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Context;
use pm_core::Workspace;
use serde_json::{Value, json};

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA, print_json};
use crate::workspace::Env;

/// The workspace's id on the hub (`/w/{workspace}/…`, `pm-hub token create
/// --workspace`): its ULID, lowercased (module docs).
pub(crate) fn hub_workspace_id(ws: &Workspace) -> String {
    ws.id.to_string().to_ascii_lowercase()
}

#[derive(clap::Subcommand, Debug)]
pub enum HubCmd {
    /// Store the hub URL in config.toml and read the token (stdin, or PM_HUB_TOKEN) into the keychain
    Login {
        /// The hub's base URL, e.g. https://hub.example (http:// or https://, no credentials)
        url: String,
    },
    /// Show the hub URL, workspace, token presence (never the value) and hub reachability; exit 1 if the hub is unreachable or rejects the token
    Status,
    /// Remove the stored token and the `hub` key from config.toml
    Logout,
}

pub fn run(ctx: &Ctx<'_>, cmd: HubCmd) -> Result<()> {
    match cmd {
        HubCmd::Login { url } => login(ctx, &url),
        HubCmd::Status => status(ctx),
        HubCmd::Logout => logout(ctx),
    }
}

// ------------------------------------------------------------- keychain

const ACCOUNT: &str = "pm";
const DEFAULT_SERVICE_PREFIX: &str = "pm-hub";

fn service_name(env: &Env, ws: &Workspace) -> String {
    let prefix = env
        .hub_keychain_prefix
        .as_deref()
        .unwrap_or(DEFAULT_SERVICE_PREFIX);
    format!("{prefix}.{}", ws.id)
}

/// A service name goes between double quotes in a `security -i` command.
fn check_service_quotable(service: &str) -> Result<()> {
    if service
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        Ok(())
    } else {
        Err(CliError::usage(
            "PM_HUB_KEYCHAIN_SERVICE_PREFIX may only contain letters, digits, '.', '-' and '_'",
        ))
    }
}

/// `Ok(true)` when an item was stored, `Ok(false)` when there is no
/// keychain on this platform.
fn keychain_store(env: &Env, service: &str, token: &str) -> Result<bool> {
    if !cfg!(target_os = "macos") {
        return Ok(false);
    }
    check_service_quotable(service)?;
    let keychain = match &env.hub_keychain {
        Some(p) => {
            let p = p.to_string_lossy();
            if p.contains(['"', '\n', '\\']) {
                return Err(CliError::usage(
                    "PM_HUB_KEYCHAIN has a character that cannot be quoted",
                ));
            }
            format!(" \"{p}\"")
        }
        None => String::new(),
    };
    let mut child = Command::new("security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running `security`")?;
    let cmd = format!(
        "add-generic-password -U -s \"{service}\" -a \"{ACCOUNT}\" -w \"{token}\"{keychain}\n"
    );
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(cmd.as_bytes())
        .context("writing to `security`")?;
    let out = child.wait_with_output().context("waiting for `security`")?;
    if out.status.success() {
        Ok(true)
    } else {
        // `security`'s stderr can echo the failing command; scrub the token.
        let msg = String::from_utf8_lossy(&out.stderr).replace(token, "<token>");
        Err(CliError::error(format!(
            "could not store the token in the keychain: {}",
            msg.trim()
        )))
    }
}

fn keychain_load(env: &Env, service: &str) -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", ACCOUNT, "-w"])
        .args(&env.hub_keychain)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| {
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .to_string()
    })
}

/// `true` when an item existed and is gone; a missing item is not an error.
fn keychain_delete(env: &Env, service: &str) -> Result<bool> {
    if !cfg!(target_os = "macos") {
        return Ok(false);
    }
    let out = Command::new("security")
        .args(["delete-generic-password", "-s", service, "-a", ACCOUNT])
        .args(&env.hub_keychain)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("running `security`")?;
    match out.status.code() {
        Some(0) => Ok(true),
        // errSecItemNotFound
        Some(44) => Ok(false),
        _ => Err(CliError::error(format!(
            "could not remove the token from the keychain: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

// ------------------------------------------------------------ config.toml

/// Sets (`Some`) or removes (`None`) the top-level `hub` key, leaving every
/// other key as it was.
fn write_hub_key(env: &Env, url: Option<&str>) -> Result<std::path::PathBuf> {
    let path = env.config_path()?;
    let mut table = match std::fs::read_to_string(&path) {
        Ok(text) => text
            .parse::<toml::Table>()
            .with_context(|| format!("parsing {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("reading {}", path.display()))
                .into());
        }
    };
    match url {
        Some(url) => {
            table.insert("hub".into(), toml::Value::String(url.into()));
        }
        None => {
            table.remove("hub");
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(
        &path,
        toml::to_string(&table).context("serializing config.toml")?,
    )
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

pub(crate) fn configured_hub(env: &Env) -> Result<Option<String>> {
    let Ok(path) = env.config_path() else {
        return Ok(None);
    };
    Ok(crate::workspace::Config::load(&path)?.and_then(|c| c.hub))
}

/// Whether ticket numbers on this machine come from the hub rather than
/// the local allocator (AGT-1398) — **the one place that decides it**.
/// `pm new` (single, `--from-file`, `--batch`) asks this before every
/// create: `true` means the ticket is committed without a number and
/// flagged pending ([`pm_store::Store::commit_batch_pending`]), reads
/// `AGT-?` and is addressed by its ULID until a sync brings the hub's
/// `field.set number`; `false` means `pm new` numbers it locally as it
/// always has.
///
/// Today this is "a `hub` is configured" (`pm hub login`), exactly what
/// `docs/hub-api.md` §Ticket numbers promises: a ticket made after login
/// is pending a hub number, whether the workspace is still in seed mode —
/// the first sync's `POST /seeded` (AGT-1396) numbers every create the
/// seed left unnumbered — or already authoritative. Numbering locally
/// while a hub is configured but unseeded would be *wrong*, not merely
/// slower: two replicas of one workspace could each mint the same local
/// number before either seeds, and the hub would refuse the second as
/// `duplicate_number`. Never contacts the hub and needs no token, so `pm
/// new` stays offline-safe.
///
/// The hub-authoritative bit does exist (`sync_state.seeded`, AGT-1396:
/// set once the first sync has seeded the hub, or joined one already
/// seeded) and is deliberately **not** ANDed in here, for the reason
/// above: a hub that is configured but not yet seeded must still get
/// pending creates, which the seed's `POST /seeded` numbers. Should that
/// ever change, change it *here* (e.g. add a `&Store` parameter) rather
/// than at the call sites, so `pm new` and its tests keep one answer.
pub(crate) fn numbers_are_hub_assigned(env: &Env) -> Result<bool> {
    Ok(configured_hub(env)?.is_some())
}

/// The token this machine holds for `ws` and where it came from:
/// `PM_HUB_TOKEN` (`"env"`) wins, else the keychain item (`"keychain"`).
fn load_token(env: &Env, ws: &Workspace) -> Option<(String, &'static str)> {
    match env.hub_token.as_deref() {
        Some(t) => Some((t.trim().to_string(), "env")),
        None => keychain_load(env, &service_name(env, ws)).map(|t| (t, "keychain")),
    }
}

// ----------------------------------------------------------------- client

/// Everything a hub call needs, resolved once per command: the configured
/// URL, the token, the workspace's hub id and an HTTP agent that never
/// follows a redirect (which could carry the bearer token elsewhere) and
/// never turns a status into an error (the caller reads the status).
pub(crate) struct HubClient {
    pub url: String,
    pub workspace: String,
    token: String,
    agent: ureq::Agent,
}

/// A transport failure: nothing reached the hub, or no response came back
/// in time. Every HTTP status, 404 included, is an `Ok` response instead.
pub(crate) struct Transport(pub String);

impl HubClient {
    /// The hub for this machine and `ws`, or an error naming what to run
    /// when no URL or no token is configured. Never contacts the hub.
    pub(crate) fn resolve(env: &Env, ws: &Workspace, timeout: Duration) -> Result<HubClient> {
        let Some(url) = configured_hub(env)? else {
            return Err(CliError::error(
                "no hub configured for this machine: run `pm hub login <url>` first",
            ));
        };
        let Some((token, _)) = load_token(env, ws) else {
            return Err(CliError::error(format!(
                "no hub token for workspace {} ({}): run `pm hub login {url}` with the token \
                 on stdin, or set PM_HUB_TOKEN",
                ws.prefix, ws.id
            )));
        };
        Ok(HubClient {
            url,
            workspace: hub_workspace_id(ws),
            token,
            agent: agent(timeout),
        })
    }

    /// `GET <url>/w/<workspace>/<route>` with the bearer token:
    /// `(status, body)`.
    pub(crate) fn get(&self, route: &str) -> std::result::Result<(u16, String), Transport> {
        let req = self
            .agent
            .get(self.route(route))
            .header("Authorization", format!("Bearer {}", self.token));
        let mut resp = req.call().map_err(|e| Transport(e.to_string()))?;
        read_response(&mut resp)
    }

    /// `POST <url>/w/<workspace>/<route>` with a JSON body and the bearer
    /// token: `(status, body)`.
    pub(crate) fn post_json(
        &self,
        route: &str,
        body: &str,
    ) -> std::result::Result<(u16, String), Transport> {
        let req = self
            .agent
            .post(self.route(route))
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json");
        let mut resp = req.send(body).map_err(|e| Transport(e.to_string()))?;
        read_response(&mut resp)
    }

    fn route(&self, route: &str) -> String {
        format!("{}/w/{}/{route}", self.url, self.workspace)
    }
}

/// ureq's default `read_to_string` stops at 10 MiB; a pull page can carry
/// a whole large document edit, so bodies are read up to the hub's own
/// per-request ceiling (`docs/hub-api.md`: 64 MiB) plus a little JSON
/// framing.
const MAX_RESPONSE_BYTES: u64 = 64 * 1024 * 1024 + 1024 * 1024;

fn read_response(
    resp: &mut ureq::http::Response<ureq::Body>,
) -> std::result::Result<(u16, String), Transport> {
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|e| Transport(format!("reading the hub's response: {e}")))?;
    Ok((status, body))
}

// ------------------------------------------------------------------ login

/// A base URL: `http(s)://host[:port][/path]`, no credentials, query or
/// fragment (a secret pasted into the URL would land in config.toml).
fn normalize_url(raw: &str) -> Result<String> {
    let url = raw.trim().trim_end_matches('/');
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| CliError::usage("the hub URL must start with http:// or https://"))?;
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty()
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
        || authority.contains('@')
        || url.contains('?')
        || url.contains('#')
    {
        return Err(CliError::usage(
            "the hub URL must be http(s)://host[:port][/path], with no credentials, query or fragment",
        ));
    }
    Ok(url.to_string())
}

/// The hub's token alphabet (`pm-hub`'s `bearer_token`): non-empty
/// `[A-Za-z0-9_-]`.
fn check_token(token: &str) -> Result<()> {
    if !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Ok(())
    } else {
        Err(CliError::usage(
            "the token must be non-empty and contain only letters, digits, '-' and '_' \
             (pipe it on stdin, or set PM_HUB_TOKEN)",
        ))
    }
}

fn login(ctx: &Ctx<'_>, url: &str) -> Result<()> {
    let url = normalize_url(url)?;
    let (_store, ws) = ctx.open()?;
    let service = service_name(ctx.env, &ws);

    let token = match ctx.env.hub_token.as_deref() {
        Some(t) => Some(t.trim().to_string()),
        None if cfg!(target_os = "macos") => {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .context("reading the token from stdin")?;
            Some(text.trim().to_string())
        }
        // No keychain: a stdin token would have nowhere to go.
        None => None,
    };
    if let Some(t) = &token {
        check_token(t)?;
    }

    // Keychain first: a token that could not be stored must not leave a
    // hub URL behind that looks logged in.
    let stored = match &token {
        Some(t) => keychain_store(ctx.env, &service, t)?,
        None => false,
    };
    let config = write_hub_key(ctx.env, Some(&url))?;

    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "hub": url,
            "workspace": ws.id.to_string(),
            "config": config,
            "token_stored": stored,
            "keychain_service": stored.then_some(service),
        }));
    } else {
        println!("hub:       {url}");
        println!("config:    {}", config.display());
        if stored {
            println!("token:     stored in the keychain (service {service})");
        } else {
            eprintln!(
                "note: no keychain on this platform; set PM_HUB_TOKEN wherever pm talks to the hub"
            );
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- logout

fn logout(ctx: &Ctx<'_>) -> Result<()> {
    let (_store, ws) = ctx.open()?;
    let service = service_name(ctx.env, &ws);
    let had_hub = configured_hub(ctx.env)?.is_some();
    let token_removed = keychain_delete(ctx.env, &service)?;
    let config = if had_hub {
        Some(write_hub_key(ctx.env, None)?)
    } else {
        None
    };
    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "hub_removed": had_hub,
            "token_removed": token_removed,
            "workspace": ws.id.to_string(),
        }));
    } else {
        match &config {
            Some(p) => println!("removed `hub` from {}", p.display()),
            None => println!("no hub configured"),
        }
        if token_removed {
            println!("removed the token from the keychain (service {service})");
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- status

/// `pm hub status`'s two probes are tiny; `pm sync` (`crate::sync`) sets
/// its own, longer budget for op batches.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How long any single call may take to connect; the rest of `timeout`
/// is for moving the body.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT.min(timeout)))
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        // A redirect must never carry the bearer token to another host.
        .max_redirects(0)
        .build()
        .into()
}

/// `(status, body)`; a transport failure is `Err(message)`.
fn get(url: &str, token: Option<&str>) -> std::result::Result<(u16, String), String> {
    let mut req = agent(TIMEOUT).get(url);
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let mut resp = req.call().map_err(|e| e.to_string())?;
    read_response(&mut resp).map_err(|Transport(e)| e)
}

fn status(ctx: &Ctx<'_>) -> Result<()> {
    let (_store, ws) = ctx.open()?;
    let service = service_name(ctx.env, &ws);
    let hub = configured_hub(ctx.env)?;

    let (token, source) = match load_token(ctx.env, &ws) {
        Some((t, source)) => (Some(t), Some(source)),
        None => (None, None),
    };

    let mut reachable: Option<bool> = None;
    let mut health: Option<Value> = None;
    let mut token_accepted: Option<bool> = None;
    let mut token_name: Option<String> = None;
    let mut error: Option<String> = None;

    if let Some(hub) = &hub {
        match get(&format!("{hub}/health"), None) {
            Ok((200, body)) => {
                reachable = Some(true);
                health = serde_json::from_str::<Value>(&body).ok();
            }
            Ok((code, _)) => {
                reachable = Some(false);
                error = Some(format!("GET /health answered HTTP {code}"));
            }
            Err(e) => {
                reachable = Some(false);
                error = Some(e);
            }
        }
        if reachable == Some(true)
            && let Some(t) = &token
        {
            match get(
                &format!("{hub}/w/{}/whoami", hub_workspace_id(&ws)),
                Some(t),
            ) {
                Ok((200, body)) => {
                    token_accepted = Some(true);
                    token_name = serde_json::from_str::<Value>(&body)
                        .ok()
                        .and_then(|v| v.get("name")?.as_str().map(str::to_string));
                }
                // The hub answers every auth failure with a bare 404.
                Ok((404, _)) => token_accepted = Some(false),
                Ok((code, _)) => error = Some(format!("GET whoami answered HTTP {code}")),
                Err(e) => error = Some(e),
            }
        }
    }

    let field = |k: &str| health.as_ref().and_then(|h| h.get(k)).cloned();
    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "configured": hub.is_some(),
            "hub": hub,
            "workspace": {"id": ws.id.to_string(), "prefix": ws.prefix},
            "token": {"present": token.is_some(), "source": source},
            "keychain_service": cfg!(target_os = "macos").then_some(&service),
            "reachable": reachable,
            "health": health.as_ref().map(|_| json!({
                "schema_version": field("schema_version"),
                "op_version": field("op_version"),
            })),
            "token_accepted": token_accepted,
            "token_name": token_name,
            "error": error,
        }));
    } else {
        println!("workspace:  {} ({})", ws.prefix, ws.id);
        match &hub {
            Some(h) => println!("hub:        {h}"),
            None => println!("hub:        not configured"),
        }
        match source {
            Some(s) => println!("token:      present ({s})"),
            None => println!("token:      absent"),
        }
        if let Some(r) = reachable {
            println!("reachable:  {r}");
        }
        if health.is_some() {
            println!(
                "hub schema: {}",
                field("schema_version").unwrap_or(Value::Null)
            );
            println!("op version: {}", field("op_version").unwrap_or(Value::Null));
        }
        match (token_accepted, &token_name) {
            (Some(true), Some(n)) => println!("token ok:   accepted (named {n})"),
            (Some(true), None) => println!("token ok:   accepted"),
            (Some(false), _) => println!("token ok:   REJECTED"),
            (None, _) => {}
        }
        if let Some(e) = &error {
            println!("error:      {e}");
        }
    }

    match (reachable, token_accepted) {
        (Some(false), _) => Err(CliError::error("the hub is unreachable")),
        (_, Some(false)) => Err(CliError::error("the hub rejected the token")),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            normalize_url("https://h.example/").unwrap(),
            "https://h.example"
        );
        assert_eq!(
            normalize_url("http://127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080"
        );
        for bad in [
            "ftp://x",
            "h.example",
            "https://",
            "https://u:p@h",
            "https://h?x=1",
            "https://h#f",
            "https://h x",
        ] {
            assert!(normalize_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tokens() {
        assert!(check_token("pmh_abc-123").is_ok());
        for bad in ["", "a b", "a\"b", "a;b", "a\nb"] {
            assert!(check_token(bad).is_err(), "{bad:?}");
        }
    }
}
