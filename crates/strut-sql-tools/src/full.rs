use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strut_sql_core::config::{source_endpoint, AppConfig};
use strut_sql_driver_api::EngineDriver;

use crate::core::{json_result, tool_error};
use crate::introspect;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SourceParams {
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeTableParams {
    pub table: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListTablesParams {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListIndexesParams {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ListTablesMeta {
    n: usize,
    offset: usize,
    limit: usize,
    has_more: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ListTablesResult {
    objects: Vec<introspect::SchemaObject>,
    meta: ListTablesMeta,
}

async fn source_driver(
    config: &Arc<AppConfig>,
    source: Option<&str>,
) -> Result<(Arc<dyn EngineDriver>, String), rmcp::ErrorData> {
    let source = config
        .source(source)
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error(e.to_string()))?;
    Ok((driver, source.url.clone()))
}

pub async fn handle_list_sources(
    config: &Arc<AppConfig>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    // SQL-SEC-15: when endpoints are hidden (explicit override, or the
    // automatic default under authenticated HTTP) only name + engine are
    // exposed — host/port/database are reconnaissance data.
    let hide = config.endpoints_hidden();
    let mut sources: Vec<_> = config
        .sources
        .values()
        .map(|s| {
            let endpoint = source_endpoint(&s.url, s.engine);
            if hide {
                serde_json::json!({
                    "name": s.name,
                    "engine": endpoint.engine,
                })
            } else {
                serde_json::json!({
                    "name": s.name,
                    "engine": endpoint.engine,
                    "host": endpoint.host,
                    "port": endpoint.port,
                    "database": endpoint.database,
                })
            }
        })
        .collect();
    sources.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|v| v.as_str()).unwrap_or(""))
    });
    let payload = serde_json::json!({
        "default": config.default_source,
        "sources": sources,
    });
    Ok(CallToolResult::success(vec![ContentBlock::text(
        payload.to_string(),
    )]))
}

pub async fn handle_list_schemas(
    config: &Arc<AppConfig>,
    params: SourceParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, _) = source_driver(config, params.source.as_deref()).await?;
    let objects = introspect::list_schemas(&driver, None)
        .await
        .map_err(|e| tool_error(e.to_string()))?;
    json_result(&objects)
}

pub async fn handle_list_tables(
    config: &Arc<AppConfig>,
    params: ListTablesParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, url) = source_driver(config, params.source.as_deref()).await?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(50).min(200);
    let objects = introspect::list_tables(
        &driver,
        Some(&url),
        params.schema.as_deref(),
        params.keyword.as_deref(),
    )
    .await
    .map_err(|e| tool_error(e.to_string()))?;
    let total = objects.len();
    let slice = introspect::paginate_objects(objects, offset, limit);
    json_result(&ListTablesResult {
        meta: ListTablesMeta {
            n: slice.len(),
            offset,
            limit,
            has_more: offset + slice.len() < total,
        },
        objects: slice,
    })
}

pub async fn handle_describe_table(
    config: &Arc<AppConfig>,
    params: DescribeTableParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, _) = source_driver(config, params.source.as_deref()).await?;
    let object = introspect::describe_table(&driver, params.schema.as_deref(), &params.table)
        .await
        .map_err(|e| tool_error(e.to_string()))?;
    json_result(&object)
}

pub async fn handle_list_indexes(
    config: &Arc<AppConfig>,
    params: ListIndexesParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, _) = source_driver(config, params.source.as_deref()).await?;
    let listed = introspect::list_indexes(
        &driver,
        params.schema.as_deref(),
        params.table.as_deref(),
        params.keyword.as_deref(),
    )
    .await
    .map_err(|e| tool_error(e.to_string()))?;
    json_result(&listed)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListForeignKeysParams {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SchemaOverviewParams {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

pub async fn handle_list_foreign_keys(
    config: &Arc<AppConfig>,
    params: ListForeignKeysParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, _) = source_driver(config, params.source.as_deref()).await?;
    let fks =
        introspect::list_foreign_keys(&driver, params.schema.as_deref(), params.table.as_deref())
            .await
            .map_err(|e| tool_error(e.to_string()))?;
    json_result(&fks)
}

pub async fn handle_schema_overview(
    config: &Arc<AppConfig>,
    params: SchemaOverviewParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, url) = source_driver(config, params.source.as_deref()).await?;
    let overview = introspect::schema_overview(&driver, Some(&url), params.schema.as_deref())
        .await
        .map_err(|e| tool_error(e.to_string()))?;
    json_result(&overview)
}
