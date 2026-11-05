//! Schema-intelligence tool handlers (PRD-F3-02/F3-03/F3-04): relationship
//! graph, join suggestions, and the token-budgeted schema context packer.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strut_sql_core::config::AppConfig;
use strut_sql_driver_api::{EngineDriver, ForeignKeyInfo, IndexInfo};
use strut_sql_guard::detect_anti_patterns;

use crate::core::{json_result, tool_error, tool_error_code, ToolErrorKind};
use crate::graph;
use crate::introspect;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelationshipGraphParams {
    /// Schema/database to inspect; omit for the source's default schema.
    #[serde(default)]
    pub schema: Option<String>,
    /// Named connection source; omit for the configured default.
    #[serde(default)]
    pub source: Option<String>,
    /// Table to focus the graph on. `focus` is accepted as an alias.
    #[serde(default, alias = "focus")]
    pub table: Option<String>,
    /// How many foreign-key hops to expand around the focus table.
    #[serde(default)]
    pub depth: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SuggestJoinsParams {
    /// Two to eight table names to connect.
    pub tables: Vec<String>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SchemaContextParams {
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub token_budget: Option<usize>,
    #[serde(default)]
    pub tables: Option<Vec<String>>,
    #[serde(default)]
    pub keyword: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaContextMeta {
    pub approx_tokens: usize,
    pub token_budget: usize,
    pub truncated: bool,
    pub tables_included: usize,
    pub tables_total: usize,
    pub tables_omitted: usize,
    pub columns_omitted: usize,
    pub relations_omitted: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub focus_missing: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyword: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaContext {
    pub schema: String,
    pub context: String,
    pub meta: SchemaContextMeta,
}

const DEFAULT_TOKEN_BUDGET: usize = 4000;
const MAX_TOKEN_BUDGET: usize = 32000;
const MIN_TOKEN_BUDGET: usize = 200;

async fn source_driver(
    config: &Arc<AppConfig>,
    source: Option<&str>,
) -> Result<Arc<dyn EngineDriver>, McpError> {
    let source = config
        .source(source)
        .map_err(|e| tool_error(e.to_string()))?;
    source
        .driver()
        .await
        .map_err(|e| tool_error_code(e.to_string(), ToolErrorKind::Execution))
}

fn graph_error(message: String) -> McpError {
    let kind = if message.starts_with("unknown table") || message.contains("requires schema") {
        ToolErrorKind::Validation
    } else {
        ToolErrorKind::Execution
    };
    tool_error_code(message, kind)
}

pub async fn handle_get_relationship_graph(
    config: &Arc<AppConfig>,
    params: RelationshipGraphParams,
) -> Result<CallToolResult, McpError> {
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source_driver(config, params.source.as_deref()).await?;
    crate::core::ensure_schema_allowed(config, driver.kind(), params.schema.as_deref())?;
    let graph = graph::relationship_graph(
        &driver,
        Some(&source.name),
        params.schema.as_deref(),
        params.table.as_deref(),
        params.depth,
        if config.tables.is_empty() {
            None
        } else {
            Some(&config.tables)
        },
    )
    .await
    .map_err(graph_error)?;
    json_result(&graph)
}

pub async fn handle_suggest_joins(
    config: &Arc<AppConfig>,
    params: SuggestJoinsParams,
) -> Result<CallToolResult, McpError> {
    if !(2..=8).contains(&params.tables.len()) {
        return Err(tool_error(
            "tables must contain between 2 and 8 table names",
        ));
    }
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source_driver(config, params.source.as_deref()).await?;
    crate::core::ensure_schema_allowed(config, driver.kind(), params.schema.as_deref())?;
    for table in &params.tables {
        crate::core::ensure_table_database_allowed(config, driver.kind(), Some(table))?;
    }
    let result = graph::suggest_joins(
        &driver,
        Some(&source.name),
        &params.tables,
        params.schema.as_deref(),
        if config.tables.is_empty() {
            None
        } else {
            Some(&config.tables)
        },
    )
    .await
    .map_err(graph_error)?;
    json_result(&result)
}

pub async fn handle_get_schema_context(
    config: &Arc<AppConfig>,
    params: SchemaContextParams,
) -> Result<CallToolResult, McpError> {
    let budget = params
        .token_budget
        .unwrap_or(DEFAULT_TOKEN_BUDGET)
        .clamp(MIN_TOKEN_BUDGET, MAX_TOKEN_BUDGET);
    let source = config
        .source(params.source.as_deref())
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source_driver(config, params.source.as_deref()).await?;
    crate::core::ensure_schema_allowed(config, driver.kind(), params.schema.as_deref())?;
    let context = build_schema_context(
        &driver,
        Some(&source.name),
        params.schema.as_deref(),
        budget,
        params.tables.as_deref(),
        params.keyword.as_deref(),
        if config.tables.is_empty() {
            None
        } else {
            Some(&config.tables)
        },
    )
    .await
    .map_err(|e| tool_error_code(e, ToolErrorKind::Execution))?;
    json_result(&context)
}

#[allow(clippy::too_many_arguments)]
async fn build_schema_context(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
    token_budget: usize,
    focus: Option<&[String]>,
    keyword: Option<&str>,
    policy: Option<&strut_sql_guard::TablePolicy>,
) -> Result<SchemaContext, String> {
    // V5: every input comes from the schema cache. The cached overview
    // carries the table list, approximate row counts and foreign keys, so a
    // warm call performs no driver introspection at all (previously two
    // all-table scans — FK and row-count — ran on every call).
    let overview = introspect::schema_overview(driver, source, schema).await?;
    let schema_name = overview.schema.clone();
    // The overview caps its own table list at 200; the cached catalog page
    // still supplies every table, so no extra scan is needed even then. Only
    // row estimates beyond that cap are absent (they are optional hints).
    let mut table_names: Vec<String> =
        introspect::list_tables(driver, source, Some(&schema_name), None)
            .await?
            .into_iter()
            .map(|t| t.name)
            .collect();
    let rows: BTreeMap<String, Option<i64>> = overview
        .tables
        .iter()
        .map(|t| (t.name.clone(), t.approx_rows))
        .collect();
    let mut columns = introspect::list_columns(driver, source, Some(&schema_name), None).await?;
    if let Some(policy) = policy {
        if !policy.is_empty() {
            for table in focus.into_iter().flatten() {
                if !policy.allows_table(Some(&schema_name), table) {
                    return Err(format!(
                        "table '{table}' is not accessible to this principal"
                    ));
                }
            }
            table_names.retain(|table| policy.allows_table(Some(&schema_name), table));
            columns.retain(|column| {
                column
                    .table
                    .as_deref()
                    .map(|table| policy.allows_table(Some(&schema_name), table))
                    .unwrap_or(true)
            });
        }
    }
    let mut fks = overview.foreign_keys.clone();
    if let Some(policy) = policy {
        if !policy.is_empty() {
            fks.retain(|fk| {
                policy.allows_table(Some(&schema_name), &fk.table)
                    && policy.allows_table(Some(&schema_name), &fk.ref_table)
            });
        }
    }
    let indexes = introspect::cached_indexes(driver, source, Some(&schema_name)).await?;

    let mut columns_by_table: BTreeMap<String, Vec<&introspect::SchemaObject>> = BTreeMap::new();
    for column in &columns {
        if let Some(table) = &column.table {
            columns_by_table
                .entry(table.clone())
                .or_default()
                .push(column);
        }
    }
    let mut indexes_by_table: BTreeMap<String, Vec<&IndexInfo>> = BTreeMap::new();
    for index in &indexes {
        indexes_by_table
            .entry(index.table.clone())
            .or_default()
            .push(index);
    }

    // Undirected FK adjacency for focus expansion and degree ordering.
    let mut adjacency: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut degree: BTreeMap<String, usize> = BTreeMap::new();
    for fk in &fks {
        adjacency
            .entry(fk.table.clone())
            .or_default()
            .insert(fk.ref_table.clone());
        adjacency
            .entry(fk.ref_table.clone())
            .or_default()
            .insert(fk.table.clone());
        *degree.entry(fk.table.clone()).or_default() += 1;
        *degree.entry(fk.ref_table.clone()).or_default() += 1;
    }

    let known: BTreeSet<&str> = table_names.iter().map(String::as_str).collect();
    let mut focus_missing = Vec::new();
    let mut priority: Vec<String> = Vec::new();
    if let Some(focus) = focus {
        for requested in focus {
            match table_names
                .iter()
                .find(|name| name.eq_ignore_ascii_case(requested))
            {
                Some(actual) => priority.push(actual.clone()),
                None => focus_missing.push(requested.clone()),
            }
        }
        // Expand around focus tables through the FK graph (depth 2).
        let mut expanded = Vec::new();
        let mut seen: BTreeSet<String> = priority.iter().cloned().collect();
        for start in &priority {
            let mut queue = VecDeque::from([(start.clone(), 0usize)]);
            while let Some((current, depth)) = queue.pop_front() {
                if depth >= 2 {
                    continue;
                }
                for neighbour in adjacency.get(&current).into_iter().flatten() {
                    if seen.insert(neighbour.clone()) {
                        expanded.push(neighbour.clone());
                        queue.push_back((neighbour.clone(), depth + 1));
                    }
                }
            }
        }
        expanded.sort();
        priority.extend(expanded);
    } else if let Some(keyword) = keyword {
        let kw = keyword.to_lowercase();
        for table in &table_names {
            let hits = table.to_lowercase().contains(&kw)
                || columns_by_table.get(table).is_some_and(|cols| {
                    cols.iter().any(|c| {
                        c.name.to_lowercase().contains(&kw)
                            || c.comment
                                .as_ref()
                                .is_some_and(|comment| comment.to_lowercase().contains(&kw))
                    })
                });
            if hits {
                priority.push(table.clone());
            }
        }
    }

    let mut seen: BTreeSet<String> = priority.iter().cloned().collect();
    for table in &table_names {
        if seen.insert(table.clone()) {
            priority.push(table.clone());
        }
    }
    priority.retain(|table| known.contains(table.as_str()));
    // Non-focus remainder: most-connected first, then name.
    let focused: BTreeSet<String> = focus
        .into_iter()
        .flatten()
        .filter_map(|requested| {
            table_names
                .iter()
                .find(|name| name.eq_ignore_ascii_case(requested))
                .cloned()
        })
        .collect();
    let mut tail: Vec<String> = priority
        .iter()
        .filter(|t| !focused.contains(*t))
        .cloned()
        .collect();
    tail.sort_by(|a, b| {
        degree
            .get(b)
            .copied()
            .unwrap_or(0)
            .cmp(&degree.get(a).copied().unwrap_or(0))
            .then_with(|| a.cmp(b))
    });
    let mut ordered = Vec::new();
    for table in priority {
        if focused.contains(&table) {
            ordered.push(table);
        }
    }
    ordered.extend(tail);

    let budget_bytes = token_budget.saturating_mul(4);
    let mut context = format!("schema: {schema_name}\n");
    let mut included = 0usize;
    let mut omitted = 0usize;
    let mut columns_omitted = 0usize;
    let mut relations_omitted = 0usize;
    for table in ordered {
        let mut block = String::new();
        block.push_str(&format!("- {table}"));
        if let Some(Some(count)) = rows.get(&table) {
            block.push_str(&format!(" rows~{count}"));
        }
        if degree.get(&table).copied().unwrap_or(0) == 0 {
            block.push_str(" [no relations]");
        }
        block.push('\n');
        if context.len() + block.len() > budget_bytes {
            omitted += 1;
            continue;
        }
        context.push_str(&block);

        let primary_columns: BTreeSet<String> = indexes_by_table
            .get(&table)
            .into_iter()
            .flatten()
            .filter(|i| i.primary)
            .flat_map(|i| i.columns.iter().cloned())
            .collect();
        let indexed_columns: BTreeSet<String> = indexes_by_table
            .get(&table)
            .into_iter()
            .flatten()
            .flat_map(|i| i.columns.iter().cloned())
            .collect();

        let mut column_overflow = false;
        for column in columns_by_table.get(&table).into_iter().flatten() {
            if column_overflow {
                columns_omitted += 1;
                continue;
            }
            let mut flags = Vec::new();
            if primary_columns
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&column.name))
            {
                flags.push("PK");
            } else if indexed_columns
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&column.name))
            {
                flags.push("IDX");
            }
            let suffix = if flags.is_empty() {
                String::new()
            } else {
                format!(" {}", flags.join(","))
            };
            let line = format!(
                "  {}{}:{}\n",
                column.name,
                suffix,
                column.data_type.as_deref().unwrap_or("?")
            );
            if context.len() + line.len() > budget_bytes {
                columns_omitted += 1;
                column_overflow = true;
                continue;
            }
            context.push_str(&line);
        }

        for fk in fks.iter().filter(|fk| fk.table == table) {
            let line = format!(
                "  fk: {}.{} -> {}.{}\n",
                fk.table,
                fk.columns.join(","),
                fk.ref_table,
                fk.ref_columns.join(",")
            );
            if context.len() + line.len() > budget_bytes {
                relations_omitted += 1;
                continue;
            }
            context.push_str(&line);
        }
        included += 1;
    }

    let tables_total = table_names.len();
    let approx_tokens = context.len().div_ceil(4);
    Ok(SchemaContext {
        schema: schema_name,
        context,
        meta: SchemaContextMeta {
            approx_tokens,
            token_budget,
            truncated: omitted > 0 || columns_omitted > 0 || relations_omitted > 0,
            tables_included: included,
            tables_total,
            tables_omitted: omitted,
            columns_omitted,
            relations_omitted,
            focus_missing,
            keyword: keyword.map(str::to_string),
        },
    })
}

/// Anti-pattern findings for an executed statement (PRD-F3-06), using the
/// cached catalog for wide-table and indexed-column context.
pub async fn execution_anti_patterns(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
    sql: &str,
) -> Vec<strut_sql_guard::AntiPattern> {
    let engine = driver.kind();
    let schema_name = match driver.resolve_schema(schema).await {
        Ok(name) => name,
        Err(_) => return Vec::new(),
    };
    let columns = introspect::list_columns(driver, source, Some(&schema_name), None)
        .await
        .unwrap_or_default();
    let mut ctx = strut_sql_guard::PatternContext::default();
    for column in &columns {
        if let Some(table) = &column.table {
            *ctx.wide_tables.entry(table.clone()).or_default() += 1;
        }
    }
    let indexes = introspect::list_indexes(driver, source, Some(&schema_name), None, None)
        .await
        .unwrap_or_default();
    for index in indexes {
        if let Some(table) = index.table {
            ctx.indexed_columns
                .entry(table)
                .or_default()
                .extend(index.columns);
        }
    }
    detect_anti_patterns(sql, engine, &ctx).unwrap_or_default()
}

pub fn fk_predicate(fk: &ForeignKeyInfo) -> String {
    fk.columns
        .iter()
        .zip(fk.ref_columns.iter())
        .map(|(left, right)| format!("{}.{left} = {}.{right}", fk.table, fk.ref_table))
        .collect::<Vec<_>>()
        .join(" AND ")
}
