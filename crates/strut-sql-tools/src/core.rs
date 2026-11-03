use std::sync::Arc;
use std::time::Instant;

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use strut_sql_core::config::AppConfig;
use strut_sql_core::redact::redact_url;
use strut_sql_driver_api::EngineKind;
use strut_sql_format::{format_columnar, resolve_result_format, to_json_text, ResultFormat};
use strut_sql_guard::{classify_sql, keyset_shape, SqlClass};

use crate::exec::{
    execute_batch, execute_keyset_page, execute_query, BatchLimits, BatchResult, ExecError,
    ExecOptions, QueryResult,
};
use crate::history;
use crate::intel;
use crate::introspect::{search_objects_ranked, ObjectType};

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
    /// Include per-object relevance score.
    #[serde(default)]
    pub include_score: Option<bool>,
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
    /// Values for `?` placeholders.
    #[serde(default)]
    pub params: Option<Vec<Value>>,
    /// Concurrent SQL batch.
    #[serde(default)]
    pub queries: Option<Vec<BatchQueryItem>>,
    /// Offset pagination (legacy).
    #[serde(default)]
    pub page_offset: Option<usize>,
    /// Rows per page.
    #[serde(default)]
    pub page_size: Option<usize>,
    /// Cursor for the next page; expires in 10 min.
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// columnar (default), rows, or auto.
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
    /// Run EXPLAIN ANALYZE (executes SQL); requires --allow-explain-analyze.
    #[serde(default)]
    pub analyze: Option<bool>,
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
    /// Caller supplied something invalid (bad params, unknown source).
    Validation,
    /// Authorization policy denied the invocation.
    Authorization,
    /// The SQL guard denied the statement.
    Guard,
    /// The database/driver failed (connect, execute); retryable.
    Execution,
}

impl ToolErrorKind {
    fn wire(self) -> (&'static str, i32, bool) {
        match self {
            Self::Validation => ("invalid_params", -32602, false),
            Self::Authorization => ("authorization_denied", -32010, false),
            Self::Guard => ("guard_denied", -32011, false),
            Self::Execution => ("execution_error", -32012, true),
        }
    }
}

pub fn tool_error(msg: impl Into<String>) -> McpError {
    tool_error_code(msg, ToolErrorKind::Validation)
}

pub fn tool_error_code(msg: impl Into<String>, kind: ToolErrorKind) -> McpError {
    // Never leak credentials embedded in URLs inside error text.
    let msg = redact_url(&msg.into());
    let (wire_code, json_rpc_code, retryable) = kind.wire();
    let data = serde_json::json!({
        "code": wire_code,
        "message": msg,
        "retryable": retryable,
    });
    McpError::new(rmcp::model::ErrorCode(json_rpc_code), msg, Some(data))
}

/// Structured cursor error (R2-01). Each failure mode has its own wire code so
/// clients can distinguish "page again" (expired) from "your query changed".
pub fn cursor_error(err: crate::keyset::KeysetError) -> McpError {
    let msg = redact_url(&err.to_string());
    let data = serde_json::json!({
        "code": err.code().to_ascii_lowercase(),
        "message": msg,
        "retryable": err.retryable(),
    });
    McpError::new(rmcp::model::ErrorCode(-32013), msg, Some(data))
}

/// Map an execution-layer error to its tool error kind.
pub(crate) fn exec_error_kind(err: &ExecError) -> ToolErrorKind {
    match err {
        ExecError::Guard(_) => ToolErrorKind::Guard,
        ExecError::Limit(_) => ToolErrorKind::Validation,
        ExecError::Cursor(_) => ToolErrorKind::Validation,
        ExecError::Database(_) | ExecError::Other(_) => ToolErrorKind::Execution,
    }
}

pub fn json_result<T: serde::Serialize>(value: &T) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(
        to_json_text(value),
    )]))
}

/// A successful DDL statement changes the catalog: drop cached introspection
/// for that source so the next schema call sees the new shape immediately.
fn invalidate_after_sql(source_name: &str, engine: EngineKind, sql: &str, ok: bool) {
    if !ok {
        return;
    }
    if let Ok(class) = classify_sql(sql, engine) {
        if class.sql_class() == SqlClass::Ddl {
            crate::introspect::invalidate_source(source_name);
        }
    }
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
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Execution))?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(50).min(200);

    let result = search_objects_ranked(
        &driver,
        Some(&source.name),
        params.object_type,
        params.keyword.as_deref(),
        params.schema.as_deref(),
        offset,
        limit,
        params.include_score.unwrap_or(false),
    )
    .await
    .map_err(|e| tool_error_code(e, ToolErrorKind::Execution))?;

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
        if params.cursor.is_some() {
            return Err(tool_error(
                "cursor is not supported with queries[] batch mode; page single queries instead",
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
            .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Execution))?;
        let batch_items: Vec<(String, Vec<Value>)> =
            queries.into_iter().map(|q| q.into_sql_params()).collect();
        let batch_sqls: Vec<String> = batch_items.iter().map(|(sql, _)| sql.clone()).collect();
        let engine = driver.kind();
        let limits = BatchLimits {
            max_statements: config.batch_max_statements,
            deadline: config.batch_timeout,
            max_bytes: config.batch_max_bytes,
        };
        let batch = execute_batch(
            &driver,
            batch_items,
            &opts,
            config.batch_concurrency,
            config.fail_fast,
            &limits,
        )
        .await
        .map_err(|e| tool_error_code(e.to_string(), exec_error_kind(&e)))?;
        for (sql, result) in batch_sqls.iter().zip(batch.results.iter()) {
            invalidate_after_sql(&source.name, engine, sql, result.ok);
        }
        return Ok(CallToolResult::success(vec![ContentBlock::text(
            to_json_text(&format_batch_result(&batch, format)),
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
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Execution))?;
    let started = Instant::now();

    // R2-01: cursor pagination. Attempted when the caller passes a cursor, or
    // a page_size on a first page (page_offset = 0). When the query cannot be
    // paged by keyset — no ORDER BY, non-unique key, joins — a first-page
    // request falls back to the legacy offset behaviour; a cursor request is
    // rejected with a structured code instead of silently restarting.
    let cursor_requested = params.cursor.is_some();
    let first_page = params.page_size.is_some() && params.page_offset.unwrap_or(0) == 0;
    let mut keyset_error: Option<crate::keyset::KeysetError> = None;
    if cursor_requested || first_page {
        match prepare_keyset_call(
            &driver,
            &source.name,
            &sql,
            &query_params,
            params.cursor.as_deref(),
            params.page_size,
            config.max_rows,
            &opts,
        )
        .await
        {
            Ok((page, page_size_used)) => {
                let mut page = page;
                if let Some(next_key) = page.next_key.clone() {
                    let token = crate::keyset::issue_cursor(
                        &source.name,
                        &sql,
                        &query_params,
                        next_key,
                        page.descending,
                        page_size_used,
                    );
                    if let Some(data) = page.result.data.as_mut() {
                        data.meta.next_cursor = Some(token);
                    }
                }
                let result = page.result;
                let elapsed_ms = started.elapsed().as_millis() as u64;
                invalidate_after_sql(&source.name, driver.kind(), &sql, result.ok);
                return finish_query_result(
                    config,
                    &driver,
                    &source.name,
                    &sql,
                    &query_params,
                    &result,
                    format,
                    elapsed_ms,
                )
                .await;
            }
            Err(KeysetFailure::NotApplicable(err)) => {
                if cursor_requested {
                    return Err(cursor_error(err));
                }
                keyset_error = Some(err);
            }
            Err(KeysetFailure::Failed(err)) => {
                return Err(tool_error_code(err.to_string(), exec_error_kind(&err)));
            }
        }
    }
    if let Some(err) = keyset_error {
        tracing::debug!(
            code = err.code(),
            "cursor pagination not applicable; using page_offset pagination"
        );
    }
    let result = execute_query(&driver, &sql, &query_params, &opts)
        .await
        .map_err(|e| tool_error_code(e.to_string(), exec_error_kind(&e)))?;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    invalidate_after_sql(&source.name, driver.kind(), &sql, result.ok);

    finish_query_result(
        config,
        &driver,
        &source.name,
        &sql,
        &query_params,
        &result,
        format,
        elapsed_ms,
    )
    .await
}

/// Shared post-processing for a single-query result (cursor and offset paths):
/// result formatting plus the optional advisor/history decorations.
#[allow(clippy::too_many_arguments)]
async fn finish_query_result(
    config: &Arc<AppConfig>,
    driver: &Arc<dyn strut_sql_driver_api::EngineDriver>,
    source_name: &str,
    sql: &str,
    query_params: &[Value],
    result: &QueryResult,
    format: ResultFormat,
    elapsed_ms: u64,
) -> Result<CallToolResult, McpError> {
    let mut payload = format_query_result(result, format);
    let select_like = classify_sql(sql, driver.kind())
        .map(|class| class.is_select_like())
        .unwrap_or(false);
    if config.full_tools && select_like {
        let patterns = intel::execution_anti_patterns(driver, Some(source_name), None, sql).await;
        if !patterns.is_empty() {
            payload["anti_patterns"] = serde_json::to_value(&patterns).unwrap_or_default();
        }
    }
    if config.query_history {
        let label = classify_sql(sql, driver.kind())
            .map(|class| class.label().to_string())
            .unwrap_or_else(|_| "UNKNOWN".to_string());
        let rows = result.data.as_ref().map(|d| d.meta.n).unwrap_or(0);
        let suggestions = history::record(
            &config.principal_id,
            sql,
            &label,
            elapsed_ms,
            rows,
            query_params,
        );
        if !suggestions.is_empty() {
            payload["suggestions"] = serde_json::to_value(&suggestions).unwrap_or_default();
        }
    }
    Ok(CallToolResult::success(vec![ContentBlock::text(
        to_json_text(&payload),
    )]))
}

/// Why a cursor attempt did not produce a page.
enum KeysetFailure {
    /// The query cannot be paged by keyset (no unique ORDER BY key, joins, …)
    /// or the cursor itself is unusable.
    NotApplicable(crate::keyset::KeysetError),
    /// Validation/execution failed; propagate, do not fall back.
    Failed(ExecError),
}

#[allow(clippy::too_many_arguments)]
async fn prepare_keyset_call(
    driver: &Arc<dyn strut_sql_driver_api::EngineDriver>,
    source_name: &str,
    sql: &str,
    query_params: &[Value],
    cursor: Option<&str>,
    page_size: Option<usize>,
    max_rows: u32,
    opts: &ExecOptions,
) -> Result<(crate::exec::KeysetPage, usize), KeysetFailure> {
    use crate::keyset::KeysetError;

    let shape = keyset_shape(sql, driver.kind())
        .map_err(|e| KeysetFailure::NotApplicable(KeysetError::Unsupported(e.to_string())))?;
    crate::keyset::ensure_unique_order_column(driver, &shape)
        .await
        .map_err(KeysetFailure::NotApplicable)?;

    let (after, requested_page_size) = match cursor {
        Some(token) => {
            let verified = crate::keyset::verify_cursor(token, source_name, sql, query_params)
                .map_err(KeysetFailure::NotApplicable)?;
            if verified.descending != shape.descending {
                return Err(KeysetFailure::NotApplicable(KeysetError::Invalid(
                    "cursor ordering does not match the query's ORDER BY direction".into(),
                )));
            }
            (Some(verified.after), verified.page_size)
        }
        None => (None, page_size.unwrap_or(max_rows as usize)),
    };
    let page_size_used = requested_page_size.clamp(1, max_rows as usize);
    let page = execute_keyset_page(
        driver,
        sql,
        query_params,
        opts,
        after,
        page_size_used as u32,
    )
    .await
    .map_err(|e| match e {
        ExecError::Cursor(err) => KeysetFailure::NotApplicable(err),
        other => KeysetFailure::Failed(other),
    })?;
    Ok((page, page_size_used))
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

    let analyze = params.analyze.unwrap_or(false);
    if analyze && !config.allow_explain_analyze {
        return Err(tool_error(
            "analyze:true requires the server opt-in --allow-explain-analyze (EXPLAIN ANALYZE executes the statement)",
        ));
    }

    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;

    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Execution))?;

    let summary = crate::explain::analyze_query_with_options(
        &driver,
        &params.sql,
        &params.params.unwrap_or_default(),
        config.write_mode,
        config.query_timeout,
        crate::explain::AnalyzeOptions {
            analyze,
            include_anti_patterns: config.full_tools,
        },
    )
    .await
    .map_err(|e| match e {
        crate::explain::ExplainError::Guard(g) => {
            tool_error_code(g.to_string(), ToolErrorKind::Guard)
        }
        crate::explain::ExplainError::Database(message) => {
            tool_error_code(message, ToolErrorKind::Execution)
        }
        crate::explain::ExplainError::Other(message) => {
            let kind = if message.contains("only supports")
                || message.contains("not supported")
                || message.contains("invalid explain")
            {
                ToolErrorKind::Validation
            } else {
                ToolErrorKind::Execution
            };
            tool_error_code(message, kind)
        }
    })?;

    json_result(&summary)
}
