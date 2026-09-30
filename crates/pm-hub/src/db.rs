//! Connecting to Postgres, with TLS when `DATABASE_URL` asks for it
//! (AGT-1451).
//!
//! The URL's `sslmode` picks the transport:
//!
//! - absent, `disable` or `prefer`: plaintext. This is right for Railway's
//!   private network (`postgres.railway.internal`) and loopback, and it is
//!   what production uses today. `prefer` is opportunistic in libpq; here it
//!   is treated as plaintext rather than guessing.
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
    Plain,
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
        None | Some("disable") | Some("prefer") => Transport::Plain,
        Some(_) => Transport::Tls,
    };
    Ok((transport, rewritten))
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
        Transport::Plain => {
            let (client, conn) = tokio_postgres::connect(&url, NoTls).await?;
            (client, Conn::Plain(Box::new(conn)))
        }
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
    fn no_sslmode_is_plaintext() {
        assert_eq!(
            t("postgres://u:p@postgres.railway.internal:5432/railway"),
            Transport::Plain
        );
        assert_eq!(
            t("postgresql://u@localhost/db?application_name=x"),
            Transport::Plain
        );
        assert_eq!(t("host=localhost user=u"), Transport::Plain);
    }

    #[test]
    fn disable_and_prefer_are_plaintext() {
        assert_eq!(t("postgres://u@h/db?sslmode=disable"), Transport::Plain);
        assert_eq!(t("postgres://u@h/db?sslmode=prefer"), Transport::Plain);
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
