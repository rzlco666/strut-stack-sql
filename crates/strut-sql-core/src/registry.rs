use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use strut_sql_driver_api::{ConnectOptions, DriverError, EngineDriver, EngineKind};
use strut_sql_driver_mysql::MySqlDriver;
use strut_sql_driver_postgres::PostgresDriver;
use strut_sql_driver_sqlite::SqliteDriver;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::redact::redact_url;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 2;
/// Fail fast when the DB host/port is unreachable (before sqlx handshake).
pub const TCP_PREFLIGHT_TIMEOUT: Duration = Duration::from_millis(500);

/// Lazily-constructed, shared driver for one configured source.
/// Consecutive connect failures before the breaker opens.
pub const BREAKER_FAILURE_THRESHOLD: u32 = 5;
/// How long the breaker stays open before one probe is allowed.
pub const BREAKER_OPEN: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct LazyDriver {
    url: String,
    engine: EngineKind,
    options: ConnectOptions,
    inner: Arc<Mutex<Option<Arc<dyn EngineDriver>>>>,
    failures: Arc<std::sync::atomic::AtomicU32>,
    open_until: Arc<Mutex<Option<std::time::Instant>>>,
}

impl std::fmt::Debug for LazyDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyDriver")
            .field("url", &redact_url(&self.url))
            .field("engine", &self.engine)
            .finish_non_exhaustive()
    }
}

impl LazyDriver {
    pub fn new(
        url: String,
        engine: EngineKind,
        pool_size: u32,
        read_only: bool,
        connect_timeout: Duration,
        statement_timeout: Duration,
    ) -> Self {
        Self {
            url,
            engine,
            options: ConnectOptions {
                pool_size,
                read_only,
                connect_timeout,
                statement_timeout,
            },
            inner: Arc::new(Mutex::new(None)),
            failures: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            open_until: Arc::new(Mutex::new(None)),
        }
    }

    /// Pre-populate an already-connected driver (integration tests).
    pub fn with_driver(
        url: String,
        engine: EngineKind,
        driver: Arc<dyn EngineDriver>,
        read_only: bool,
    ) -> Self {
        Self {
            url,
            engine,
            options: ConnectOptions {
                pool_size: 1,
                read_only,
                connect_timeout: CONNECT_TIMEOUT,
                statement_timeout: Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS.max(10)),
            },
            inner: Arc::new(Mutex::new(Some(driver))),
            failures: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            open_until: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn get(&self) -> Result<Arc<dyn EngineDriver>> {
        // Circuit breaker: five consecutive connect failures open it for 30 s.
        // After that one probe is allowed (half-open); a failed probe reopens.
        if let Some(until) = *self.open_until.lock().await {
            let now = std::time::Instant::now();
            if now < until {
                let remaining = (until - now).as_secs().max(1);
                anyhow::bail!(
                    "source {} is failing repeatedly; fast-fail for {remaining}s (circuit breaker)",
                    redact_url(&self.url)
                );
            }
            // Half-open: clear the window and let exactly this attempt probe.
            *self.open_until.lock().await = None;
            self.failures.store(0, std::sync::atomic::Ordering::Relaxed);
        }
        let mut guard = self.inner.lock().await;
        if let Some(driver) = guard.as_ref() {
            return Ok(driver.clone());
        }
        match connect_driver(&self.url, self.engine, &self.options).await {
            Ok(driver) => {
                self.failures.store(0, std::sync::atomic::Ordering::Relaxed);
                *guard = Some(driver.clone());
                Ok(driver)
            }
            Err(e) => {
                let failures = self
                    .failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                if failures >= BREAKER_FAILURE_THRESHOLD {
                    *self.open_until.lock().await = Some(std::time::Instant::now() + BREAKER_OPEN);
                }
                Err(anyhow::anyhow!(
                    "cannot connect to {}: {}",
                    redact_url(&self.url),
                    e
                ))
            }
        }
    }

    pub async fn ping(&self) -> Result<()> {
        self.get().await?.ping().await?;
        Ok(())
    }
}

/// Build a driver for `url` with the given pool options.
pub async fn connect_driver(
    url: &str,
    engine: EngineKind,
    options: &ConnectOptions,
) -> Result<Arc<dyn EngineDriver>, DriverError> {
    if !matches!(engine, EngineKind::Sqlite) {
        tcp_preflight(url, engine)
            .await
            .map_err(|e| DriverError::Connection(e.to_string()))?;
    }

    let driver: Arc<dyn EngineDriver> = match engine {
        EngineKind::Postgres => {
            Arc::new(*PostgresDriver::from_connection_string_with_options(url, options).await?)
        }
        EngineKind::MySql => {
            Arc::new(*MySqlDriver::from_connection_string_with_options(url, options).await?)
        }
        EngineKind::Sqlite => {
            Arc::new(*SqliteDriver::from_connection_string_with_options(url, options).await?)
        }
    };
    Ok(driver)
}

async fn tcp_preflight(url: &str, engine: EngineKind) -> Result<()> {
    let Some((host, port)) = parse_host_port(url, engine) else {
        return Ok(());
    };
    let addr = format!("{host}:{port}");
    match tokio::time::timeout(TCP_PREFLIGHT_TIMEOUT, TcpStream::connect(&addr)).await {
        Ok(Ok(_stream)) => Ok(()),
        Ok(Err(e)) => bail!(
            "TCP preflight failed in <{}ms ({addr}): {e}",
            TCP_PREFLIGHT_TIMEOUT.as_millis()
        ),
        Err(_) => bail!(
            "TCP preflight failed in <{}ms ({addr}): timed out",
            TCP_PREFLIGHT_TIMEOUT.as_millis()
        ),
    }
}

/// Extract host/port from mysql:// or postgres(ql):// URLs. Returns None for unparsable forms.
pub fn parse_host_port(url: &str, engine: EngineKind) -> Option<(String, u16)> {
    let lower = url.to_ascii_lowercase();
    let (rest, default_port) = match engine {
        EngineKind::MySql if lower.starts_with("mysql://") => {
            (url.get("mysql://".len()..)?, 3306u16)
        }
        EngineKind::Postgres if lower.starts_with("postgresql://") => {
            (url.get("postgresql://".len()..)?, 5432u16)
        }
        EngineKind::Postgres if lower.starts_with("postgres://") => {
            (url.get("postgres://".len()..)?, 5432u16)
        }
        _ => return None,
    };

    let after_at = rest.rsplit('@').next().unwrap_or(rest);
    let host_port = after_at.split('/').next().unwrap_or(after_at);
    let host_port = host_port.split('?').next().unwrap_or(host_port);
    // Strip IPv6 brackets if present: [::1]:5432
    if let Some(inner) = host_port.strip_prefix('[') {
        let (host, rest) = inner.split_once(']')?;
        let port = if let Some(p) = rest.strip_prefix(':') {
            p.parse().ok()?
        } else {
            default_port
        };
        return Some((host.to_string(), port));
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => (h, p.parse().unwrap_or(default_port)),
        _ => (host_port, default_port),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

pub fn detect_engine_from_url(hint: Option<&str>, url: &str) -> Result<EngineKind> {
    if let Some(h) = hint {
        return match h.to_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Ok(EngineKind::Postgres),
            "mysql" | "mariadb" => Ok(EngineKind::MySql),
            "sqlite" => Ok(EngineKind::Sqlite),
            other => bail!("unknown engine hint '{other}'"),
        };
    }
    let lower = url.to_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        Ok(EngineKind::Postgres)
    } else if lower.starts_with("mysql://") {
        Ok(EngineKind::MySql)
    } else if lower.starts_with("sqlite:") || lower.starts_with("sqlite://") {
        Ok(EngineKind::Sqlite)
    } else {
        bail!("cannot detect engine from URL scheme; use postgresql://, mysql://, or sqlite:")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mysql_host_port() {
        assert_eq!(
            parse_host_port("mysql://u:p@127.0.0.1:3307/db", EngineKind::MySql),
            Some(("127.0.0.1".into(), 3307))
        );
        assert_eq!(
            parse_host_port("mysql://u:p@localhost/db", EngineKind::MySql),
            Some(("localhost".into(), 3306))
        );
    }

    #[test]
    fn parse_postgres_host_port() {
        assert_eq!(
            parse_host_port(
                "postgresql://u:p@db.example.com:5433/app",
                EngineKind::Postgres
            ),
            Some(("db.example.com".into(), 5433))
        );
    }

    #[tokio::test]
    async fn tcp_preflight_fails_fast_on_closed_port() {
        // Port 1 is typically closed / unprivileged refuse
        let err = match connect_driver(
            "mysql://u:p@127.0.0.1:1/db",
            EngineKind::MySql,
            &ConnectOptions::default(),
        )
        .await
        {
            Ok(_) => panic!("connection to port 1 should fail"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("TCP preflight failed"),
            "unexpected error: {msg}"
        );
    }
}
