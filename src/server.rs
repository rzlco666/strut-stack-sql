use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
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
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::db::EngineKind;
use crate::tools::core::{
    handle_analyze_query, handle_execute_sql, handle_search_objects, AnalyzeQueryParams,
    ExecuteSqlParams, SearchObjectsParams,
};
use crate::tools::ddl::{
    handle_add_column, handle_alter_column, handle_create_table, handle_drop_column,
    handle_drop_table, handle_schema_mutate, handle_truncate_table, AddColumnParams,
    AlterColumnParams, CreateTableParams, DropColumnParams, DropTableParams, SchemaMutateParams,
    TruncateTableParams,
};
use crate::tools::full::{
    handle_describe_table, handle_list_foreign_keys, handle_list_indexes, handle_list_schemas,
    handle_list_sources, handle_list_tables, handle_schema_overview, DescribeTableParams,
    ListForeignKeysParams, ListIndexesParams, ListTablesParams, SchemaOverviewParams, SourceParams,
};

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
}

#[tool_router]
impl McpSqlServer {
    #[tool(description = "Search schemas, tables, columns, or indexes with keyword filter.")]
    async fn search_objects(
        &self,
        params: Parameters<SearchObjectsParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_search_objects(&self.config, params.0).await
    }

    #[tool(description = "Execute one SQL statement or a concurrent batch via queries[].")]
    async fn execute_sql(
        &self,
        params: Parameters<ExecuteSqlParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_execute_sql(&self.config, params.0).await
    }

    #[tool(description = "EXPLAIN query plan summary for slow SELECT diagnostics.")]
    async fn analyze_query_performance(
        &self,
        params: Parameters<AnalyzeQueryParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_analyze_query(&self.config, params.0).await
    }

    #[tool(description = "List configured database sources.")]
    async fn list_sources(&self) -> Result<CallToolResult, McpError> {
        handle_list_sources(&self.config).await
    }

    #[tool(description = "List schemas/databases.")]
    async fn list_schemas(
        &self,
        params: Parameters<SourceParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_list_schemas(&self.config, params.0).await
    }

    #[tool(description = "List tables in a schema.")]
    async fn list_tables(
        &self,
        params: Parameters<ListTablesParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_list_tables(&self.config, params.0).await
    }

    #[tool(description = "Describe table columns and indexes.")]
    async fn describe_table(
        &self,
        params: Parameters<DescribeTableParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_describe_table(&self.config, params.0).await
    }

    #[tool(description = "List indexes (flat: name, columns, unique) for a schema or table.")]
    async fn list_indexes(
        &self,
        params: Parameters<ListIndexesParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_list_indexes(&self.config, params.0).await
    }

    #[tool(description = "List foreign keys for a schema or table.")]
    async fn list_foreign_keys(
        &self,
        params: Parameters<ListForeignKeysParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_list_foreign_keys(&self.config, params.0).await
    }

    #[tool(
        description = "One-shot schema survey: tables + approx row counts (planner stats; omit if unknown; use COUNT(*) for exact) + FKs."
    )]
    async fn schema_overview(
        &self,
        params: Parameters<SchemaOverviewParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_schema_overview(&self.config, params.0).await
    }

    #[tool(description = "DDL schema mutations (requires --allow-ddl).")]
    async fn schema_mutate(
        &self,
        params: Parameters<SchemaMutateParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_schema_mutate(&self.config, params.0).await
    }

    #[tool(description = "Create table from DDL (requires --allow-ddl).")]
    async fn create_table(
        &self,
        params: Parameters<CreateTableParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_create_table(&self.config, params.0).await
    }

    #[tool(description = "Drop table; needs confirm:true and --allow-ddl.")]
    async fn drop_table(
        &self,
        params: Parameters<DropTableParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_drop_table(&self.config, params.0).await
    }

    #[tool(description = "Add a column to a table (requires --allow-ddl).")]
    async fn add_column(
        &self,
        params: Parameters<AddColumnParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_add_column(&self.config, params.0).await
    }

    #[tool(description = "Alter column type (requires --allow-ddl).")]
    async fn alter_column(
        &self,
        params: Parameters<AlterColumnParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_alter_column(&self.config, params.0).await
    }

    #[tool(description = "Drop column; needs confirm:true and --allow-ddl.")]
    async fn drop_column(
        &self,
        params: Parameters<DropColumnParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_drop_column(&self.config, params.0).await
    }

    #[tool(description = "Truncate table; needs confirm:true and --allow-ddl.")]
    async fn truncate_table(
        &self,
        params: Parameters<TruncateTableParams>,
    ) -> Result<CallToolResult, McpError> {
        handle_truncate_table(&self.config, params.0).await
    }
}

#[tool_handler]
impl ServerHandler for McpSqlServer {
    fn get_info(&self) -> ServerInfo {
        let mode = match self.config.write_mode {
            crate::config::WriteMode::ReadOnly => "read-only",
            crate::config::WriteMode::AllowWrites => "writes",
            crate::config::WriteMode::AllowDdl => "ddl",
        };
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(format!(
                "strut-stack-sql {} ({mode}). Core tools: search_objects, execute_sql, analyze_query_performance. \
                 Results are compact columnar JSON. Default max {} rows. Use page_offset/page_size on execute_sql for pagination.",
                env!("CARGO_PKG_VERSION"),
                self.config.max_rows
            ))
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
struct HealthSource {
    name: String,
    engine: &'static str,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    sources: Vec<HealthSource>,
}

fn engine_label(engine: EngineKind) -> &'static str {
    match engine {
        EngineKind::Postgres => "postgres",
        EngineKind::Mysql => "mysql",
        EngineKind::Sqlite => "sqlite",
    }
}

async fn healthz(State(state): State<HttpState>) -> impl IntoResponse {
    let mut sources = Vec::new();
    let mut all_ok = true;

    for source in state.config.sources.values() {
        match source.pool().await {
            Ok(pool) => match pool.ping().await {
                Ok(()) => sources.push(HealthSource {
                    name: source.name.clone(),
                    engine: engine_label(source.engine),
                    ok: true,
                    error: None,
                }),
                Err(e) => {
                    all_ok = false;
                    sources.push(HealthSource {
                        name: source.name.clone(),
                        engine: engine_label(source.engine),
                        ok: false,
                        error: Some(e.to_string()),
                    });
                }
            },
            Err(e) => {
                all_ok = false;
                sources.push(HealthSource {
                    name: source.name.clone(),
                    engine: engine_label(source.engine),
                    ok: false,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    let status = if all_ok { "ok" } else { "degraded" };
    let code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (code, Json(HealthResponse { status, sources }))
}

pub fn build_http_router(config: AppConfig) -> (Router, CancellationToken) {
    let ct = CancellationToken::new();
    let cfg = config.clone();
    let http_state = HttpState {
        config: Arc::new(config),
    };

    let service = StreamableHttpService::new(
        move || Ok(McpSqlServer::new(cfg.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let router = Router::new()
        .route("/healthz", get(healthz))
        .nest_service("/mcp", service)
        .with_state(http_state);

    (router, ct)
}

pub async fn run_http(config: AppConfig, addr: &str) -> Result<()> {
    let (router, ct) = build_http_router(config);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("strut-stack-sql HTTP listening on http://{addr}/mcp (health: /healthz)");

    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            ct.cancel();
        })
        .await?;

    Ok(())
}
