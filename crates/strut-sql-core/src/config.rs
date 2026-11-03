use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use std::sync::Arc;

use strut_sql_driver_api::{EngineDriver, EngineKind};
pub use strut_sql_guard::WriteMode;

use crate::auth::{Authorization, Grant, GrantSource, MaskMode, Policy, Tier};
use crate::registry::{
    detect_engine_from_url, parse_host_port, LazyDriver, DEFAULT_CONNECT_TIMEOUT_SECS,
};

pub const DEFAULT_MAX_ROWS: u32 = 100;
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;
pub const DEFAULT_POOL_SIZE: u32 = 10;
pub const DEFAULT_QUERY_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_BATCH_CONCURRENCY: usize = 8;
/// PRD-F2-10 batch governance defaults.
pub const DEFAULT_BATCH_MAX_STATEMENTS: usize = 50;
pub const DEFAULT_BATCH_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_BATCH_MAX_BYTES: usize = 1024 * 1024;

/// One HTTP bearer token with its own authorization grant (SQL-SEC-05 /
/// SQL-GAP-01, pragmatic v1.1 form): a name, the secret, the max
/// [`WriteMode`] it may exercise, and an optional source allowlist
/// (empty = all sources).
///
/// `token` may be a raw secret or a SHA-256 digest written as
/// `sha256:<64 lowercase hex chars>`. Hashed entries are compared in
/// constant time; raw entries are hashed on both sides so the comparison is
/// fixed-length. Secrets starting with the literal prefix `sha256:` are
/// therefore always interpreted as digests.
#[derive(Clone, PartialEq, Eq)]
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

    /// True when the stored credential is a `sha256:<hex>` digest.
    pub fn is_hashed(&self) -> bool {
        self.token.starts_with(SHA256_PREFIX)
    }

    /// Constant-time verification of a presented bearer token.
    pub fn verify(&self, presented: &str) -> bool {
        verify_token_secret(&self.token, presented)
    }
}

/// Never print token material — only whether it is hashed.
impl std::fmt::Debug for HttpToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpToken")
            .field("name", &self.name)
            .field(
                "token",
                &if self.is_hashed() {
                    "<sha256-digest>"
                } else {
                    "<redacted>"
                },
            )
            .field("mode", &self.mode)
            .field("sources", &self.sources)
            .finish()
    }
}

const SHA256_PREFIX: &str = "sha256:";

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Encode a raw API key for storage in configuration:
/// `sha256:<64 hex chars>`.
pub fn hash_token(secret: &str) -> String {
    format!("{SHA256_PREFIX}{}", sha256_hex(secret.as_bytes()))
}

fn decode_sha256_hex(raw: &str) -> Option<[u8; 32]> {
    if raw.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in raw.as_bytes().chunks_exact(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// Constant-time credential comparison supporting raw and hashed storage.
pub fn verify_token_secret(stored: &str, presented: &str) -> bool {
    if let Some(hex) = stored.strip_prefix(SHA256_PREFIX) {
        let Some(expected) = decode_sha256_hex(hex) else {
            // A malformed digest must never match anything.
            return false;
        };
        let actual = Sha256::digest(presented.as_bytes());
        ct_eq(actual.as_slice(), &expected)
    } else {
        // Hash both sides: fixed-length, constant-time even for raw secrets.
        let stored_digest = Sha256::digest(stored.as_bytes());
        let presented_digest = Sha256::digest(presented.as_bytes());
        ct_eq(stored_digest.as_slice(), presented_digest.as_slice())
    }
}

/// Parse the `STRUT_STACK_SQL_HTTP_TOKENS` format: comma-separated entries
/// of `name:token:mode[:source1|source2|...]`, e.g.
/// `alice:s3cr3t:writes:main|reporting,bob:tok123:readonly`.
///
/// A token may be given as a `sha256:<hex>` digest (F1-08), e.g.
/// `alice:sha256:<64 hex>:writes:main|reporting`. Token secrets must not
/// contain `:` or `,` other than in the `sha256:` digest prefix.
pub fn parse_http_tokens(spec: &str) -> Result<Vec<HttpToken>> {
    let mut out: Vec<HttpToken> = Vec::new();
    for entry in spec.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let parts: Vec<&str> = entry.split(':').collect();
        let (name, token, mode, sources) = parse_token_parts(&parts)
            .with_context(|| format!("invalid HTTP token entry '{entry}'"))?;
        let name = name.trim().to_string();
        let token = token.trim().to_string();
        if name.is_empty() || token.is_empty() {
            bail!("invalid HTTP token entry '{entry}': name and token must be non-empty");
        }
        if out.iter().any(|t: &HttpToken| t.name == name) {
            bail!("duplicate HTTP token name '{name}'");
        }
        let mode =
            WriteMode::parse_mode(mode).with_context(|| format!("in HTTP token entry '{name}'"))?;
        out.push(HttpToken {
            name,
            token,
            mode,
            sources,
        });
    }
    Ok(out)
}

type ParsedTokenEntry<'a> = (&'a str, String, &'a str, Vec<String>);

fn parse_token_parts<'a>(parts: &[&'a str]) -> Result<ParsedTokenEntry<'a>> {
    let sources = |raw: Option<&str>| -> Vec<String> {
        raw.map(|raw| {
            raw.split('|')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
    };

    // `name:sha256:<hex>:mode[:sources]` — the digest prefix adds one
    // colon to the legacy shape.
    if parts.len() >= 4 && parts[1].trim() == "sha256" {
        let hex = parts[2].trim();
        if decode_sha256_hex(hex).is_none() {
            bail!("invalid sha256 digest '{hex}' (expected 64 hex characters)");
        }
        return match parts.len() {
            4 => Ok((parts[0], format!("sha256:{hex}"), parts[3], Vec::new())),
            5 => Ok((
                parts[0],
                format!("sha256:{hex}"),
                parts[3],
                sources(Some(parts[4])),
            )),
            _ => bail!("expected name:sha256:<hex>:mode[:source1|source2]"),
        };
    }

    match parts.len() {
        3 => Ok((parts[0], parts[1].to_string(), parts[2], Vec::new())),
        4 => Ok((
            parts[0],
            parts[1].to_string(),
            parts[2],
            sources(Some(parts[3])),
        )),
        _ => bail!("expected name:token:mode[:source1|source2]"),
    }
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum Command {
    /// Check configuration, credentials and connectivity; non-zero exit on problems.
    Doctor,
    /// Translate a V1 deployment configuration to V2.
    Migrate {
        /// Treat the input as a V1 (1.x) configuration.
        #[arg(long)]
        from_v1: bool,
        /// V1 TOML file to read (default: ./strut-stack-sql.toml or the --config file).
        #[arg(long)]
        input: Option<PathBuf>,
        /// Write the V2 file instead of printing it.
        #[arg(long)]
        write: bool,
        /// Output path for --write (default: ./strut-stack-sql.toml).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Parser)]
#[command(
    name = "strut-stack-sql",
    version,
    about = "Strut Stack SQL — token-efficient MCP for MySQL, PostgreSQL, and SQLite"
)]
pub struct Cli {
    /// Maintenance subcommands. Without one the server starts normally.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Database connection URL (overrides .env discovery).
    #[arg(long, global = true)]
    pub url: Option<String>,

    /// Environment variable holding the connection URL.
    #[arg(long, global = true)]
    pub url_env: Option<String>,

    /// Path to strut-stack-sql.toml for multi-source configuration.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Workspace root for .env discovery and SQLite relative paths.
    #[arg(long, global = true)]
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

    /// Tool profile preset: lite, standard, full, or admin (R4-02). Explicit
    /// flags may raise, never lower, what the profile sets.
    #[arg(long, global = true)]
    pub profile: Option<String>,

    /// Directories or files a SQLite source may live under (repeatable).
    #[arg(long, global = true, value_delimiter = ',')]
    pub sqlite_allowlist: Vec<PathBuf>,

    /// Include the prepared SQL text in trace/log events (off by default;
    /// ignored when the grant carries table rules or row filters).
    #[arg(long, global = true)]
    pub trace_sql: bool,

    /// Require an accepted approval for destructive DDL (drop/truncate/drop
    /// column). Also: STRUT_STACK_SQL_REQUIRE_APPROVAL=1.
    #[arg(long)]
    pub require_approval: bool,

    /// Enable Admin-class SQL (GRANT/REVOKE) for principals whose grant has
    /// max_tier = "admin". Requires --allow-ddl as well. Also:
    /// STRUT_STACK_SQL_ALLOW_GRANTS=1. Default: denied even for Admin grants.
    #[arg(long)]
    pub allow_grants: bool,

    /// Allow analyze_query_performance(analyze=true). EXPLAIN ANALYZE
    /// executes the statement, so this is always an explicit opt-in.
    /// Also: STRUT_STACK_SQL_ALLOW_EXPLAIN_ANALYZE=1.
    #[arg(long)]
    pub allow_explain_analyze: bool,

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
    #[arg(long, global = true, default_value_t = DEFAULT_MAX_ROWS)]
    pub max_rows: u32,

    /// Max response bytes per query result.
    #[arg(long, global = true, default_value_t = DEFAULT_MAX_BYTES)]
    pub max_bytes: usize,

    /// Connection pool size per source.
    #[arg(long, default_value_t = DEFAULT_POOL_SIZE)]
    pub pool_size: u32,

    /// Query timeout in seconds.
    #[arg(long, global = true, default_value_t = DEFAULT_QUERY_TIMEOUT_SECS)]
    pub query_timeout: u64,

    /// Pool acquire / connect timeout in seconds (default: 2).
    #[arg(long, global = true, default_value_t = DEFAULT_CONNECT_TIMEOUT_SECS)]
    pub connect_timeout: u64,

    /// Max concurrent queries in a batch.
    #[arg(long, default_value_t = DEFAULT_BATCH_CONCURRENCY)]
    pub batch_concurrency: usize,

    /// Max statements accepted in one execute_sql batch (PRD-F2-10).
    #[arg(long, default_value_t = DEFAULT_BATCH_MAX_STATEMENTS)]
    pub batch_max_statements: usize,

    /// Total wall-clock deadline for one batch, in seconds (PRD-F2-10).
    #[arg(long, default_value_t = DEFAULT_BATCH_TIMEOUT_SECS)]
    pub batch_timeout: u64,

    /// Total result-byte ceiling across all batch items (PRD-F2-10).
    #[arg(long, default_value_t = DEFAULT_BATCH_MAX_BYTES)]
    pub batch_max_bytes: usize,

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
    /// Principal declarations (id -> optional display name).
    #[serde(default)]
    pub principals: Vec<PrincipalConfig>,
    /// Authorization grants (principal x source x tier [+ ceilings]).
    #[serde(default)]
    pub grants: Vec<GrantConfig>,
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

/// `[[principals]]` table: `id` plus an optional display name.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrincipalConfig {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
}

/// `[[grants]]` table. `source` defaults to `*` (every source); `max_tier`
/// accepts `read`, `writes`, `ddl`, or `admin`; the ceilings are optional.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantConfig {
    pub principal: String,
    #[serde(default = "default_grant_source")]
    pub source: String,
    pub max_tier: String,
    #[serde(default)]
    pub max_rows: Option<u32>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// Per-call query timeout in seconds.
    #[serde(default)]
    pub query_timeout: Option<u64>,
    /// Opt-in in-memory session query history (PRD-F3-08). Default false.
    #[serde(default)]
    pub query_history: bool,
    /// Tables this principal may touch. Empty = every table not denied.
    #[serde(default)]
    pub allow_tables: Vec<String>,
    /// Tables this principal can never touch; deny wins over allow (R2-04).
    #[serde(default)]
    pub deny_tables: Vec<String>,
    /// Row filters: `{ table = "orders", predicate = "tenant_id = 7" }` (R2-04).
    #[serde(default)]
    pub row_filters: Vec<RowFilterConfig>,
    /// Column masks: `{ column = "email", mode = "partial" }` (R2-04).
    #[serde(default)]
    pub mask: Vec<MaskConfig>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct RowFilterConfig {
    pub table: String,
    pub predicate: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct MaskConfig {
    /// Restrict the mask to one table (default: every table with that column).
    #[serde(default)]
    pub table: Option<String>,
    pub column: String,
    pub mode: MaskMode,
}

/// Validate and convert the per-grant table rules (R2-04).
///
/// Row-filter predicates are checked by the guard at the point of use; here
/// they are rejected when empty or containing statement separators, so a
/// config typo fails at startup rather than at the first query.
fn table_policy_from_config(grant: &GrantConfig) -> anyhow::Result<crate::auth::TablePolicy> {
    use crate::auth::{ColumnMask, RowFilter, TablePolicy, TableRule};
    let rule = |raw: &str| TableRule {
        schema: raw.rsplit_once('.').map(|(s, _)| s.to_string()),
        table: raw
            .rsplit_once('.')
            .map(|(_, t)| t.to_string())
            .unwrap_or_else(|| raw.to_string()),
    };
    for raw in grant.allow_tables.iter().chain(grant.deny_tables.iter()) {
        if raw.trim().is_empty() {
            bail!("[[grants]] table rule must not be empty");
        }
    }
    let mut filters = Vec::new();
    for filter in &grant.row_filters {
        if filter.predicate.trim().is_empty() {
            bail!(
                "[[grants]] row filter for table '{}' has an empty predicate",
                filter.table
            );
        }
        if filter.predicate.contains(';') {
            bail!(
                "[[grants]] row filter for table '{}' must be a single expression",
                filter.table
            );
        }
        if filter.predicate.contains("--") || filter.predicate.contains("/*") {
            bail!(
                "[[grants]] row filter for table '{}' must not contain SQL comments",
                filter.table
            );
        }
        filters.push(RowFilter {
            schema: filter.table.rsplit_once('.').map(|(s, _)| s.to_string()),
            table: filter
                .table
                .rsplit_once('.')
                .map(|(_, t)| t.to_string())
                .unwrap_or_else(|| filter.table.clone()),
            predicate: filter.predicate.clone(),
        });
    }
    Ok(TablePolicy {
        allow: grant.allow_tables.iter().map(|raw| rule(raw)).collect(),
        deny: grant.deny_tables.iter().map(|raw| rule(raw)).collect(),
        row_filters: filters,
        masks: grant
            .mask
            .iter()
            .map(|mask| ColumnMask {
                table: mask.table.as_deref().map(rule),
                column: mask.column.clone(),
                mode: mask.mode,
            })
            .collect(),
    })
}

fn default_grant_source() -> String {
    "*".to_string()
}

/// One configured source: its metadata plus the lazily connected driver.
#[derive(Debug, Clone)]
pub struct ResolvedSource {
    pub name: String,
    pub url: String,
    pub engine: EngineKind,
    lazy_driver: LazyDriver,
}

impl ResolvedSource {
    pub async fn driver(&self) -> Result<Arc<dyn EngineDriver>> {
        self.lazy_driver.get().await
    }

    pub fn with_driver(
        name: impl Into<String>,
        url: String,
        engine: EngineKind,
        driver: Arc<dyn EngineDriver>,
        read_only: bool,
    ) -> Self {
        let name = name.into();
        Self {
            name: name.clone(),
            url: url.clone(),
            engine,
            lazy_driver: LazyDriver::with_driver(url, engine, driver, read_only),
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
    pub batch_max_statements: usize,
    pub batch_timeout: Duration,
    pub batch_max_bytes: usize,
    pub fail_fast: bool,
    pub deny_columns: Vec<String>,
    /// Explicit opt-in for EXPLAIN ANALYZE in the advisor (PRD-F3-04).
    pub allow_explain_analyze: bool,
    /// Authenticated principal id for this scoped view (history attribution).
    pub principal_id: String,
    /// In-memory session query history enabled for this principal (PRD-F3-08).
    pub query_history: bool,
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
    /// Explicit `--allow-grants` opt-in for Admin-class SQL.
    pub allow_grants: bool,
    /// OAuth 2.1 resource-server settings (None = OAuth path disabled).
    pub oauth: Option<OAuthSettings>,
    /// Authorization policy derived from tokens + TOML principals/grants.
    pub policy: Policy,
    /// Destructive DDL requires an accepted approval (R2-05).
    pub require_approval: bool,
    /// Trace events may carry the prepared SQL text (R2-07). Never enabled
    /// when the grant narrows tables: the statement may name them.
    pub trace_sql: bool,
    /// SQLite sources must live under one of these paths when non-empty.
    pub sqlite_allowlist: Vec<PathBuf>,
    /// Per-principal query concurrency limit (R2-08); 0 disables it.
    pub limiter: std::sync::Arc<crate::limits::PrincipalLimiter>,
    /// Shared approval queue (in-memory, 15-minute TTL).
    pub approvals: std::sync::Arc<crate::approvals::ApprovalQueue>,
    /// Shared migration plan store (in-memory, 1-hour TTL).
    pub plans: std::sync::Arc<crate::migrations::PlanStore>,
    /// Effective table allow/deny, row-filter and mask rules (R2-04). Empty
    /// for the unscoped process view; populated from the grant by
    /// [`AppConfig::scoped_for`].
    pub tables: crate::auth::TablePolicy,
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
            source.driver().await?.ping().await?;
        }
        Ok(())
    }

    /// Whether any HTTP authentication (bearer token or OAuth) is
    /// configured.
    pub fn http_auth_enabled(&self) -> bool {
        !self.http_tokens.is_empty() || self.oauth.is_some()
    }

    /// Derive a per-invocation config view with the authorized write mode
    /// and the grant's per-call ceilings. Used by the tool dispatch so
    /// every handler sees the caller's effective limits, not the process
    /// defaults.
    pub fn scoped_for(&self, authz: &Authorization) -> AppConfig {
        let mut scoped = self.clone();
        scoped.write_mode = authz.write_mode();
        scoped.principal_id = authz.principal_id.clone();
        scoped.query_history = authz.query_history;
        scoped.tables = authz.tables.clone();
        scoped.limiter = self.limiter.clone();
        if !scoped.tables.is_empty() {
            scoped.trace_sql = false;
        }
        // The queue is process-shared: scoped views must see the same entries.
        scoped.approvals = self.approvals.clone();
        scoped.plans = self.plans.clone();
        if let Some(max_rows) = authz.max_rows {
            scoped.max_rows = scoped.max_rows.min(max_rows);
        }
        if let Some(max_bytes) = authz.max_bytes {
            scoped.max_bytes = scoped.max_bytes.min(max_bytes);
        }
        if let Some(timeout) = authz.query_timeout {
            scoped.query_timeout = scoped.query_timeout.min(timeout);
        }
        scoped
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

/// OAuth 2.1 resource-server settings (F1-07). The server validates
/// Bearer JWTs issued by `issuer` for `audience`; it never issues tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthSettings {
    /// Authorization server issuer (`STRUT_STACK_SQL_OAUTH_ISSUER`).
    pub issuer: String,
    /// Expected `aud` (`STRUT_STACK_SQL_OAUTH_AUDIENCE`).
    pub audience: String,
    /// Claim carrying the principal id (`STRUT_STACK_SQL_OAUTH_SUBJECT_CLAIM`,
    /// default `sub`).
    pub subject_claim: String,
}

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Resolve `--allow-grants` / `STRUT_STACK_SQL_ALLOW_GRANTS=1`.
fn resolve_allow_grants(cli: &Cli) -> bool {
    cli.allow_grants || env_truthy("STRUT_STACK_SQL_ALLOW_GRANTS")
}

fn resolve_allow_explain_analyze(cli: &Cli) -> bool {
    cli.allow_explain_analyze || env_truthy("STRUT_STACK_SQL_ALLOW_EXPLAIN_ANALYZE")
}

/// Resolve the optional OAuth resource-server settings.
fn resolve_oauth_settings() -> Result<Option<OAuthSettings>> {
    let read = |key: &str| {
        std::env::var(key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    parse_oauth_settings(
        read("STRUT_STACK_SQL_OAUTH_ISSUER"),
        read("STRUT_STACK_SQL_OAUTH_AUDIENCE"),
        read("STRUT_STACK_SQL_OAUTH_SUBJECT_CLAIM"),
    )
}

fn parse_oauth_settings(
    issuer: Option<String>,
    audience: Option<String>,
    subject_claim: Option<String>,
) -> Result<Option<OAuthSettings>> {
    let subject_claim = subject_claim.unwrap_or_else(|| "sub".to_string());
    match (issuer, audience) {
        (None, None) => Ok(None),
        (Some(_), None) => {
            bail!("STRUT_STACK_SQL_OAUTH_ISSUER is set but STRUT_STACK_SQL_OAUTH_AUDIENCE is not")
        }
        (None, Some(_)) => {
            bail!("STRUT_STACK_SQL_OAUTH_AUDIENCE is set but STRUT_STACK_SQL_OAUTH_ISSUER is not")
        }
        (Some(issuer), Some(audience)) => {
            if !(issuer.starts_with("https://") || issuer.starts_with("http://")) {
                bail!(
                    "STRUT_STACK_SQL_OAUTH_ISSUER must be an absolute http(s) URL (got '{issuer}')"
                );
            }
            let issuer = issuer.trim_end_matches('/').to_string();
            if issuer
                .split_once("://")
                .map(|(_, rest)| rest.is_empty())
                .unwrap_or(true)
            {
                bail!("STRUT_STACK_SQL_OAUTH_ISSUER must include a host (got '{issuer}')");
            }
            Ok(Some(OAuthSettings {
                issuer,
                audience,
                subject_claim,
            }))
        }
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
    if !(1..=10_000).contains(&cli.batch_max_statements) {
        bail!(
            "--batch-max-statements must be between 1 and 10000 (got {})",
            cli.batch_max_statements
        );
    }
    if cli.batch_timeout == 0 {
        bail!("--batch-timeout must be > 0 seconds");
    }
    if cli.batch_max_bytes == 0 {
        bail!("--batch-max-bytes must be > 0");
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

/// Map one legacy HTTP token onto grants: token name -> principal id,
/// token mode -> max tier, source allowlist -> per-source grants (any
/// source when the allowlist is empty).
fn token_grants(token: &HttpToken) -> Vec<Grant> {
    let tier = Tier::from_write_mode(token.mode);
    if token.sources.is_empty() {
        vec![Grant::new(token.name.clone(), GrantSource::Any, tier)]
    } else {
        token
            .sources
            .iter()
            .map(|source| Grant::new(token.name.clone(), GrantSource::Name(source.clone()), tier))
            .collect()
    }
}

/// Build the single authorization policy from the process mode, legacy
/// HTTP tokens, and the optional TOML `[[principals]]` / `[[grants]]`
/// tables. Validation is fail-closed: unknown sources, duplicate grants,
/// out-of-range ceilings, and bad tier names are configuration errors.
fn build_policy(
    write_mode: WriteMode,
    full_tools: bool,
    allow_grants: bool,
    tokens: &[HttpToken],
    principals: &[PrincipalConfig],
    grants: &[GrantConfig],
    source_names: &[String],
) -> Result<Policy> {
    let mut policy = Policy::new(Tier::from_write_mode(write_mode), full_tools, allow_grants);

    let mut seen_principals: std::collections::HashSet<String> = std::collections::HashSet::new();
    for principal in principals {
        let id = principal.id.trim();
        if id.is_empty() {
            bail!("[[principals]] entry with empty id");
        }
        if !seen_principals.insert(id.to_string()) {
            bail!("duplicate principal id '{id}'");
        }
        if let Some(name) = &principal.display_name {
            if !name.trim().is_empty() {
                policy.set_display_name(id, name.trim());
            }
        }
    }

    let mut seen_grants: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for token in tokens {
        policy.set_display_name(&token.name, token.name.clone());
        for grant in token_grants(token) {
            let key = (grant.principal.clone(), grant.source.as_str().to_string());
            if !seen_grants.insert(key) {
                bail!(
                    "duplicate grant for principal '{}' on source '{}'",
                    grant.principal,
                    grant.source.as_str()
                );
            }
            policy.add_grant(grant);
        }
    }

    let mut seen_toml_grants: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for grant in grants {
        let principal = grant.principal.trim();
        if principal.is_empty() {
            bail!("[[grants]] entry with empty principal");
        }
        let max_tier = Tier::parse(&grant.max_tier)
            .with_context(|| format!("[[grants]] for principal '{principal}'"))?;
        let source = GrantSource::parse(&grant.source)
            .with_context(|| format!("[[grants]] for principal '{principal}'"))?;
        if let GrantSource::Name(name) = &source {
            if !source_names.iter().any(|known| known == name) {
                bail!("[[grants]] for principal '{principal}' references unknown source '{name}'");
            }
        }
        if let Some(rows) = grant.max_rows {
            if !(1..=100_000).contains(&rows) {
                bail!(
                    "[[grants]] for principal '{principal}': max_rows must be between 1 and 100000 (got {rows})"
                );
            }
        }
        if grant.max_bytes == Some(0) {
            bail!("[[grants]] for principal '{principal}': max_bytes must be > 0");
        }
        if grant.query_timeout == Some(0) {
            bail!("[[grants]] for principal '{principal}': query_timeout must be > 0 seconds");
        }
        let key = (principal.to_string(), source.as_str().to_string());
        if !seen_toml_grants.insert(key) {
            bail!(
                "duplicate grant for principal '{principal}' on source '{}' in [[grants]]",
                source.as_str()
            );
        }
        // Explicit config wins over the implicit grant derived from a
        // legacy HTTP token entry with the same principal/source.
        policy.set_grant(Grant {
            principal: principal.to_string(),
            source,
            max_tier,
            max_rows: grant.max_rows,
            max_bytes: grant.max_bytes,
            query_timeout: grant.query_timeout.map(Duration::from_secs),
            query_history: grant.query_history,
            tables: table_policy_from_config(grant)?,
        });
    }

    if allow_grants && !write_mode.allows_ddl() {
        tracing::warn!(
            "--allow-grants is set without --allow-ddl: Admin-class statements stay denied"
        );
    }

    // The synthesized local stdio principal defaults to the process
    // ceiling on all sources unless config declared its own grants.
    Ok(policy.with_local_default())
}

/// Extract the filesystem path from a sqlite URL. `None` for in-memory
/// databases (which are always allowed).
pub fn sqlite_path(url: &str) -> Option<PathBuf> {
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))?;
    let rest = rest.split('?').next().unwrap_or(rest);
    if rest.is_empty() || rest == ":memory:" || rest == "memory:" {
        return None;
    }
    Some(PathBuf::from(rest))
}

/// Reject SQLite sources outside the configured allowlist (R2-08, fail closed).
fn enforce_sqlite_allowlist(
    sources: &std::collections::HashMap<String, ResolvedSource>,
    allowlist: &[PathBuf],
) -> anyhow::Result<()> {
    if allowlist.is_empty() {
        return Ok(());
    }
    let mut names: Vec<&String> = sources.keys().collect();
    names.sort();
    for name in names {
        let source = &sources[name];
        if !matches!(source.engine, EngineKind::Sqlite) {
            continue;
        }
        let Some(path) = sqlite_path(&source.url) else {
            continue; // in-memory
        };
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path))
                .unwrap_or(path.clone())
        };
        let resolved = absolute
            .canonicalize()
            .or_else(|_| -> std::io::Result<PathBuf> {
                let parent = absolute
                    .parent()
                    .ok_or_else(|| std::io::Error::other("no parent directory"))?
                    .canonicalize()?;
                Ok(parent.join(absolute.file_name().unwrap_or_default()))
            })
            .unwrap_or(absolute);
        let allowed = allowlist.iter().any(|root| {
            let root = if root.is_absolute() {
                root.clone()
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(root))
                    .unwrap_or_else(|_| root.clone())
            };
            let root = root.canonicalize().unwrap_or(root);
            resolved == root || resolved.starts_with(&root)
        });
        if !allowed {
            anyhow::bail!(
                "SQLite source '{name}' points outside the allowlist: {} (allowed: {}); \
                 pass --sqlite-allowlist to widen it",
                resolved.display(),
                allowlist
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    Ok(())
}

pub async fn load_config(cli: &Cli) -> Result<AppConfig> {
    validate_cli_numbers(cli)?;
    let workspace = cli.workspace.clone();
    if let Some(ws) = &workspace {
        std::env::set_current_dir(ws)
            .with_context(|| format!("cannot chdir to workspace {}", ws.display()))?;
    }

    let searched_paths = discover_dotenv();
    let profile = match cli.profile.as_deref() {
        Some(name) => Some(crate::profiles::parse(name).ok_or_else(|| {
            anyhow::anyhow!("unknown profile '{name}' (expected lite, standard, full, or admin)")
        })?),
        None => None,
    };
    let flag_mode = if cli.allow_ddl {
        WriteMode::AllowDdl
    } else if cli.allow_writes {
        WriteMode::AllowWrites
    } else {
        WriteMode::ReadOnly
    };
    let profile_mode = profile
        .map(|profile| profile.max_tier.write_mode())
        .unwrap_or(WriteMode::ReadOnly);
    let write_mode = flag_mode.max(profile_mode);
    let full_tools = cli.full_tools || profile.is_some_and(|profile| profile.full_tools);
    let profile_approval = profile.is_some_and(|profile| profile.require_approval);
    let http_tokens = resolve_http_tokens(cli, write_mode)?;
    let hide_endpoints = resolve_hide_endpoints(cli);
    let allow_grants =
        resolve_allow_grants(cli) || profile.is_some_and(|profile| profile.allow_grants);
    let allow_explain_analyze = resolve_allow_explain_analyze(cli);
    let oauth = resolve_oauth_settings()?;

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
                    lazy_driver: LazyDriver::new(
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
        let source_names: Vec<String> = sources.keys().cloned().collect();
        let policy = build_policy(
            write_mode,
            cli.full_tools,
            allow_grants,
            &http_tokens,
            &toml_cfg.principals,
            &toml_cfg.grants,
            &source_names,
        )?;
        let mut policy = policy;
        policy.set_local_query_history(env_truthy("STRUT_STACK_SQL_QUERY_HISTORY"));
        enforce_sqlite_allowlist(&sources, &cli.sqlite_allowlist)?;
        return Ok(AppConfig {
            write_mode,
            full_tools,
            max_rows: cli.max_rows,
            max_bytes: cli.max_bytes,
            query_timeout: Duration::from_secs(cli.query_timeout),
            batch_concurrency: cli.batch_concurrency,
            batch_max_statements: cli.batch_max_statements,
            batch_timeout: Duration::from_secs(cli.batch_timeout),
            batch_max_bytes: cli.batch_max_bytes,
            fail_fast: cli.fail_fast,
            deny_columns,
            allow_explain_analyze,
            principal_id: crate::auth::Principal::LOCAL_ID.to_string(),
            query_history: false,
            default_source,
            sources,
            searched_paths,
            workspace,
            http_tokens,
            http_rate_limit_per_minute: cli.http_rate_limit_per_minute,
            allow_unauthenticated_http: cli.allow_unauthenticated_http,
            allow_grants,
            oauth,
            policy,
            trace_sql: cli.trace_sql,
            sqlite_allowlist: cli.sqlite_allowlist.clone(),
            limiter: std::sync::Arc::new(crate::limits::PrincipalLimiter::new(
                cli.pool_size as usize,
            )),
            require_approval: cli.require_approval
                || profile_approval
                || env_truthy("STRUT_STACK_SQL_REQUIRE_APPROVAL"),
            approvals: std::sync::Arc::new(crate::approvals::ApprovalQueue::new()),
            plans: std::sync::Arc::new(crate::migrations::PlanStore::new()),
            tables: crate::auth::TablePolicy::default(),
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
            lazy_driver: LazyDriver::new(
                url,
                engine,
                pool_size,
                read_only,
                connect_timeout,
                statement_timeout,
            ),
        },
    );

    let policy = build_policy(
        write_mode,
        cli.full_tools,
        allow_grants,
        &http_tokens,
        &[],
        &[],
        &["default".to_string()],
    )?;
    let mut policy = policy;
    policy.set_local_query_history(env_truthy("STRUT_STACK_SQL_QUERY_HISTORY"));

    enforce_sqlite_allowlist(&sources, &cli.sqlite_allowlist)?;
    Ok(AppConfig {
        write_mode,
        full_tools,
        max_rows: cli.max_rows,
        max_bytes: cli.max_bytes,
        query_timeout: Duration::from_secs(cli.query_timeout),
        batch_concurrency: cli.batch_concurrency,
        batch_max_statements: cli.batch_max_statements,
        batch_timeout: Duration::from_secs(cli.batch_timeout),
        batch_max_bytes: cli.batch_max_bytes,
        fail_fast: cli.fail_fast,
        deny_columns,
        allow_explain_analyze,
        principal_id: crate::auth::Principal::LOCAL_ID.to_string(),
        query_history: false,
        default_source: "default".into(),
        sources,
        searched_paths,
        workspace,
        http_tokens,
        http_rate_limit_per_minute: cli.http_rate_limit_per_minute,
        allow_unauthenticated_http: cli.allow_unauthenticated_http,
        allow_grants,
        oauth,
        policy,
        trace_sql: cli.trace_sql,
        sqlite_allowlist: cli.sqlite_allowlist.clone(),
        limiter: std::sync::Arc::new(crate::limits::PrincipalLimiter::new(cli.pool_size as usize)),
        require_approval: cli.require_approval
            || profile_approval
            || env_truthy("STRUT_STACK_SQL_REQUIRE_APPROVAL"),
        approvals: std::sync::Arc::new(crate::approvals::ApprovalQueue::new()),
        plans: std::sync::Arc::new(crate::migrations::PlanStore::new()),
        tables: crate::auth::TablePolicy::default(),
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
            EngineKind::MySql => "mysql".into(),
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
        EngineKind::Postgres | EngineKind::MySql => {
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
    use crate::auth::{AuthMethod, Principal};

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
            EngineKind::MySql
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
    fn hashed_token_entry_parses_and_verifies() {
        let digest = hash_token("s3cr3t");
        let spec = format!("alice:{digest}:writes:main|reporting");
        let tokens = parse_http_tokens(&spec).unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].name, "alice");
        assert_eq!(tokens[0].token, digest);
        assert!(tokens[0].is_hashed());
        assert_eq!(tokens[0].mode, WriteMode::AllowWrites);
        assert_eq!(
            tokens[0].sources,
            vec!["main".to_string(), "reporting".to_string()]
        );
        assert!(tokens[0].verify("s3cr3t"));
        assert!(!tokens[0].verify("wrong"));
        assert!(!tokens[0].verify(""));
    }

    #[test]
    fn raw_token_still_verifies() {
        let token = HttpToken {
            name: "a".into(),
            token: "raw-secret".into(),
            mode: WriteMode::ReadOnly,
            sources: vec![],
        };
        assert!(!token.is_hashed());
        assert!(token.verify("raw-secret"));
        assert!(!token.verify("raw-secre"));
    }

    #[test]
    fn malformed_hash_never_matches() {
        assert!(!verify_token_secret("sha256:nothex", "nothex"));
        assert!(!verify_token_secret(
            &format!("sha256:{}", "0".repeat(63)),
            "x"
        ));
        assert!(verify_token_secret(&hash_token("abc"), "abc"));
    }

    #[test]
    fn debug_does_not_leak_token_material() {
        let raw = HttpToken {
            name: "a".into(),
            token: "super-secret-value".into(),
            mode: WriteMode::ReadOnly,
            sources: vec![],
        };
        let rendered = format!("{raw:?}");
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");

        let hashed = HttpToken {
            token: hash_token("another-secret"),
            ..raw
        };
        let rendered = format!("{hashed:?}");
        assert!(
            !rendered.contains(&hash_token("another-secret")),
            "{rendered}"
        );
        assert!(!rendered.contains("another-secret"), "{rendered}");
        assert!(rendered.contains("<sha256-digest>"), "{rendered}");
    }

    #[test]
    fn parse_hashed_token_rejects_short_digest() {
        assert!(parse_http_tokens("a:sha256:deadbeef:writes").is_err());
    }

    #[test]
    fn build_policy_maps_legacy_tokens() {
        let tokens = parse_http_tokens("alice:tok:writes:main,bob:tok2:readonly").unwrap();
        let policy = build_policy(
            WriteMode::AllowDdl,
            true,
            false,
            &tokens,
            &[],
            &[],
            &["main".to_string(), "reporting".to_string()],
        )
        .unwrap();

        // Legacy token name -> principal id; mode -> tier; sources -> grants.
        let alice = policy.resolve_principal("alice", AuthMethod::ApiKey);
        let allowed = policy.authorize(&alice, "execute_sql", "main", Tier::Write);
        assert!(allowed.is_allowed(), "{allowed:?}");
        let denied = policy.authorize(&alice, "execute_sql", "reporting", Tier::Read);
        assert!(!denied.is_allowed(), "{denied:?}");

        let bob = policy.resolve_principal("bob", AuthMethod::ApiKey);
        assert!(policy
            .authorize(&bob, "execute_sql", "reporting", Tier::Read)
            .is_allowed());
        // Local stdio principal still gets the process ceiling.
        let local = policy.local_principal();
        assert!(policy
            .authorize(&local, "execute_sql", "reporting", Tier::Ddl)
            .is_allowed());
    }

    #[test]
    fn build_policy_toml_grant_overrides_legacy_token_grant() {
        let tokens = parse_http_tokens("root:tok:ddl").unwrap();
        let grants = vec![GrantConfig {
            principal: "root".into(),
            source: "*".into(),
            max_tier: "admin".into(),
            max_rows: None,
            max_bytes: None,
            query_timeout: None,
            query_history: false,
            allow_tables: vec![],
            deny_tables: vec![],
            row_filters: vec![],
            mask: vec![],
        }];
        let policy = build_policy(
            WriteMode::AllowDdl,
            true,
            true,
            &tokens,
            &[],
            &grants,
            &["main".to_string()],
        )
        .unwrap();
        let root = policy.resolve_principal("root", AuthMethod::ApiKey);
        match policy.authorize(&root, "execute_sql", "main", Tier::Admin) {
            crate::auth::Decision::Allow(authz) => assert_eq!(authz.tier, Tier::Admin),
            other => panic!("expected admin allow: {other:?}"),
        }
    }

    #[test]
    fn build_policy_honours_toml_grants_and_local_restriction() {
        let principals = vec![PrincipalConfig {
            id: "alice".into(),
            display_name: Some("Alice".into()),
        }];
        let grants = vec![
            GrantConfig {
                principal: "alice".into(),
                source: "main".into(),
                max_tier: "writes".into(),
                max_rows: Some(5),
                max_bytes: Some(2048),
                query_timeout: Some(3),
                query_history: false,
                allow_tables: vec![],
                deny_tables: vec![],
                row_filters: vec![],
                mask: vec![],
            },
            GrantConfig {
                principal: Principal::LOCAL_ID.into(),
                source: "main".into(),
                max_tier: "read".into(),
                max_rows: None,
                max_bytes: None,
                query_timeout: None,
                query_history: false,
                allow_tables: vec![],
                deny_tables: vec![],
                row_filters: vec![],
                mask: vec![],
            },
        ];
        let policy = build_policy(
            WriteMode::AllowDdl,
            true,
            false,
            &[],
            &principals,
            &grants,
            &["main".to_string()],
        )
        .unwrap();

        let alice = policy.resolve_principal("alice", AuthMethod::OAuth);
        assert_eq!(alice.display_name.as_deref(), Some("Alice"));
        match policy.authorize(&alice, "execute_sql", "main", Tier::Write) {
            crate::auth::Decision::Allow(a) => {
                assert_eq!(a.max_rows, Some(5));
                assert_eq!(a.max_bytes, Some(2048));
                assert_eq!(a.query_timeout, Some(Duration::from_secs(3)));
            }
            other => panic!("expected allow: {other:?}"),
        }
        // Config restricted the local principal: no wildcard fallback.
        let local = policy.local_principal();
        assert!(!policy
            .authorize(&local, "execute_sql", "other", Tier::Read)
            .is_allowed());
    }

    #[test]
    fn build_policy_rejects_unknown_grant_source() {
        let grants = vec![GrantConfig {
            principal: "alice".into(),
            source: "nope".into(),
            max_tier: "read".into(),
            max_rows: None,
            max_bytes: None,
            query_timeout: None,
            query_history: false,
            allow_tables: vec![],
            deny_tables: vec![],
            row_filters: vec![],
            mask: vec![],
        }];
        let err = build_policy(
            WriteMode::ReadOnly,
            true,
            false,
            &[],
            &[],
            &grants,
            &["main".to_string()],
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown source 'nope'"), "{err}");
    }

    #[test]
    fn build_policy_rejects_duplicate_and_invalid_grants() {
        let duplicate = vec![
            GrantConfig {
                principal: "alice".into(),
                source: "*".into(),
                max_tier: "read".into(),
                max_rows: None,
                max_bytes: None,
                query_timeout: None,
                query_history: false,
                allow_tables: vec![],
                deny_tables: vec![],
                row_filters: vec![],
                mask: vec![],
            },
            GrantConfig {
                principal: "alice".into(),
                source: "*".into(),
                max_tier: "writes".into(),
                max_rows: None,
                max_bytes: None,
                query_timeout: None,
                query_history: false,
                allow_tables: vec![],
                deny_tables: vec![],
                row_filters: vec![],
                mask: vec![],
            },
        ];
        assert!(build_policy(
            WriteMode::ReadOnly,
            true,
            false,
            &[],
            &[],
            &duplicate,
            &["main".to_string()]
        )
        .is_err());

        let zero_rows = vec![GrantConfig {
            principal: "alice".into(),
            source: "*".into(),
            max_tier: "read".into(),
            max_rows: Some(0),
            max_bytes: None,
            query_timeout: None,
            query_history: false,
            allow_tables: vec![],
            deny_tables: vec![],
            row_filters: vec![],
            mask: vec![],
        }];
        assert!(build_policy(
            WriteMode::ReadOnly,
            true,
            false,
            &[],
            &[],
            &zero_rows,
            &["main".to_string()]
        )
        .is_err());
    }

    #[tokio::test]
    async fn toml_principals_and_grants_load() {
        let path = write_temp_toml(
            "auth",
            r#"
[[sources]]
name = "main"
url = "sqlite::memory:"

[[principals]]
id = "alice"
display_name = "Alice"

[[grants]]
principal = "alice"
source = "main"
max_tier = "writes"
max_rows = 7
"#,
        );
        let mut cli = test_cli(Some(path));
        cli.allow_writes = true;
        let cfg = load_config(&cli).await.unwrap();
        let alice = cfg.policy.resolve_principal("alice", AuthMethod::ApiKey);
        match cfg
            .policy
            .authorize(&alice, "execute_sql", "main", Tier::Write)
        {
            crate::auth::Decision::Allow(authz) => assert_eq!(authz.max_rows, Some(7)),
            other => panic!("expected allow: {other:?}"),
        }
    }

    #[tokio::test]
    async fn toml_bad_grant_tier_is_error() {
        let path = write_temp_toml(
            "badtier",
            "[[sources]]\nname = \"main\"\nurl = \"sqlite::memory:\"\n[[grants]]\nprincipal = \"alice\"\nmax_tier = \"root\"\n",
        );
        let err = load_config(&test_cli(Some(path))).await.unwrap_err();
        assert!(format!("{err:#}").contains("invalid tier"), "{err:#}");
    }

    #[test]
    fn oauth_settings_require_issuer_and_audience_together() {
        assert_eq!(parse_oauth_settings(None, None, None).unwrap(), None);
        assert!(parse_oauth_settings(Some("https://as.example.com".into()), None, None).is_err());
        assert!(parse_oauth_settings(None, Some("aud".into()), None).is_err());
        let settings = parse_oauth_settings(
            Some("https://as.example.com".into()),
            Some("https://mcp.example.com/mcp".into()),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(settings.issuer, "https://as.example.com");
        assert_eq!(settings.audience, "https://mcp.example.com/mcp");
        assert_eq!(settings.subject_claim, "sub");
        let custom = parse_oauth_settings(
            Some("https://as.example.com".into()),
            Some("aud".into()),
            Some("preferred_username".into()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(custom.subject_claim, "preferred_username");
        assert!(
            parse_oauth_settings(Some("as.example.com".into()), Some("aud".into()), None).is_err()
        );
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
            command: None,
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
            allow_grants: false,
            trace_sql: false,
            profile: None,
            sqlite_allowlist: Vec::new(),
            require_approval: false,
            allow_explain_analyze: false,
            http_rate_limit_per_minute: 600,
            ephemeral: false,
            hide_source_endpoints: false,
            max_rows: DEFAULT_MAX_ROWS,
            max_bytes: DEFAULT_MAX_BYTES,
            pool_size: DEFAULT_POOL_SIZE,
            query_timeout: DEFAULT_QUERY_TIMEOUT_SECS,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT_SECS,
            batch_concurrency: DEFAULT_BATCH_CONCURRENCY,
            batch_max_statements: DEFAULT_BATCH_MAX_STATEMENTS,
            batch_timeout: DEFAULT_BATCH_TIMEOUT_SECS,
            batch_max_bytes: DEFAULT_BATCH_MAX_BYTES,
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
