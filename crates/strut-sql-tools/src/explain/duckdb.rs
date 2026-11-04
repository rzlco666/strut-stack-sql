//! DuckDB `EXPLAIN (FORMAT JSON)` normalization.
//!
//! DuckDB returns two text columns (`explain_key`, `explain_value`) and one
//! row: `physical_plan` for plain EXPLAIN and `analyzed_plan` for
//! `EXPLAIN ANALYZE`. Both plans are a JSON array of root nodes (DuckDB can
//! return several); `analyzed_plan` wraps the same operator tree under
//! `operator`. Plain nodes carry `name`/`children`/`extra_info`; analyzed
//! operators use `type` and add `intermediate_rows` and `timing` (seconds) per
//! node. DuckDB has no planner cost model, so `cost` stays absent and the
//! advisor falls back to row estimates.
//!
//! Parsing tolerates both shapes and key spellings, but never invents values:
//! unknown fields are omitted rather than echoed.

use serde_json::{Map, Value};
use strut_sql_driver_api::{QueryResult, SqlValue};

use super::{ExplainError, ExplainSummary, PlanNode};

pub(super) fn normalize(
    query: &str,
    result: &QueryResult,
    analyze: bool,
) -> Result<ExplainSummary, ExplainError> {
    let plan = plan_json(result)?
        .ok_or_else(|| ExplainError::Other("DuckDB EXPLAIN returned no plan".into()))?;
    let mut nodes = Vec::new();
    match &plan {
        Value::Array(roots) => {
            for root in roots {
                walk(root, &mut nodes, 0);
            }
        }
        root => walk(root, &mut nodes, 0),
    }
    if nodes.is_empty() {
        return Err(ExplainError::Other(
            "DuckDB EXPLAIN JSON contained no plan operators".into(),
        ));
    }
    let plan_rows = nodes.first().and_then(|n| n.rows);
    Ok(ExplainSummary {
        engine: "duckdb".into(),
        query: query.into(),
        analyze,
        total_cost: None,
        plan_rows,
        nodes,
        issues: Vec::new(),
        index_suggestions: Vec::new(),
        warnings: Vec::new(),
    })
}

/// Prefer the `physical_plan` row, then `analyzed_plan`, then the first row
/// that parses as JSON. The analyzed wrapper is unwrapped to its operator tree.
fn plan_json(result: &QueryResult) -> Result<Option<Value>, ExplainError> {
    let key_index = result
        .cols
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("explain_key"));
    let Some(value_index) = result
        .cols
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case("explain_value"))
        .or_else(|| {
            // The value column is the only one holding JSON text if a DuckDB
            // release renames the pair.
            result
                .cols
                .iter()
                .position(|c| c.name.to_ascii_lowercase().contains("value"))
        })
        .or_else(|| result.cols.len().checked_sub(1))
    else {
        return Ok(None);
    };
    let mut physical = None;
    let mut analyzed = None;
    let mut fallback = None;
    for row in &result.rows {
        let key = key_index
            .and_then(|index| row.get(index))
            .and_then(sql_text);
        let Some(text) = row.get(value_index).and_then(sql_text) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        match key {
            Some(key) if key.eq_ignore_ascii_case("physical_plan") => {
                physical = Some(value);
                break;
            }
            Some(key) if key.eq_ignore_ascii_case("analyzed_plan") => {
                let operators = match value {
                    Value::Object(mut wrapper) => {
                        wrapper.remove("operator").unwrap_or(Value::Object(wrapper))
                    }
                    value => value,
                };
                analyzed.get_or_insert(operators);
            }
            _ => {
                fallback.get_or_insert(value);
            }
        }
    }
    Ok(physical.or(analyzed).or(fallback))
}

fn sql_text(value: &SqlValue) -> Option<&str> {
    match value {
        SqlValue::String(text) => Some(text),
        _ => None,
    }
}

/// DuckDB pads some operator names with a trailing space.
const CHILD_KEYS: [&str; 2] = ["children", "plans"];

fn walk(node: &Value, nodes: &mut Vec<PlanNode>, depth: u32) {
    let Some(object) = node.as_object() else {
        return;
    };
    let extra = object.get("extra_info").and_then(Value::as_object);
    // Analyzed operators describe the scan as `Type: Sequential Scan`; plain
    // physical nodes only carry the mechanical `name` (SEQ_SCAN).
    let name = extra_value(extra, &["Type"])
        .or_else(|| {
            ["name", "operator_name", "operator_type", "operator", "type"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|name| !name.is_empty())
        })
        .unwrap_or("UNKNOWN")
        .to_owned();
    nodes.push(PlanNode {
        rows: extra_value(extra, &["Estimated Cardinality", "Cardinality"]).and_then(parse_count),
        actual_rows: object.get("intermediate_rows").and_then(Value::as_i64),
        actual_time_ms: object
            .get("timing")
            .and_then(Value::as_f64)
            .map(|seconds| seconds * 1_000.0),
        node_type: name,
        depth,
        relation: extra_value(extra, &["Table", "Relation", "Tables"]).map(first_table),
        cost: None,
        filter: extra_value(extra, &["Filters", "Filter", "Conditions", "Expression"])
            .map(str::to_owned),
        join_type: extra_value(extra, &["Join Type", "JoinType"]).map(str::to_owned),
        spill: extra_value(extra, &["Sort Method"])
            .filter(|method| method.to_ascii_lowercase().contains("external"))
            .map(str::to_owned),
    });
    for children in CHILD_KEYS {
        if let Some(array) = object.get(children).and_then(Value::as_array) {
            for child in array {
                walk(child, nodes, depth.saturating_add(1));
            }
        }
    }
}

fn extra_value<'a>(extra: Option<&'a Map<String, Value>>, keys: &[&str]) -> Option<&'a str> {
    let extra = extra?;
    keys.iter()
        .find_map(|key| extra.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_count(value: &str) -> Option<i64> {
    let cleaned: String = value
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '-' || *c == '.')
        .collect();
    if cleaned.is_empty() || cleaned == "-" {
        return None;
    }
    cleaned.parse::<f64>().ok().map(|v| v.round() as i64)
}

fn first_table(value: &str) -> String {
    value
        .split(',')
        .next()
        .unwrap_or(value)
        .trim()
        .trim_matches('"')
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use strut_sql_driver_api::{ColumnMeta, QueryMeta};

    fn result(rows: Vec<Vec<SqlValue>>) -> QueryResult {
        QueryResult {
            cols: vec![
                ColumnMeta {
                    name: "explain_key".into(),
                    type_name: "VARCHAR".into(),
                    nullable: true,
                },
                ColumnMeta {
                    name: "explain_value".into(),
                    type_name: "VARCHAR".into(),
                    nullable: true,
                },
            ],
            rows,
            meta: QueryMeta {
                row_count: 0,
                truncated: false,
                rows_affected: None,
            },
        }
    }

    #[test]
    fn physical_plan_array_is_preferred_and_extra_info_is_mapped() {
        let logical = SqlValue::String(r#"{"name":"LOGICAL_ROOT","children":[]}"#.into());
        // DuckDB 1.2 shape: a JSON array of roots, `name` + `extra_info`.
        let physical = SqlValue::String(
            r#"[{"name":"PROJECTION","children":[{"name":"SEQ_SCAN","extra_info":{"Type":"Sequential Scan","Table":"main.items","Filters":"(id > 3)","Estimated Cardinality":"1,024"}}],"extra_info":{"Estimated Cardinality":"1,024"}}]"#.into(),
        );
        let summary = normalize(
            "SELECT * FROM items",
            &result(vec![
                vec![SqlValue::String("logical_plan".into()), logical],
                vec![SqlValue::String("physical_plan".into()), physical],
            ]),
            false,
        )
        .expect("plan");
        assert_eq!(summary.engine, "duckdb");
        assert_eq!(summary.plan_rows, Some(1024));
        assert_eq!(summary.nodes.len(), 2);
        assert_eq!(summary.nodes[0].node_type, "PROJECTION");
        assert_eq!(summary.nodes[1].depth, 1);
        assert_eq!(summary.nodes[1].node_type, "Sequential Scan");
        assert_eq!(summary.nodes[1].relation.as_deref(), Some("main.items"));
        assert_eq!(summary.nodes[1].rows, Some(1024));
        assert_eq!(summary.nodes[1].filter.as_deref(), Some("(id > 3)"));
        assert_eq!(summary.nodes[1].actual_rows, None);
    }

    #[test]
    fn analyzed_plan_operator_tree_carries_rows_and_timing() {
        let analyzed = SqlValue::String(
            r#"{"operator":[{"type":"EXECUTE","children":[{"type":"TABLE_SCAN","intermediate_rows":42,"intermediate_size_bytes":84,"timing":0.001,"extra_info":{"Type":"Sequential Scan","Table":"main.items","Estimated Cardinality":"1,024"}}],"intermediate_rows":0,"timing":0.0,"extra_info":{}}],"query":{"total_time":0.002},"io":{}}"#.into(),
        );
        let summary = normalize(
            "SELECT * FROM items",
            &result(vec![vec![
                SqlValue::String("analyzed_plan".into()),
                analyzed,
            ]]),
            true,
        )
        .expect("plan");
        assert!(summary.analyze);
        assert_eq!(summary.nodes.len(), 2);
        assert_eq!(summary.nodes[0].node_type, "EXECUTE");
        assert_eq!(summary.nodes[0].actual_rows, Some(0));
        assert_eq!(summary.nodes[1].node_type, "Sequential Scan");
        assert_eq!(summary.nodes[1].actual_rows, Some(42));
        assert_eq!(summary.nodes[1].actual_time_ms, Some(1.0));
        assert_eq!(summary.nodes[1].rows, Some(1024));
    }

    #[test]
    fn missing_plan_is_an_error_not_an_empty_summary() {
        assert!(normalize("SELECT 1", &result(Vec::new()), false).is_err());
        let not_json = SqlValue::String("not json".into());
        assert!(normalize(
            "SELECT 1",
            &result(vec![vec![
                SqlValue::String("physical_plan".into()),
                not_json
            ]]),
            false
        )
        .is_err());
        let empty = SqlValue::String("[]".into());
        assert!(normalize(
            "SELECT 1",
            &result(vec![vec![SqlValue::String("physical_plan".into()), empty]]),
            false
        )
        .is_err());
    }
}
