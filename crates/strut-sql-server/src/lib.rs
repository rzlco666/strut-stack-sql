use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, Method, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ListToolsResult, ServerCapabilities, ServerInfo, Tool};
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use strut_sql_core::config::{AppConfig, HttpToken, WriteMode};
use strut_sql_core::redact::redact_url;
use strut_sql_driver_api::EngineKind;
use strut_sql_guard::validate_and_prepare;
use strut_sql_tools::audit::{audit_call, sql_fingerprint};
use strut_sql_tools::core::{
    handle_analyze_query, handle_execute_sql, handle_search_objects, AnalyzeQueryParams,
    BatchQueryItem, ExecuteSqlParams, SearchObjectsParams,
};
use strut_sql_tools::ddl::{
    handle_add_column, handle_alter_column, handle_create_table, handle_drop_column,
    handle_drop_table, handle_schema_mutate, handle_truncate_table, AddColumnParams,
    AlterColumnParams, CreateTableParams, DropColumnParams, DropTableParams, SchemaMutateParams,
    TruncateTableParams,
};
use strut_sql_tools::full::{
    handle_describe_table, handle_list_foreign_keys, handle_list_indexes, handle_list_schemas,
    handle_list_sources, handle_list_tables, handle_schema_overview, DescribeTableParams,
    ListForeignKeysParams, ListIndexesParams, ListTablesParams, SchemaOverviewParams, SourceParams,
};

/// Default tracing filter. The `sqlx` target directive only sets that log
/// target's level (no sqlx dependency outside the driver crates); audit
/// events are info-level so the default filter shows them.
pub const DEFAULT_LOG_FILTER: &str = "info,sqlx=warn,audit=info";

const CORE_TOOLS: &[&str] = &["search_objects", "execute_sql", "analyze_query_performance"];

/// Introspection tools exposed with `--full-tools` (always, including read-only).
const FULL_INTROSPECT_TOOLS: &[&str] = &[
    "list_sources",
    "list_schemas",
    "list_tables",
    "describe_table",
    "list_indexes",
    "list_foreign_keys",
    "schema_overview",
];

/// DDL tools — only listed when `--full-tools` and `--allow-ddl`.
const DDL_TOOLS: &[&str] = &[
    "schema_mutate",
    "create_table",
    "drop_table",
    "add_column",
    "alter_column",
    "drop_column",
    "truncate_table",
];

#[derive(Clone)]
pub struct McpSqlServer {
    config: Arc<AppConfig>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl McpSqlServer {
    pub fn new(config: AppConfig) -> Self {
        Self {
            config: Arc::new(config),
            tool_router: Self::tool_router(),
        }
    }

    /// Invocation-time DDL gate (SQL-SEC-07): `list_tools` filtering is
    /// only visibility — every DDL handler re-checks the process mode
    /// before doing any work (defense in depth).
    fn require_ddl(&self) -> Result<(), McpError> {
        if self.config.write_mode.allows_ddl() {
            Ok(())
        } else {
            Err(McpError::invalid_params(
                "DDL tools require --allow-ddl (invocation rejected)".to_string(),
                None,
            ))
        }
    }
}

#[tool_router]
impl McpSqlServer {
    #[tool(description = "Search schemas, tables, columns, or indexes with keyword filter.")]
    async fn search_objects(
        &self,
        params: Parameters<SearchObjectsParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "search_objects",
            source,
            None,
            handle_search_objects(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Execute one SQL statement or a concurrent batch via queries[].")]
    async fn execute_sql(
        &self,
        params: Parameters<ExecuteSqlParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        let mut acc = String::new();
        if let Some(sql) = &params.0.sql {
            acc.push_str(sql);
        }
        if let Some(queries) = &params.0.queries {
            for q in queries {
                match q {
                    BatchQueryItem::Legacy(sql) => acc.push_str(sql),
                    BatchQueryItem::Parameterized { sql, .. } => acc.push_str(sql),
                }
                acc.push('\n');
            }
        }
        let fp = if acc.is_empty() {
            None
        } else {
            Some(sql_fingerprint(&acc))
        };
        audit_call(
            "execute_sql",
            source,
            fp,
            handle_execute_sql(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "EXPLAIN query plan summary for slow SELECT diagnostics.")]
    async fn analyze_query_performance(
        &self,
        params: Parameters<AnalyzeQueryParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        let fp = Some(sql_fingerprint(&params.0.sql));
        audit_call(
            "analyze_query_performance",
            source,
            fp,
            handle_analyze_query(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "List configured database sources.")]
    async fn list_sources(&self) -> Result<CallToolResult, McpError> {
        audit_call(
            "list_sources",
            None,
            None,
            handle_list_sources(&self.config),
        )
        .await
    }

    #[tool(description = "List schemas/databases.")]
    async fn list_schemas(
        &self,
        params: Parameters<SourceParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "list_schemas",
            source,
            None,
            handle_list_schemas(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "List tables in a schema.")]
    async fn list_tables(
        &self,
        params: Parameters<ListTablesParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "list_tables",
            source,
            None,
            handle_list_tables(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Describe table columns and indexes.")]
    async fn describe_table(
        &self,
        params: Parameters<DescribeTableParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "describe_table",
            source,
            None,
            handle_describe_table(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "List indexes (flat: name, columns, unique) for a schema or table.")]
    async fn list_indexes(
        &self,
        params: Parameters<ListIndexesParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "list_indexes",
            source,
            None,
            handle_list_indexes(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "List foreign keys for a schema or table.")]
    async fn list_foreign_keys(
        &self,
        params: Parameters<ListForeignKeysParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "list_foreign_keys",
            source,
            None,
            handle_list_foreign_keys(&self.config, params.0),
        )
        .await
    }

    #[tool(
        description = "One-shot schema survey: tables + approx row counts (planner stats; omit if unknown; use COUNT(*) for exact) + FKs."
    )]
    async fn schema_overview(
        &self,
        params: Parameters<SchemaOverviewParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "schema_overview",
            source,
            None,
            handle_schema_overview(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "DDL schema mutations (requires --allow-ddl).")]
    async fn schema_mutate(
        &self,
        params: Parameters<SchemaMutateParams>,
    ) -> Result<CallToolResult, McpError> {
        self.require_ddl()?;
        let source = params.0.source.clone();
        audit_call(
            "schema_mutate",
            source,
            None,
            handle_schema_mutate(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Create table from DDL (requires --allow-ddl).")]
    async fn create_table(
        &self,
        params: Parameters<CreateTableParams>,
    ) -> Result<CallToolResult, McpError> {
        self.require_ddl()?;
        let source = params.0.source.clone();
        audit_call(
            "create_table",
            source,
            None,
            handle_create_table(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Drop table; needs confirm:true and --allow-ddl.")]
    async fn drop_table(
        &self,
        params: Parameters<DropTableParams>,
    ) -> Result<CallToolResult, McpError> {
        self.require_ddl()?;
        let source = params.0.source.clone();
        audit_call(
            "drop_table",
            source,
            None,
            handle_drop_table(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Add a column to a table (requires --allow-ddl).")]
    async fn add_column(
        &self,
        params: Parameters<AddColumnParams>,
    ) -> Result<CallToolResult, McpError> {
        self.require_ddl()?;
        let source = params.0.source.clone();
        audit_call(
            "add_column",
            source,
            None,
            handle_add_column(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Alter column type (requires --allow-ddl).")]
    async fn alter_column(
        &self,
        params: Parameters<AlterColumnParams>,
    ) -> Result<CallToolResult, McpError> {
        self.require_ddl()?;
        let source = params.0.source.clone();
        audit_call(
            "alter_column",
            source,
            None,
            handle_alter_column(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Drop column; needs confirm:true and --allow-ddl.")]
    async fn drop_column(
        &self,
        params: Parameters<DropColumnParams>,
    ) -> Result<CallToolResult, McpError> {
        self.require_ddl()?;
        let source = params.0.source.clone();
        audit_call(
            "drop_column",
            source,
            None,
            handle_drop_column(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Truncate table; needs confirm:true and --allow-ddl.")]
    async fn truncate_table(
        &self,
        params: Parameters<TruncateTableParams>,
    ) -> Result<CallToolResult, McpError> {
        self.require_ddl()?;
        let source = params.0.source.clone();
        audit_call(
            "truncate_table",
            source,
            None,
            handle_truncate_table(&self.config, params.0),
        )
        .await
    }
}

#[tool_handler]
impl ServerHandler for McpSqlServer {
    fn get_info(&self) -> ServerInfo {
        let mode = match self.config.write_mode {
            WriteMode::ReadOnly => "read-only",
            WriteMode::AllowWrites => "writes",
            WriteMode::AllowDdl => "ddl",
        };
        let mut instructions = format!(
            "strut-stack-sql {} ({mode}). Core tools: search_objects, execute_sql, analyze_query_performance. \
             Results are compact columnar JSON. Default max {} rows. Use page_offset/page_size on execute_sql for pagination.",
            env!("CARGO_PKG_VERSION"),
            self.config.max_rows
        );
        if self.config.ephemeral {
            instructions.push_str(
                " WARNING: no database was configured, so this server is running on an \
                 EPHEMERAL in-memory SQLite database: it starts out empty and all writes \
                 are lost when the server stops. Configure DATABASE_URL / --url / --config \
                 for a real database.",
            );
        }
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let all = self.tool_router.list_all();
        let mut allowed: Vec<&str> = CORE_TOOLS.to_vec();
        if self.config.full_tools {
            allowed.extend(FULL_INTROSPECT_TOOLS.iter().copied());
            if self.config.write_mode.allows_ddl() {
                allowed.extend(DDL_TOOLS.iter().copied());
            }
        }

        let tools: Vec<Tool> = all
            .into_iter()
            .filter(|t| allowed.iter().any(|name| *name == t.name))
            .collect();

        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }
}

pub async fn run_stdio(config: AppConfig) -> Result<()> {
    let server = McpSqlServer::new(config);
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("stdio serve error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}

#[derive(Clone)]
pub struct HttpState {
    config: Arc<AppConfig>,
}

#[derive(Serialize)]
struct ReadySource {
    name: String,
    engine: &'static str,
    ok: bool,
    status: &'static str,
}

#[derive(Serialize)]
struct ReadyResponse {
    status: &'static str,
    sources: Vec<ReadySource>,
}

#[derive(Serialize)]
struct LiveResponse {
    status: &'static str,
}

fn engine_label(engine: EngineKind) -> &'static str {
    match engine {
        EngineKind::Postgres => "postgres",
        EngineKind::MySql => "mysql",
        EngineKind::Sqlite => "sqlite",
    }
}

/// Liveness (SQL-SEC-06): always 200, no information, never touches a pool.
async fn livez() -> impl IntoResponse {
    (StatusCode::OK, Json(LiveResponse { status: "ok" }))
}

/// Readiness (SQL-SEC-06): per-source ok/degraded ONLY — raw error text
/// goes to the server log (redacted), never to the caller. `/healthz` is
/// kept as a safe alias of this handler.
///
/// Note on cost: `LazyEnginePool` exposes no non-connecting peek, so a
/// readiness probe may establish a source's pool — that is exactly what a
/// readiness probe is for, and each source is bounded by a 2s timeout.
/// `/livez` is the cheap always-on endpoint and never connects.
async fn readyz(State(state): State<HttpState>) -> impl IntoResponse {
    let mut names: Vec<String> = state.config.sources.keys().cloned().collect();
    names.sort();
    let mut sources = Vec::new();
    let mut all_ok = true;

    for name in names {
        let source = &state.config.sources[&name];
        let check = async {
            let driver = source.driver().await?;
            driver
                .ping()
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        };
        let ok = match tokio::time::timeout(Duration::from_secs(2), check).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!(
                    target: "health",
                    source = %name,
                    error = %redact_url(&e.to_string()),
                    "readiness check failed"
                );
                false
            }
            Err(_) => {
                tracing::warn!(
                    target: "health",
                    source = %name,
                    "readiness check timed out after 2s"
                );
                false
            }
        };
        if !ok {
            all_ok = false;
        }
        sources.push(ReadySource {
            name: source.name.clone(),
            engine: engine_label(source.engine),
            ok,
            status: if ok { "ok" } else { "degraded" },
        });
    }

    let status = if all_ok { "ok" } else { "degraded" };
    let code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (code, Json(ReadyResponse { status, sources }))
}

// ---------------------------------------------------------------------------
// HTTP auth + per-token authorization + rate limiting
// (SQL-SEC-05, SQL-GAP-01 pragmatic form, SQL-GAP-04 light form)
//
// When tokens are configured, every /mcp request needs
// `Authorization: Bearer <token>`. For JSON-RPC `tools/call` POSTs the
// middleware additionally enforces, BEFORE the request reaches the MCP
// service:
//   * the requested source must be inside the token's allowlist;
//   * DDL tools require the token's effective mode (token mode capped by
//     the process mode) to allow DDL;
//   * execute_sql / analyze_query_performance SQL is run through the SQL
//     guard with the token's effective mode, so a read-only token cannot
//     smuggle an INSERT past a writes-enabled process.
// Non-loopback binds without any token are refused in `run_http` unless
// --allow-unauthenticated-http was given.
// ---------------------------------------------------------------------------

const MAX_MCP_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct HttpAuthState {
    config: Arc<AppConfig>,
    /// Per-token-name fixed-window counters: name -> (window start, count).
    rate: Arc<Mutex<HashMap<String, (Instant, u64)>>>,
}

impl HttpAuthState {
    fn rate_limited(&self, token_name: &str) -> bool {
        let limit = self.config.http_rate_limit_per_minute;
        if limit == 0 {
            return false;
        }
        let now = Instant::now();
        let mut map = self.rate.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(token_name.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= Duration::from_secs(60) {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 > limit as u64
    }
}

fn unauthorized_response() -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized: missing or invalid bearer token" })),
    )
        .into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        header::HeaderValue::from_static("Bearer"),
    );
    resp
}

fn jsonrpc_denied(id: &Value, message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "jsonrpc": "2.0",
            "id": id.clone(),
            "error": { "code": -32003, "message": message }
        })),
    )
        .into_response()
}

fn authenticate(config: &AppConfig, req: &Request<Body>) -> Option<HttpToken> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, presented) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let presented = presented.trim();
    config
        .http_tokens
        .iter()
        .find(|t| t.token == presented)
        .cloned()
}

/// Returns `Some(denial message)` when this single JSON-RPC request must
/// not proceed under the token's grant.
fn check_jsonrpc_request(config: &AppConfig, token: &HttpToken, value: &Value) -> Option<String> {
    if value.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    let params = value.get("params")?;
    let tool = params.get("name").and_then(Value::as_str)?;
    let empty = Value::Object(serde_json::Map::new());
    let args = params.get("arguments").unwrap_or(&empty);
    let source_name = args
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or(&config.default_source)
        .to_string();

    if !token.allows_source(&source_name) {
        return Some(format!(
            "token '{}' is not allowed to use source '{source_name}'",
            token.name
        ));
    }

    let effective = token.effective_mode(config.write_mode);

    if DDL_TOOLS.contains(&tool) {
        if !effective.allows_ddl() {
            return Some(format!(
                "token '{}' (mode {:?}) may not call DDL tool '{tool}'",
                token.name, effective
            ));
        }
        return None;
    }

    if tool == "execute_sql" || tool == "analyze_query_performance" {
        let Ok(source) = config.source(Some(&source_name)) else {
            return None; // unknown source: the handler produces the error
        };
        let mut statements: Vec<(String, Vec<Value>)> = Vec::new();
        if let Some(sql) = args.get("sql").and_then(Value::as_str) {
            let p = args
                .get("params")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            statements.push((sql.to_string(), p));
        }
        if let Some(queries) = args.get("queries").and_then(Value::as_array) {
            for item in queries {
                match item {
                    Value::String(sql) => statements.push((sql.clone(), Vec::new())),
                    Value::Object(obj) => {
                        if let Some(sql) = obj.get("sql").and_then(Value::as_str) {
                            let p = obj
                                .get("params")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            statements.push((sql.to_string(), p));
                        }
                    }
                    _ => {}
                }
            }
        }
        for (sql, p) in &statements {
            if let Err(e) = validate_and_prepare(sql, p, source.engine, effective, config.max_rows)
            {
                return Some(format!(
                    "token '{}' (mode {:?}) may not run this SQL: {e}",
                    token.name, effective
                ));
            }
        }
    }

    None
}

async fn http_auth_middleware(
    State(auth): State<HttpAuthState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // No tokens configured: local/loopback mode (run_http refuses
    // unauthenticated non-loopback binds before we ever get here).
    if !auth.config.http_auth_enabled() {
        return next.run(req).await;
    }

    let Some(token) = authenticate(&auth.config, &req) else {
        return unauthorized_response();
    };

    if auth.rate_limited(&token.name) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "rate limit exceeded for this token" })),
        )
            .into_response();
    }

    // Session GET (SSE stream) / DELETE carry no JSON-RPC body to inspect.
    if req.method() != Method::POST {
        return next.run(req).await;
    }

    let (mut parts, body) = req.into_parts();
    let bytes: Bytes = match axum::body::to_bytes(body, MAX_MCP_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({ "error": "request body too large" })),
            )
                .into_response()
        }
    };

    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
        let items: Vec<&Value> = match &value {
            Value::Array(arr) => arr.iter().collect(),
            single => vec![single],
        };
        for item in items {
            if let Some(message) = check_jsonrpc_request(&auth.config, &token, item) {
                tracing::warn!(
                    target: "audit",
                    token = %token.name,
                    "http tools/call denied by token policy"
                );
                let id = item.get("id").cloned().unwrap_or(Value::Null);
                return jsonrpc_denied(&id, &message);
            }
        }
    }
    // Unparseable bodies are passed through for the MCP service to reject.

    // Content-Length changed meaning after buffering; drop the stale one.
    parts.headers.remove(header::CONTENT_LENGTH);
    let req = Request::from_parts(parts, Body::from(bytes));
    next.run(req).await
}

pub fn build_http_router(config: AppConfig) -> (Router, CancellationToken) {
    let ct = CancellationToken::new();
    let factory_config = config.clone();
    let http_state = HttpState {
        config: Arc::new(config.clone()),
    };
    let auth_state = HttpAuthState {
        config: Arc::new(config),
        rate: Arc::new(Mutex::new(HashMap::new())),
    };

    let service = StreamableHttpService::new(
        move || Ok(McpSqlServer::new(factory_config.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let mcp_router =
        Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn_with_state(
                auth_state,
                http_auth_middleware,
            ));

    let router = Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/healthz", get(readyz))
        .with_state(http_state)
        .merge(mcp_router);

    (router, ct)
}

fn is_loopback_bind(addr: &str) -> bool {
    if let Ok(sock) = addr.parse::<std::net::SocketAddr>() {
        return sock.ip().is_loopback();
    }
    let host = addr
        .rsplit_once(':')
        .map(|(h, _)| h.trim_matches(['[', ']']))
        .unwrap_or(addr);
    host.eq_ignore_ascii_case("localhost") || host == "::1"
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

pub async fn run_http(config: AppConfig, addr: &str) -> Result<()> {
    // SQL-SEC-05: never serve an unauthenticated MCP endpoint on a
    // non-loopback interface unless the operator explicitly opted in.
    let loopback = is_loopback_bind(addr);
    if !loopback && !config.http_auth_enabled() && !config.allow_unauthenticated_http {
        anyhow::bail!(
            "refusing to start HTTP on non-loopback address {addr} without authentication: \
             configure STRUT_STACK_SQL_HTTP_TOKENS or pass --http-token, \
             or pass --allow-unauthenticated-http if this exposure is intentional"
        );
    }
    if !config.http_auth_enabled() {
        if loopback {
            tracing::warn!(
                "HTTP authentication is NOT configured; /mcp is unauthenticated (loopback bind only)"
            );
        } else {
            tracing::error!(
                "DANGER: serving /mcp UNAUTHENTICATED on non-loopback address {addr} because --allow-unauthenticated-http was given"
            );
        }
    }

    let (router, ct) = build_http_router(config);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        "strut-stack-sql HTTP listening on http://{addr}/mcp (liveness: /livez, readiness: /readyz)"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            ct.cancel();
        })
        .await?;

    Ok(())
}
