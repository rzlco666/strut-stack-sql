//! Server-side summary/aggregate mode tests (R2-02).

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::CallToolResult;
use serde_json::{json, Value};
use strut_sql_driver_api::{ConnectOptions, EngineDriver, EngineKind};
use strut_sql_driver_sqlite::SqliteDriver;
use strut_stack_sql::config::{AppConfig, ResolvedSource, WriteMode};
use strut_stack_sql::core::auth::{Policy, Tier};
use strut_stack_sql::tools::core::{handle_execute_sql, ExecuteSqlParams};
use strut_stack_sql::tools::exec::{execute_query, ExecOptions};

fn policy(write_mode: WriteMode) -> Policy {
    Policy::new(Tier::from_write_mode(write_mode), false, false).with_local_default()
}

async fn config_with_deny(deny_columns: Vec<String>) -> (Arc<AppConfig>, Arc<dyn EngineDriver>) {
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
    let driver: Arc<dyn EngineDriver> = Arc::new(*driver);
    let mut sources = std::collections::HashMap::new();
    sources.insert(
        "default".into(),
        ResolvedSource::with_driver(
            "default",
            "sqlite::memory:".into(),
            EngineKind::Sqlite,
            driver.clone(),
            false,
        ),
    );
    let mut config = AppConfig {
        write_mode: WriteMode::AllowDdl,
        full_tools: false,
        max_rows: 100,
        max_bytes: 64 * 1024,
        query_timeout: Duration::from_secs(10),
        batch_concurrency: 4,
        batch_max_statements: 50,
        batch_timeout: Duration::from_secs(60),
        batch_max_bytes: 1024 * 1024,
        fail_fast: false,
        deny_columns,
        allow_explain_analyze: false,
        principal_id: "local".into(),
        query_history: false,
        default_source: "default".into(),
        sources,
        searched_paths: vec![],
        workspace: None,
        http_tokens: vec![],
        http_rate_limit_per_minute: 600,
        allow_unauthenticated_http: false,
        allow_grants: false,
        oauth: None,
        policy: policy(WriteMode::AllowDdl),
        require_approval: false,
        approvals: std::sync::Arc::new(strut_stack_sql::core::approvals::ApprovalQueue::new()),
        tables: strut_stack_sql::core::auth::TablePolicy::default(),
        ephemeral: false,
        hide_endpoints: None,
    };
    config.deny_columns = config.deny_columns.clone();
    (Arc::new(config), driver)
}

fn opts() -> ExecOptions {
    ExecOptions {
        write_mode: WriteMode::AllowDdl,
        max_rows: 100,
        max_bytes: 64 * 1024,
        timeout: Duration::from_secs(10),
        limit_injected: false,
        page_offset: 0,
        page_size: None,
        deny_columns: vec![],
        masks: vec![],
        table_policy: None,
    }
}

async fn seed(driver: &Arc<dyn EngineDriver>) {
    for sql in [
        "CREATE TABLE users (id INTEGER PRIMARY KEY, region TEXT, amount REAL, secret TEXT)",
        "INSERT INTO users (id, region, amount, secret) VALUES (1, 'eu', 10.0, 's1')",
        "INSERT INTO users (id, region, amount, secret) VALUES (2, 'eu', 20.0, 's2')",
        "INSERT INTO users (id, region, amount, secret) VALUES (3, 'us', 30.0, 's3')",
    ] {
        execute_query(driver, sql, &[], &opts())
            .await
            .expect("seed");
    }
}

fn summary_args(value: Value) -> ExecuteSqlParams {
    serde_json::from_value(json!({ "summary": value })).expect("summary params")
}

fn body(result: CallToolResult) -> Value {
    let text = result
        .content
        .first()
        .and_then(|b| b.as_text())
        .map(|t| t.text.as_str())
        .expect("tool text");
    serde_json::from_str(text).expect("json")
}

#[tokio::test]
async fn count_is_exact_and_groupable() {
    let (config, driver) = config_with_deny(vec![]).await;
    seed(&driver).await;

    let total = body(
        handle_execute_sql(
            &config,
            summary_args(json!({"kind":"count","table":"users"})),
        )
        .await
        .expect("count"),
    );
    assert_eq!(total["data"]["rows"], json!([[3]]), "{total}");
    assert_eq!(total["data"]["meta"]["summary"]["exact"], json!(true));

    let grouped = body(
        handle_execute_sql(
            &config,
            summary_args(json!({"kind":"count","table":"users","group_by":["region"]})),
        )
        .await
        .expect("grouped count"),
    );
    assert_eq!(
        grouped["data"]["rows"],
        json!([["eu", 2], ["us", 1]]),
        "{grouped}"
    );
}

#[tokio::test]
async fn sample_is_bounded_and_never_marked_exact() {
    let (config, driver) = config_with_deny(vec![]).await;
    seed(&driver).await;
    let payload = body(
        handle_execute_sql(
            &config,
            summary_args(
                json!({"kind":"sample","table":"users","columns":["id","region"],"limit":2}),
            ),
        )
        .await
        .expect("sample"),
    );
    assert_eq!(payload["data"]["rows"].as_array().expect("rows").len(), 2);
    assert_eq!(payload["data"]["meta"]["summary"]["kind"], json!("sample"));
    assert_eq!(
        payload["data"]["meta"]["summary"]["exact"],
        json!(false),
        "a sample must never be presented as an exact total: {payload}"
    );
}

#[tokio::test]
async fn aggregates_compute_over_rows() {
    let (config, driver) = config_with_deny(vec![]).await;
    seed(&driver).await;
    let payload = body(
        handle_execute_sql(
            &config,
            summary_args(
                json!({"kind":"aggregate","table":"users","aggregate":"sum","column":"amount"}),
            ),
        )
        .await
        .expect("sum"),
    );
    assert_eq!(payload["data"]["rows"], json!([[60.0]]), "{payload}");
    assert_eq!(payload["data"]["meta"]["summary"]["exact"], json!(true));

    let counted = body(
        handle_execute_sql(
            &config,
            summary_args(json!({"kind":"aggregate","table":"users","aggregate":"count","group_by":["region"]})),
        )
        .await
        .expect("grouped count aggregate"),
    );
    assert_eq!(
        counted["data"]["rows"],
        json!([["eu", 2], ["us", 1]]),
        "{counted}"
    );
}

#[tokio::test]
async fn deny_columns_cannot_be_used_in_group_by_or_metric() {
    let (config, driver) = config_with_deny(vec!["secret".into()]).await;
    seed(&driver).await;

    for summary in [
        json!({"kind":"count","table":"users","group_by":["secret"]}),
        json!({"kind":"aggregate","table":"users","aggregate":"max","column":"secret"}),
        json!({"kind":"sample","table":"users","columns":["secret"]}),
    ] {
        let err = handle_execute_sql(&config, summary_args(summary.clone()))
            .await
            .expect_err("deny-listed column must be rejected");
        assert!(
            err.message.contains("deny-listed"),
            "unexpected error for {summary}: {err}"
        );
    }
}

#[tokio::test]
async fn unknown_table_and_columns_are_rejected() {
    let (config, driver) = config_with_deny(vec![]).await;
    seed(&driver).await;

    let err = handle_execute_sql(
        &config,
        summary_args(json!({"kind":"count","table":"nope"})),
    )
    .await
    .expect_err("unknown table");
    assert!(err.message.contains("cannot summarize"), "{err}");

    let err = handle_execute_sql(
        &config,
        summary_args(json!({"kind":"count","table":"users","group_by":["missing"]})),
    )
    .await
    .expect_err("unknown column");
    assert!(err.message.contains("unknown column"), "{err}");
}

#[tokio::test]
async fn injection_shaped_table_name_does_not_execute_sql() {
    let (config, driver) = config_with_deny(vec![]).await;
    seed(&driver).await;
    let err = handle_execute_sql(
        &config,
        summary_args(json!({"kind":"count","table":"users; DROP TABLE users"})),
    )
    .await
    .expect_err("identifier is not a table");
    assert!(err.message.contains("cannot summarize"), "{err}");
    // The table still exists and still has its rows.
    let payload = body(
        handle_execute_sql(
            &config,
            summary_args(json!({"kind":"count","table":"users"})),
        )
        .await
        .expect("count after rejected injection"),
    );
    assert_eq!(payload["data"]["rows"], json!([[3]]));
}

#[tokio::test]
async fn summary_cannot_be_combined_with_sql() {
    let (config, _driver) = config_with_deny(vec![]).await;
    let params: ExecuteSqlParams = serde_json::from_value(json!({
        "sql": "SELECT 1",
        "summary": {"kind":"count","table":"users"}
    }))
    .expect("params");
    let err = handle_execute_sql(&config, params)
        .await
        .expect_err("mutually exclusive");
    assert!(err.message.contains("not more than one"), "{err}");
}
