use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use strut_sql_core::config::AppConfig;
use strut_sql_core::redact::redact_url;
use strut_sql_format::{format_columnar, resolve_result_format, to_json_text, ResultFormat};

use crate::exec::{execute_batch, execute_query, BatchResult, ExecError, ExecOptions, QueryResult};
use crate::introspect::{search_objects, ObjectType};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchObjectsParams {
    /// Object kind to search.
    pub object_type: ObjectType,
    /// Optional keyword filter.
    #[serde(default)]
    pub keyword: Option<String>,
    /// Schema/database name scope.
    #[serde(default)]
    pub schema: Option<String>,
    /// Named connection source.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum BatchQueryItem {
    /// Legacy batch entry: raw SQL string.
    Legacy(String),
    /// Parameterized batch entry.
    Parameterized {
        sql: String,
        #[serde(default)]
        params: Option<Vec<Value>>,
    },
}

impl BatchQueryItem {
    fn into_sql_params(self) -> (String, Vec<Value>) {
        match self {
            Self::Legacy(sql) => (sql, Vec::new()),
            Self::Parameterized { sql, params } => (sql, params.unwrap_or_default()),
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ExecuteSqlParams {
    /// Single SQL statement.
    #[serde(default)]
    pub sql: Option<String>,
    /// Bound values for `?` placeholders (single-query mode only).
    #[serde(default)]
    pub params: Option<Vec<Value>>,
    /// Batch of SQL statements executed concurrently.
    #[serde(default)]
    pub queries: Option<Vec<BatchQueryItem>>,
    /// Row offset for paginated single-query results (after guard LIMIT).
    #[serde(default)]
    pub page_offset: Option<usize>,
    /// Rows per page; defaults to --max-rows when omitted.
    #[serde(default)]
    pub page_size: Option<usize>,
    #[serde(default)]
    pub source: Option<String>,
    /// Response shape: auto (rows if ≤10), columnar, or rows.
    #[serde(default)]
    pub format: Option<ResultFormat>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnalyzeQueryParams {
    pub sql: String,
    /// Bound values for `?` placeholders.
    #[serde(default)]
    pub params: Option<Vec<Value>>,
    #[serde(default)]
    pub source: Option<String>,
}

pub fn exec_options(config: &AppConfig) -> ExecOptions {
    ExecOptions {
        write_mode: config.write_mode,
        max_rows: config.max_rows,
        max_bytes: config.max_bytes,
        timeout: config.query_timeout,
        limit_injected: false,
        page_offset: 0,
        page_size: None,
        deny_columns: config.deny_columns.clone(),
    }
}

/// Error category for tool failures (SQL-BUG-14): clients/agents judge
/// retryability from the JSON-RPC error code, so validation problems and
/// database execution failures must not share one code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// Caller supplied something invalid (bad params, guard denial,
    /// unknown source) → `invalid_params`; retrying unchanged is pointless.
    Validation,
    /// The database/driver failed (connect, execute) → `internal_error`;
    /// retrying later may succeed.
    Db,
}

pub fn tool_error(msg: impl Into<String>) -> McpError {
    tool_error_code(msg, ToolErrorKind::Validation)
}

pub fn tool_error_code(msg: impl Into<String>, kind: ToolErrorKind) -> McpError {
    // Never leak credentials embedded in URLs inside error text.
    let msg = redact_url(&msg.into());
    match kind {
        ToolErrorKind::Validation => McpError::invalid_params(msg, None),
        ToolErrorKind::Db => McpError::internal_error(msg, None),
    }
}

/// Map an execution-layer error to its tool error kind.
fn exec_error_kind(err: &ExecError) -> ToolErrorKind {
    match err {
        ExecError::Guard(_) => ToolErrorKind::Validation,
        ExecError::Database(_) | ExecError::Other(_) => ToolErrorKind::Db,
    }
}

pub fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(
        to_json_text(value),
    )]))
}

fn format_query_result(result: &QueryResult, format: ResultFormat) -> serde_json::Value {
    let mut out = serde_json::json!({
        "ok": result.ok,
        "error": result.error,
    });
    if let Some(data) = &result.data {
        out["data"] = format_columnar(format, data.clone());
    }
    out
}

fn format_batch_result(batch: &BatchResult, format: ResultFormat) -> serde_json::Value {
    serde_json::json!({
        "results": batch
            .results
            .iter()
            .map(|r| format_query_result(r, format))
            .collect::<Vec<_>>()
    })
}

pub async fn handle_search_objects(
    config: &Arc<AppConfig>,
    params: SearchObjectsParams,
) -> Result<CallToolResult, McpError> {
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Db))?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(50).min(200);

    let result = search_objects(
        &driver,
        Some(&source.url),
        params.object_type,
        params.keyword.as_deref(),
        params.schema.as_deref(),
        offset,
        limit,
    )
    .await
    .map_err(|e| tool_error_code(e, ToolErrorKind::Db))?;

    json_result(&result)
}

pub async fn handle_execute_sql(
    config: &Arc<AppConfig>,
    params: ExecuteSqlParams,
) -> Result<CallToolResult, McpError> {
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let format = resolve_result_format(params.format);
    let opts = exec_options(config);

    if let Some(queries) = params.queries {
        if params.sql.is_some() {
            return Err(tool_error("provide either sql or queries, not both"));
        }
        if params.params.is_some() {
            return Err(tool_error(
                "provide params on each queries[] item, not at the top level",
            ));
        }
        if params.page_offset.is_some() || params.page_size.is_some() {
            return Err(tool_error(
                "page_offset and page_size are not supported with queries[] batch mode",
            ));
        }
        if queries.is_empty() {
            return Err(tool_error("queries array is empty"));
        }
        let driver = source
            .driver()
            .await
            .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Db))?;
        let batch_items: Vec<(String, Vec<Value>)> =
            queries.into_iter().map(|q| q.into_sql_params()).collect();
        let batch = execute_batch(
            &driver,
            batch_items,
            &opts,
            config.batch_concurrency,
            config.fail_fast,
        )
        .await;
        return Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string(&format_batch_result(&batch, format)).unwrap_or_default(),
        )]));
    }

    let sql = params
        .sql
        .ok_or_else(|| tool_error("sql or queries is required"))?;
    let query_params = params.params.unwrap_or_default();
    let mut opts = exec_options(config);
    opts.page_offset = params.page_offset.unwrap_or(0);
    opts.page_size = params.page_size;

    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Db))?;
    let result = execute_query(&driver, &sql, &query_params, &opts)
        .await
        .map_err(|e| tool_error_code(e.to_string(), exec_error_kind(&e)))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string(&format_query_result(&result, format)).unwrap_or_default(),
    )]))
}

pub async fn handle_analyze_query(
    config: &Arc<AppConfig>,
    params: AnalyzeQueryParams,
) -> Result<CallToolResult, McpError> {
    let trimmed = params.sql.trim_start();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("EXPLAIN") {
        return Err(tool_error(
            "pass the raw SELECT to analyze_query_performance; do not wrap with EXPLAIN (it wraps for you)",
        ));
    }

    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;

    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Db))?;

    let summary = crate::explain::analyze_query(
        &driver,
        &params.sql,
        &params.params.unwrap_or_default(),
        config.write_mode,
        config.query_timeout,
    )
    .await
    .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Db))?;

    json_result(&summary)
}
