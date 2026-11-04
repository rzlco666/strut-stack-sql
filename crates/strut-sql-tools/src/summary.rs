//! Server-side summary / aggregate mode for `execute_sql` (R2-02).
//!
//! The caller never sends SQL here: the server builds `COUNT(*)`, a bounded
//! sample, or an aggregate over explicitly named columns. Identifiers are
//! validated against the live catalog before they reach any SQL text, so a
//! table or column name cannot smuggle syntax.
//!
//! Numbers are either exact (COUNT/SUM/AVG/MIN/MAX over the real rows) or
//! explicitly marked as not exact (`kind: "sample"`); the server never returns
//! a planner estimate as a row count. Deny-listed columns are rejected in
//! every summary position — group keys, the aggregated column and the sample
//! projection — not merely redacted.

use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::Deserialize;
use strut_sql_core::config::AppConfig;
use strut_sql_driver_api::EngineKind;
use strut_sql_format::{format_columnar, to_json_text, ResultFormat};

use crate::core::{exec_options, tool_error};
use crate::exec::{execute_query, ExecOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SummaryKind {
    /// Exact row count (`COUNT(*)`), optionally grouped.
    Count,
    /// Bounded sample of rows; never a total.
    Sample,
    /// `count` / `avg` / `sum` / `min` / `max` over one column.
    Aggregate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AggregateFn {
    Count,
    Avg,
    Sum,
    Min,
    Max,
}

impl AggregateFn {
    fn sql(self) -> &'static str {
        match self {
            Self::Count => "COUNT",
            Self::Avg => "AVG",
            Self::Sum => "SUM",
            Self::Min => "MIN",
            Self::Max => "MAX",
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SummaryParams {
    /// What to compute.
    pub kind: SummaryKind,
    /// Table to summarize; `schema.table` is accepted.
    pub table: String,
    /// Columns to return (sample only). Defaults to every catalog column.
    #[serde(default)]
    pub columns: Option<Vec<String>>,
    /// Group keys (count/aggregate only).
    #[serde(default)]
    pub group_by: Option<Vec<String>>,
    /// Aggregate function (aggregate only).
    #[serde(default)]
    pub aggregate: Option<AggregateFn>,
    /// Column the aggregate applies to (aggregate only; count may omit it).
    #[serde(default)]
    pub column: Option<String>,
    /// Rows (sample) or groups (count/aggregate) to return; capped by --max-rows.
    #[serde(default)]
    pub limit: Option<usize>,
}

fn quote_ident(name: &str, engine: EngineKind) -> String {
    match engine {
        EngineKind::MySql => format!("`{}`", name.replace('`', "``")),
        EngineKind::Postgres | EngineKind::Sqlite | EngineKind::DuckDb => {
            format!("\"{}\"", name.replace('"', "\"\""))
        }
    }
}

fn split_table(table: &str) -> (Option<String>, String) {
    match table.rsplit_once('.') {
        Some((schema, name)) if !schema.is_empty() && !name.is_empty() => {
            (Some(schema.to_string()), name.to_string())
        }
        _ => (None, table.to_string()),
    }
}

/// Case-insensitive lookup that returns the catalog's own spelling, so the
/// generated SQL uses identifiers as the engine defines them.
fn catalog_name<'a>(
    columns: &'a [strut_sql_driver_api::ColumnInfo],
    requested: &str,
) -> Option<&'a str> {
    columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(requested))
        .map(|c| c.name.as_str())
}

fn denied(config: &AppConfig, column: &str) -> bool {
    config
        .deny_columns
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(column))
}

pub async fn handle_summary(
    config: &Arc<AppConfig>,
    params: SummaryParams,
    source_name: Option<&str>,
) -> Result<CallToolResult, McpError> {
    // Resolve exactly the source the dispatcher authorized, not the default.
    let source = config
        .source(source_name)
        .map_err(|e| tool_error(e.to_string()))?;
    let driver = source
        .driver()
        .await
        .map_err(|e| tool_error(e.to_string()))?;
    let engine = driver.kind();
    let (schema, table) = split_table(&params.table);
    let info = driver
        .describe_table(schema.as_deref(), &table)
        .await
        .map_err(|e| tool_error(format!("cannot summarize table '{}': {e}", params.table)))?;
    if info.columns.is_empty() {
        return Err(tool_error(format!(
            "cannot summarize table '{}': no such table",
            params.table
        )));
    }
    let from = match &schema {
        Some(schema) => format!(
            "{}.{}",
            quote_ident(schema, engine),
            quote_ident(&table, engine)
        ),
        None => quote_ident(&table, engine),
    };

    let requested_limit = params.limit.unwrap_or(config.max_rows as usize);
    let limit = requested_limit.clamp(1, config.max_rows as usize);

    let group_by: Vec<String> = params.group_by.clone().unwrap_or_default();
    let mut checked: Vec<String> = Vec::new();
    for column in group_by
        .iter()
        .chain(params.column.iter())
        .chain(params.columns.iter().flatten())
    {
        let resolved = catalog_name(&info.columns, column).ok_or_else(|| {
            tool_error(format!(
                "unknown column '{column}' on table '{}'",
                params.table
            ))
        })?;
        if denied(config, resolved) {
            return Err(tool_error(format!(
                "column '{resolved}' is deny-listed and cannot be used in a summary \
                 (group_by, aggregate column or sample projection)"
            )));
        }
        checked.push(resolved.to_string());
    }
    let group_sql: Vec<String> = group_by
        .iter()
        .filter_map(|c| catalog_name(&info.columns, c))
        .map(|c| quote_ident(c, engine))
        .collect();

    let (sql, meta_kind, exact) = match params.kind {
        SummaryKind::Count => {
            let groups = if group_sql.is_empty() {
                String::new()
            } else {
                format!("{}, ", group_sql.join(", "))
            };
            let group_clause = if group_sql.is_empty() {
                String::new()
            } else {
                format!(" GROUP BY {}", group_sql.join(", "))
            };
            (
                format!(
                    "SELECT {groups}COUNT(*) AS \"count\" FROM {from}{group_clause} ORDER BY \"count\" DESC LIMIT {limit}"
                ),
                "count",
                true,
            )
        }
        SummaryKind::Sample => {
            let cols: Vec<String> = match &params.columns {
                Some(columns) => columns
                    .iter()
                    .filter_map(|c| catalog_name(&info.columns, c))
                    .map(|c| quote_ident(c, engine))
                    .collect(),
                None => info
                    .columns
                    .iter()
                    .map(|c| quote_ident(&c.name, engine))
                    .collect(),
            };
            if cols.is_empty() {
                return Err(tool_error("summary sample returned no columns"));
            }
            (
                format!(
                    "SELECT {} FROM {from} ORDER BY {} LIMIT {limit}",
                    cols.join(", "),
                    cols[0]
                ),
                "sample",
                // A sample is not a total; it is never presented as exact.
                false,
            )
        }
        SummaryKind::Aggregate => {
            let aggregate = params
                .aggregate
                .ok_or_else(|| tool_error("aggregate summary requires `aggregate`"))?;
            let target = match (&params.column, aggregate) {
                (Some(column), _) => {
                    let resolved = catalog_name(&info.columns, column).ok_or_else(|| {
                        tool_error(format!(
                            "unknown column '{column}' on table '{}'",
                            params.table
                        ))
                    })?;
                    quote_ident(resolved, engine)
                }
                (None, AggregateFn::Count) => "*".to_string(),
                (None, _) => {
                    return Err(tool_error(format!(
                        "aggregate {} requires `column`",
                        aggregate.sql().to_lowercase()
                    )))
                }
            };
            let group_select = if group_sql.is_empty() {
                String::new()
            } else {
                format!("{}, ", group_sql.join(", "))
            };
            let group_clause = if group_sql.is_empty() {
                String::new()
            } else {
                format!(" GROUP BY {}", group_sql.join(", "))
            };
            (
                format!(
                    "SELECT {group_select}{}({target}) AS \"value\" FROM {from}{group_clause} LIMIT {limit}",
                    aggregate.sql()
                ),
                "aggregate",
                true,
            )
        }
    };

    let mut opts: ExecOptions = exec_options(config);
    opts.page_offset = 0;
    opts.page_size = None;
    let result = execute_query(&driver, &sql, &[], &opts)
        .await
        .map_err(|e| tool_error(e.to_string()))?;

    let mut payload = serde_json::json!({
        "ok": result.ok,
        "error": result.error,
    });
    if let Some(data) = &result.data {
        payload["data"] = format_columnar(ResultFormat::Columnar, data.clone());
        payload["data"]["meta"]["summary"] = serde_json::json!({
            "kind": meta_kind,
            // Counts and aggregates are computed over real rows; a sample is
            // explicitly not exact, and no planner estimate is ever reported.
            "exact": exact,
            "source": "computed",
            "limit": limit,
            "group_by": group_by,
            "aggregate": params.aggregate.map(|a| a.sql().to_lowercase()),
            "table": params.table,
        });
    }
    Ok(CallToolResult::success(vec![ContentBlock::text(
        to_json_text(&payload),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_quoted_per_engine() {
        assert_eq!(quote_ident("a`b", EngineKind::MySql), "`a``b`");
        assert_eq!(quote_ident("a\"b", EngineKind::Sqlite), "\"a\"\"b\"");
        assert_eq!(quote_ident("plain", EngineKind::Postgres), "\"plain\"");
        assert_eq!(quote_ident("a\"b", EngineKind::DuckDb), "\"a\"\"b\"");
    }

    #[test]
    fn table_names_split_on_the_last_dot() {
        assert_eq!(
            split_table("public.users"),
            (Some("public".into()), "users".into())
        );
        assert_eq!(split_table("users"), (None, "users".into()));
        assert_eq!(
            split_table("weird.name.with.dots"),
            (Some("weird.name.with".into()), "dots".into())
        );
    }
}
