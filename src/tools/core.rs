use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::config::AppConfig;
use crate::db::{
    analyze_query, execute_batch, execute_query, search_objects, ExecOptions, ObjectType,
};
use crate::format::{format_columnar, resolve_result_format, to_json_text, ResultFormat};

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

pub fn tool_error(msg: impl Into<String>) -> McpError {
    McpError::invalid_params(msg.into(), None)
}

pub fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(
        to_json_text(value),
    )]))
}

fn format_query_result(result: &crate::db::QueryResult, format: ResultFormat) -> serde_json::Value {
    let mut out = serde_json::json!({
        "ok": result.ok,
        "error": result.error,
    });
    if let Some(data) = &result.data {
        out["data"] = format_columnar(format, data.clone());
    }
    out
}

fn format_batch_result(batch: &crate::db::BatchResult, format: ResultFormat) -> serde_json::Value {
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
    let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(50).min(200);

    let result = search_objects(
        &pool,
        Some(&source.url),
        params.object_type,
        params.keyword.as_deref(),
        params.schema.as_deref(),
        offset,
        limit,
    )
    .await
    .map_err(|e| tool_error(e.to_string()))?;

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
        let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;
        let batch_items: Vec<(String, Vec<Value>)> =
            queries.into_iter().map(|q| q.into_sql_params()).collect();
        let batch = execute_batch(
            &pool,
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

    let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;
    let result = execute_query(&pool, &sql, &query_params, &opts)
        .await
        .map_err(|e| tool_error(e.to_string()))?;
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

    let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;

    let summary = analyze_query(
        &pool,
        &params.sql,
        &params.params.unwrap_or_default(),
        config.write_mode,
        config.query_timeout,
    )
    .await
    .map_err(|e| tool_error(e.to_string()))?;

    json_result(&summary)
}
