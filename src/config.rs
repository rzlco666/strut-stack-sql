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

    /// The stricter (lower) of two modes — used to cap a per-token mode by
    /// the process-wide mode so a token can never exceed the process grant.
    pub fn capped(self, ceiling: WriteMode) -> WriteMode {
        self.min(ceiling)
    }

    /// Parse a token/config mode string: `readonly` | `writes` | `ddl`.
    pub fn parse_mode(s: &str) -> Result<WriteMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "readonly" | "read-only" | "read_only" | "read" | "ro" => Ok(Self::ReadOnly),
            "writes" | "write" | "dml" => Ok(Self::AllowWrites),
            "ddl" | "admin" => Ok(Self::AllowDdl),
            other => bail!("invalid write mode '{other}' (expected readonly, writes, or ddl)"),
        }
    }
}

/// One HTTP bearer token with its own authorization grant (SQL-SEC-05 /
/// SQL-GAP-01, pragmatic v1.1 form): a name, the secret, the max
/// [`WriteMode`] it may exercise, and an optional source allowlist
/// (empty = all sources).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpToken {
    pub name: String,
    pub token: String,
    pub mode: WriteMode,
    pub sources: Vec<String>,
}

impl HttpToken {
    pub fn allows_source(&self, source: &str) -> bool {
        self.sources.is_empty() || self.sources.iter().any(|s| s == source)
    }

    /// Effective mode of this token under the process-wide ceiling.
    pub fn effective_mode(&self, process_mode: WriteMode) -> WriteMode {
        self.mode.capped(process_mode)
    }
}

/// Parse the `STRUT_STACK_SQL_HTTP_TOKENS` format: comma-separated entries
/// of `name:token:mode[:source1|source2|...]`, e.g.
/// `alice:s3cr3t:writes:main|reporting,bob:tok123:readonly`.
/// Token secrets must not contain `:` or `,`.
pub fn parse_http_tokens(spec: &str) -> Result<Vec<HttpToken>> {
    let mut out: Vec<HttpToken> = Vec::new();
    for entry in spec.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let parts: Vec<&str> = entry.split(':').collect();
        if parts.len() < 3 || parts.len() > 4 {
            bail!("invalid HTTP token entry '{entry}': expected name:token:mode[:source1|source2]");
        }
        let name = parts[0].trim().to_string();
        let token = parts[1].trim().to_string();
        if name.is_empty() || token.is_empty() {
            bail!("invalid HTTP token entry '{entry}': name and token must be non-empty");
        }
        if out.iter().any(|t: &HttpToken| t.name == name) {
            bail!("duplicate HTTP token name '{name}'");
        }
        let mode = WriteMode::parse_mode(parts[2])
            .with_context(|| format!("in HTTP token entry '{name}'"))?;
        let sources = if parts.len() == 4 {
            parts[3]
                .split('|')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        } else {
            Vec::new()
        };
        out.push(HttpToken {
            name,
            token,
            mode,
            sources,
        });
    }
    Ok(out)
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

    /// Single bearer token for HTTP mode (name "cli", mode = process mode).
    /// For named tokens with per-token modes/sources use the
    /// STRUT_STACK_SQL_HTTP_TOKENS env var (name:token:mode[:src1|src2],...).
    #[arg(long)]
    pub http_token: Option<String>,

    /// Allow HTTP on a non-loopback bind WITHOUT any token (dangerous;
    /// required to start in that combination, loudly warned).
    #[arg(long)]
    pub allow_unauthenticated_http: bool,

    /// Per-token HTTP rate limit, requests per minute (0 = off).
    #[arg(long, default_value_t = 600)]
    pub http_rate_limit_per_minute: u32,

    /// Explicitly opt in to the ephemeral in-memory SQLite fallback used
    /// when no database is configured (also: STRUT_STACK_SQL_EPHEMERAL=1).
    /// Without this the server still starts, but with a loud warning.
    #[arg(long)]
    pub ephemeral: bool,

    /// Hide source host/port/database in the list_sources tool
    /// (also: STRUT_STACK_SQL_HIDE_ENDPOINTS=1).
    #[arg(long)]
    pub hide_source_endpoints: bool,

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
#[serde(deny_unknown_fields)]
pub struct TomlConfig {
    #[serde(default)]
    pub default_source: Option<String>,
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
    #[serde(default)]
    pub deny_columns: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// HTTP bearer tokens (empty = HTTP auth not configured).
    pub http_tokens: Vec<HttpToken>,
    /// Per-token HTTP rate limit, requests/minute (0 = off).
    pub http_rate_limit_per_minute: u32,
    /// Operator explicitly accepted unauthenticated non-loopback HTTP.
    pub allow_unauthenticated_http: bool,
    /// True when running on the ephemeral `sqlite::memory:` fallback.
    pub ephemeral: bool,
    /// Explicit endpoint-hiding override; `None` = automatic default
    /// (hidden when HTTP auth is active and process mode is not DDL).
    pub hide_endpoints: Option<bool>,
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

    /// Whether any HTTP bearer token is configured.
    pub fn http_auth_enabled(&self) -> bool {
        !self.http_tokens.is_empty()
    }

    /// Effective endpoint visibility for `list_sources` (SQL-SEC-15).
    pub fn endpoints_hidden(&self) -> bool {
        if let Some(v) = self.hide_endpoints {
            return v;
        }
        // Default: with authenticated HTTP, do not hand every caller the
        // host/port map — unless the process itself runs in DDL mode,
        // where the operator is fully trusted.
        self.http_auth_enabled() && !self.write_mode.allows_ddl()
    }
}

/// Fail-fast validation of numeric CLI values (SQL-BUG-09).
fn validate_cli_numbers(cli: &Cli) -> Result<()> {
    if !(1..=100_000).contains(&cli.max_rows) {
        bail!(
            "--max-rows must be between 1 and 100000 (got {})",
            cli.max_rows
        );
    }
    if cli.max_bytes == 0 {
        bail!("--max-bytes must be > 0");
    }
    if cli.pool_size == 0 || cli.pool_size > 1000 {
        bail!(
            "--pool-size must be between 1 and 1000 (got {})",
            cli.pool_size
        );
    }
    if cli.query_timeout == 0 {
        bail!("--query-timeout must be > 0 seconds");
    }
    if cli.connect_timeout == 0 {
        bail!("--connect-timeout must be > 0 seconds");
    }
    if cli.batch_concurrency == 0 {
        bail!("--batch-concurrency must be >= 1");
    }
    Ok(())
}

/// Assemble HTTP tokens from `--http-token` and the
/// `STRUT_STACK_SQL_HTTP_TOKENS` env var.
fn resolve_http_tokens(cli: &Cli, process_mode: WriteMode) -> Result<Vec<HttpToken>> {
    let mut tokens = Vec::new();
    if let Ok(spec) = std::env::var("STRUT_STACK_SQL_HTTP_TOKENS") {
        if !spec.trim().is_empty() {
            tokens = parse_http_tokens(&spec).context("parsing STRUT_STACK_SQL_HTTP_TOKENS")?;
        }
    }
    if let Some(single) = &cli.http_token {
        let single = single.trim();
        if single.is_empty() {
            bail!("--http-token must not be empty");
        }
        if tokens.iter().any(|t| t.name == "cli") {
            bail!("HTTP token name 'cli' clashes with --http-token; rename it in STRUT_STACK_SQL_HTTP_TOKENS");
        }
        tokens.push(HttpToken {
            name: "cli".into(),
            token: single.to_string(),
            mode: process_mode,
            sources: Vec::new(),
        });
    }
    Ok(tokens)
}

/// Resolve the explicit endpoint-hiding override (flag > env > automatic).
fn resolve_hide_endpoints(cli: &Cli) -> Option<bool> {
    if cli.hide_source_endpoints {
        return Some(true);
    }
    for key in [
        "STRUT_STACK_SQL_HIDE_ENDPOINTS",
        "STRUT_STACK_SQL_HIDE_SOURCE_ENDPOINTS",
    ] {
        if let Ok(v) = std::env::var(key) {
            match v.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" => return Some(true),
                "0" | "false" | "no" => return Some(false),
                _ => {}
            }
        }
    }
    None
}

pub async fn load_config(cli: &Cli) -> Result<AppConfig> {
    validate_cli_numbers(cli)?;
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
    let http_tokens = resolve_http_tokens(cli, write_mode)?;
    let hide_endpoints = resolve_hide_endpoints(cli);

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
            // SQL-SEC-08: a TOML source must carry its OWN url/url_env.
            // Never fall back to the process-global DATABASE_URL — a typo
            // would silently point this source at the primary database
            // with the primary credentials.
            let url = resolve_source_url(&src.name, src.url.as_deref(), src.url_env.as_deref())?;
            let url = resolve_sqlite_path(&url, workspace.as_deref());
            let engine = detect_engine(src.engine.as_deref(), &url)?;
            // SQL-BUG-09: duplicate names are a hard config error, not a
            // silent overwrite.
            if sources.contains_key(&src.name) {
                bail!("duplicate source name '{}' in {}", src.name, path.display());
            }
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
        // SQL-BUG-04: deterministic default — the lexicographically
        // smallest source name when the TOML does not name one.
        let default_source = match toml_cfg.default_source {
            Some(name) => {
                if !sources.contains_key(&name) {
                    bail!("default_source '{name}' does not match any configured source");
                }
                name
            }
            None => {
                let name = sources
                    .keys()
                    .min()
                    .cloned()
                    .context("no sources configured in TOML")?;
                tracing::warn!(
                    default_source = %name,
                    "TOML sets no default_source; using lexicographically first source"
                );
                name
            }
        };
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
            http_tokens,
            http_rate_limit_per_minute: cli.http_rate_limit_per_minute,
            allow_unauthenticated_http: cli.allow_unauthenticated_http,
            ephemeral: false,
            hide_endpoints,
        });
    }

    let deny_columns = resolve_deny_columns(cli, None);

    let mut ephemeral = false;
    let url = if let Some(url) = &cli.url {
        url.clone()
    } else if let Some(env_key) = &cli.url_env {
        std::env::var(env_key)
            .with_context(|| format!("environment variable '{env_key}' is not set"))?
    } else {
        match resolve_url_from_env() {
            Ok(url) => url,
            Err(err) => {
                // SQL-GAP-12: the ephemeral fallback exists so MCP
                // initialize / tools/list succeed on a first run without
                // credentials — but it must never be silent. It is an
                // explicit opt-in via --ephemeral /
                // STRUT_STACK_SQL_EPHEMERAL=1; without that opt-in the
                // server still starts (first-run compatibility) but with
                // a loud multi-line banner, and ServerInfo announces the
                // ephemeral mode to the client.
                ephemeral = true;
                let explicit = cli.ephemeral
                    || std::env::var("STRUT_STACK_SQL_EPHEMERAL")
                        .map(|v| matches!(v.trim(), "1" | "true" | "TRUE"))
                        .unwrap_or(false);
                if explicit {
                    tracing::warn!(
                        error = %err,
                        "ephemeral mode explicitly enabled: using in-memory SQLite; all data is lost on restart"
                    );
                } else {
                    tracing::error!(
                        "\n==================== EPHEMERAL DATABASE ====================\n\
                         No database URL was found ({err}).\n\
                         Falling back to an IN-MEMORY SQLite database so the MCP\n\
                         handshake can succeed. This database is EMPTY and all\n\
                         writes are LOST when the server stops.\n\
                         Set DATABASE_URL (or pass --url / --config) for a real\n\
                         database, or pass --ephemeral to acknowledge this mode.\n\
                         =============================================================="
                    );
                }
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
        http_tokens,
        http_rate_limit_per_minute: cli.http_rate_limit_per_minute,
        allow_unauthenticated_http: cli.allow_unauthenticated_http,
        ephemeral,
        hide_endpoints,
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

/// Resolve the URL of ONE named TOML source (SQL-SEC-08). Fail-closed:
/// a source without its own `url`/`url_env`, or whose `url_env` does not
/// resolve, is a configuration error naming the source — it must never
/// silently inherit the process-global `DATABASE_URL`.
fn resolve_source_url(
    source_name: &str,
    url: Option<&str>,
    url_env: Option<&str>,
) -> Result<String> {
    if let Some(raw) = url {
        if raw.trim().is_empty() {
            bail!("source '{source_name}': url is empty");
        }
        return expand_env_placeholders(raw)
            .with_context(|| format!("source '{source_name}': invalid url"));
    }
    if let Some(key) = url_env {
        return std::env::var(key).with_context(|| {
            format!(
                "source '{source_name}': environment variable '{key}' (from url_env) is not set"
            )
        });
    }
    bail!(
        "source '{source_name}': no url or url_env configured; each TOML source must define its own connection (refusing to fall back to the global DATABASE_URL)"
    )
}

fn expand_env_placeholders(raw: &str) -> Result<String> {
    if raw.starts_with("${") && raw.ends_with('}') {
        let key = &raw[2..raw.len() - 1];
        return std::env::var(key)
            .with_context(|| format!("environment variable '{key}' is not set"));
    }
    Ok(raw.to_string())
}

/// Percent-encode a URL component (SQL-SEC-10): everything except the
/// RFC 3986 unreserved set `A-Z a-z 0-9 - . _ ~` is `%XX`-encoded, so a
/// password like `p@ss:w/rd` can neither corrupt the URL nor redirect it
/// to a different host/database.
pub fn url_encode_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
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
    Some(format!(
        "postgresql://{}:{}@{host}:{port}/{}",
        url_encode_component(&user),
        url_encode_component(&password),
        url_encode_component(&db)
    ))
}

fn build_mysql_url_from_parts() -> Option<String> {
    let host = std::env::var("MYSQL_HOST").ok()?;
    let user = std::env::var("MYSQL_USER").ok()?;
    let password = std::env::var("MYSQL_PASSWORD").ok().unwrap_or_default();
    let db = std::env::var("MYSQL_DATABASE")
        .or_else(|_| std::env::var("MYSQL_DB"))
        .ok()?;
    let port = std::env::var("MYSQL_PORT").unwrap_or_else(|_| "3306".into());
    Some(format!(
        "mysql://{}:{}@{host}:{port}/{}",
        url_encode_component(&user),
        url_encode_component(&password),
        url_encode_component(&db)
    ))
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

    #[test]
    fn url_encode_component_encodes_reserved_chars() {
        assert_eq!(url_encode_component("p@ss:w/rd"), "p%40ss%3Aw%2Frd");
        assert_eq!(url_encode_component("plain-user_1.~"), "plain-user_1.~");
        assert_eq!(url_encode_component("a b#c?d"), "a%20b%23c%3Fd");
    }

    #[test]
    fn postgres_parts_url_encodes_password() {
        // Unique values; no other test in this binary reads PG* part envs.
        std::env::set_var("POSTGRES_HOST", "db.internal");
        std::env::set_var("POSTGRES_USER", "us er");
        std::env::set_var("POSTGRES_PASSWORD", "p@ss:w/rd");
        std::env::set_var("POSTGRES_DB", "app/db");
        std::env::set_var("POSTGRES_PORT", "5432");
        std::env::remove_var("PGHOST");
        let url = build_postgres_url_from_parts().expect("parts url");
        assert!(
            url.contains("us%20er:p%40ss%3Aw%2Frd@db.internal:5432/app%2Fdb"),
            "unexpected encoded url: {url}"
        );
    }

    #[test]
    fn parse_http_tokens_full_format() {
        let tokens =
            parse_http_tokens("alice:s3cr3t:writes:main|reporting, bob:tok123:readonly").unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].name, "alice");
        assert_eq!(tokens[0].mode, WriteMode::AllowWrites);
        assert_eq!(
            tokens[0].sources,
            vec!["main".to_string(), "reporting".to_string()]
        );
        assert!(tokens[0].allows_source("main"));
        assert!(!tokens[0].allows_source("archive"));
        assert_eq!(tokens[1].mode, WriteMode::ReadOnly);
        assert!(tokens[1].allows_source("anything"));
        // Process ceiling caps the token mode.
        assert_eq!(
            tokens[0].effective_mode(WriteMode::ReadOnly),
            WriteMode::ReadOnly
        );
    }

    #[test]
    fn parse_http_tokens_rejects_garbage() {
        assert!(parse_http_tokens("no-colons").is_err());
        assert!(parse_http_tokens("a:t:wrongmode").is_err());
        assert!(parse_http_tokens("a:t:readonly,b:t:readonly").is_ok());
        assert!(parse_http_tokens("a:t:readonly,a:u:readonly").is_err());
    }

    #[test]
    fn toml_unknown_fields_rejected() {
        // SQL-BUG-09: typos in TOML keys must not be silently ignored.
        let bad = "default_source = \"x\"\n[[sources]]\nname = \"x\"\nurl = \"sqlite::memory:\"\nurlenv = \"TYPO\"\n";
        assert!(toml::from_str::<TomlConfig>(bad).is_err());
        let bad_top = "defualt_source = \"x\"\n";
        assert!(toml::from_str::<TomlConfig>(bad_top).is_err());
    }

    fn test_cli(config: Option<PathBuf>) -> Cli {
        Cli {
            url: None,
            url_env: None,
            config,
            workspace: None,
            eager_connect: false,
            allow_writes: false,
            allow_ddl: false,
            full_tools: false,
            http: None,
            http_token: None,
            allow_unauthenticated_http: false,
            http_rate_limit_per_minute: 600,
            ephemeral: false,
            hide_source_endpoints: false,
            max_rows: DEFAULT_MAX_ROWS,
            max_bytes: DEFAULT_MAX_BYTES,
            pool_size: DEFAULT_POOL_SIZE,
            query_timeout: DEFAULT_QUERY_TIMEOUT_SECS,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT_SECS,
            batch_concurrency: DEFAULT_BATCH_CONCURRENCY,
            fail_fast: false,
            deny_columns: vec![],
        }
    }

    fn write_temp_toml(tag: &str, body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "strut-stack-sql-test-{tag}-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, body).expect("write temp toml");
        path
    }

    #[tokio::test]
    async fn toml_duplicate_source_is_error() {
        let path = write_temp_toml(
            "dup",
            "[[sources]]\nname = \"a\"\nurl = \"sqlite::memory:\"\n[[sources]]\nname = \"a\"\nurl = \"sqlite::memory:\"\n",
        );
        let err = load_config(&test_cli(Some(path))).await.unwrap_err();
        assert!(
            err.to_string().contains("duplicate source name 'a'"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn toml_source_without_url_is_error_naming_source() {
        let path = write_temp_toml("nourl", "[[sources]]\nname = \"orphan\"\n");
        let err = load_config(&test_cli(Some(path))).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("orphan"), "{msg}");
    }

    #[tokio::test]
    async fn toml_default_source_is_deterministic() {
        let path = write_temp_toml(
            "default",
            "[[sources]]\nname = \"zeta\"\nurl = \"sqlite::memory:\"\n[[sources]]\nname = \"alpha\"\nurl = \"sqlite::memory:\"\n",
        );
        let cfg = load_config(&test_cli(Some(path))).await.unwrap();
        assert_eq!(cfg.default_source, "alpha");
    }

    #[tokio::test]
    async fn cli_numeric_validation_rejects_zero_max_rows() {
        let mut cli = test_cli(None);
        cli.max_rows = 0;
        let err = load_config(&cli).await.unwrap_err();
        assert!(err.to_string().contains("--max-rows"), "{err}");
    }
}
