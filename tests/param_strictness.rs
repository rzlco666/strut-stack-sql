//! V2 regression tests: tool parameter structs reject unknown arguments
//! instead of silently ignoring them, and accepted aliases still work.

use serde_json::json;
use strut_stack_sql::tools::core::{
    AnalyzeQueryParams, BatchQueryItem, ExecuteSqlParams, SearchObjectsParams,
};
use strut_stack_sql::tools::full::{ListTablesParams, SchemaOverviewParams};
use strut_stack_sql::tools::intel::RelationshipGraphParams;

fn error(value: serde_json::Value) -> String {
    serde_json::from_value::<SearchObjectsParams>(value)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| panic!("expected a deserialization error"))
}

#[test]
fn unknown_search_argument_is_rejected_with_valid_field_list() {
    let message = error(json!({"object_type": "table", "keywords": "users"}));
    assert!(
        message.contains("unknown field `keywords`"),
        "message must name the unknown field: {message}"
    );
    assert!(
        message.contains("keyword"),
        "message must list the valid fields: {message}"
    );
}

#[test]
fn unknown_list_tables_argument_is_rejected() {
    let message = serde_json::from_value::<ListTablesParams>(json!({
        "schema": "main",
        "page_size": 10
    }))
    .expect_err("page_size is not a list_tables argument")
    .to_string();
    assert!(message.contains("unknown field `page_size`"), "{message}");
    assert!(message.contains("limit"), "{message}");
}

#[test]
fn unknown_schema_overview_argument_is_rejected() {
    serde_json::from_value::<SchemaOverviewParams>(json!({"only": "orders"}))
        .expect_err("unknown argument must not be silently ignored");
}

#[test]
fn relationship_graph_accepts_focus_alias_and_rejects_typos() {
    let params: RelationshipGraphParams =
        serde_json::from_value(json!({"focus": "orders", "depth": 2}))
            .expect("focus is a documented alias for table");
    assert_eq!(params.table.as_deref(), Some("orders"));
    assert_eq!(params.depth, Some(2));

    serde_json::from_value::<RelationshipGraphParams>(json!({"focuss": "orders"}))
        .expect_err("misspelled focus must be rejected");
}

#[test]
fn unknown_execute_sql_argument_is_rejected() {
    let message = serde_json::from_value::<ExecuteSqlParams>(json!({
        "sql": "SELECT 1",
        "limit": 5
    }))
    .expect_err("limit is not an execute_sql argument")
    .to_string();
    assert!(message.contains("unknown field `limit`"), "{message}");
    assert!(message.contains("page_size"), "{message}");
}

#[test]
fn unknown_batch_item_argument_is_rejected() {
    serde_json::from_value::<ExecuteSqlParams>(json!({
        "queries": [{"sql": "SELECT 1", "statement": "SELECT 1"}]
    }))
    .expect_err("batch entries accept sql/params only");

    let item: BatchQueryItem = serde_json::from_value(json!({"sql": "SELECT ?", "params": [1]}))
        .expect("parameterized batch entry");
    assert!(matches!(item, BatchQueryItem::Parameterized(_)));
}

#[test]
fn unknown_analyze_argument_is_rejected() {
    serde_json::from_value::<AnalyzeQueryParams>(json!({
        "sql": "SELECT 1",
        "analyze_only": true
    }))
    .expect_err("unknown analyze argument must be rejected");
}
