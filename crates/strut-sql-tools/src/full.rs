use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strut_sql_core::config::{source_endpoint, AppConfig};
use strut_sql_driver_api::EngineDriver;

use crate::core::{json_result, tool_error, tool_error_code, ToolErrorKind};
use crate::introspect;

fn catalog_error(message: String) -> rmcp::ErrorData {
    tool_error_code(message, ToolErrorKind::Execution)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceParams {
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeTableParams {
    pub table: String,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
) -> Result<(Arc<dyn EngineDriver>, String, String), rmcp::ErrorData> {
    let source = config
        .source(source)
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Execution))?;
    Ok((driver, source.name.clone(), source.url.clone()))
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
    let (driver, _, _) = source_driver(config, params.source.as_deref()).await?;
    let objects = introspect::list_schemas(&driver, None)
        .await
        .map_err(catalog_error)?;
    // V1: an allowlist decides which databases exist as far as the agent is
    // concerned.
    let objects: Vec<_> = objects
        .into_iter()
        .filter(|object| config.tables.allows_database(driver.kind(), &object.name))
        .collect();
    json_result(&objects)
}

pub async fn handle_list_tables(
    config: &Arc<AppConfig>,
    params: ListTablesParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, source_name, _url) = source_driver(config, params.source.as_deref()).await?;
    crate::core::ensure_schema_allowed(config, driver.kind(), params.schema.as_deref())?;
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(50).min(200);
    let page = introspect::list_tables_page(
        &driver,
        Some(&source_name),
        params.schema.as_deref(),
        params.keyword.as_deref(),
        offset,
        limit,
    )
    .await
    .map_err(catalog_error)?;
    // Deny/allow rules apply to schema metadata too: a table the principal
    // cannot query must not be discoverable through this tool.
    let objects: Vec<_> = page
        .objects
        .into_iter()
        .filter(|object| introspect::object_visible(&config.tables, object))
        .collect();
    json_result(&ListTablesResult {
        meta: ListTablesMeta {
            n: objects.len(),
            offset: page.meta.offset,
            limit: page.meta.limit,
            has_more: page.meta.has_more,
        },
        objects,
    })
}

pub async fn handle_describe_table(
    config: &Arc<AppConfig>,
    params: DescribeTableParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, _, _) = source_driver(config, params.source.as_deref()).await?;
    crate::core::ensure_schema_allowed(config, driver.kind(), params.schema.as_deref())?;
    if !introspect::table_visible(&config.tables, params.schema.as_deref(), &params.table) {
        return Err(crate::core::tool_error(format!(
            "table '{}' is not accessible to this principal",
            params.table
        )));
    }
    let object = introspect::describe_table(&driver, params.schema.as_deref(), &params.table)
        .await
        .map_err(catalog_error)?;
    json_result(&object)
}

pub async fn handle_list_indexes(
    config: &Arc<AppConfig>,
    params: ListIndexesParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, source_name, _) = source_driver(config, params.source.as_deref()).await?;
    crate::core::ensure_schema_allowed(config, driver.kind(), params.schema.as_deref())?;
    let listed = introspect::list_indexes(
        &driver,
        Some(&source_name),
        params.schema.as_deref(),
        params.table.as_deref(),
        params.keyword.as_deref(),
    )
    .await
    .map_err(catalog_error)?;
    let listed: Vec<_> = listed
        .into_iter()
        .filter(|index| {
            index
                .table
                .as_deref()
                .map(|table| {
                    introspect::table_visible(&config.tables, index.schema.as_deref(), table)
                })
                .unwrap_or(true)
        })
        .collect();
    json_result(&listed)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListForeignKeysParams {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
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
    let (driver, _, _) = source_driver(config, params.source.as_deref()).await?;
    crate::core::ensure_schema_allowed(config, driver.kind(), params.schema.as_deref())?;
    let fks =
        introspect::list_foreign_keys(&driver, params.schema.as_deref(), params.table.as_deref())
            .await
            .map_err(catalog_error)?;
    let fks: Vec<_> = fks
        .into_iter()
        .filter(|fk| {
            introspect::table_visible(&config.tables, None, &fk.table)
                && introspect::table_visible(&config.tables, None, &fk.ref_table)
        })
        .collect();
    json_result(&fks)
}

pub async fn handle_schema_overview(
    config: &Arc<AppConfig>,
    params: SchemaOverviewParams,
) -> Result<CallToolResult, rmcp::ErrorData> {
    let (driver, source_name, _) = source_driver(config, params.source.as_deref()).await?;
    crate::core::ensure_schema_allowed(config, driver.kind(), params.schema.as_deref())?;
    let mut overview =
        introspect::schema_overview(&driver, Some(&source_name), params.schema.as_deref())
            .await
            .map_err(catalog_error)?;
    if !config.tables.is_empty() {
        overview.tables.retain(|table| {
            introspect::table_visible(&config.tables, Some(&overview.schema), &table.name)
        });
        overview.foreign_keys.retain(|fk| {
            introspect::table_visible(&config.tables, None, &fk.table)
                && introspect::table_visible(&config.tables, None, &fk.ref_table)
        });
        overview.meta.n_tables = overview.tables.len();
        overview.meta.n_foreign_keys = overview.foreign_keys.len();
    }
    json_result(&overview)
}
