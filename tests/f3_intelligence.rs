//! PRD-F3 acceptance tests: ranked search, join graph, context budget,
//! advisor findings, anti-patterns, error contract, and session history.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::CallToolResult;
use serde_json::{json, Value};
use strut_sql_core::auth::{Grant, GrantSource, Policy, Tier};
use strut_sql_core::config::{AppConfig, ResolvedSource, WriteMode};
use strut_sql_driver_api::{ConnectOptions, EngineDriver, EngineKind};
use strut_sql_driver_sqlite::SqliteDriver;
use strut_stack_sql::tools::core::{
    handle_analyze_query, handle_execute_sql, handle_search_objects, AnalyzeQueryParams,
    ExecuteSqlParams, SearchObjectsParams,
};
use strut_stack_sql::tools::intel::{
    handle_get_relationship_graph, handle_get_schema_context, handle_suggest_joins,
    RelationshipGraphParams, SchemaContextParams, SuggestJoinsParams,
};
use strut_stack_sql::tools::introspect::{schema_overview, ObjectType};

fn test_policy(write_mode: WriteMode, full_tools: bool, history: bool) -> Policy {
    let mut policy = Policy::new(Tier::from_write_mode(write_mode), full_tools, false);
    policy.add_grant(Grant {
        principal: "local".into(),
        source: GrantSource::Any,
        max_tier: Tier::from_write_mode(write_mode),
        max_rows: None,
        max_bytes: None,
        query_timeout: None,
        query_history: history,
    });
    policy
}

async fn test_config(
    name: &str,
    write_mode: WriteMode,
    full_tools: bool,
    history: bool,
) -> Arc<AppConfig> {
    std::env::set_var("MCP_SQL_FORMAT", "columnar");
    let driver = SqliteDriver::from_connection_string_with_options(
        "sqlite::memory:",
        &ConnectOptions {
            pool_size: 1,
            read_only: false,
            ..ConnectOptions::default()
        },
    )
    .await
    .expect("sqlite driver");
    let mut sources = HashMap::new();
    sources.insert(
        name.to_string(),
        ResolvedSource::with_driver(
            name,
            "sqlite::memory:".into(),
            EngineKind::Sqlite,
            Arc::new(*driver),
            !write_mode.allows_dml(),
        ),
    );
    Arc::new(AppConfig {
        write_mode,
        full_tools,
        max_rows: 100,
        max_bytes: 64 * 1024,
        query_timeout: Duration::from_secs(10),
        batch_concurrency: 8,
        batch_max_statements: 50,
        batch_timeout: Duration::from_secs(60),
        batch_max_bytes: 1024 * 1024,
        fail_fast: false,
        deny_columns: vec![],
        allow_explain_analyze: false,
        principal_id: name.into(),
        query_history: history,
        default_source: name.into(),
        sources,
        searched_paths: vec![],
        workspace: None,
        http_tokens: vec![],
        http_rate_limit_per_minute: 600,
        allow_unauthenticated_http: false,
        allow_grants: false,
        oauth: None,
        policy: test_policy(write_mode, full_tools, history),
        ephemeral: false,
        hide_endpoints: None,
    })
}

fn tool_json(result: CallToolResult) -> Value {
    let text = result
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|t| t.text.as_str())
        .expect("tool text content");
    serde_json::from_str(text).expect("valid tool json")
}

async fn exec(config: &Arc<AppConfig>, name: &str, sql: &str, params: Vec<Value>) -> Value {
    tool_json(
        handle_execute_sql(
            config,
            ExecuteSqlParams {
                sql: Some(sql.into()),
                params: Some(params),
                queries: None,
                page_offset: None,
                page_size: None,
                cursor: None,
                source: Some(name.into()),
                format: None,
            },
        )
        .await
        .expect("execute_sql"),
    )
}

async fn seed_graph(config: &Arc<AppConfig>, name: &str) {
    for sql in [
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER REFERENCES users(id), status TEXT)",
        "CREATE INDEX idx_orders_status ON orders(status)",
        "CREATE TABLE order_items (id INTEGER PRIMARY KEY, order_id INTEGER REFERENCES orders(id), sku TEXT)",
        "CREATE TABLE audit (id INTEGER)",
    ] {
        let body = exec(config, name, sql, Vec::new()).await;
        assert_eq!(body["ok"], true, "{body}");
    }
}

#[tokio::test]
async fn graph_and_join_paths_are_exact_and_copy_ready() {
    let name = "f3-graph";
    let config = test_config(name, WriteMode::AllowDdl, true, false).await;
    seed_graph(&config, name).await;

    let graph = tool_json(
        handle_get_relationship_graph(
            &config,
            RelationshipGraphParams {
                schema: Some("main".into()),
                source: Some(name.into()),
                table: None,
                depth: Some(3),
            },
        )
        .await
        .expect("graph"),
    );
    assert_eq!(graph["meta"]["n_nodes"], 4, "{graph}");
    assert!(graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| { e["from_table"] == "orders" && e["to_table"] == "users" }));

    let joins = tool_json(
        handle_suggest_joins(
            &config,
            SuggestJoinsParams {
                tables: vec!["order_items".into(), "users".into()],
                schema: Some("main".into()),
                source: Some(name.into()),
            },
        )
        .await
        .expect("suggest_joins"),
    );
    let joins = joins["joins"].as_array().expect("joins");
    assert_eq!(
        joins.len(),
        2,
        "shortest path order_items -> orders -> users"
    );
    assert_eq!(
        joins[0]["on"], "order_items.order_id = orders.id",
        "{joins:?}"
    );
    assert_eq!(joins[1]["on"], "orders.user_id = users.id");
    assert_eq!(joins[0]["confidence"], "fk");
}

#[tokio::test]
async fn suggest_joins_marks_same_name_candidates_as_guesses() {
    let name = "f3-guess";
    let config = test_config(name, WriteMode::AllowDdl, true, false).await;
    for sql in [
        "CREATE TABLE left_side (id INTEGER PRIMARY KEY, user_id INTEGER)",
        "CREATE TABLE right_side (id INTEGER PRIMARY KEY, user_id INTEGER)",
    ] {
        exec(&config, name, sql, Vec::new()).await;
    }
    let body = tool_json(
        handle_suggest_joins(
            &config,
            SuggestJoinsParams {
                tables: vec!["left_side".into(), "right_side".into()],
                schema: Some("main".into()),
                source: Some(name.into()),
            },
        )
        .await
        .expect("suggest_joins"),
    );
    assert_eq!(body["unreachable"], json!(["right_side"]));
    let joins = body["joins"].as_array().expect("guesses");
    assert!(joins
        .iter()
        .any(|j| j["guess"] == true && j["confidence"] == "guess"));
}

#[tokio::test]
async fn schema_overview_reports_relation_counts_hubs_and_orphans() {
    let name = "f3-overview";
    let config = test_config(name, WriteMode::AllowDdl, true, false).await;
    seed_graph(&config, name).await;
    let driver = config.source(Some(name)).unwrap().driver().await.unwrap();
    let overview = schema_overview(&driver, Some(name), Some("main"))
        .await
        .expect("overview");
    let orders = overview
        .tables
        .iter()
        .find(|t| t.name == "orders")
        .expect("orders");
    assert_eq!(orders.fk_in, 1);
    assert_eq!(orders.fk_out, 1);
    assert!(orders.hub);
    let audit = overview
        .tables
        .iter()
        .find(|t| t.name == "audit")
        .expect("audit");
    assert!(audit.orphan);
    assert_eq!(overview.meta.hubs, vec!["orders".to_string()]);
    assert!(overview.meta.orphans.contains(&"audit".to_string()));
    assert!(!overview.meta.truncated);

    let stats = driver.table_stats(Some("main")).await.expect("table_stats");
    let orders_stats = stats
        .iter()
        .find(|s| s.table == "orders")
        .expect("orders stats");
    assert!(orders_stats
        .indexes
        .iter()
        .any(|i| i.columns == vec!["status".to_string()]));
    assert_eq!(
        orders_stats.row_estimate,
        strut_sql_driver_api::RowEstimateKind::Unavailable
    );
}

#[tokio::test]
async fn schema_context_respects_budget_and_is_deterministic() {
    let name = "f3-context";
    let config = test_config(name, WriteMode::AllowDdl, true, false).await;
    seed_graph(&config, name).await;
    for i in 0..12 {
        let sql = format!("CREATE TABLE filler_{i} (id INTEGER, a TEXT, b TEXT, c TEXT)");
        exec(&config, name, &sql, Vec::new()).await;
    }

    let first = tool_json(
        handle_get_schema_context(
            &config,
            SchemaContextParams {
                schema: Some("main".into()),
                source: Some(name.into()),
                token_budget: Some(200),
                tables: Some(vec!["users".into()]),
                keyword: None,
            },
        )
        .await
        .expect("context"),
    );
    let second = tool_json(
        handle_get_schema_context(
            &config,
            SchemaContextParams {
                schema: Some("main".into()),
                source: Some(name.into()),
                token_budget: Some(200),
                tables: Some(vec!["users".into()]),
                keyword: None,
            },
        )
        .await
        .expect("context repeat"),
    );
    assert_eq!(first["context"], second["context"], "deterministic output");
    assert!(first["meta"]["approx_tokens"].as_u64().unwrap() <= 200);
    assert!(first["meta"]["tables_included"].as_u64().unwrap() <= 16);
    assert!(first["context"].as_str().unwrap().contains("users"));

    let large = tool_json(
        handle_get_schema_context(
            &config,
            SchemaContextParams {
                schema: Some("main".into()),
                source: Some(name.into()),
                token_budget: Some(4000),
                tables: None,
                keyword: None,
            },
        )
        .await
        .expect("large context"),
    );
    assert_eq!(
        large["meta"]["tables_included"],
        large["meta"]["tables_total"]
    );
    assert!(large["meta"]["approx_tokens"].as_u64().unwrap() <= 4000);
}

#[tokio::test]
async fn ranked_search_orders_and_hides_score_by_default() {
    let name = "f3-ranked";
    let config = test_config(name, WriteMode::AllowDdl, true, false).await;
    for table in ["alpha", "alpine", "beta"] {
        exec(
            &config,
            name,
            &format!("CREATE TABLE {table} (id INTEGER)"),
            Vec::new(),
        )
        .await;
    }

    let compact = tool_json(
        handle_search_objects(
            &config,
            SearchObjectsParams {
                object_type: ObjectType::Table,
                keyword: Some("alpha".into()),
                schema: Some("main".into()),
                source: Some(name.into()),
                offset: None,
                limit: None,
                include_score: None,
            },
        )
        .await
        .expect("search"),
    );
    assert_eq!(compact["objects"][0]["name"], "alpha");
    assert!(compact["objects"][0].get("score").is_none());

    let scored = tool_json(
        handle_search_objects(
            &config,
            SearchObjectsParams {
                object_type: ObjectType::Table,
                keyword: Some("alp".into()),
                schema: Some("main".into()),
                source: Some(name.into()),
                offset: None,
                limit: None,
                include_score: Some(true),
            },
        )
        .await
        .expect("search scored"),
    );
    assert_eq!(scored["objects"][0]["name"], "alpha");
    assert_eq!(scored["objects"][0]["score"], 800);
    assert_eq!(scored["objects"][1]["name"], "alpine");
}

#[tokio::test]
async fn execute_sql_emits_structured_anti_patterns_in_full_mode() {
    let name = "f3-patterns";
    let config = test_config(name, WriteMode::AllowDdl, true, false).await;
    let columns = (0..21)
        .map(|i| format!("c{i} TEXT"))
        .collect::<Vec<_>>()
        .join(", ");
    exec(
        &config,
        name,
        &format!("CREATE TABLE wide (id INTEGER PRIMARY KEY, {columns})"),
        Vec::new(),
    )
    .await;
    let body = exec(&config, name, "SELECT * FROM wide", Vec::new()).await;
    let patterns = body["anti_patterns"].as_array().expect("anti_patterns");
    assert!(patterns.iter().any(|p| p["code"] == "select_star_wide"));
}

#[tokio::test]
async fn advisor_reports_full_scan_for_sqlite() {
    let name = "f3-advisor";
    let config = test_config(name, WriteMode::AllowDdl, true, false).await;
    exec(
        &config,
        name,
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT)",
        Vec::new(),
    )
    .await;
    let body = tool_json(
        handle_analyze_query(
            &config,
            AnalyzeQueryParams {
                sql: "SELECT * FROM orders WHERE status = 'open'".into(),
                params: None,
                source: Some(name.into()),
                analyze: None,
            },
        )
        .await
        .expect("analyze"),
    );
    assert_eq!(body["engine"], "sqlite");
    assert!(
        body["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["code"] == "full_scan" && i["severity"] == "warning"),
        "{body}"
    );
}

#[tokio::test]
async fn explain_analyze_requires_explicit_server_opt_in() {
    let name = "f3-analyze-optin";
    let config = test_config(name, WriteMode::AllowWrites, true, false).await;
    let error = handle_analyze_query(
        &config,
        AnalyzeQueryParams {
            sql: "SELECT 1".into(),
            params: None,
            source: Some(name.into()),
            analyze: Some(true),
        },
    )
    .await
    .expect_err("analyze must be opt-in");
    assert_eq!(error.data.as_ref().unwrap()["code"], "invalid_params");
    assert!(error.to_string().contains("--allow-explain-analyze"));
}

#[tokio::test]
async fn error_categories_are_distinct_and_retryable_is_explicit() {
    let name = "f3-errors";
    let config = test_config(name, WriteMode::ReadOnly, true, false).await;

    let unknown = handle_search_objects(
        &config,
        SearchObjectsParams {
            object_type: ObjectType::Table,
            keyword: None,
            schema: None,
            source: Some("nope".into()),
            offset: None,
            limit: None,
            include_score: None,
        },
    )
    .await
    .expect_err("unknown source");
    assert_eq!(unknown.code.0, -32602);
    assert_eq!(unknown.data.as_ref().unwrap()["code"], "invalid_params");
    assert_eq!(unknown.data.as_ref().unwrap()["retryable"], false);

    let guard = handle_execute_sql(
        &config,
        ExecuteSqlParams {
            sql: Some("DROP TABLE users".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            cursor: None,
            source: Some(name.into()),
            format: None,
        },
    )
    .await
    .expect_err("guard denial");
    assert_eq!(guard.data.as_ref().unwrap()["code"], "guard_denied");
    assert_eq!(guard.data.as_ref().unwrap()["retryable"], false);

    let execution = handle_execute_sql(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT * FROM missing_table".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            cursor: None,
            source: Some(name.into()),
            format: None,
        },
    )
    .await
    .expect_err("execution error");
    assert_eq!(execution.data.as_ref().unwrap()["code"], "execution_error");
    assert_eq!(execution.data.as_ref().unwrap()["retryable"], true);
}

#[tokio::test]
async fn session_history_detects_n_plus_one_when_opted_in() {
    let name = "f3-history";
    let config = test_config(name, WriteMode::AllowDdl, true, true).await;
    exec(
        &config,
        name,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)",
        Vec::new(),
    )
    .await;
    let mut last = Value::Null;
    for i in 0..10 {
        last = exec(
            &config,
            name,
            "SELECT * FROM users WHERE id = ?",
            vec![json!(i)],
        )
        .await;
    }
    assert!(
        last["suggestions"]
            .as_array()
            .is_some_and(|s| s.iter().any(|item| item["code"] == "n_plus_one")),
        "{last}"
    );
}
