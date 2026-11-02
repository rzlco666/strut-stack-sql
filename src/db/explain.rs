use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use sqlx::{Column, Row};

use crate::config::WriteMode;
use crate::db::bind::{bind_mysql_params, bind_pg_params, bind_sqlite_params};
use crate::db::{EngineKind, EnginePool};
use crate::guard::{validate_and_prepare, GuardError};

#[derive(Debug, thiserror::Error)]
pub enum ExplainError {
    #[error(transparent)]
    Guard(#[from] GuardError),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct ExplainSummary {
    pub engine: String,
    pub query: String,
    pub total_cost: Option<f64>,
    pub plan_rows: Option<i64>,
    pub warnings: Vec<String>,
    pub nodes: Vec<PlanNode>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlanNode {
    pub node_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    pub issues: Vec<String>,
}

pub async fn analyze_query(
    pool: &EnginePool,
    sql: &str,
    params: &[Value],
    write_mode: WriteMode,
    timeout: Duration,
) -> Result<ExplainSummary, ExplainError> {
    let engine = pool.engine();
    let prepared = validate_and_prepare(sql, params, engine, write_mode, 100)?;

    if !prepared.class.is_select_like() && prepared.class.label() != "SELECT" {
        let label = prepared.class.label();
        if label != "EXPLAIN" && !sql.trim().to_uppercase().starts_with("SELECT") {
            return Err(ExplainError::Other(
                "analyze_query_performance only supports SELECT-like queries".into(),
            ));
        }
    }

    let explain_sql = match engine {
        EngineKind::Postgres => format!("EXPLAIN (FORMAT JSON) {}", prepared.sql),
        EngineKind::Mysql => format!("EXPLAIN FORMAT=JSON {}", prepared.sql),
        EngineKind::Sqlite => format!("EXPLAIN QUERY PLAN {}", prepared.sql),
    };

    if engine == EngineKind::Sqlite {
        let rows = tokio::time::timeout(
            timeout,
            fetch_explain_sqlite(pool, &explain_sql, &prepared.params),
        )
        .await
        .map_err(|_| ExplainError::Other("explain timeout".into()))??;
        return Ok(normalize_sqlite(&prepared.sql, rows));
    }

    let json = tokio::time::timeout(
        timeout,
        fetch_explain_json(pool, &explain_sql, engine, &prepared.params),
    )
    .await
    .map_err(|_| ExplainError::Other("explain timeout".into()))??;

    Ok(normalize_explain(engine, &prepared.sql, json))
}

async fn fetch_explain_json(
    pool: &EnginePool,
    explain_sql: &str,
    engine: EngineKind,
    params: &[Value],
) -> Result<Value, ExplainError> {
    match engine {
        EngineKind::Postgres => {
            let pool = pool
                .postgres()
                .map_err(|e| ExplainError::Other(e.to_string()))?;
            let row = bind_pg_params(explain_sql, params)
                .map_err(|e| ExplainError::Other(e.to_string()))?
                .fetch_one(pool)
                .await?;
            let val: Value = row.try_get(0)?;
            Ok(val)
        }
        EngineKind::Mysql => {
            let pool = pool
                .mysql()
                .map_err(|e| ExplainError::Other(e.to_string()))?;
            let row = bind_mysql_params(explain_sql, params)
                .map_err(|e| ExplainError::Other(e.to_string()))?
                .fetch_one(pool)
                .await?;
            let text: String = row.try_get(0)?;
            let val: Value = serde_json::from_str(&text)
                .map_err(|e| ExplainError::Other(format!("invalid explain json: {e}")))?;
            Ok(val)
        }
        EngineKind::Sqlite => Err(ExplainError::Other(
            "sqlite explain uses row-based path".into(),
        )),
    }
}

async fn fetch_explain_sqlite(
    pool: &EnginePool,
    explain_sql: &str,
    params: &[Value],
) -> Result<Vec<String>, ExplainError> {
    let pool = pool
        .sqlite()
        .map_err(|e| ExplainError::Other(e.to_string()))?;
    let rows = bind_sqlite_params(explain_sql, params)
        .map_err(|e| ExplainError::Other(e.to_string()))?
        .fetch_all(pool)
        .await?;
    // SQL-BUG-15: do not hardcode column index 3. `EXPLAIN QUERY PLAN`
    // returns (id, parent, notused, detail) — locate `detail` by name and
    // fall back gracefully (`opcode` for plain EXPLAIN, else last column).
    let detail_idx = rows.first().and_then(sqlite_detail_column_index);
    rows.iter()
        .map(|row| match detail_idx {
            Some(idx) => row.try_get::<String, _>(idx).map_err(ExplainError::from),
            None => Ok(String::new()),
        })
        .collect()
}

/// Index of the human-readable plan column in a SQLite EXPLAIN result row.
fn sqlite_detail_column_index(row: &sqlx::sqlite::SqliteRow) -> Option<usize> {
    let cols = row.columns();
    for wanted in ["detail", "opcode"] {
        if let Some(idx) = cols
            .iter()
            .position(|c| c.name().eq_ignore_ascii_case(wanted))
        {
            return Some(idx);
        }
    }
    cols.len().checked_sub(1)
}

fn normalize_sqlite(query: &str, details: Vec<String>) -> ExplainSummary {
    let mut warnings = Vec::new();
    let mut nodes = Vec::new();

    for detail in details {
        let upper = detail.to_uppercase();
        let node_type = if upper.contains("SCAN TABLE") {
            "SCAN".into()
        } else if upper.contains("SEARCH") {
            "SEARCH".into()
        } else {
            detail.clone()
        };
        let relation = sqlite_relation_from_detail(&detail);
        let mut issues = Vec::new();
        if upper.contains("SCAN TABLE") {
            issues.push("full table scan".into());
        }
        nodes.push(PlanNode {
            node_type,
            relation,
            cost: None,
            rows: None,
            filter: None,
            issues,
        });
    }

    if nodes.iter().any(|n| n.node_type == "SCAN") {
        warnings.push("Sequential scan detected — consider adding an index".into());
    }

    ExplainSummary {
        engine: "sqlite".into(),
        query: query.into(),
        total_cost: None,
        plan_rows: None,
        warnings,
        nodes,
    }
}

fn sqlite_relation_from_detail(detail: &str) -> Option<String> {
    let upper = detail.to_uppercase();
    if let Some(rest) = upper.strip_prefix("SCAN TABLE ") {
        return Some(rest.split_whitespace().next()?.to_lowercase());
    }
    if let Some(idx) = detail.to_uppercase().find("SEARCH TABLE ") {
        let rest = &detail[idx + "SEARCH TABLE ".len()..];
        return Some(
            rest.split_whitespace()
                .next()?
                .trim_matches('"')
                .to_string(),
        );
    }
    None
}

fn normalize_explain(engine: EngineKind, query: &str, json: Value) -> ExplainSummary {
    match engine {
        EngineKind::Postgres => normalize_postgres(query, json),
        EngineKind::Mysql => normalize_mysql(query, json),
        EngineKind::Sqlite => ExplainSummary {
            engine: "sqlite".into(),
            query: query.into(),
            total_cost: None,
            plan_rows: None,
            warnings: vec![],
            nodes: vec![],
        },
    }
}

fn normalize_postgres(query: &str, json: Value) -> ExplainSummary {
    let mut warnings = Vec::new();
    let mut nodes = Vec::new();
    let mut total_cost = None;
    let mut plan_rows = None;

    if let Some(arr) = json.as_array() {
        if let Some(plan_root) = arr.first().and_then(|v| v.get("Plan")) {
            walk_pg_plan(plan_root, &mut nodes);
            total_cost = plan_root.get("Total Cost").and_then(|v| v.as_f64());
            plan_rows = plan_root.get("Plan Rows").and_then(|v| v.as_i64());
        }
    }

    if nodes.iter().any(|n| n.node_type.contains("Seq Scan")) {
        warnings.push("Sequential scan detected — consider adding an index".into());
    }

    ExplainSummary {
        engine: "postgresql".into(),
        query: query.into(),
        total_cost,
        plan_rows,
        warnings,
        nodes,
    }
}

fn walk_pg_plan(plan: &Value, nodes: &mut Vec<PlanNode>) {
    let node_type = plan
        .get("Node Type")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let relation = plan
        .get("Relation Name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let cost = plan.get("Total Cost").and_then(|v| v.as_f64());
    let rows = plan.get("Plan Rows").and_then(|v| v.as_i64());
    let filter = plan
        .get("Filter")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut issues = Vec::new();
    if node_type.contains("Seq Scan") {
        issues.push("full table scan".into());
    }
    if let Some(r) = rows {
        if r > 100_000 {
            issues.push("high estimated row count".into());
        }
    }

    nodes.push(PlanNode {
        node_type,
        relation,
        cost,
        rows,
        filter,
        issues,
    });

    if let Some(children) = plan.get("Plans").and_then(|v| v.as_array()) {
        for child in children {
            walk_pg_plan(child, nodes);
        }
    }
}

fn normalize_mysql(query: &str, json: Value) -> ExplainSummary {
    let mut warnings = Vec::new();
    let mut nodes = Vec::new();
    let mut total_cost = None;
    let mut plan_rows = None;

    if let Some(query_block) = json.get("query_block") {
        if let Some(cost) = query_block.get("cost_info") {
            total_cost = cost.get("query_cost").and_then(|v| v.as_f64());
            plan_rows = cost.get("rows_examined_per_scan").and_then(|v| v.as_i64());
        }
        walk_mysql_block(query_block, &mut nodes);
    }

    if total_cost.is_none() {
        total_cost = nodes.iter().filter_map(|n| n.cost).reduce(|a, b| a + b);
    }
    if plan_rows.is_none() {
        plan_rows = nodes.iter().filter_map(|n| n.rows).max();
    }

    if nodes
        .iter()
        .any(|n| n.node_type.to_uppercase().contains("ALL"))
    {
        warnings.push("Full table scan (type ALL) — consider adding an index".into());
    }

    ExplainSummary {
        engine: "mysql".into(),
        query: query.into(),
        total_cost,
        plan_rows,
        warnings,
        nodes,
    }
}

fn walk_mysql_block(block: &Value, nodes: &mut Vec<PlanNode>) {
    if let Some(table) = block.get("table") {
        let node_type = table
            .get("access_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let relation = table
            .get("table_name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let rows = table
            .get("rows_examined_per_scan")
            .or_else(|| table.get("rows"))
            .and_then(|v| v.as_i64());
        let cost = table
            .get("cost_info")
            .and_then(|c| c.get("read_cost").or_else(|| c.get("prefix_cost")))
            .and_then(|v| v.as_f64());
        let filter = table
            .get("attached_condition")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let mut issues = Vec::new();
        if node_type.eq_ignore_ascii_case("ALL") {
            issues.push("full table scan".into());
        }

        nodes.push(PlanNode {
            node_type,
            relation,
            cost,
            rows,
            filter,
            issues,
        });
    }

    for key in [
        "nested_loop",
        "grouping_operation",
        "ordering_operation",
        "table",
    ] {
        if let Some(nested) = block.get(key) {
            if let Some(arr) = nested.as_array() {
                for item in arr {
                    walk_mysql_block(item, nodes);
                }
            } else if nested.is_object() && key != "table" {
                walk_mysql_block(nested, nodes);
            }
        }
    }
}
