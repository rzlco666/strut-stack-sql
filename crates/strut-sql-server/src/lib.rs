use std::collections::HashMap;
use std::future::Future;
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
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ClientRequest, Implementation, JsonObject, JsonRpcMessage, ListToolsResult,
    ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::transport::Transport;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use strut_sql_core::auth::{
    sql_required_tier, tool_tier, AuthMethod, Decision, Denial, Principal, Tier, DDL_TOOLS,
};
use strut_sql_core::config::{AppConfig, WriteMode};
use strut_sql_core::redact::redact_url;
use strut_sql_driver_api::EngineKind;
use strut_sql_tools::audit::{audit_call, sql_fingerprint};
use strut_sql_tools::core::{
    handle_analyze_query, handle_execute_sql, handle_search_objects, tool_error_code,
    AnalyzeQueryParams, BatchQueryItem, ExecuteSqlParams, SearchObjectsParams, ToolErrorKind,
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
use strut_sql_tools::intel::{
    handle_get_relationship_graph, handle_get_schema_context, handle_suggest_joins,
    RelationshipGraphParams, SchemaContextParams, SuggestJoinsParams,
};

pub mod approvals_http;
pub mod export;
pub mod oauth;

pub use oauth::{OAuthValidator, ProtectedResourceMetadata};

/// Default tracing filter. The `sqlx` target directive only sets that log
/// target's level (no sqlx dependency outside the driver crates); audit
/// events are info-level so the default filter shows them.
pub const DEFAULT_LOG_FILTER: &str = "info,sqlx=warn,audit=info";

/// The exact MCP specification version this server implements (PRD-CONF-02).
///
/// 2025-06-18 is the last spec revision whose server requirements this
/// codebase actually satisfies end to end. Older versions are *not*
/// advertised: 2025-03-26 and earlier require receiving JSON-RPC batches,
/// which this transport stack does not implement. A client that asks for an
/// unsupported version is answered with this version, as the lifecycle spec
/// requires.
pub const SUPPORTED_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2025_06_18;

/// Whether `version` is exactly the pinned supported spec version.
pub fn is_supported_protocol_version(version: &ProtocolVersion) -> bool {
    version == &SUPPORTED_PROTOCOL_VERSION
}

/// Principal attached to an HTTP request by the auth middleware. The MCP
/// service copies HTTP request extensions into the per-call
/// [`RequestContext`], so the tool dispatch reads the same identity the
/// middleware authenticated.
#[derive(Clone, Debug)]
pub struct CallerPrincipal(pub Principal);

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

    /// Ask the client to confirm a destructive action (R2-05). Returns the
    /// accepted approval id, or an `approval_required` error when the client
    /// cannot be asked (the HTTP queue is then the only path).
    async fn elicit_approval(
        &self,
        context: &RequestContext<rmcp::RoleServer>,
        principal: &str,
        source: &str,
        tool: &str,
        summary: &str,
    ) -> Result<String, McpError> {
        use rmcp::service::ElicitationError;

        let entry = self
            .config
            .approvals
            .request(principal, source, tool, summary);
        let message = format!(
            "Approve '{tool}'? {summary}. This executes immediately after approval \
             and cannot be undone; the approval expires in 15 minutes (id {}).",
            entry.id
        );
        let reply = context.peer.elicit::<ApprovalReply>(message).await;
        match reply {
            Ok(Some(reply)) if reply.approve => {
                if self
                    .config
                    .approvals
                    .decide(&entry.id, principal, true)
                    .is_none()
                {
                    return Err(approval_rejected("approval could not be recorded"));
                }
                Ok(entry.id)
            }
            Ok(_) => {
                self.config.approvals.decide(&entry.id, principal, false);
                Err(approval_rejected("destructive action was not approved"))
            }
            Err(ElicitationError::CapabilityNotSupported) => Err(approval_required_error(&entry)),
            Err(_) => {
                self.config.approvals.decide(&entry.id, principal, false);
                Err(approval_rejected("destructive action was not approved"))
            }
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

/// Identity for this call: the middleware-authenticated principal when the
/// transport supplied one, otherwise the synthesized local stdio operator.
/// Both paths go through the same [`Policy::authorize`] check.
fn principal_from_context(config: &AppConfig, ctx: &RequestContext<rmcp::RoleServer>) -> Principal {
    ctx.extensions
        .get::<CallerPrincipal>()
        .map(|caller| caller.0.clone())
        .unwrap_or_else(|| config.policy.local_principal())
}

fn request_source(config: &AppConfig, arguments: &JsonObject) -> String {
    arguments
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or(&config.default_source)
        .to_string()
}

/// SQL statements carried by an `execute_sql` / `analyze_query_performance`
/// arguments object (single `sql` or a `queries[]` batch).
fn request_statements(arguments: &JsonObject) -> Vec<String> {
    let mut statements = Vec::new();
    if let Some(sql) = arguments.get("sql").and_then(Value::as_str) {
        statements.push(sql.to_string());
    }
    if let Some(queries) = arguments.get("queries").and_then(Value::as_array) {
        for item in queries {
            match item {
                Value::String(sql) => statements.push(sql.clone()),
                Value::Object(obj) => {
                    if let Some(sql) = obj.get("sql").and_then(Value::as_str) {
                        statements.push(sql.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    statements
}

/// Tier required by this invocation: the tool's static tier raised to the
/// heaviest classified SQL statement it carries.
fn required_tier_for_call(
    config: &AppConfig,
    tool: &str,
    arguments: &JsonObject,
    source: &str,
) -> Tier {
    let mut required = tool_tier(tool, config.full_tools).unwrap_or(Tier::Read);
    if matches!(tool, "execute_sql" | "analyze_query_performance") {
        if let Ok(source_config) = config.source(Some(source)) {
            for sql in request_statements(arguments) {
                if let Some(tier) = sql_required_tier(&sql, source_config.engine) {
                    required = required.max(tier);
                }
            }
        }
    }
    required
}

/// Elicitation reply for destructive actions (R2-05).
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
struct ApprovalReply {
    /// Approve the destructive action.
    approve: bool,
}
rmcp::elicit_safe!(ApprovalReply);

/// Tools/actions that need an approval when the process requires one.
fn destructive_call(
    tool: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Option<(&'static str, String)> {
    let table = arguments
        .get("table")
        .and_then(|v| v.as_str())
        .unwrap_or("<table>");
    match tool {
        "drop_table" => Some(("drop_table", format!("drop table {table}"))),
        "truncate_table" => Some(("truncate_table", format!("truncate table {table}"))),
        "drop_column" => {
            let column = arguments
                .get("column")
                .and_then(|c| c.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("<column>");
            Some(("drop_column", format!("drop column {column} on {table}")))
        }
        "schema_mutate" => match arguments.get("action").and_then(|v| v.as_str()) {
            Some("drop_table") => Some(("drop_table", format!("drop table {table}"))),
            Some("truncate_table") => Some(("truncate_table", format!("truncate table {table}"))),
            Some("drop_column") => Some(("drop_column", format!("drop column on {table}"))),
            _ => None,
        },
        _ => None,
    }
}

fn approval_required_error(entry: &strut_sql_core::approvals::ApprovalEntry) -> McpError {
    let message = format!(
        "approval required for {}: accept approval {} through the HTTP queue, then retry with approval_id",
        entry.tool, entry.id
    );
    McpError::new(
        rmcp::model::ErrorCode(-32015),
        message.clone(),
        Some(serde_json::json!({
            "code": "approval_required",
            "message": message,
            "retryable": false,
            "approval_id": entry.id,
            "expires_unix": entry.expires_unix,
            "ttl_seconds": strut_sql_core::approvals::APPROVAL_TTL_SECS,
            "accept_path": format!("/approvals/{}/accept", entry.id),
        })),
    )
}

fn approval_rejected(message: &str) -> McpError {
    McpError::new(
        rmcp::model::ErrorCode(-32015),
        message.to_string(),
        Some(serde_json::json!({
            "code": "approval_rejected",
            "message": message,
            "retryable": false,
        })),
    )
}

/// Static docs resource: the SQL surface and contracts, kept short so it fits
/// comfortably in a client context window.
const SQL_SURFACE_RESOURCE: &str = r#"# Strut Stack SQL — SQL surface

Read: SELECT, SHOW, DESCRIBE, EXPLAIN, read-only PRAGMAs.
DML: INSERT/UPDATE/DELETE/MERGE (requires --allow-writes or --allow-ddl).
DDL: CREATE/ALTER/DROP/TRUNCATE (requires --allow-ddl; destructive actions
also require confirm=true and, with --require-approval, an accepted approval).
Admin: GRANT/REVOKE (requires --allow-grants plus an Admin grant).
Always rejected: COPY, ATTACH DATABASE, LOAD DATA, CALL/PREPARE/EXECUTE,
SELECT ... INTO, INTO OUTFILE/DUMPFILE, transaction control.

Caps: --max-rows auto-injects/clamps LIMIT; --max-bytes truncates columnar
payloads; --query-timeout bounds execution; batches are capped by statement
count, total deadline and total bytes. Results are columnar JSON by default.

Errors (data.code): invalid_params, authorization_denied, guard_denied,
execution_error (retryable), plus cursor_* and approval_* codes.
"#;

/// Effective policy for this caller, safe to hand to the client: no
/// credentials, no URLs, no row-filter predicates.
fn policy_summary(config: &AppConfig) -> serde_json::Value {
    let mut sources: Vec<serde_json::Value> = config
        .sources
        .values()
        .map(|source| {
            serde_json::json!({
                "name": source.name,
                "engine": format!("{:?}", source.engine),
            })
        })
        .collect();
    sources.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    serde_json::json!({
        "principal": config.principal_id,
        "write_mode": format!("{:?}", config.write_mode),
        "full_tools": config.full_tools,
        "admin_gate_open": config.allow_grants,
        "approvals_required": config.require_approval,
        "default_source": config.default_source,
        "sources": sources,
        "deny_columns": config.deny_columns,
        "table_rules": {
            "allow": config.tables.allow.iter().map(|r| r.table.clone()).collect::<Vec<_>>(),
            "deny": config.tables.deny.iter().map(|r| r.table.clone()).collect::<Vec<_>>(),
            "row_filter_tables": config.tables.row_filters.iter().map(|r| r.table.clone()).collect::<Vec<_>>(),
            "masked_columns": config.tables.masks.iter().map(|m| m.column.clone()).collect::<Vec<_>>(),
        },
        "caps": {
            "max_rows": config.max_rows,
            "max_bytes": config.max_bytes,
            "batch_max_statements": config.batch_max_statements,
            "batch_timeout_s": config.batch_timeout.as_secs(),
            "batch_max_bytes": config.batch_max_bytes,
            "principal_concurrency": config.limiter.per_principal(),
        },
    })
}

fn authorization_error(denial: &Denial) -> McpError {
    tool_error_code(
        format!("authorization denied: {denial}"),
        ToolErrorKind::Authorization,
    )
}

#[tool_router]
impl McpSqlServer {
    #[tool(description = "Search schemas, tables, columns, indexes (ranked).")]
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

    #[tool(description = "Run SQL or a concurrent batch.")]
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

    #[tool(description = "EXPLAIN advisor; analyze:true runs EXPLAIN ANALYZE.")]
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

    #[tool(description = "List indexes.")]
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

    #[tool(description = "Schema survey: tables, approx rows, FKs, hubs/orphans.")]
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

    #[tool(description = "FK relationship graph; optional focus + depth.")]
    async fn get_relationship_graph(
        &self,
        params: Parameters<RelationshipGraphParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "get_relationship_graph",
            source,
            None,
            handle_get_relationship_graph(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Shortest FK join paths for 2-8 tables.")]
    async fn suggest_joins(
        &self,
        params: Parameters<SuggestJoinsParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "suggest_joins",
            source,
            None,
            handle_suggest_joins(&self.config, params.0),
        )
        .await
    }

    #[tool(description = "Token-budgeted schema digest.")]
    async fn get_schema_context(
        &self,
        params: Parameters<SchemaContextParams>,
    ) -> Result<CallToolResult, McpError> {
        let source = params.0.source.clone();
        audit_call(
            "get_schema_context",
            source,
            None,
            handle_get_schema_context(&self.config, params.0),
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
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
        )
        .with_server_info(Implementation::new(
            "strut-stack-sql",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(instructions)
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, McpError> {
        let resources = vec![
            rmcp::model::Resource::new("strut://docs/sql-surface", "sql-surface")
                .with_description("Guard-classified SQL surface, caps and error contract")
                .with_mime_type("text/markdown"),
            rmcp::model::Resource::new("strut://policy/summary", "policy-summary")
                .with_description(
                    "Effective authorization for this caller: tier, sources, table rules, masks",
                )
                .with_mime_type("application/json"),
        ];
        Ok(rmcp::model::ListResourcesResult {
            resources,
            next_cursor: None,
            meta: None,
        })
    }

    async fn list_resource_templates(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourceTemplatesResult, McpError> {
        Ok(rmcp::model::ListResourceTemplatesResult {
            resource_templates: vec![
                rmcp::model::ResourceTemplate::new("strut://schema/{source}/{schema}", "schema")
                    .with_title("Schema digest")
                    .with_description(
                        "Compact schema context for one source/schema; token budget defaults to 4000, cached for the schema-cache TTL",
                    )
                    .with_mime_type("application/json"),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResult, McpError> {
        let uri = request.uri.as_str();
        // Resources go through the same Policy as tools: a caller without a
        // read grant for the source cannot read the schema or policy summary.
        let principal = principal_from_context(&self.config, &context);
        let source = uri
            .strip_prefix("strut://schema/")
            .and_then(|rest| rest.split('/').next())
            .filter(|source| !source.is_empty())
            .unwrap_or(&self.config.default_source)
            .to_string();
        let authz =
            match self
                .config
                .policy
                .authorize(&principal, "resources.read", &source, Tier::Read)
            {
                Decision::Allow(authz) => authz,
                Decision::Deny(denial) => return Err(authorization_error(&denial)),
            };
        let scoped = Arc::new(self.config.scoped_for(&authz));
        let contents = if uri == "strut://docs/sql-surface" {
            rmcp::model::ResourceContents::text(SQL_SURFACE_RESOURCE, uri)
                .with_mime_type("text/markdown")
        } else if uri == "strut://policy/summary" {
            let summary = policy_summary(&scoped);
            rmcp::model::ResourceContents::text(
                serde_json::to_string_pretty(&summary).unwrap_or_default(),
                uri,
            )
            .with_mime_type("application/json")
        } else if let Some(rest) = uri.strip_prefix("strut://schema/") {
            let mut parts = rest.splitn(2, '/');
            let source = parts.next().unwrap_or_default();
            let schema = parts.next().unwrap_or_default();
            let params = strut_sql_tools::intel::SchemaContextParams {
                schema: if schema.is_empty() {
                    None
                } else {
                    Some(schema.to_string())
                },
                source: if source.is_empty() {
                    None
                } else {
                    Some(source.to_string())
                },
                token_budget: None,
                tables: None,
                keyword: None,
            };
            let result =
                strut_sql_tools::intel::handle_get_schema_context(&self.config, params).await?;
            let text = result
                .content
                .first()
                .and_then(|block| block.as_text())
                .map(|text| text.text.clone())
                .unwrap_or_default();
            rmcp::model::ResourceContents::text(text, uri).with_mime_type("application/json")
        } else {
            return Err(McpError::invalid_params(
                format!("unknown resource '{uri}'"),
                None,
            ));
        };
        Ok(rmcp::model::ReadResourceResult::new(vec![contents]))
    }

    async fn list_prompts(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListPromptsResult, McpError> {
        Ok(rmcp::model::ListPromptsResult {
            prompts: vec![
                rmcp::model::Prompt::new(
                    "diagnose",
                    Some("Diagnose slow or failing SQL with the advisor"),
                    Some(vec![rmcp::model::PromptArgument::new("sql")
                        .with_description("Statement to diagnose")
                        .with_required(true)]),
                ),
                rmcp::model::Prompt::new(
                    "explore",
                    Some("Explore a schema before writing queries"),
                    Some(vec![rmcp::model::PromptArgument::new("keyword")
                        .with_description("Table or column keyword")
                        .with_required(false)]),
                ),
                rmcp::model::Prompt::new(
                    "safe-migration",
                    Some("Plan, approve and apply a migration safely"),
                    Some(vec![rmcp::model::PromptArgument::new("goal")
                        .with_description("Migration goal")
                        .with_required(true)]),
                ),
            ],
            next_cursor: None,
            meta: None,
        })
    }

    async fn get_prompt(
        &self,
        request: rmcp::model::GetPromptRequestParams,
        _context: RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::GetPromptResult, McpError> {
        let argument = |name: &str| {
            request
                .arguments
                .as_ref()
                .and_then(|args| args.get(name))
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string()
        };
        let text = match request.name.as_str() {
            "diagnose" => format!(
                "Use analyze_query_performance on this statement, then summarize the dominant cost node, \
                 any row-estimate misses, and the index advice. Statement:\n{}",
                argument("sql")
            ),
            "explore" => format!(
                "Start with schema_overview, then search_objects for '{}', then get_schema_context for the \
                 tables you will touch. Report relations and the 3 riskiest columns (nullable/FK/deny-listed).",
                argument("keyword")
            ),
            "safe-migration" => format!(
                "Goal: {}.\n1. Call schema_mutate with action=plan_migration and the statements.\n2. Review \
                 the returned inverse_defined/inverse.\n3. Apply with plan_id + an accepted approval_id.\n4. \
                 Verify with describe_table. Rollback only if an inverse is defined.",
                argument("goal")
            ),
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown prompt '{other}'"),
                    None,
                ))
            }
        };
        Ok(rmcp::model::GetPromptResult::new(vec![
            rmcp::model::PromptMessage::new_text(rmcp::model::Role::User, text),
        ]))
    }

    /// Single invocation-time authorization gate (PRD-F1-10). Runs before
    /// every tool handler, including tools hidden from `list_tools`; the
    /// same [`Policy::authorize`] call backs the listing below.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let principal = principal_from_context(&self.config, &context);
        let tool = request.name.to_string();
        let arguments = request.arguments.clone().unwrap_or_default();
        let source = request_source(&self.config, &arguments);
        let required = required_tier_for_call(&self.config, &tool, &arguments, &source);

        match self
            .config
            .policy
            .authorize(&principal, &tool, &source, required)
        {
            Decision::Allow(authz) => {
                if authz.tier == Tier::Admin {
                    tracing::info!(
                        target: "audit",
                        event = "authorization",
                        decision = "allow",
                        principal_id = %principal.id,
                        auth_method = principal.auth_method.as_str(),
                        tool = %tool,
                        source_name = %source,
                        sql_class = "admin",
                        tier = "admin",
                        "admin_gate"
                    );
                }
                // R2-05: destructive DDL. When approvals are required and the
                // client supports MCP elicitation, ask the user first; an
                // accepted answer becomes a single-use approval id. Clients
                // without elicitation fall through to the handler, which
                // returns `approval_required` for the HTTP queue.
                let mut request = request;
                if self.config.require_approval {
                    if let Some((label, summary)) = destructive_call(&tool, &arguments) {
                        let has_id = arguments
                            .get("approval_id")
                            .and_then(|v| v.as_str())
                            .is_some();
                        if !has_id {
                            match self
                                .elicit_approval(&context, &principal.id, &source, label, &summary)
                                .await
                            {
                                Ok(id) => {
                                    if let Some(args) = request.arguments.as_mut() {
                                        args.insert("approval_id".into(), serde_json::json!(id));
                                    }
                                }
                                Err(err) => return Err(err),
                            }
                        }
                    }
                }
                // The handlers read their limits from AppConfig; hand them a
                // scoped view carrying the grant's ceiling and effective mode.
                let scoped = McpSqlServer {
                    config: Arc::new(self.config.scoped_for(&authz)),
                    tool_router: self.tool_router.clone(),
                };
                let call = ToolCallContext::new(&scoped, request, context);
                self.tool_router.call(call).await
            }
            Decision::Deny(denial) => {
                tracing::warn!(
                    target: "audit",
                    event = "authorization",
                    decision = "deny",
                    principal_id = %principal.id,
                    auth_method = principal.auth_method.as_str(),
                    tool = %tool,
                    source_name = %source,
                    required_tier = required.as_str(),
                    admin_class = (required == Tier::Admin),
                    reason = %denial,
                    "tool_call_denied"
                );
                Err(authorization_error(&denial))
            }
        }
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        ctx: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let principal = principal_from_context(&self.config, &ctx);
        let all = self.tool_router.list_all();

        // Same authorize() as invocation: a tool is listed only when at
        // least one configured source would allow it for this principal.
        let tools: Vec<Tool> = all
            .into_iter()
            .filter(|tool| match tool_tier(&tool.name, self.config.full_tools) {
                Some(tier) => self.config.sources.keys().any(|source| {
                    self.config
                        .policy
                        .authorize(&principal, &tool.name, source, tier)
                        .is_allowed()
                }),
                None => false,
            })
            .collect();

        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }
}

/// Transport wrapper that pins the initialize handshake to the supported
/// spec version. rmcp negotiates by echoing any version its SDK knows, so a
/// client asking for 2025-03-26 would otherwise be told 2025-03-26 even
/// though this server does not implement that revision's batch requirement.
/// Rewriting the requested version makes rmcp's echo land on our pin; the
/// lifecycle spec explicitly permits answering an unsupported version with a
/// supported one.
struct PinProtocolVersion<T>(T);

impl<T> Transport<rmcp::RoleServer> for PinProtocolVersion<T>
where
    T: Transport<rmcp::RoleServer> + Send + 'static,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.0.send(item)
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<rmcp::RoleServer>> {
        let mut message = self.0.receive().await?;
        if let JsonRpcMessage::Request(request) = &mut message {
            if let ClientRequest::InitializeRequest(initialize) = &mut request.request {
                initialize.params.protocol_version = SUPPORTED_PROTOCOL_VERSION;
            }
        }
        Some(message)
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.0.close().await
    }
}

pub async fn run_stdio(config: AppConfig) -> Result<()> {
    let server = McpSqlServer::new(config);
    let transport = PinProtocolVersion(AsyncRwTransport::new_server(
        tokio::io::stdin(),
        tokio::io::stdout(),
    ));
    let service = server
        .serve(transport)
        .await
        .inspect_err(|e| tracing::error!("stdio serve error: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}

#[derive(Clone)]
pub struct HttpState {
    pub(crate) config: Arc<AppConfig>,
    /// Present when OAuth is configured; served at
    /// `/.well-known/oauth-protected-resource`.
    protected_resource: Option<Arc<ProtectedResourceMetadata>>,
    /// Extra non-loopback origins allowed to call the MCP endpoint
    /// (`STRUT_STACK_SQL_ALLOWED_ORIGINS`, comma-separated).
    pub(crate) allowed_origins: Arc<Vec<String>>,
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
        EngineKind::DuckDb => "duckdb",
        EngineKind::ClickHouse => "clickhouse",
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
// HTTP auth (API keys + OAuth 2.1 bearer JWTs) + Policy enforcement
// (SQL-SEC-05, F1-07..F1-10)
//
// Every /mcp request must present a Bearer credential when auth is
// configured. The credential resolves to a Principal:
//   * a configured API key (`STRUT_STACK_SQL_HTTP_TOKENS` / `--http-token`,
//     raw or `sha256:<hex>`), or
//   * an RS256 JWT validated against the configured issuer/JWKS (OAuth).
// The middleware inserts the principal into the request extensions, where
// the MCP dispatch reads it for its own `authorize()` call. Middleware
// additionally rejects JSON-RPC `tools/call` requests early with HTTP 403
// using the SAME `authorize()` function the dispatch uses.
// Non-loopback binds without auth are refused in `run_http` unless
// --allow-unauthenticated-http was given.
// ---------------------------------------------------------------------------

const MAX_MCP_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct HttpAuthState {
    config: Arc<AppConfig>,
    oauth: Option<Arc<OAuthValidator>>,
    /// Per-principal fixed-window counters: id -> (window start, count).
    rate: Arc<Mutex<HashMap<String, (Instant, u64)>>>,
}

impl HttpAuthState {
    fn rate_limited(&self, principal_id: &str) -> bool {
        let limit = self.config.http_rate_limit_per_minute;
        if limit == 0 {
            return false;
        }
        let now = Instant::now();
        let mut map = self.rate.lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(principal_id.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= Duration::from_secs(60) {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 > limit as u64
    }
}

fn unauthorized_response(oauth: Option<&OAuthValidator>, invalid_token: bool) -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "unauthorized: missing or invalid bearer token" })),
    )
        .into_response();
    let challenge = oauth
        .map(|validator| validator.www_authenticate(invalid_token))
        .unwrap_or_else(|| "Bearer".to_string());
    if let Ok(value) = header::HeaderValue::from_str(&challenge) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
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

/// Resolve the bearer credential to a principal. API keys are checked
/// first (constant-time), then the OAuth validator when configured.
fn authenticate(
    config: &AppConfig,
    oauth: Option<&OAuthValidator>,
    req: &Request<Body>,
) -> Option<Principal> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, presented) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let presented = presented.trim();
    if presented.is_empty() {
        return None;
    }

    if let Some(token) = config.http_tokens.iter().find(|t| t.verify(presented)) {
        return Some(
            config
                .policy
                .resolve_principal(&token.name, AuthMethod::ApiKey),
        );
    }

    let validator = oauth?;
    match validator.validate(presented) {
        Ok(mut principal) => {
            principal.display_name = config
                .policy
                .resolve_principal(&principal.id, AuthMethod::OAuth)
                .display_name;
            Some(principal)
        }
        Err(err) => {
            // Never log the token itself.
            tracing::warn!(error = %err, "OAuth bearer token rejected");
            None
        }
    }
}

/// Message for an early HTTP denial. Text keeps the v1 contract for legacy
/// token callers (source allowlist / DDL tool wording) while covering the
/// new tier and Admin-gate denials.
fn denial_message(principal: &Principal, tool: &str, source: &str, denial: &Denial) -> String {
    match denial {
        Denial::NoGrant { .. } => format!(
            "token '{}' is not allowed to use source '{source}'",
            principal.id
        ),
        Denial::ToolNotExposed { .. } => {
            format!("token '{}' may not call tool '{tool}' (not exposed)", principal.id)
        }
        Denial::TierTooLow { effective, required, .. } => {
            if DDL_TOOLS.contains(&tool) {
                format!(
                    "token '{}' (mode {:?}) may not call DDL tool '{tool}'",
                    principal.id,
                    effective.write_mode()
                )
            } else {
                format!(
                    "token '{}' (mode {:?}) may not run this SQL: tier '{}' is below required '{}' on source '{source}'",
                    principal.id,
                    effective.write_mode(),
                    effective.as_str(),
                    required.as_str()
                )
            }
        }
        Denial::AdminGateDisabled => format!(
            "token '{}' may not run this SQL: GRANT/REVOKE require an Admin grant and --allow-grants",
            principal.id
        ),
    }
}

/// Returns `Some(denial message)` when this single JSON-RPC request must
/// not proceed under the principal's policy.
fn check_jsonrpc_request(
    config: &AppConfig,
    principal: &Principal,
    value: &Value,
) -> Option<String> {
    let method = value.get("method").and_then(Value::as_str)?;
    if method == "resources/read" {
        // Resources carry data (schema digest, policy summary), so they are
        // authorized in the middleware exactly like tools/call; a principal
        // without a read grant for the source must not reach the handler.
        let params = value.get("params")?;
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let source = uri
            .strip_prefix("strut://schema/")
            .and_then(|rest| rest.split('/').next())
            .filter(|source| !source.is_empty())
            .unwrap_or(&config.default_source)
            .to_string();
        return match config
            .policy
            .authorize(principal, "resources.read", &source, Tier::Read)
        {
            Decision::Allow(_) => None,
            Decision::Deny(denial) => Some(denial_message(
                principal,
                "resources/read",
                &source,
                &denial,
            )),
        };
    }
    if method != "tools/call" {
        return None;
    }
    let params = value.get("params")?;
    let tool = params.get("name").and_then(Value::as_str)?;
    let empty = JsonObject::new();
    let args = params
        .get("arguments")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let source = request_source(config, args);
    let required = required_tier_for_call(config, tool, args, &source);

    match config.policy.authorize(principal, tool, &source, required) {
        Decision::Allow(_) => None,
        Decision::Deny(denial) => Some(denial_message(principal, tool, &source, &denial)),
    }
}

async fn http_auth_middleware(
    State(auth): State<HttpAuthState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    // No credentials configured: local/loopback mode (run_http refuses
    // unauthenticated non-loopback binds before we ever get here). The
    // dispatch still authorizes the synthesized local principal.
    //
    // `config.oauth.is_some()` keeps the gate closed even when startup
    // discovery failed (validator None): OAuth stays fail-closed and API
    // keys, if any, still work.
    if !auth.config.http_auth_enabled() && auth.oauth.is_none() {
        req.extensions_mut()
            .insert(CallerPrincipal(auth.config.policy.local_principal()));
        return next.run(req).await;
    }

    let presented = req.headers().contains_key(header::AUTHORIZATION);
    let Some(principal) = authenticate(&auth.config, auth.oauth.as_deref(), &req) else {
        return unauthorized_response(auth.oauth.as_deref(), presented);
    };

    if auth.rate_limited(&principal.id) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "rate limit exceeded for this principal" })),
        )
            .into_response();
    }

    req.extensions_mut()
        .insert(CallerPrincipal(principal.clone()));

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
            if let Some(message) = check_jsonrpc_request(&auth.config, &principal, item) {
                tracing::warn!(
                    target: "audit",
                    event = "authorization",
                    decision = "deny",
                    principal_id = %principal.id,
                    auth_method = principal.auth_method.as_str(),
                    "http tools/call denied by policy"
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

/// RFC 9728 discovery document. 404 when OAuth is not configured.
async fn protected_resource(State(state): State<HttpState>) -> Response {
    match &state.protected_resource {
        Some(metadata) => Json(metadata.as_ref()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const ALLOWED_ORIGINS_ENV: &str = "STRUT_STACK_SQL_ALLOWED_ORIGINS";

/// Enforce the pinned spec version on Streamable HTTP:
/// * a present `MCP-Protocol-Version` header must equal the supported version
///   (the spec requires 400 for an unsupported version);
/// * an `initialize` body asking for another version is rewritten to the
///   supported version, which is the spec-mandated fallback answer.
async fn mcp_protocol_version_middleware(req: Request<Body>, next: Next) -> Response {
    if let Some(value) = req.headers().get(MCP_PROTOCOL_VERSION_HEADER) {
        let supported = value
            .to_str()
            .map(|value| value == SUPPORTED_PROTOCOL_VERSION.as_str())
            .unwrap_or(false);
        if !supported {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "unsupported MCP-Protocol-Version header" })),
            )
                .into_response();
        }
    }

    if req.method() != Method::POST {
        return next.run(req).await;
    }

    let (mut parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_MCP_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({ "error": "request body too large" })),
            )
                .into_response();
        }
    };
    let rewritten = match serde_json::from_slice::<Value>(&bytes) {
        Ok(mut value) => {
            if rewrite_initialize_version(&mut value) {
                serde_json::to_vec(&value).unwrap_or_else(|_| bytes.to_vec())
            } else {
                bytes.to_vec()
            }
        }
        Err(_) => bytes.to_vec(),
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    next.run(Request::from_parts(parts, Body::from(rewritten)))
        .await
}

/// Point an `initialize` request at the pinned version. Returns whether the
/// document changed.
fn rewrite_initialize_version(value: &mut Value) -> bool {
    if value.get("method").and_then(Value::as_str) != Some("initialize") {
        return false;
    }
    let Some(params) = value.get_mut("params").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(version) = params.get("protocolVersion").and_then(Value::as_str) else {
        return false;
    };
    if version == SUPPORTED_PROTOCOL_VERSION.as_str() {
        return false;
    }
    params.insert(
        "protocolVersion".to_string(),
        Value::String(SUPPORTED_PROTOCOL_VERSION.as_str().to_string()),
    );
    true
}

/// Split an HTTP authority (`host`, `host:port`, `[v6]`, `[v6]:port`).
fn split_authority(authority: &str) -> (String, Option<u16>) {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').unwrap_or((rest, ""));
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
        return (host.to_ascii_lowercase(), port);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host.to_ascii_lowercase(), port.parse().ok()),
        _ => (authority.to_ascii_lowercase(), None),
    }
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host == "::1"
        || host
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// DNS-rebinding defense (MCP transports spec, Security Warning): browser
/// requests carry an `Origin` header. Accept loopback origins only when the
/// `Host` header is loopback too and the ports agree; accept other origins
/// only when explicitly allowlisted.
fn origin_allowed(origin: &str, host_header: Option<&str>, allowed: &[String]) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return false;
    }
    let (origin_host, origin_port) = split_authority(authority);
    if is_loopback_host(&origin_host) {
        let Some(host) = host_header else {
            return true;
        };
        let (host_host, host_port) = split_authority(host);
        if !is_loopback_host(&host_host) {
            return false;
        }
        let default_origin_port = if scheme == "https" { 443 } else { 80 };
        return origin_port.unwrap_or(default_origin_port) == host_port.unwrap_or(80);
    }
    allowed
        .iter()
        .any(|allowed_origin| allowed_origin.eq_ignore_ascii_case(origin))
}

async fn validate_origin_middleware(
    State(state): State<HttpState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|v| v.to_str().ok());
        let ok = origin
            .to_str()
            .map(|origin| origin_allowed(origin, host, &state.allowed_origins))
            .unwrap_or(false);
        if !ok {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "error": "forbidden: origin not allowed" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Build the HTTP router without OAuth (API keys only).
pub fn build_http_router(config: AppConfig) -> (Router, CancellationToken) {
    build_http_router_with_auth(config, None)
}

/// Build the HTTP router with an optional OAuth resource-server validator.
pub fn build_http_router_with_auth(
    config: AppConfig,
    oauth: Option<Arc<OAuthValidator>>,
) -> (Router, CancellationToken) {
    let ct = CancellationToken::new();
    let factory_config = config.clone();
    let protected_metadata = oauth
        .as_ref()
        .map(|validator| Arc::new(validator.protected_resource_metadata()));
    let http_state = HttpState {
        config: Arc::new(config.clone()),
        protected_resource: protected_metadata,
        allowed_origins: Arc::new(
            std::env::var(ALLOWED_ORIGINS_ENV)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|origin| !origin.is_empty())
                .map(str::to_string)
                .collect(),
        ),
    };
    let origin_state = http_state.clone();
    let auth_state = HttpAuthState {
        config: Arc::new(config),
        oauth,
        rate: Arc::new(Mutex::new(HashMap::new())),
    };

    let service = StreamableHttpService::new(
        move || Ok(McpSqlServer::new(factory_config.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let mcp_router = Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(
            auth_state.clone(),
            http_auth_middleware,
        ))
        .layer(axum::middleware::from_fn(mcp_protocol_version_middleware));

    // /export authenticates exactly like /mcp (API keys and OAuth), then
    // authorizes through the same Policy in its handler.
    let approval_routes = Router::new()
        .route(
            "/approvals",
            axum::routing::get(approvals_http::list_approvals),
        )
        .route(
            "/approvals/{id}/accept",
            axum::routing::post(approvals_http::accept_approval),
        )
        .route(
            "/approvals/{id}/reject",
            axum::routing::post(approvals_http::reject_approval),
        )
        .with_state(http_state.clone())
        .layer(axum::middleware::from_fn_with_state(
            auth_state.clone(),
            http_auth_middleware,
        ));

    let export_router = Router::new()
        .route("/export", axum::routing::post(export::export_handler))
        .with_state(http_state.clone())
        .layer(axum::middleware::from_fn_with_state(
            auth_state.clone(),
            http_auth_middleware,
        ));

    let router = Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/healthz", get(readyz))
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/{*path}",
            get(protected_resource),
        )
        .with_state(http_state)
        .merge(mcp_router)
        .merge(export_router)
        .merge(approval_routes)
        .layer(axum::middleware::from_fn_with_state(
            origin_state,
            validate_origin_middleware,
        ));

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

    let oauth = match config.oauth.as_ref() {
        Some(settings) => match OAuthValidator::discover(settings).await {
            Ok(validator) => Some(Arc::new(validator)),
            Err(err) => {
                // Fail closed: OAuth bearer tokens are rejected, API keys
                // keep working. Never fall back to accepting unverified JWT.
                tracing::error!(
                    error = %err,
                    "OAuth discovery/JWKS fetch failed; OAuth bearer tokens will be REJECTED \
                     (fail closed). API keys still work."
                );
                None
            }
        },
        None => None,
    };

    let (router, ct) = build_http_router_with_auth(config, oauth);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        "strut-stack-sql HTTP listening on http://{addr}/mcp (liveness: /livez, readiness: /readyz)"
    );

    // Keep-alive MCP clients issue many small POSTs on one connection. Without
    // TCP_NODELAY the kernel can hold a response segment waiting for a delayed
    // ACK (~40 ms on Linux), which shows up as per-call latency; measured in
    // the 2.0.0 benchmark harness (R1-04).
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|stream| {
        if let Err(err) = stream.set_nodelay(true) {
            tracing::debug!(error = %err, "failed to set TCP_NODELAY on accepted connection");
        }
    });
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            ct.cancel();
        })
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_router_exposes_exactly_twenty_tools() {
        let router = McpSqlServer::tool_router();
        let mut names: Vec<String> = router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        names.sort();
        assert_eq!(names.len(), 20, "tool surface: {names:?}");
        for expected in [
            "get_relationship_graph",
            "suggest_joins",
            "get_schema_context",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
    }
}
