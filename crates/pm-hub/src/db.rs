//! Connecting to Postgres, with TLS when `DATABASE_URL` asks for it
//! (AGT-1451).
//!
//! The URL's `sslmode` picks the transport:
//!
//! - absent or `disable`: plaintext. This is right for Railway's private
//!   network (`postgres.railway.internal`) and loopback, and it is what
//!   production uses today. An *absent* `sslmode` logs a one-line notice
//!   the first time a connection is made (AGT-1467), so a deployment that
//!   moves the database off the private network without adding one does
//!   not silently send credentials and ops in the clear; `disable` is an
//!   explicit choice and logs nothing.
//! - `prefer`: TLS first, then plaintext, as libpq does (AGT-1467). The
//!   first attempt negotiates TLS (and falls back inside `tokio-postgres`
//!   if the server offers none); if that attempt fails — a certificate the
//!   bundled roots do not verify, a failed handshake — the connection is
//!   retried in plaintext and the failure logged. Like libpq's `prefer`,
//!   this protects only against a passive listener; use `require` or
//!   stronger when the network is not trusted.
//! - `require`, `verify-ca` or `verify-full`: TLS through rustls (ring
//!   provider, pure Rust, no system OpenSSL). The server certificate is
//!   always verified against the bundled Mozilla roots (`webpki-roots`) and
//!   its name is checked against the URL's host — so all three are as strict
//!   as `verify-full`, stricter than libpq's `require`. A server with a
//!   private CA or a self-signed certificate is refused; there is no
//!   insecure mode on purpose.
//!
//! `tokio-postgres` itself only understands `disable|prefer|require`, so the
//! verify-* spellings are rewritten to `require` before it parses the URL.
//! Error messages never include the URL (it carries the password).

use std::sync::Arc;

use rustls::ClientConfig;
use tokio_postgres::tls::MakeTlsConnect;
use tokio_postgres::{Client, Connection, NoTls, Socket};
use tokio_postgres_rustls::MakeRustlsConnect;

#[derive(Debug, PartialEq, Eq)]
pub enum Transport {
    /// `sslmode=disable`.
    Plain,
    /// No `sslmode` at all: plaintext, with a notice (module docs).
    PlainByDefault,
    /// `sslmode=prefer`: TLS, falling back to plaintext.
    Prefer,
    /// `require`, `verify-ca`, `verify-full`.
    Tls,
}

/// The transport `database_url` asks for and the URL to hand to
/// `tokio_postgres` (verify-* rewritten to `require`).
pub fn plan(database_url: &str) -> Result<(Transport, String), String> {
    let is_uri =
        database_url.starts_with("postgres://") || database_url.starts_with("postgresql://");
    let mut mode = None;
    let rewritten = if is_uri {
        match database_url.split_once('?') {
            None => database_url.to_string(),
            Some((base, query)) => {
                let params: Vec<String> = query
                    .split('&')
                    .map(|p| rewrite_param(p, '=', &mut mode))
                    .collect::<Result<_, _>>()?;
                format!("{base}?{}", params.join("&"))
            }
        }
    } else {
        // libpq key/value form: `host=h user=u sslmode=require`.
        database_url
            .split_whitespace()
            .map(|p| rewrite_param(p, '=', &mut mode))
            .collect::<Result<Vec<_>, _>>()?
            .join(" ")
    };
    let transport = match mode.as_deref() {
        None => Transport::PlainByDefault,
        Some("disable") => Transport::Plain,
        Some("prefer") => Transport::Prefer,
        Some(_) => Transport::Tls,
    };
    Ok((transport, rewritten))
}

/// `url` (a `plan` output with `sslmode=prefer`) with `sslmode=disable`
/// instead: the plaintext retry of a `prefer` connection.
fn without_tls(url: &str) -> String {
    let param = |p: &str| {
        if p == "sslmode=prefer" {
            "sslmode=disable".to_string()
        } else {
            p.to_string()
        }
    };
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        match url.split_once('?') {
            None => url.to_string(),
            Some((base, query)) => {
                let params: Vec<String> = query.split('&').map(param).collect();
                format!("{base}?{}", params.join("&"))
            }
        }
    } else {
        url.split_whitespace()
            .map(param)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The one-line notice for a URL with no `sslmode` (module docs), at most
/// once per process: a hub opens several connections at start-up.
fn plaintext_notice() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        eprintln!(
            "pm-hub: notice: the database URL sets no sslmode, so Postgres is reached in \
             plaintext (right on a private network or loopback; add sslmode=require, or \
             sslmode=disable to silence this)"
        );
    });
}

fn rewrite_param(param: &str, sep: char, mode: &mut Option<String>) -> Result<String, String> {
    let Some((key, value)) = param.split_once(sep) else {
        return Ok(param.to_string());
    };
    if key != "sslmode" {
        return Ok(param.to_string());
    }
    match value {
        "disable" | "prefer" | "require" => {
            *mode = Some(value.to_string());
            Ok(param.to_string())
        }
        "verify-ca" | "verify-full" => {
            *mode = Some(value.to_string());
            Ok(format!("sslmode{sep}require"))
        }
        // `allow` is libpq's "try plaintext first" — the wrong direction.
        other => Err(format!(
            "DATABASE_URL sslmode={other:?} is not supported; use disable, prefer, require, \
             verify-ca or verify-full"
        )),
    }
}

fn tls_connector() -> MakeRustlsConnect {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
    MakeRustlsConnect::new(config)
}

/// Either kind of connection, so callers can drive it the same way.
pub enum Conn {
    Plain(Box<Connection<Socket, tokio_postgres::tls::NoTlsStream>>),
    Tls(Box<Connection<Socket, <MakeRustlsConnect as MakeTlsConnect<Socket>>::Stream>>),
}

impl Conn {
    pub async fn drive(self) -> Result<(), tokio_postgres::Error> {
        match self {
            Conn::Plain(c) => c.await,
            Conn::Tls(c) => c.await,
        }
    }
}

pub async fn connect(database_url: &str) -> Result<(Client, Conn), Box<dyn std::error::Error>> {
    let (transport, url) = plan(database_url)?;
    Ok(match transport {
        Transport::Plain | Transport::PlainByDefault => {
            if transport == Transport::PlainByDefault {
                plaintext_notice();
            }
            let (client, conn) = tokio_postgres::connect(&url, NoTls).await?;
            (client, Conn::Plain(Box::new(conn)))
        }
        Transport::Prefer => match tokio_postgres::connect(&url, tls_connector()).await {
            Ok((client, conn)) => (client, Conn::Tls(Box::new(conn))),
            Err(e) => {
                eprintln!(
                    "pm-hub: sslmode=prefer: the TLS attempt failed ({e}); retrying in plaintext"
                );
                let (client, conn) = tokio_postgres::connect(&without_tls(&url), NoTls).await?;
                (client, Conn::Plain(Box::new(conn)))
            }
        },
        Transport::Tls => {
            let (client, conn) = tokio_postgres::connect(&url, tls_connector()).await?;
            (client, Conn::Tls(Box::new(conn)))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(url: &str) -> Transport {
        plan(url).unwrap().0
    }

    #[test]
    fn no_sslmode_is_plaintext_with_a_notice() {
        assert_eq!(
            t("postgres://u:p@postgres.railway.internal:5432/railway"),
            Transport::PlainByDefault
        );
        assert_eq!(
            t("postgresql://u@localhost/db?application_name=x"),
            Transport::PlainByDefault
        );
        assert_eq!(t("host=localhost user=u"), Transport::PlainByDefault);
    }

    #[test]
    fn disable_is_plaintext_and_prefer_tries_tls_first() {
        assert_eq!(t("postgres://u@h/db?sslmode=disable"), Transport::Plain);
        assert_eq!(t("host=h sslmode=disable"), Transport::Plain);
        assert_eq!(t("postgres://u@h/db?sslmode=prefer"), Transport::Prefer);
        assert_eq!(t("host=h sslmode=prefer user=u"), Transport::Prefer);
        // The plaintext retry parses and says so.
        for (url, retry) in [
            (
                "postgres://u:p@h/db?application_name=x&sslmode=prefer&connect_timeout=5",
                "postgres://u:p@h/db?application_name=x&sslmode=disable&connect_timeout=5",
            ),
            (
                "host=h sslmode=prefer user=u",
                "host=h sslmode=disable user=u",
            ),
        ] {
            let (_, planned) = plan(url).unwrap();
            assert_eq!(without_tls(&planned), retry);
            let config: tokio_postgres::Config = retry.parse().unwrap();
            assert_eq!(
                config.get_ssl_mode(),
                tokio_postgres::config::SslMode::Disable
            );
        }
    }

    #[test]
    fn require_and_verify_modes_use_tls() {
        for m in ["require", "verify-ca", "verify-full"] {
            assert_eq!(
                t(&format!("postgres://u@h/db?sslmode={m}")),
                Transport::Tls,
                "{m}"
            );
            assert_eq!(
                t(&format!("host=h user=u sslmode={m}")),
                Transport::Tls,
                "{m}"
            );
        }
    }

    #[test]
    fn verify_modes_are_rewritten_for_tokio_postgres() {
        let (_, url) = plan("postgres://u:p@h/db?a=1&sslmode=verify-full&b=2").unwrap();
        assert_eq!(url, "postgres://u:p@h/db?a=1&sslmode=require&b=2");
        let (_, url) = plan("host=h sslmode=verify-ca user=u").unwrap();
        assert_eq!(url, "host=h sslmode=require user=u");
        // The rewritten forms parse.
        for u in [
            "postgres://u:p@h/db?sslmode=verify-ca",
            "host=h sslmode=verify-full",
        ] {
            u.parse::<tokio_postgres::Config>().unwrap_err();
            plan(u)
                .unwrap()
                .1
                .parse::<tokio_postgres::Config>()
                .unwrap();
        }
    }

    #[test]
    fn unknown_sslmode_is_refused_without_echoing_the_url() {
        let err = plan("postgres://u:secret@h/db?sslmode=allow").unwrap_err();
        assert!(err.contains("allow") && !err.contains("secret"), "{err}");
        assert!(plan("postgres://u@h/db?sslmode=bogus").is_err());
    }

    #[test]
    fn tls_config_builds() {
        let _ = tls_connector();
    }
}
