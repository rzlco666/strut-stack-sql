//! Connection-string parsing for the ClickHouse HTTP interface.
//!
//! Accepted forms:
//!
//! ```text
//! clickhouse://user:password@host:8123/database
//! clickhouses://user:password@host:8443/database   (TLS)
//! http://host:8123/database                        (explicit transport)
//! https://host:8443/database
//! ```
//!
//! Any query parameter other than `database`/`db` and `secure` is passed to
//! ClickHouse verbatim as a session setting for every request (for example
//! `?max_memory_usage=2000000000`). Credentials in the URL are sent as the
//! `X-ClickHouse-User` / `X-ClickHouse-Key` headers, never in the query.

use std::time::Duration;

use percent_encoding::percent_decode_str;
use reqwest::Url;
use strut_sql_driver_api::{ConnectOptions, DriverError};

pub(crate) const DEFAULT_HTTP_PORT: u16 = 8123;
pub(crate) const DEFAULT_HTTPS_PORT: u16 = 8443;
pub(crate) const DEFAULT_DATABASE: &str = "default";

#[derive(Debug, Clone)]
pub(crate) struct Endpoint {
    /// Base URL with an empty path and no query (`http://host:8123/`).
    pub base: Url,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: String,
    /// Settings taken from the connection string's query string.
    pub settings: Vec<(String, String)>,
}

impl Endpoint {
    pub fn parse(connection_string: &str) -> Result<Self, DriverError> {
        let url = Url::parse(connection_string).map_err(|e| {
            DriverError::InvalidConfig(format!("invalid ClickHouse connection string: {e}"))
        })?;
        let scheme = url.scheme().to_ascii_lowercase();
        let (transport_scheme, default_port) = match scheme.as_str() {
            "clickhouse" | "http" => ("http", DEFAULT_HTTP_PORT),
            "clickhouses" | "https" => ("https", DEFAULT_HTTPS_PORT),
            other => {
                return Err(DriverError::InvalidConfig(format!(
                    "unsupported ClickHouse URL scheme '{other}' (expected clickhouse://, \
                     clickhouses://, http:// or https://)"
                )))
            }
        };

        let host = url
            .host_str()
            .ok_or_else(|| DriverError::InvalidConfig("ClickHouse URL has no host".to_string()))?;
        let port = url.port().unwrap_or(default_port);
        let base = Url::parse(&format!("{transport_scheme}://{host}:{port}/")).map_err(|e| {
            DriverError::InvalidConfig(format!("invalid ClickHouse URL authority: {e}"))
        })?;

        let user = {
            let decoded = decode(url.username());
            (!decoded.is_empty()).then_some(decoded)
        };
        let password = {
            let decoded = url.password().map(decode).unwrap_or_default();
            (!decoded.is_empty()).then_some(decoded)
        };

        let mut database = url
            .path()
            .trim_start_matches('/')
            .trim_end_matches('/')
            .to_string();
        if database.is_empty() {
            database = DEFAULT_DATABASE.to_string();
        }

        let mut settings = Vec::new();
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "database" | "db" => {
                    if !value.is_empty() {
                        database = value.into_owned();
                    }
                }
                // `secure=true` is accepted for parity with other ClickHouse
                // clients; it cannot change the transport here, so a plain
                // `clickhouse://` URL with `secure=true` is rejected rather
                // than silently sent over cleartext.
                "secure" => {
                    if matches!(value.as_ref(), "1" | "true" | "yes") && transport_scheme == "http"
                    {
                        return Err(DriverError::InvalidConfig(
                            "secure=true requires the clickhouses:// or https:// scheme"
                                .to_string(),
                        ));
                    }
                }
                _ => settings.push((key.into_owned(), value.into_owned())),
            }
        }

        Ok(Self {
            base,
            user,
            password,
            database,
            settings,
        })
    }

    /// Effective per-request timeout: the server-side limit plus a grace
    /// period, so a `max_execution_time` abort surfaces before the client
    /// gives up. `Duration::ZERO` disables both.
    pub fn request_timeout(&self, options: &ConnectOptions) -> Option<Duration> {
        if options.statement_timeout.is_zero() {
            None
        } else {
            Some(options.statement_timeout + Duration::from_secs(5))
        }
    }
}

fn decode(value: &str) -> String {
    percent_decode_str(value).decode_utf8_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_url_with_database() {
        let endpoint =
            Endpoint::parse("clickhouse://bob:s3cret@localhost:18123/analytics").expect("parse");
        assert_eq!(endpoint.base.as_str(), "http://localhost:18123/");
        assert_eq!(endpoint.user.as_deref(), Some("bob"));
        assert_eq!(endpoint.password.as_deref(), Some("s3cret"));
        assert_eq!(endpoint.database, "analytics");
        assert!(endpoint.settings.is_empty());
    }

    #[test]
    fn defaults_database_and_port() {
        let endpoint = Endpoint::parse("clickhouse://localhost").expect("parse");
        assert_eq!(endpoint.base.as_str(), "http://localhost:8123/");
        assert_eq!(endpoint.database, "default");
        assert!(endpoint.user.is_none());
    }

    #[test]
    fn tls_scheme_uses_https_and_8443() {
        let endpoint = Endpoint::parse("clickhouses://ch.example.com/db").expect("parse");
        assert_eq!(endpoint.base.as_str(), "https://ch.example.com:8443/");
        assert_eq!(endpoint.database, "db");
    }

    #[test]
    fn keeps_unknown_query_pairs_as_settings() {
        let endpoint = Endpoint::parse("clickhouse://h/db?max_memory_usage=1000&database=other")
            .expect("parse");
        assert_eq!(endpoint.database, "other");
        assert_eq!(
            endpoint.settings,
            vec![("max_memory_usage".to_string(), "1000".to_string())]
        );
    }

    #[test]
    fn rejects_secure_flag_on_cleartext() {
        let err = Endpoint::parse("clickhouse://h?secure=true").unwrap_err();
        assert!(err.to_string().contains("secure=true"), "{err}");
    }

    #[test]
    fn rejects_foreign_scheme() {
        let err = Endpoint::parse("postgres://h/db").unwrap_err();
        assert!(err
            .to_string()
            .contains("unsupported ClickHouse URL scheme"));
    }
}
