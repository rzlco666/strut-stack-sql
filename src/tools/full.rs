use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::{source_endpoint, AppConfig};
use crate::db::{
    describe_table, list_foreign_keys, list_indexes, listed_index_from_schema_object, list_schemas,
    list_tables, schema_overview,
};
use crate::tools::core::{json_result, tool_error};

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
    objects: Vec<crate::db::SchemaObject>,
    meta: ListTablesMeta,
}

pub async fn handle_list_sources(
    config: &Arc<AppConfig>,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let mut sources: Vec<_> = config
        .sources
        .values()
        .map(|s| {
            let endpoint = source_endpoint(&s.url, s.engine);
            serde_json::json!({
                "name": s.name,
                "engine": endpoint.engine,
                "host": endpoint.host,
                "port": endpoint.port,
                "database": endpoint.database,
            })
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
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;
    let objects = list_schemas(&pool, None)
        .await
        .map_err(|e| tool_error(e.to_string()))?;
    json_result(&objects)
}

pub async fn handle_list_tables(
    config: &Arc<AppConfig>,
    params: ListTablesParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(50).min(200);
    let mut objects = list_tables(
        &pool,
        Some(&source.url),
        params.schema.as_deref(),
        params.keyword.as_deref(),
    )
    .await
    .map_err(|e| tool_error(e.to_string()))?;
    let total = objects.len();
    let slice = objects
        .drain(offset..total.min(offset + limit))
        .collect::<Vec<_>>();
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
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;
    let object = describe_table(
        &pool,
        Some(&source.url),
        params.schema.as_deref(),
        &params.table,
    )
    .await
    .map_err(|e| tool_error(e.to_string()))?;
    json_result(&object)
}

pub async fn handle_list_indexes(
    config: &Arc<AppConfig>,
    params: ListIndexesParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;
    let objects = list_indexes(
        &pool,
        Some(&source.url),
        params.schema.as_deref(),
        params.table.as_deref(),
        params.keyword.as_deref(),
    )
    .await
    .map_err(|e| tool_error(e.to_string()))?;
    let listed: Vec<_> = objects
        .into_iter()
        .map(listed_index_from_schema_object)
        .collect();
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
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;
    let fks = list_foreign_keys(
        &pool,
        Some(&source.url),
        params.schema.as_deref(),
        params.table.as_deref(),
    )
    .await
    .map_err(|e| tool_error(e.to_string()))?;
    json_result(&fks)
}

pub async fn handle_schema_overview(
    config: &Arc<AppConfig>,
    params: SchemaOverviewParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let pool = source.pool().await.map_err(|e| tool_error(e.to_string()))?;
    let overview = schema_overview(&pool, Some(&source.url), params.schema.as_deref())
        .await
        .map_err(|e| tool_error(e.to_string()))?;
    json_result(&overview)
}
