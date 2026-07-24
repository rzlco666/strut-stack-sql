use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::db::{
    detect_engine_from_url, parse_host_port, EngineKind, EnginePool, LazyEnginePool,
    DEFAULT_CONNECT_TIMEOUT_SECS,
};

pub const DEFAULT_MAX_ROWS: u32 = 100;
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;
pub const DEFAULT_POOL_SIZE: u32 = 10;
pub const DEFAULT_QUERY_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_BATCH_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum WriteMode {
    ReadOnly,
    AllowWrites,
    AllowDdl,
}

impl WriteMode {
    pub fn allows_dml(self) -> bool {
        matches!(self, Self::AllowWrites | Self::AllowDdl)
    }

    pub fn allows_ddl(self) -> bool {
        matches!(self, Self::AllowDdl)
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "strut-stack-sql",
    version,
    about = "Strut Stack SQL — token-efficient MCP for MySQL, PostgreSQL, and SQLite"
)]
pub struct Cli {
    /// Database connection URL (overrides .env discovery).
    #[arg(long)]
    pub url: Option<String>,

    /// Environment variable holding the connection URL.
    #[arg(long)]
    pub url_env: Option<String>,

    /// Path to strut-stack-sql.toml for multi-source configuration.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Workspace root for .env discovery and SQLite relative paths.
    #[arg(long)]
    pub workspace: Option<PathBuf>,

    /// Connect to all sources at startup (default: lazy connect on first tool call).
    #[arg(long)]
    pub eager_connect: bool,

    /// Allow INSERT/UPDATE/DELETE.
    #[arg(long, conflicts_with = "allow_ddl")]
    pub allow_writes: bool,

    /// Allow DDL (DROP/ALTER/TRUNCATE/CREATE). Implies --allow-writes.
    #[arg(long)]
    pub allow_ddl: bool,

    /// Expose additional schema tools (list_schemas, describe_table, etc.).
    #[arg(long)]
    pub full_tools: bool,

    /// Streamable HTTP bind address (e.g. 127.0.0.1:8080).
    #[arg(long)]
    pub http: Option<String>,

    /// Max rows returned per query.
    #[arg(long, default_value_t = DEFAULT_MAX_ROWS)]
    pub max_rows: u32,

    /// Max response bytes per query result.
    #[arg(long, default_value_t = DEFAULT_MAX_BYTES)]
    pub max_bytes: usize,

    /// Connection pool size per source.
    #[arg(long, default_value_t = DEFAULT_POOL_SIZE)]
    pub pool_size: u32,

    /// Query timeout in seconds.
    #[arg(long, default_value_t = DEFAULT_QUERY_TIMEOUT_SECS)]
    pub query_timeout: u64,

    /// Pool acquire / connect timeout in seconds (default: 2).
    #[arg(long, default_value_t = DEFAULT_CONNECT_TIMEOUT_SECS)]
    pub connect_timeout: u64,

    /// Max concurrent queries in a batch.
    #[arg(long, default_value_t = DEFAULT_BATCH_CONCURRENCY)]
    pub batch_concurrency: usize,

    /// Stop batch on first error.
    #[arg(long)]
    pub fail_fast: bool,

    /// Comma-separated column names to redact in query results (opt-in).
    #[arg(long, value_delimiter = ',')]
    pub deny_columns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TomlConfig {
    #[serde(default)]
    pub default_source: Option<String>,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub deny_columns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub url_env: Option<String>,
    #[serde(default)]
    pub engine: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedSource {
    pub name: String,
    pub url: String,
    pub engine: EngineKind,
    lazy_pool: LazyEnginePool,
}

impl ResolvedSource {
    pub async fn pool(&self) -> Result<EnginePool> {
        self.lazy_pool.get().await
    }

    pub fn with_connected_pool(
        name: impl Into<String>,
        url: String,
        engine: EngineKind,
        pool: EnginePool,
        read_only: bool,
    ) -> Self {
        let name = name.into();
        Self {
            name: name.clone(),
            url: url.clone(),
            engine,
            lazy_pool: LazyEnginePool::with_pool(url, engine, pool, read_only),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub write_mode: WriteMode,
    pub full_tools: bool,
    pub max_rows: u32,
    pub max_bytes: usize,
    pub query_timeout: Duration,
    pub batch_concurrency: usize,
    pub fail_fast: bool,
    pub deny_columns: Vec<String>,
    pub default_source: String,
    pub sources: HashMap<String, ResolvedSource>,
    pub searched_paths: Vec<PathBuf>,
    pub workspace: Option<PathBuf>,
}

impl AppConfig {
    pub fn source(&self, name: Option<&str>) -> Result<&ResolvedSource> {
        let key = name.unwrap_or(&self.default_source);
        self.sources
            .get(key)
            .with_context(|| format!("unknown source '{key}'"))
    }

    pub async fn connect_all(&self) -> Result<()> {
        for source in self.sources.values() {
            source.pool().await?.ping().await?;
        }
        Ok(())
    }
}

pub async fn load_config(cli: &Cli) -> Result<AppConfig> {
    let workspace = cli.workspace.clone();
    if let Some(ws) = &workspace {
        std::env::set_current_dir(ws)
            .with_context(|| format!("cannot chdir to workspace {}", ws.display()))?;
    }

    let searched_paths = discover_dotenv();
    let write_mode = if cli.allow_ddl {
        WriteMode::AllowDdl
    } else if cli.allow_writes {
        WriteMode::AllowWrites
    } else {
        WriteMode::ReadOnly
    };

    let read_only = !write_mode.allows_dml();
    let pool_size = cli.pool_size;
    let connect_timeout = Duration::from_secs(cli.connect_timeout);
    let statement_timeout = Duration::from_secs(cli.query_timeout);
    let mut sources = HashMap::new();

    if let Some(path) = &cli.config {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let toml_cfg: TomlConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let deny_columns = resolve_deny_columns(cli, Some(&toml_cfg.deny_columns));
        for src in toml_cfg.sources {
            let url = resolve_url(src.url.as_deref(), src.url_env.as_deref())?;
            let url = resolve_sqlite_path(&url, workspace.as_deref());
            let engine = detect_engine(src.engine.as_deref(), &url)?;
            sources.insert(
                src.name.clone(),
                ResolvedSource {
                    name: src.name.clone(),
                    url: url.clone(),
                    engine,
                    lazy_pool: LazyEnginePool::new(
                        url,
                        engine,
                        pool_size,
                        read_only,
                        connect_timeout,
                        statement_timeout,
                    ),
                },
            );
        }
        let default_source = toml_cfg
            .default_source
            .or_else(|| sources.keys().next().cloned())
            .context("no sources configured in TOML")?;
        return Ok(AppConfig {
            write_mode,
            full_tools: cli.full_tools,
            max_rows: cli.max_rows,
            max_bytes: cli.max_bytes,
            query_timeout: Duration::from_secs(cli.query_timeout),
            batch_concurrency: cli.batch_concurrency,
            fail_fast: cli.fail_fast,
            deny_columns,
            default_source,
            sources,
            searched_paths,
            workspace,
        });
    }

    let deny_columns = resolve_deny_columns(cli, None);

    let url = if let Some(url) = &cli.url {
        url.clone()
    } else if let Some(env_key) = &cli.url_env {
        std::env::var(env_key)
            .with_context(|| format!("environment variable '{env_key}' is not set"))?
    } else {
        match resolve_url_from_env() {
            Ok(url) => url,
            Err(err) => {
                // Allow MCP initialize / tools/list without credentials.
                // First tool call that needs a real DB should set DATABASE_URL.
                tracing::warn!(
                    error = %err,
                    "no database URL; using ephemeral sqlite::memory: so the server can start"
                );
                "sqlite::memory:".to_string()
            }
        }
    };

    let url = resolve_sqlite_path(&url, workspace.as_deref());
    let engine = detect_engine(None, &url)?;
    sources.insert(
        "default".into(),
        ResolvedSource {
            name: "default".into(),
            url: url.clone(),
            engine,
            lazy_pool: LazyEnginePool::new(
                url,
                engine,
                pool_size,
                read_only,
                connect_timeout,
                statement_timeout,
            ),
        },
    );

    Ok(AppConfig {
        write_mode,
        full_tools: cli.full_tools,
        max_rows: cli.max_rows,
        max_bytes: cli.max_bytes,
        query_timeout: Duration::from_secs(cli.query_timeout),
        batch_concurrency: cli.batch_concurrency,
        fail_fast: cli.fail_fast,
        deny_columns,
        default_source: "default".into(),
        sources,
        searched_paths,
        workspace,
    })
}

fn discover_dotenv() -> Vec<PathBuf> {
    let mut searched = Vec::new();
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let env_path = d.join(".env");
        searched.push(env_path.clone());
        if env_path.exists() {
            let _ = dotenvy::from_path(&env_path);
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    searched
}

fn resolve_sqlite_path(url: &str, workspace: Option<&Path>) -> String {
    let lower = url.to_lowercase();
    if !lower.starts_with("sqlite:") {
        return url.to_string();
    }
    let path_part = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url);
    if path_part.starts_with("./") || path_part.starts_with(".\\") {
        if let Some(ws) = workspace {
            let rel = path_part.trim_start_matches("./").trim_start_matches(".\\");
            let abs = ws.join(rel);
            return format!("sqlite://{}", abs.display());
        }
    }
    url.to_string()
}

fn resolve_url_from_env() -> Result<String> {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        if !url.is_empty() {
            return Ok(url);
        }
    }

    if let Ok(url) = std::env::var("POSTGRES_URL") {
        if !url.is_empty() {
            return Ok(url);
        }
    }

    if let Ok(url) = std::env::var("MYSQL_URL") {
        if !url.is_empty() {
            return Ok(url);
        }
    }

    if let Ok(url) = std::env::var("SQLITE_URL") {
        if !url.is_empty() {
            return Ok(url);
        }
    }

    if let Some(url) = build_mysql_url_from_parts() {
        return Ok(url);
    }

    if let Some(url) = build_postgres_url_from_parts() {
        return Ok(url);
    }

    bail!(
        "no database credentials found. Set DATABASE_URL in .env, use --url, or provide strut-stack-sql.toml"
    )
}

fn resolve_url(url: Option<&str>, url_env: Option<&str>) -> Result<String> {
    if let Some(raw) = url {
        return expand_env_placeholders(raw);
    }
    if let Some(key) = url_env {
        return std::env::var(key)
            .with_context(|| format!("environment variable '{key}' is not set"));
    }
    resolve_url_from_env()
}

fn expand_env_placeholders(raw: &str) -> Result<String> {
    if raw.starts_with("${") && raw.ends_with('}') {
        let key = &raw[2..raw.len() - 1];
        return std::env::var(key)
            .with_context(|| format!("environment variable '{key}' is not set"));
    }
    Ok(raw.to_string())
}

fn build_postgres_url_from_parts() -> Option<String> {
    let host = std::env::var("POSTGRES_HOST")
        .or_else(|_| std::env::var("PGHOST"))
        .ok()?;
    let user = std::env::var("POSTGRES_USER")
        .or_else(|_| std::env::var("PGUSER"))
        .ok()?;
    let password = std::env::var("POSTGRES_PASSWORD")
        .or_else(|_| std::env::var("PGPASSWORD"))
        .ok()
        .unwrap_or_default();
    let db = std::env::var("POSTGRES_DB")
        .or_else(|_| std::env::var("PGDATABASE"))
        .ok()?;
    let port = std::env::var("POSTGRES_PORT")
        .or_else(|_| std::env::var("PGPORT"))
        .unwrap_or_else(|_| "5432".into());
    Some(format!("postgresql://{user}:{password}@{host}:{port}/{db}"))
}

fn build_mysql_url_from_parts() -> Option<String> {
    let host = std::env::var("MYSQL_HOST").ok()?;
    let user = std::env::var("MYSQL_USER").ok()?;
    let password = std::env::var("MYSQL_PASSWORD").ok().unwrap_or_default();
    let db = std::env::var("MYSQL_DATABASE")
        .or_else(|_| std::env::var("MYSQL_DB"))
        .ok()?;
    let port = std::env::var("MYSQL_PORT").unwrap_or_else(|_| "3306".into());
    Some(format!("mysql://{user}:{password}@{host}:{port}/{db}"))
}

pub fn detect_engine(hint: Option<&str>, url: &str) -> Result<EngineKind> {
    detect_engine_from_url(hint, url)
}

/// Resolve deny-list: CLI wins, then env `MCP_SQL_DENY_COLUMNS`, then TOML.
fn resolve_deny_columns(cli: &Cli, toml_cols: Option<&[String]>) -> Vec<String> {
    if !cli.deny_columns.is_empty() {
        return normalize_deny_columns(cli.deny_columns.clone());
    }
    if let Ok(env) = std::env::var("MCP_SQL_DENY_COLUMNS") {
        let cols: Vec<String> = env
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !cols.is_empty() {
            return normalize_deny_columns(cols);
        }
    }
    if let Some(cols) = toml_cols {
        if !cols.is_empty() {
            return normalize_deny_columns(cols.to_vec());
        }
    }
    Vec::new()
}

fn normalize_deny_columns(cols: Vec<String>) -> Vec<String> {
    cols.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Public connection target metadata for `list_sources` (no credentials).
pub fn source_endpoint(url: &str, engine: EngineKind) -> SourceEndpoint {
    let (host, port) = match parse_host_port(url, engine) {
        Some((h, p)) => (Some(h), Some(p)),
        None => (None, None),
    };
    SourceEndpoint {
        engine: match engine {
            EngineKind::Postgres => "postgres".into(),
            EngineKind::Mysql => "mysql".into(),
            EngineKind::Sqlite => "sqlite".into(),
        },
        host,
        port,
        database: database_from_url(url, engine),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceEndpoint {
    pub engine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
}

fn database_from_url(url: &str, engine: EngineKind) -> Option<String> {
    match engine {
        EngineKind::Sqlite => {
            let path = url
                .strip_prefix("sqlite://")
                .or_else(|| url.strip_prefix("sqlite:"))
                .unwrap_or(url);
            if path.is_empty() || path == ":memory:" || path == "memory:" {
                Some(":memory:".into())
            } else {
                Some(path.split('?').next().unwrap_or(path).to_string())
            }
        }
        EngineKind::Postgres | EngineKind::Mysql => {
            let lower = url.to_ascii_lowercase();
            let rest = if lower.starts_with("postgresql://") {
                url.get("postgresql://".len()..)?
            } else if lower.starts_with("postgres://") {
                url.get("postgres://".len()..)?
            } else if lower.starts_with("mysql://") {
                url.get("mysql://".len()..)?
            } else {
                return None;
            };
            let after_at = rest.rsplit('@').next().unwrap_or(rest);
            let db = after_at.split('/').nth(1)?;
            let db = db.split('?').next()?.trim();
            if db.is_empty() {
                None
            } else {
                Some(db.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_mode_tiers() {
        assert!(!WriteMode::ReadOnly.allows_dml());
        assert!(WriteMode::AllowWrites.allows_dml());
        assert!(!WriteMode::AllowWrites.allows_ddl());
        assert!(WriteMode::AllowDdl.allows_ddl());
    }

    #[test]
    fn detect_engine_from_url() {
        assert_eq!(
            detect_engine(None, "postgresql://localhost/db").unwrap(),
            EngineKind::Postgres
        );
        assert_eq!(
            detect_engine(None, "mysql://localhost/db").unwrap(),
            EngineKind::Mysql
        );
        assert_eq!(
            detect_engine(None, "sqlite::memory:").unwrap(),
            EngineKind::Sqlite
        );
        assert_eq!(
            detect_engine(None, "sqlite://./data.db").unwrap(),
            EngineKind::Sqlite
        );
    }

    #[test]
    fn resolve_sqlite_relative_path() {
        let url = resolve_sqlite_path("sqlite://./data.db", Some(Path::new("/workspace/proj")));
        assert!(url.contains("/workspace/proj/data.db"));
    }
}
