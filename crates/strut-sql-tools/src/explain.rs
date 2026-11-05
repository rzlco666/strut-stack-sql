//! EXPLAIN advisor v2 (PRD-F3-04/05/06).
//!
//! Plans from PostgreSQL, MySQL, SQLite, DuckDB, ClickHouse and SQL Server are normalized to one flat node
//! model. Detection is deterministic rule checking over plan + catalog
//! stats; nothing here executes DDL. `EXPLAIN ANALYZE` is opt-in at the
//! handler because it executes the statement.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use strut_sql_driver_api::{BoundQuery, EngineDriver, EngineKind, SqlValue, TableStats};
use strut_sql_guard::{detect_anti_patterns, GuardError, PatternContext, WriteMode};

use crate::bind::to_sql_values;

mod duckdb;
#[path = "explain_sqlserver.rs"]
mod sqlserver;

#[derive(Debug, thiserror::Error)]
pub enum ExplainError {
    #[error(transparent)]
    Guard(#[from] GuardError),
    #[error("database error: {0}")]
    Database(String),
    #[error("Unsupported: {0}")]
    Unsupported(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplainIssue {
    pub severity: String,
    pub code: String,
    pub message: String,
    pub suggestion: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanNode {
    pub node_type: String,
    /// Depth in the normalized plan tree (0 = root).
    #[serde(skip_serializing_if = "is_zero_depth")]
    pub depth: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_time_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_type: Option<String>,
    /// Engine-reported disk spill indicator (e.g. external sort/hash batches).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spill: Option<String>,
}

fn is_zero_depth(depth: &u32) -> bool {
    *depth == 0
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexSuggestion {
    /// Advice text only — never executed by the advisor.
    pub sql: String,
    pub table: String,
    pub columns: Vec<String>,
    pub plan_node: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_rows: Option<i64>,
    pub confidence: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplainSummary {
    pub engine: String,
    pub query: String,
    /// True when EXPLAIN ANALYZE actually executed the statement.
    pub analyze: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_rows: Option<i64>,
    pub nodes: Vec<PlanNode>,
    pub issues: Vec<ExplainIssue>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub index_suggestions: Vec<IndexSuggestion>,
    /// Compact compatibility list derived from issues.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AnalyzeOptions {
    /// Table allow/deny + row filters for the caller (R2-04). The advisor must
    /// not explain a table the principal cannot query.
    pub table_policy: Option<strut_sql_guard::TablePolicy>,
    /// Request EXPLAIN ANALYZE (executes). Caller must also allow it in config.
    pub analyze: bool,
    /// Include AST anti-pattern findings (default true in full mode).
    pub include_anti_patterns: bool,
}

/// Backwards-compatible entry point (plain EXPLAIN).
pub async fn analyze_query(
    driver: &Arc<dyn EngineDriver>,
    sql: &str,
    params: &[Value],
    write_mode: WriteMode,
    timeout: Duration,
) -> Result<ExplainSummary, ExplainError> {
    analyze_query_with_options(
        driver,
        sql,
        params,
        write_mode,
        timeout,
        AnalyzeOptions::default(),
    )
    .await
}

pub async fn analyze_query_with_options(
    driver: &Arc<dyn EngineDriver>,
    sql: &str,
    params: &[Value],
    write_mode: WriteMode,
    timeout: Duration,
    options: AnalyzeOptions,
) -> Result<ExplainSummary, ExplainError> {
    let engine = driver.kind();
    if options.analyze && matches!(engine, EngineKind::Sqlite | EngineKind::ClickHouse) {
        return Err(ExplainError::Other(format!(
            "EXPLAIN ANALYZE is not supported on {}",
            match engine {
                EngineKind::Sqlite => "SQLite",
                _ => "ClickHouse",
            }
        )));
    }
    if engine == EngineKind::SqlServer && options.analyze && !write_mode.allows_dml() {
        return Err(ExplainError::Guard(GuardError::Denied(
            "analyze:true executes the query and requires --allow-writes".into(),
        )));
    }
    let prepared = strut_sql_guard::validate_and_prepare_with_options(
        sql,
        params,
        engine,
        write_mode,
        100,
        strut_sql_guard::PrepareOptions {
            page_offset: 0,
            page_size: None,
            table_policy: options.table_policy.clone(),
        },
    )?;

    if engine == EngineKind::SqlServer && !prepared.class.is_select_like() {
        return Err(ExplainError::Other(
            "analyze_query_performance only supports SELECT-like queries".into(),
        ));
    }
    if !prepared.class.is_select_like()
        && prepared.class.label() != "SELECT"
        && prepared.class.label() != "EXPLAIN"
        && !sql.trim().to_uppercase().starts_with("SELECT")
    {
        return Err(ExplainError::Other(
            "analyze_query_performance only supports SELECT-like queries".into(),
        ));
    }

    let explain_sql = match (engine, options.analyze) {
        (EngineKind::Postgres, true) => {
            format!("EXPLAIN (ANALYZE, FORMAT JSON) {}", prepared.sql)
        }
        (EngineKind::Postgres, false) => format!("EXPLAIN (FORMAT JSON) {}", prepared.sql),
        (EngineKind::MySql, true) => format!("EXPLAIN ANALYZE {}", prepared.sql),
        (EngineKind::MySql, false) => format!("EXPLAIN FORMAT=JSON {}", prepared.sql),
        (EngineKind::DuckDb, true) => {
            format!("EXPLAIN (ANALYZE, FORMAT JSON) {}", prepared.sql)
        }
        (EngineKind::DuckDb, false) => format!("EXPLAIN (FORMAT JSON) {}", prepared.sql),
        (EngineKind::Sqlite, false) => format!("EXPLAIN QUERY PLAN {}", prepared.sql),
        (EngineKind::Sqlite, true) => unreachable!("checked above"),
        (EngineKind::ClickHouse, false) => format!("EXPLAIN PLAN {}", prepared.sql),
        (EngineKind::ClickHouse, true) => unreachable!("checked above"),
        (EngineKind::SqlServer, _) => prepared.sql.clone(),
    };

    let bound = BoundQuery {
        sql: explain_sql,
        params: to_sql_values(&prepared.params).map_err(ExplainError::Other)?,
        row_cap: None,
        fetch_rows: true,
    };
    let execute = async {
        if engine == EngineKind::SqlServer {
            driver.explain(&bound, options.analyze).await
        } else {
            driver.execute(&bound).await
        }
    };
    let result = tokio::time::timeout(timeout, execute)
        .await
        .map_err(|_| ExplainError::Other("explain timeout".into()))?
        .map_err(|error| {
            let message = error.to_string();
            if engine == EngineKind::DuckDb {
                let lower = message.to_ascii_lowercase();
                if matches!(error, strut_sql_driver_api::DriverError::Unsupported(_))
                    || ((lower.contains("not implemented") || lower.contains("not supported"))
                        && (lower.contains("explain")
                            || lower.contains("format")
                            || lower.contains("json")))
                {
                    return ExplainError::Unsupported(format!(
                        "DuckDB EXPLAIN{} (FORMAT JSON): {message}",
                        if options.analyze { " ANALYZE" } else { "" }
                    ));
                }
            }
            ExplainError::Database(message)
        })?;

    let mut summary = match engine {
        EngineKind::Sqlite => {
            let rows = sqlite_details(
                result.cols.iter().map(|c| c.name.as_str()).collect(),
                result.rows,
            );
            normalize_sqlite(&prepared.sql, rows)
        }
        EngineKind::Postgres => {
            let json = explain_json(&result.rows)?;
            normalize_postgres(&prepared.sql, json)
        }
        EngineKind::MySql if options.analyze => {
            let lines = text_rows(&result.rows);
            normalize_mysql_analyze(&prepared.sql, lines)
        }
        EngineKind::MySql => {
            let json = explain_json(&result.rows)?;
            normalize_mysql(&prepared.sql, json)
        }
        EngineKind::DuckDb => duckdb::normalize(&prepared.sql, &result, options.analyze)?,
        EngineKind::ClickHouse => {
            let lines = text_rows(&result.rows);
            normalize_clickhouse(&prepared.sql, lines)
        }
        EngineKind::SqlServer => sqlserver::normalize(&prepared.sql, &result.rows)?,
    };
    summary.analyze = options.analyze;

    let stats = driver.table_stats(None).await.unwrap_or_default();
    let columns = driver.list_columns(None).await.unwrap_or_default();
    let mut ctx = PatternContext::default();
    for c in &columns {
        if let Some(table) = &c.table {
            *ctx.wide_tables.entry(table.clone()).or_default() += 1;
        }
    }
    for stat in &stats {
        let indexed: BTreeSet<String> = stat
            .indexes
            .iter()
            .flat_map(|i| i.columns.iter().cloned())
            .collect();
        if !indexed.is_empty() {
            ctx.indexed_columns.insert(stat.table.clone(), indexed);
        }
    }

    advise(&mut summary, &stats, engine);
    if options.include_anti_patterns {
        if let Ok(found) = detect_anti_patterns(&prepared.sql, engine, &ctx) {
            for pattern in found {
                summary.issues.push(ExplainIssue {
                    severity: pattern.severity,
                    code: pattern.code,
                    message: pattern.message,
                    suggestion: pattern.suggestion,
                });
            }
        }
    }
    summary.issues.sort_by(|a, b| {
        (a.code.as_str(), a.message.as_str()).cmp(&(b.code.as_str(), b.message.as_str()))
    });
    summary
        .issues
        .dedup_by(|a, b| a.code == b.code && a.message == b.message);
    summary.warnings = summary
        .issues
        .iter()
        .filter(|i| i.severity != "info")
        .map(|i| format!("{}: {}", i.code, i.message))
        .collect();
    Ok(summary)
}

fn text_rows(rows: &[Vec<SqlValue>]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            row.first()
                .map(|cell| match cell {
                    SqlValue::String(s) => s.clone(),
                    SqlValue::Json(v) => v.to_string(),
                    other => format!("{other:?}"),
                })
                .unwrap_or_default()
        })
        .collect()
}

fn explain_json(rows: &[Vec<SqlValue>]) -> Result<Value, ExplainError> {
    let cell = rows
        .first()
        .and_then(|r| r.first())
        .ok_or_else(|| ExplainError::Other("explain returned no rows".into()))?;
    match cell {
        SqlValue::Json(v) => Ok(v.clone()),
        SqlValue::String(s) => serde_json::from_str(s)
            .map_err(|e| ExplainError::Other(format!("invalid explain json: {e}"))),
        other => Err(ExplainError::Other(format!(
            "unexpected explain value: {other:?}"
        ))),
    }
}

/// Human-readable plan lines from a SQLite EXPLAIN result, located by column
/// name (`detail`, then `opcode`, else the last column).
fn sqlite_details(cols: Vec<&str>, rows: Vec<Vec<SqlValue>>) -> Vec<String> {
    let detail_idx = cols
        .iter()
        .position(|c| c.eq_ignore_ascii_case("detail"))
        .or_else(|| cols.iter().position(|c| c.eq_ignore_ascii_case("opcode")))
        .or_else(|| cols.len().checked_sub(1));
    rows.into_iter()
        .map(|row| match detail_idx.and_then(|i| row.get(i)) {
            Some(SqlValue::String(s)) => s.clone(),
            Some(SqlValue::I64(n)) => n.to_string(),
            _ => String::new(),
        })
        .collect()
}

fn normalize_clickhouse(query: &str, lines: Vec<String>) -> ExplainSummary {
    // `EXPLAIN PLAN` prints one line per node, indented two spaces per level:
    //   Expression ((Project names + Projection))
    //     Aggregating
    //       ReadFromMergeTree (default.events)
    let mut nodes = Vec::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let depth = (line.len().saturating_sub(line.trim_start().len()) / 2) as u32;
        let (label, detail) = match trimmed.split_once(" (") {
            // The split consumed the opening `(`; drop exactly one closing one.
            Some((label, rest)) => (label.trim(), Some(rest.strip_suffix(')').unwrap_or(rest))),
            None => (trimmed, None),
        };
        let node_type = label
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let relation = label.starts_with("ReadFrom").then(|| {
            detail
                .unwrap_or_default()
                .split('.')
                .next_back()
                .unwrap_or_default()
                .trim_matches(|c| c == ')' || c == ' ')
                .to_string()
        });
        nodes.push(PlanNode {
            node_type,
            depth,
            relation: relation.filter(|r| !r.is_empty()),
            cost: None,
            rows: None,
            actual_rows: None,
            actual_time_ms: None,
            filter: detail.map(str::to_string),
            join_type: None,
            spill: None,
        });
    }
    ExplainSummary {
        engine: "clickhouse".into(),
        query: query.into(),
        analyze: false,
        total_cost: None,
        plan_rows: None,
        nodes,
        issues: Vec::new(),
        index_suggestions: Vec::new(),
        warnings: Vec::new(),
    }
}

fn normalize_sqlite(query: &str, details: Vec<String>) -> ExplainSummary {
    let mut nodes = Vec::new();
    for detail in details {
        let upper = detail.to_uppercase();
        let node_type = if upper.contains("SCAN TABLE") || upper.starts_with("SCAN ") {
            "SCAN"
        } else if upper.contains("SEARCH") {
            "SEARCH"
        } else if upper.contains("TEMP B-TREE") {
            "Sort"
        } else if upper.contains("CORRELATED") {
            "Correlated Subquery"
        } else {
            "OTHER"
        };
        if node_type == "OTHER" {
            continue;
        }
        nodes.push(PlanNode {
            node_type: node_type.into(),
            depth: 0,
            relation: sqlite_relation_from_detail(&detail),
            cost: None,
            rows: None,
            actual_rows: None,
            actual_time_ms: None,
            filter: detail.contains('(').then(|| detail.clone()),
            join_type: upper.contains("LEFT JOIN").then(|| "LEFT".to_string()),
            spill: None,
        });
    }
    ExplainSummary {
        engine: "sqlite".into(),
        query: query.into(),
        analyze: false,
        total_cost: None,
        plan_rows: None,
        nodes,
        issues: Vec::new(),
        index_suggestions: Vec::new(),
        warnings: Vec::new(),
    }
}

fn sqlite_relation_from_detail(detail: &str) -> Option<String> {
    let upper = detail.to_uppercase();
    for marker in ["SCAN TABLE ", "SEARCH TABLE ", "SCAN ", "SEARCH "] {
        if let Some(idx) = upper.find(marker) {
            let rest = &detail[idx + marker.len()..];
            return rest
                .split_whitespace()
                .next()
                .map(|s| s.trim_matches('"').to_lowercase());
        }
    }
    None
}

fn normalize_postgres(query: &str, json: Value) -> ExplainSummary {
    let mut nodes = Vec::new();
    let mut total_cost = None;
    let mut plan_rows = None;
    if let Some(arr) = json.as_array() {
        if let Some(root) = arr.first().and_then(|v| v.get("Plan")) {
            total_cost = root.get("Total Cost").and_then(Value::as_f64);
            plan_rows = root.get("Plan Rows").and_then(Value::as_i64);
            walk_pg_plan(root, &mut nodes, 0);
        }
    }
    ExplainSummary {
        engine: "postgresql".into(),
        query: query.into(),
        analyze: false,
        total_cost,
        plan_rows,
        nodes,
        issues: Vec::new(),
        index_suggestions: Vec::new(),
        warnings: Vec::new(),
    }
}

fn walk_pg_plan(plan: &Value, nodes: &mut Vec<PlanNode>, depth: u32) {
    let node_type = plan
        .get("Node Type")
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_string();
    let relation = plan
        .get("Relation Name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let cost = plan.get("Total Cost").and_then(Value::as_f64);
    let rows = plan.get("Plan Rows").and_then(Value::as_i64);
    let actual_rows = plan.get("Actual Rows").and_then(Value::as_i64);
    let actual_time_ms = plan.get("Actual Total Time").and_then(Value::as_f64);
    let filter = plan
        .get("Filter")
        .or_else(|| plan.get("Index Cond"))
        .or_else(|| plan.get("Join Filter"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let join_type = plan
        .get("Join Type")
        .and_then(Value::as_str)
        .map(str::to_string);
    let spill = plan
        .get("Sort Method")
        .and_then(Value::as_str)
        .filter(|method| method.to_ascii_lowercase().contains("external"))
        .map(str::to_string)
        .or_else(|| {
            plan.get("Hash Batches")
                .and_then(Value::as_i64)
                .filter(|batches| *batches > 1)
                .map(|batches| format!("hash batches: {batches}"))
        });
    nodes.push(PlanNode {
        node_type,
        depth,
        relation,
        cost,
        rows,
        actual_rows,
        actual_time_ms,
        filter,
        join_type,
        spill,
    });
    if let Some(children) = plan.get("Plans").and_then(Value::as_array) {
        for child in children {
            walk_pg_plan(child, nodes, depth.saturating_add(1));
        }
    }
}

fn normalize_mysql(query: &str, json: Value) -> ExplainSummary {
    let mut nodes = Vec::new();
    let mut total_cost = None;
    let mut plan_rows = None;
    if let Some(query_block) = json.get("query_block") {
        if let Some(cost) = query_block.get("cost_info") {
            total_cost = cost.get("query_cost").and_then(Value::as_f64);
            plan_rows = cost.get("rows_examined_per_scan").and_then(Value::as_i64);
        }
        walk_mysql_object(query_block, &mut nodes, 0);
    }
    if total_cost.is_none() {
        total_cost = nodes.iter().filter_map(|n| n.cost).reduce(|a, b| a + b);
    }
    if plan_rows.is_none() {
        plan_rows = nodes.iter().filter_map(|n| n.rows).max();
    }
    ExplainSummary {
        engine: "mysql".into(),
        query: query.into(),
        analyze: false,
        total_cost,
        plan_rows,
        nodes,
        issues: Vec::new(),
        index_suggestions: Vec::new(),
        warnings: Vec::new(),
    }
}

/// MySQL EXPLAIN ANALYZE returns a TREE-formatted text plan. Parse the stable
/// parts (node label, relation, estimated/actual rows and time).
fn normalize_mysql_analyze(query: &str, lines: Vec<String>) -> ExplainSummary {
    let mut nodes = Vec::new();
    for line in lines {
        let indent = line.len().saturating_sub(line.trim_start().len());
        let depth = (indent / 2) as u32;
        let Some(rest) = line.trim_start().strip_prefix("-> ") else {
            continue;
        };
        let label = rest.split("  (").next().unwrap_or(rest).trim().to_string();
        let relation = label
            .split(" on ")
            .nth(1)
            .map(|s| s.split_whitespace().next().unwrap_or(s).to_string());
        let rows = extract_metric(rest, "rows=");
        let actual_time_ms = extract_metric(rest, "actual time=");
        let actual_rows = rest
            .split("actual time=")
            .nth(1)
            .and_then(|tail| extract_metric(tail, "rows="))
            .map(|v| v as i64);
        nodes.push(PlanNode {
            node_type: label
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" "),
            depth,
            relation,
            cost: None,
            rows: rows.map(|v| v as i64),
            actual_rows,
            actual_time_ms,
            filter: label.contains("Filter").then_some(label.clone()),
            join_type: label.contains("join").then(|| "HASH".to_string()),
            spill: None,
        });
    }
    ExplainSummary {
        engine: "mysql".into(),
        query: query.into(),
        analyze: true,
        total_cost: None,
        plan_rows: nodes.iter().filter_map(|n| n.rows).max(),
        nodes,
        issues: Vec::new(),
        index_suggestions: Vec::new(),
        warnings: Vec::new(),
    }
}

fn extract_metric(text: &str, key: &str) -> Option<f64> {
    let idx = text.find(key)? + key.len();
    let tail = &text[idx..];
    let end = tail
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}

fn walk_mysql_object(value: &Value, nodes: &mut Vec<PlanNode>, depth: u32) {
    let Some(object) = value.as_object() else {
        return;
    };
    if let Some(table) = object.get("table") {
        if let Some(node) = mysql_table_node(table, depth) {
            nodes.push(node);
        }
    }
    for key in [
        "query_block",
        "nested_loop",
        "grouping_operation",
        "ordering_operation",
        "duplicates_removal",
        "union_result",
        "query_specifications",
        "materialized_from_subquery",
        "attached_subqueries",
    ] {
        if let Some(child) = object.get(key) {
            walk_mysql_child(key, child, nodes, depth);
        }
    }
}

fn walk_mysql_child(key: &str, value: &Value, nodes: &mut Vec<PlanNode>, depth: u32) {
    if key == "nested_loop" {
        let rows = value.as_array().and_then(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.get("table")
                        .and_then(|t| t.get("rows_examined_per_scan"))
                        .and_then(Value::as_i64)
                })
                .max()
        });
        nodes.push(PlanNode {
            node_type: "Nested Loop".into(),
            depth,
            relation: None,
            cost: None,
            rows,
            actual_rows: None,
            actual_time_ms: None,
            filter: None,
            join_type: None,
            spill: None,
        });
        if let Some(array) = value.as_array() {
            for item in array {
                walk_mysql_object(item, nodes, depth.saturating_add(1));
            }
        }
        return;
    }
    if let Some(array) = value.as_array() {
        for item in array {
            walk_mysql_object(item, nodes, depth.saturating_add(1));
        }
        return;
    }
    if key == "ordering_operation" || key == "grouping_operation" {
        let rows = value
            .get("cost_info")
            .and_then(|c| c.get("sort_cost").or_else(|| c.get("prefix_cost")))
            .and_then(Value::as_f64);
        let label = if key == "ordering_operation" {
            "Sort"
        } else {
            "Aggregate"
        };
        let filter = value
            .get("using_temporary_table")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then(|| "using temporary table".to_string());
        nodes.push(PlanNode {
            node_type: label.into(),
            depth,
            relation: None,
            cost: rows,
            rows: value
                .get("cost_info")
                .and_then(|c| c.get("rows_examined_per_scan"))
                .and_then(Value::as_i64),
            actual_rows: None,
            actual_time_ms: None,
            filter,
            join_type: None,
            spill: None,
        });
    }
    walk_mysql_object(value, nodes, depth.saturating_add(1));
}

fn mysql_table_node(table: &Value, depth: u32) -> Option<PlanNode> {
    if table.is_null() {
        return None;
    }
    let access = table
        .get("access_type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let relation = table
        .get("table_name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let rows = table
        .get("rows_examined_per_scan")
        .or_else(|| table.get("rows"))
        .and_then(Value::as_i64);
    let cost = table
        .get("cost_info")
        .and_then(|c| c.get("read_cost").or_else(|| c.get("prefix_cost")))
        .and_then(Value::as_f64);
    let filter = table
        .get("attached_condition")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(PlanNode {
        node_type: access,
        depth,
        relation,
        cost,
        rows,
        actual_rows: None,
        actual_time_ms: None,
        filter,
        join_type: table
            .get("using_join_buffer")
            .and_then(Value::as_str)
            .map(str::to_string),
        spill: None,
    })
}

fn advise(summary: &mut ExplainSummary, stats: &[TableStats], engine: EngineKind) {
    let stats_by_table: BTreeMap<&str, &TableStats> =
        stats.iter().map(|s| (s.table.as_str(), s)).collect();
    let mut issues = Vec::new();
    let mut suggestions = Vec::new();
    let mut seen_suggestions: BTreeSet<(String, Vec<String>)> = BTreeSet::new();
    let total_cost = summary.total_cost;

    for (node_index, node) in summary.nodes.iter().enumerate() {
        let upper = if engine == EngineKind::DuckDb {
            node.node_type.to_uppercase().replace('_', " ")
        } else {
            node.node_type.to_uppercase()
        };
        let table_stats = node
            .relation
            .as_deref()
            .and_then(|relation| stats_by_table.get(relation).copied());
        let estimated_rows = node
            .rows
            .or_else(|| table_stats.and_then(|s| s.approx_rows));

        let full_scan = upper.contains("SEQ SCAN")
            || upper.contains("SEQUENTIAL SCAN")
            || upper.contains("SCAN TABLE")
            || upper == "SCAN"
            || upper == "ALL"
            || (engine == EngineKind::SqlServer
                && matches!(
                    upper.as_str(),
                    "TABLE SCAN" | "INDEX SCAN" | "CLUSTERED INDEX SCAN"
                ))
            || upper.contains("FULL SCAN");
        if full_scan {
            let (severity, message) = match estimated_rows {
                Some(rows) if rows >= 1_000_000 => {
                    ("critical", format!("full scan over ~{rows} rows"))
                }
                Some(rows) if rows >= 1_000 => ("warning", format!("full scan over ~{rows} rows")),
                Some(rows) => ("info", format!("full scan over ~{rows} rows")),
                None => ("warning", "full scan over unknown row count".to_string()),
            };
            // V7: name the columns the plan actually compares instead of a
            // generic sentence. A multi-column filter gets a composite index.
            let predicate = node.filter.as_deref().map(scan_filter).unwrap_or_default();
            let advice = if engine == EngineKind::DuckDb {
                "Sequential scans are normal in DuckDB; narrow predicates/projections and inspect selectivity before considering an ART index for selective equality lookups.".to_string()
            } else {
                match predicate.columns.as_slice() {
                    [] => "Add an index on the filter/join columns, or narrow the predicate."
                        .to_string(),
                    [only] => format!(
                        "The scan filter compares {only}; add an index on {only}, or narrow the predicate."
                    ),
                    columns => format!(
                        "The scan filter compares {}; add a composite index on ({}) with the most selective column first.",
                        columns.join(", "),
                        columns.join(", ")
                    ),
                }
            };
            issues.push(issue(severity, "full_scan", &message, &advice));
            if let Some(table) = node
                .relation
                .as_deref()
                .filter(|_| engine != EngineKind::DuckDb)
            {
                let columns = predicate.columns;
                let covered = table_stats.is_some_and(|s| {
                    s.indexes.iter().any(|idx| {
                        idx.columns.first().is_some_and(|leading| {
                            columns.iter().any(|c| c.eq_ignore_ascii_case(leading))
                        })
                    })
                });
                let key = (table.to_string(), columns.clone());
                if !covered && !columns.is_empty() && seen_suggestions.insert(key) {
                    let sql = create_index_sql(engine, table, &columns);
                    let reason = if columns.len() > 1 {
                        format!(
                            "sequential scan filters on unindexed columns {} — a composite index is needed",
                            columns.join(", ")
                        )
                    } else {
                        format!(
                            "sequential scan with a filter on unindexed column {}",
                            columns[0]
                        )
                    };
                    suggestions.push(IndexSuggestion {
                        sql,
                        table: table.to_string(),
                        columns,
                        plan_node: node.node_type.clone(),
                        estimated_rows,
                        confidence: if estimated_rows.is_some_and(|r| r >= 1_000) {
                            "high".into()
                        } else {
                            "medium".into()
                        },
                        reason,
                    });
                }
            }
        }

        if upper.contains("NESTED LOOP") {
            if let Some(rows) = estimated_rows {
                if rows >= 10_000 {
                    // V7: join keys seen in this node or its subtree, so the
                    // advice names the actual columns instead of a template.
                    let pairs = subtree_join_pairs(&summary.nodes[node_index..]);
                    let advice = if pairs.is_empty() {
                        "Add a join index, reduce the input with a predicate, or prefer a hash join."
                            .to_string()
                    } else {
                        let rendered: Vec<String> = pairs
                            .iter()
                            .take(3)
                            .map(|(l, r)| format!("{l} = {r}"))
                            .collect();
                        let inner: Vec<String> = pairs
                            .iter()
                            .take(3)
                            .map(|(_, r)| last_segment(r).to_string())
                            .collect();
                        format!(
                            "Join keys are {}; index the inner side on ({}) or prefer a hash join.",
                            rendered.join(", "),
                            inner.join(", ")
                        )
                    };
                    issues.push(issue(
                        if rows >= 1_000_000 {
                            "critical"
                        } else {
                            "warning"
                        },
                        "nested_loop_large",
                        &format!("nested loop over ~{rows} estimated rows"),
                        &advice,
                    ));
                }
            }
        }

        let expensive_sort = (upper.contains("SORT")
            || upper.contains("HASH")
            || upper.contains("AGGREGATE")
            || upper.contains("TEMPORARY")
            || (engine == EngineKind::DuckDb && upper.contains("ORDER BY")))
            && estimated_rows.is_some_and(|rows| rows >= 100_000);
        if expensive_sort {
            issues.push(issue(
                "warning",
                "expensive_sort_hash",
                &format!(
                    "{} over ~{} rows",
                    node.node_type,
                    estimated_rows.unwrap_or_default()
                ),
                "Consider an index matching the ORDER BY/GROUP BY keys or reduce input rows.",
            ));
        }

        if let Some(spill) = &node.spill {
            issues.push(issue(
                "warning",
                "plan_spill",
                &format!("{} spills to disk ({spill})", node.node_type),
                "Increase work_mem or reduce the input rows before sorting/hashing.",
            ));
        }

        if let (Some(cost), Some(total)) = (node.cost, total_cost) {
            if total >= 100.0 && cost / total >= 0.4 {
                issues.push(issue(
                    "info",
                    "dominant_cost_node",
                    &format!(
                        "{} accounts for {:.0}% of total plan cost",
                        node.node_type,
                        (cost / total) * 100.0
                    ),
                    "Focus optimization on this node first.",
                ));
            }
        }

        if let (Some(planned), Some(actual)) = (node.rows, node.actual_rows) {
            let miss = if planned <= 1 {
                actual >= 100
            } else {
                actual >= planned.saturating_mul(10) || actual.saturating_mul(10) <= planned
            };
            if miss {
                issues.push(issue(
                    "warning",
                    "row_estimate_miss",
                    &format!("planned {planned} rows but executed {actual}"),
                    "Refresh planner statistics or rewrite the predicate so estimates are usable.",
                ));
            }
        }

        if let Some(filter) = &node.filter {
            if filter.contains("::") || filter.to_uppercase().contains("CAST(") {
                issues.push(issue(
                    "info",
                    "implicit_type_cast",
                    "filter contains an explicit/implicit type cast",
                    "Compare columns with matching types so indexes remain usable.",
                ));
            }
            if filter.contains("LIKE '%") || filter.contains("LIKE \"%") {
                issues.push(issue(
                    "warning",
                    "leading_wildcard_like",
                    "LIKE pattern starts with a wildcard",
                    "Use a prefix/trigram index or restructure the predicate.",
                ));
            }
        }
    }

    summary.issues = issues;
    summary.index_suggestions = suggestions;
}

fn issue(severity: &str, code: &str, message: &str, suggestion: &str) -> ExplainIssue {
    ExplainIssue {
        severity: severity.into(),
        code: code.into(),
        message: message.into(),
        suggestion: suggestion.into(),
    }
}

/// Columns compared in a plan filter, plus `a = b` join equalities.
#[derive(Debug, Default, PartialEq, Eq)]
struct FilterPredicate {
    /// Column names (last path segment) that take part in a comparison.
    columns: Vec<String>,
    /// Equality pairs where both sides are identifiers (join predicates),
    /// rendered as the names they appear with (`t1.a`, `t2.a`).
    equalities: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PredicateToken {
    Ident { full: String, last: String },
    Compare,
    Keyword(String),
    Literal,
    Punct,
    Cast,
}

fn is_filter_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "AND"
            | "OR"
            | "NOT"
            | "IS"
            | "NULL"
            | "IN"
            | "LIKE"
            | "BETWEEN"
            | "ON"
            | "USING"
            | "WHERE"
            | "FILTER"
            | "TRUE"
            | "FALSE"
            | "AS"
    )
}

/// Parse an identifier chain (`a`, `t.a`, `"db"."t"."c"`, `[t].[c]`).
/// Returns the unquoted full name, its last segment, and the next byte index.
fn parse_ident(bytes: &[u8], start: usize) -> Option<(String, String, usize)> {
    let mut index = start;
    let mut segments: Vec<String> = Vec::new();
    loop {
        while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
            index += 1;
        }
        let segment = match bytes.get(index).copied()? {
            quote @ (b'"' | b'`') => {
                index += 1;
                let mut segment = String::new();
                loop {
                    let byte = *bytes.get(index)?;
                    if byte == quote {
                        if bytes.get(index + 1) == Some(&quote) {
                            segment.push(quote as char);
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    segment.push(byte as char);
                    index += 1;
                }
                segment
            }
            b'[' => {
                index += 1;
                let mut segment = String::new();
                loop {
                    let byte = *bytes.get(index)?;
                    if byte == b']' {
                        if bytes.get(index + 1) == Some(&b']') {
                            segment.push(']');
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    segment.push(byte as char);
                    index += 1;
                }
                segment
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let begin = index;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                String::from_utf8_lossy(&bytes[begin..index]).into_owned()
            }
            _ => return None,
        };
        segments.push(segment);
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            continue;
        }
        break;
    }
    let last = segments.last()?.clone();
    Some((segments.join("."), last, index))
}

/// Tokenizer for textual plan filters. String literals, numbers, placeholders
/// and cast type names become `Literal`/`Cast`, so they can never be mistaken
/// for a column (`status = 'open'::text` yields exactly `status`).
fn predicate_tokens(filter: &str) -> Vec<PredicateToken> {
    let bytes = filter.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b' ' | b'\t' | b'\n' | b'\r' => index += 1,
            b'\'' => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\'' {
                        if bytes.get(index + 1) == Some(&b'\'') {
                            index += 2;
                            continue;
                        }
                        index += 1;
                        break;
                    }
                    index += 1;
                }
                tokens.push(PredicateToken::Literal);
            }
            byte if byte.is_ascii_digit() => {
                while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.')
                {
                    index += 1;
                }
                tokens.push(PredicateToken::Literal);
            }
            b'?' => {
                index += 1;
                tokens.push(PredicateToken::Literal);
            }
            b'$' | b'@' | b':' => {
                let is_cast = bytes[index] == b':' && bytes.get(index + 1) == Some(&b':');
                index += if is_cast { 2 } else { 1 };
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                tokens.push(if is_cast {
                    PredicateToken::Cast
                } else {
                    PredicateToken::Literal
                });
            }
            b'=' | b'<' | b'>' | b'!' => {
                index += 1;
                while index < bytes.len() && matches!(bytes[index], b'=' | b'<' | b'>' | b'!') {
                    index += 1;
                }
                tokens.push(PredicateToken::Compare);
            }
            b'(' | b')' | b',' => {
                index += 1;
                tokens.push(PredicateToken::Punct);
            }
            _ => match parse_ident(bytes, index) {
                Some((full, last, next)) => {
                    index = next;
                    if is_filter_keyword(&full) {
                        tokens.push(PredicateToken::Keyword(full.to_ascii_uppercase()));
                    } else {
                        tokens.push(PredicateToken::Ident { full, last });
                    }
                }
                None => index += 1,
            },
        }
    }
    tokens
}

fn push_unique(columns: &mut Vec<String>, column: &str) {
    if !columns.iter().any(|c| c.eq_ignore_ascii_case(column)) {
        columns.push(column.to_string());
    }
}

/// Extract the compared columns (and equality join pairs) from a plan filter.
fn scan_filter(filter: &str) -> FilterPredicate {
    let tokens = predicate_tokens(filter);
    let mut predicate = FilterPredicate::default();
    for (position, token) in tokens.iter().enumerate() {
        let PredicateToken::Ident { full, last } = token else {
            continue;
        };
        let mut next = position + 1;
        while matches!(tokens.get(next), Some(PredicateToken::Cast)) {
            next += 1;
        }
        let compares = matches!(tokens.get(next), Some(PredicateToken::Compare))
            || matches!(
                tokens.get(next),
                Some(PredicateToken::Keyword(word))
                    if matches!(word.as_str(), "LIKE" | "IN" | "BETWEEN" | "IS")
            );
        if compares {
            push_unique(&mut predicate.columns, last);
            if matches!(tokens.get(next), Some(PredicateToken::Compare)) {
                let mut right = next + 1;
                while matches!(tokens.get(right), Some(PredicateToken::Cast)) {
                    right += 1;
                }
                if let Some(PredicateToken::Ident {
                    full: right_full, ..
                }) = tokens.get(right)
                {
                    let pair = (full.clone(), right_full.clone());
                    if !predicate.equalities.contains(&pair) {
                        predicate.equalities.push(pair);
                    }
                }
            }
        } else if position > 0
            && matches!(tokens.get(position - 1), Some(PredicateToken::Compare))
            && !matches!(
                tokens.get(position.wrapping_sub(2)),
                Some(PredicateToken::Ident { .. })
            )
        {
            // `? = column`: the column is on the right-hand side. A join
            // equality (`a.x = b.y`) is reported through `equalities`, not as
            // a second filter column.
            push_unique(&mut predicate.columns, last);
        }
        if predicate.columns.len() >= 4 {
            break;
        }
    }
    predicate
}

fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Equality join pairs in the filter of `start` or of its immediate children.
fn subtree_join_pairs(nodes: &[PlanNode]) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let Some(root) = nodes.first() else {
        return pairs;
    };
    let root_depth = root.depth;
    let filters = std::iter::once(root.filter.as_deref()).chain(
        nodes
            .iter()
            .filter(|n| n.depth > root_depth)
            .map(|n| n.filter.as_deref()),
    );
    for filter in filters.flatten() {
        for pair in scan_filter(filter).equalities {
            if !pairs.contains(&pair) {
                pairs.push(pair);
            }
        }
    }
    pairs
}

fn quote_ident(engine: EngineKind, ident: &str) -> String {
    match engine {
        EngineKind::MySql | EngineKind::ClickHouse => {
            format!("`{}`", ident.replace('`', "``"))
        }
        EngineKind::SqlServer => format!("[{}]", ident.replace(']', "]]")),
        _ => format!("\"{}\"", ident.replace('"', "\"\"")),
    }
}

fn create_index_sql(engine: EngineKind, table: &str, columns: &[String]) -> String {
    let suffix: Vec<String> = columns.iter().map(|c| sanitize_name(c)).collect();
    let name = format!("idx_{}_{}", sanitize_name(table), suffix.join("_"));
    let name = name.chars().take(60).collect::<String>();
    let cols = columns
        .iter()
        .map(|c| quote_ident(engine, c))
        .collect::<Vec<_>>()
        .join(", ");
    if engine == EngineKind::ClickHouse {
        // ClickHouse has no CREATE INDEX; skipping indexes are added to the
        // table and become effective for new parts.
        return format!(
            "ALTER TABLE {} ADD INDEX {} ({}) TYPE minmax GRANULARITY 1;",
            quote_ident(engine, table),
            quote_ident(engine, &name),
            cols
        );
    }
    format!(
        "CREATE INDEX {} ON {} ({});",
        quote_ident(engine, &name),
        quote_ident(engine, table),
        cols
    )
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pg_fixture() -> Value {
        json!([{
            "Plan": {
                "Node Type": "Seq Scan",
                "Relation Name": "orders",
                "Total Cost": 100.0,
                "Plan Rows": 50000,
                "Filter": "(status = 'open'::text)",
                "Plans": []
            }
        }])
    }

    fn mysql_fixture() -> Value {
        json!({
            "query_block": {
                "select_id": 1,
                "cost_info": { "query_cost": "80.00" },
                "table": {
                    "table_name": "orders",
                    "access_type": "ALL",
                    "rows_examined_per_scan": 40000,
                    "attached_condition": "(`demo`.`orders`.`status` = 'open')"
                }
            }
        })
    }

    fn stats() -> Vec<TableStats> {
        vec![TableStats {
            table: "orders".into(),
            approx_rows: Some(50000),
            row_estimate: strut_sql_driver_api::RowEstimateKind::PlannerStats,
            table_bytes: Some(1_000_000),
            index_bytes: Some(0),
            indexes: vec![],
        }]
    }

    #[test]
    fn pg_full_scan_fixture_yields_warning_and_index_advice() {
        let mut summary =
            normalize_postgres("SELECT * FROM orders WHERE status = 'open'", pg_fixture());
        advise(&mut summary, &stats(), EngineKind::Postgres);
        assert!(summary.issues.iter().any(|i| i.code == "full_scan"));
        assert!(!summary.index_suggestions.is_empty());
        assert!(summary.index_suggestions[0].sql.contains("CREATE INDEX"));
        assert!(summary.index_suggestions[0]
            .columns
            .contains(&"status".to_string()));
    }

    #[test]
    fn pg_full_scan_advice_names_the_filter_column_only() {
        let mut summary =
            normalize_postgres("SELECT * FROM orders WHERE status = 'open'", pg_fixture());
        advise(&mut summary, &stats(), EngineKind::Postgres);
        let issue = summary
            .issues
            .iter()
            .find(|i| i.code == "full_scan")
            .expect("full_scan issue");
        assert!(issue.suggestion.contains("status"), "{}", issue.suggestion);
        // String literal and cast type are not columns.
        assert!(!issue.suggestion.contains("open"), "{}", issue.suggestion);
        assert!(!issue.suggestion.contains("text"), "{}", issue.suggestion);
        assert_eq!(
            summary.index_suggestions[0].columns,
            vec!["status".to_string()]
        );
    }

    #[test]
    fn multi_column_filter_recommends_a_composite_index() {
        let mut summary = normalize_postgres(
            "SELECT * FROM orders WHERE status = 'open' AND region = 'eu'",
            json!([{
                "Plan": {
                    "Node Type": "Seq Scan",
                    "Relation Name": "orders",
                    "Total Cost": 100.0,
                    "Plan Rows": 50000,
                    "Filter": "((status = 'open'::text) AND (region = 'eu'::text))",
                    "Plans": []
                }
            }]),
        );
        advise(&mut summary, &stats(), EngineKind::Postgres);
        let issue = summary
            .issues
            .iter()
            .find(|i| i.code == "full_scan")
            .expect("full_scan issue");
        assert!(
            issue.suggestion.contains("(status, region)"),
            "{}",
            issue.suggestion
        );
        let suggestion = &summary.index_suggestions[0];
        assert_eq!(
            suggestion.columns,
            vec!["status".to_string(), "region".to_string()]
        );
        assert!(suggestion.sql.contains("\"status\", \"region\""));
        assert!(suggestion.reason.contains("composite index"));
    }

    #[test]
    fn nested_loop_advice_names_join_columns() {
        let mut summary = normalize_postgres(
            "SELECT * FROM a JOIN b ON a.id = b.a_id",
            json!([{
                "Plan": {
                    "Node Type": "Nested Loop",
                    "Total Cost": 900.0,
                    "Plan Rows": 50000,
                    "Plans": [
                        {
                            "Node Type": "Seq Scan",
                            "Relation Name": "b",
                            "Total Cost": 800.0,
                            "Plan Rows": 50000,
                            "Filter": "(b.a_id = a.id)",
                            "Plans": []
                        },
                        {
                            "Node Type": "Index Scan",
                            "Relation Name": "a",
                            "Total Cost": 0.3,
                            "Plan Rows": 1,
                            "Plans": []
                        }
                    ]
                }
            }]),
        );
        advise(&mut summary, &stats(), EngineKind::Postgres);
        let issue = summary
            .issues
            .iter()
            .find(|i| i.code == "nested_loop_large")
            .expect("nested_loop_large issue");
        assert!(
            issue.suggestion.contains("b.a_id = a.id"),
            "{}",
            issue.suggestion
        );
    }

    #[test]
    fn sqlite_pk_lookup_keeps_plan_node_without_findings() {
        let mut summary = normalize_sqlite(
            "SELECT * FROM orders WHERE id = 1",
            vec!["SEARCH orders USING INTEGER PRIMARY KEY (rowid=?)".into()],
        );
        advise(&mut summary, &stats(), EngineKind::Sqlite);
        assert_eq!(summary.nodes.len(), 1);
        assert_eq!(summary.nodes[0].node_type, "SEARCH");
        assert_eq!(summary.nodes[0].relation.as_deref(), Some("orders"));
        assert!(summary.issues.is_empty(), "{:?}", summary.issues);
        assert!(summary.index_suggestions.is_empty());
    }

    #[test]
    fn filter_scanner_ignores_literals_casts_and_placeholders() {
        let pg = scan_filter("((status = 'open'::text) AND (id > 42))");
        assert_eq!(pg.columns, vec!["status".to_string(), "id".to_string()]);
        let join = scan_filter("(b.a_id = a.id)");
        assert_eq!(join.columns, vec!["a_id".to_string()]);
        assert_eq!(
            join.equalities,
            vec![("b.a_id".to_string(), "a.id".to_string())]
        );
        let sqlserver = scan_filter("[a]]b].[id]>@P1");
        assert_eq!(sqlserver.columns, vec!["id".to_string()]);
        let mysql = scan_filter("(`demo`.`orders`.`status` = 'open')");
        assert_eq!(mysql.columns, vec!["status".to_string()]);
    }

    #[test]
    fn mysql_full_scan_fixture_yields_finding_without_double_count() {
        let mut summary = normalize_mysql("SELECT * FROM orders", mysql_fixture());
        assert_eq!(summary.nodes.len(), 1);
        advise(&mut summary, &stats(), EngineKind::MySql);
        assert!(summary.issues.iter().any(|i| i.code == "full_scan"));
    }

    #[test]
    fn sqlite_full_scan_fixture_yields_finding() {
        let mut summary =
            normalize_sqlite("SELECT * FROM orders", vec!["SCAN TABLE orders".into()]);
        advise(&mut summary, &stats(), EngineKind::Sqlite);
        assert!(summary.issues.iter().any(|i| i.code == "full_scan"));
        assert_eq!(summary.nodes[0].relation.as_deref(), Some("orders"));
    }

    #[test]
    fn indexed_pk_lookup_has_no_findings() {
        let mut summary = normalize_postgres(
            "SELECT * FROM orders WHERE id = 1",
            json!([{
                "Plan": {
                    "Node Type": "Index Scan",
                    "Relation Name": "orders",
                    "Total Cost": 1.0,
                    "Plan Rows": 1,
                    "Index Cond": "(id = 1)",
                    "Plans": []
                }
            }]),
        );
        advise(&mut summary, &stats(), EngineKind::Postgres);
        assert!(summary.issues.is_empty(), "{:?}", summary.issues);
    }

    #[test]
    fn external_sort_spill_is_reported() {
        let mut summary = normalize_postgres(
            "SELECT * FROM orders ORDER BY status",
            json!([{
                "Plan": {
                    "Node Type": "Sort",
                    "Total Cost": 500.0,
                    "Plan Rows": 50000,
                    "Sort Method": "external merge  Disk: 2048kB",
                    "Plans": [{
                        "Node Type": "Seq Scan",
                        "Relation Name": "orders",
                        "Total Cost": 400.0,
                        "Plan Rows": 50000,
                        "Plans": []
                    }]
                }
            }]),
        );
        advise(&mut summary, &stats(), EngineKind::Postgres);
        assert!(summary.issues.iter().any(|i| i.code == "plan_spill"));
    }

    #[test]
    fn row_estimate_miss_is_reported() {
        let mut summary = normalize_postgres(
            "SELECT * FROM orders",
            json!([{
                "Plan": {
                    "Node Type": "Seq Scan",
                    "Relation Name": "orders",
                    "Total Cost": 10.0,
                    "Plan Rows": 1,
                    "Actual Rows": 1000,
                    "Plans": []
                }
            }]),
        );
        advise(&mut summary, &stats(), EngineKind::Postgres);
        assert!(summary.issues.iter().any(|i| i.code == "row_estimate_miss"));
    }
}
