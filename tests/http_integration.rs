//! HTTP / handler integration tests — encodes the manual live-test matrix.
//!
//! Phase 1: SQLite in-memory (always runs in CI).
//! Phase 2: PostgreSQL + MySQL via docker-compose (`#[ignore]` unless env set).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::model::CallToolResult;
use serde_json::{json, Value};
use strut_sql_driver_api::{ConnectOptions, EngineDriver, EngineKind};
use strut_sql_driver_mysql::MySqlDriver;
use strut_sql_driver_postgres::PostgresDriver;
use strut_sql_driver_sqlite::SqliteDriver;
use strut_stack_sql::config::{AppConfig, ResolvedSource, WriteMode};
use strut_stack_sql::guard::validate_and_prepare;
use strut_stack_sql::server::build_http_router;
use strut_stack_sql::tools::core::{
    handle_execute_sql, handle_search_objects, BatchQueryItem, ExecuteSqlParams,
    SearchObjectsParams,
};
use strut_stack_sql::tools::introspect::{describe_table, ObjectType};
use tower::ServiceExt;

fn force_columnar_test_format() {
    std::env::set_var("MCP_SQL_FORMAT", "columnar");
}

async fn sqlite_config(write_mode: WriteMode) -> AppConfig {
    force_columnar_test_format();
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
        "default".into(),
        ResolvedSource::with_driver(
            "default",
            "sqlite::memory:".into(),
            EngineKind::Sqlite,
            Arc::new(*driver),
            !write_mode.allows_dml(),
        ),
    );
    AppConfig {
        write_mode,
        full_tools: true,
        max_rows: 100,
        max_bytes: 64 * 1024,
        query_timeout: Duration::from_secs(10),
        batch_concurrency: 8,
        fail_fast: false,
        deny_columns: vec![],
        default_source: "default".into(),
        sources,
        searched_paths: vec![],
        workspace: None,
        http_tokens: vec![],
        http_rate_limit_per_minute: 600,
        allow_unauthenticated_http: false,
        ephemeral: false,
        hide_endpoints: None,
    }
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

async fn exec_sql_ok(config: &Arc<AppConfig>, params: ExecuteSqlParams) -> Value {
    let result = handle_execute_sql(config, params)
        .await
        .expect("execute_sql handler");
    tool_json(result)
}

async fn exec_sql_err(config: &Arc<AppConfig>, params: ExecuteSqlParams) -> String {
    match handle_execute_sql(config, params).await {
        Ok(result) => {
            let body = tool_json(result);
            if body["ok"] == false {
                body["error"].as_str().unwrap_or("unknown").to_string()
            } else {
                panic!("expected error, got ok: {body}");
            }
        }
        Err(e) => e.to_string(),
    }
}

async fn seed_users(config: &Arc<AppConfig>) {
    exec_sql_ok(
        config,
        ExecuteSqlParams {
            sql: Some("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, note TEXT)".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    exec_sql_ok(
        config,
        ExecuteSqlParams {
            sql: Some("INSERT INTO users (id, name, note) VALUES (?, ?, ?)".into()),
            params: Some(vec![json!(1), json!("alice"), json!("hello")]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
}

// --- healthz ---

#[tokio::test]
async fn healthz_returns_ok_for_sqlite() {
    let config = sqlite_config(WriteMode::ReadOnly).await;
    let (app, _ct) = build_http_router(config);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// --- security guard ---

#[tokio::test]
async fn guard_blocks_dml_in_readonly() {
    let setup = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    seed_users(&setup).await;
    let mut readonly = (*setup).clone();
    readonly.write_mode = WriteMode::ReadOnly;
    let config = Arc::new(readonly);
    let err = exec_sql_err(
        &config,
        ExecuteSqlParams {
            sql: Some("DELETE FROM users WHERE id = 1".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(err.contains("DML blocked"));
}

#[tokio::test]
async fn guard_blocks_ddl_in_readonly() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let err = exec_sql_err(
        &config,
        ExecuteSqlParams {
            sql: Some("DROP TABLE users".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(err.contains("DDL blocked"));
}

#[tokio::test]
async fn guard_blocks_transaction_control() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let err = exec_sql_err(
        &config,
        ExecuteSqlParams {
            sql: Some("BEGIN".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(err.contains("transaction control"));
}

#[tokio::test]
async fn guard_blocks_multi_statement() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let err = exec_sql_err(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT 1; DROP TABLE users".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(err.contains("multiple statements"));
}

#[tokio::test]
async fn guard_blocks_empty_query() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let err = exec_sql_err(
        &config,
        ExecuteSqlParams {
            sql: Some("   ".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(err.contains("empty") || err.contains("parse"));
}

#[tokio::test]
async fn guard_blocks_grant() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let err = exec_sql_err(
        &config,
        ExecuteSqlParams {
            sql: Some("GRANT ALL ON users TO evil".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(!err.is_empty());
}

// --- parameterized queries ---

#[tokio::test]
async fn params_basic_select() {
    let config = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    seed_users(&config).await;
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT name FROM users WHERE id = ?".into()),
            params: Some(vec![json!(1)]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["rows"][0][0], "alice");
}

#[tokio::test]
async fn params_unicode_and_emoji() {
    let config = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    seed_users(&config).await;
    let note = "café 🎉 日本語";
    exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("UPDATE users SET note = ? WHERE id = ?".into()),
            params: Some(vec![json!(note), json!(1)]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT note FROM users WHERE id = ?".into()),
            params: Some(vec![json!(1)]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["data"]["rows"][0][0], note);
}

#[tokio::test]
async fn params_null_value() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT CASE WHEN ? IS NULL THEN 1 ELSE 0 END AS is_null".into()),
            params: Some(vec![Value::Null]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["data"]["rows"][0][0], 1);
}

// --- param count validation ---

#[tokio::test]
async fn params_missing_placeholder_rejected() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let err = exec_sql_err(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT * FROM users WHERE id = ?".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(err.contains("mismatch"));
}

#[tokio::test]
async fn params_extra_rejected() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let err = exec_sql_err(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT 1".into()),
            params: Some(vec![json!(1)]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(err.contains("unexpected params"));
}

// --- SQL injection via params ---

#[tokio::test]
async fn injection_param_treated_as_data() {
    let config = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    seed_users(&config).await;
    let evil = "1; DROP TABLE users";
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT id FROM users WHERE name = ?".into()),
            params: Some(vec![json!(evil)]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["rows"].as_array().unwrap().len(), 0);
    let check = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT COUNT(*) FROM users".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(check["data"]["rows"][0][0], 1);
}

#[tokio::test]
async fn injection_union_in_param_is_literal() {
    let config = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    seed_users(&config).await;
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT id FROM users WHERE name = ?".into()),
            params: Some(vec![json!("1 UNION SELECT 999")]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["rows"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn injection_or_in_param_is_literal() {
    let config = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    seed_users(&config).await;
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT id FROM users WHERE name = ?".into()),
            params: Some(vec![json!("1 OR 1=1")]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["rows"].as_array().unwrap().len(), 0);
}

// --- type boundaries ---

#[tokio::test]
async fn types_large_int_and_float() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT ?, ? AS f".into()),
            params: Some(vec![
                json!(9_007_199_254_740_991_i64),
                json!(-314.0 / 100.0),
            ]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["data"]["rows"][0][0], 9_007_199_254_740_991_i64);
}

#[tokio::test]
async fn count_serializes_as_number_not_bool() {
    let config = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    seed_users(&config).await;
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT COUNT(*) FROM users".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(body["data"]["rows"][0][0].is_number());
}

// --- batch ---

#[tokio::test]
async fn batch_legacy_strings() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: None,
            params: None,
            queries: Some(vec![
                BatchQueryItem::Legacy("SELECT 1 AS a".into()),
                BatchQueryItem::Legacy("SELECT 2 AS b".into()),
            ]),
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["results"][0]["ok"], true);
    assert_eq!(body["results"][1]["ok"], true);
}

#[tokio::test]
async fn batch_parameterized() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: None,
            params: None,
            queries: Some(vec![BatchQueryItem::Parameterized {
                sql: "SELECT ? AS v".into(),
                params: Some(vec![json!(42)]),
            }]),
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["results"][0]["data"]["rows"][0][0], 42);
}

#[tokio::test]
async fn batch_mixed_valid_and_invalid() {
    let config = Arc::new(sqlite_config(WriteMode::ReadOnly).await);
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: None,
            params: None,
            queries: Some(vec![
                BatchQueryItem::Legacy("SELECT 1".into()),
                BatchQueryItem::Legacy("DROP TABLE users".into()),
            ]),
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    let results = body["results"].as_array().expect("batch results");
    assert_eq!(results.len(), 2);
    let ok_count = results.iter().filter(|r| r["ok"] == true).count();
    let fail_count = results.iter().filter(|r| r["ok"] == false).count();
    assert_eq!(ok_count, 1);
    assert_eq!(fail_count, 1);
}

// --- pagination ---

#[tokio::test]
async fn pagination_offset_and_size() {
    let config = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("CREATE TABLE seq (id INTEGER PRIMARY KEY)".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    for id in 1..=120 {
        exec_sql_ok(
            &config,
            ExecuteSqlParams {
                sql: Some("INSERT INTO seq (id) VALUES (?)".into()),
                params: Some(vec![json!(id)]),
                queries: None,
                page_offset: None,
                page_size: None,
                source: None,
                format: None,
            },
        )
        .await;
    }
    let page1 = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT id FROM seq ORDER BY id".into()),
            params: None,
            queries: None,
            page_offset: Some(0),
            page_size: Some(40),
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(page1["data"]["rows"].as_array().unwrap().len(), 40);
    assert_eq!(page1["data"]["meta"]["has_more"], true);

    let page2 = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT id FROM seq ORDER BY id".into()),
            params: None,
            queries: None,
            page_offset: Some(40),
            page_size: Some(40),
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(page2["data"]["rows"].as_array().unwrap().len(), 40);
    assert_eq!(page2["data"]["meta"]["has_more"], true);
}

// --- describe_table regression ---

#[tokio::test]
async fn describe_table_returns_columns() {
    let config = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    seed_users(&config).await;
    let driver = config.source(None).unwrap().driver().await.expect("driver");
    let object = describe_table(&driver, Some("main"), "users")
        .await
        .expect("describe");
    let cols = object.columns.expect("columns");
    assert!(cols.iter().any(|c| c.name == "name"));
}

#[tokio::test]
async fn search_objects_finds_table() {
    let config = Arc::new(sqlite_config(WriteMode::AllowDdl).await);
    seed_users(&config).await;
    let result = handle_search_objects(
        &config,
        SearchObjectsParams {
            object_type: ObjectType::Table,
            keyword: Some("users".into()),
            schema: None,
            source: None,
            offset: None,
            limit: None,
        },
    )
    .await
    .expect("search");
    let body = tool_json(result);
    assert!(body["objects"]
        .as_array()
        .unwrap()
        .iter()
        .any(|o| o["name"] == "users"));
}

// --- Phase 2: PostgreSQL (docker-compose) ---

async fn postgres_config() -> Option<AppConfig> {
    force_columnar_test_format();
    let url = std::env::var("POSTGRES_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()?;
    if !url.to_lowercase().starts_with("postgres") {
        return None;
    }
    let driver = PostgresDriver::from_connection_string_with_options(
        &url,
        &ConnectOptions {
            pool_size: 2,
            read_only: true,
            ..ConnectOptions::default()
        },
    )
    .await
    .ok()?;
    let mut sources = HashMap::new();
    sources.insert(
        "default".into(),
        ResolvedSource::with_driver(
            "default",
            url.clone(),
            EngineKind::Postgres,
            Arc::new(*driver),
            true,
        ),
    );
    Some(AppConfig {
        write_mode: WriteMode::ReadOnly,
        full_tools: true,
        max_rows: 100,
        max_bytes: 64 * 1024,
        query_timeout: Duration::from_secs(10),
        batch_concurrency: 8,
        fail_fast: false,
        deny_columns: vec![],
        default_source: "default".into(),
        sources,
        searched_paths: vec![],
        workspace: None,
        http_tokens: vec![],
        http_rate_limit_per_minute: 600,
        allow_unauthenticated_http: false,
        ephemeral: false,
        hide_endpoints: None,
    })
}

#[tokio::test]
#[ignore = "requires POSTGRES_DATABASE_URL (docker compose postgres on :5433)"]
async fn postgres_parameterized_select() {
    let config = Arc::new(postgres_config().await.expect("postgres dsn"));
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT id FROM demo.users WHERE id = ?".into()),
            params: Some(vec![json!(42)]),
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["ok"], true);
}

#[tokio::test]
#[ignore = "requires POSTGRES_DATABASE_URL"]
async fn postgres_healthz() {
    let config = postgres_config().await.expect("postgres dsn");
    let (app, _ct) = build_http_router(config);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// --- Phase 2: MySQL (docker-compose) ---

async fn mysql_config() -> Option<AppConfig> {
    force_columnar_test_format();
    let url = std::env::var("MYSQL_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()?;
    if !url.to_lowercase().starts_with("mysql://") {
        return None;
    }
    let driver = MySqlDriver::from_connection_string_with_options(
        &url,
        &ConnectOptions {
            pool_size: 2,
            read_only: true,
            ..ConnectOptions::default()
        },
    )
    .await
    .ok()?;
    let mut sources = HashMap::new();
    sources.insert(
        "default".into(),
        ResolvedSource::with_driver(
            "default",
            url.clone(),
            EngineKind::MySql,
            Arc::new(*driver),
            true,
        ),
    );
    Some(AppConfig {
        write_mode: WriteMode::ReadOnly,
        full_tools: true,
        max_rows: 100,
        max_bytes: 64 * 1024,
        query_timeout: Duration::from_secs(10),
        batch_concurrency: 8,
        fail_fast: false,
        deny_columns: vec![],
        default_source: "default".into(),
        sources,
        searched_paths: vec![],
        workspace: None,
        http_tokens: vec![],
        http_rate_limit_per_minute: 600,
        allow_unauthenticated_http: false,
        ephemeral: false,
        hide_endpoints: None,
    })
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL (docker compose mysql on :3307)"]
async fn mysql_show_processlist_allowed() {
    validate_and_prepare(
        "SHOW PROCESSLIST",
        &[],
        EngineKind::MySql,
        WriteMode::ReadOnly,
        100,
    )
    .expect("guard allows SHOW PROCESSLIST");
    let config = Arc::new(mysql_config().await.expect("mysql dsn"));
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SHOW PROCESSLIST".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert_eq!(body["ok"], true);
    assert!(!body["data"]["rows"].as_array().unwrap().is_empty());
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL"]
async fn mysql_count_serializes_as_number() {
    let config = Arc::new(mysql_config().await.expect("mysql dsn"));
    let body = exec_sql_ok(
        &config,
        ExecuteSqlParams {
            sql: Some("SELECT COUNT(*) AS c FROM (SELECT 1 AS x) t".into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size: None,
            source: None,
            format: None,
        },
    )
    .await;
    assert!(body["data"]["rows"][0][0].is_number());
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL"]
async fn mysql_describe_table_has_columns() {
    let config = Arc::new(mysql_config().await.expect("mysql dsn"));
    let source = config.source(None).unwrap();
    let driver = source.driver().await.expect("driver");
    let schema = driver.resolve_schema(None).await.expect("schema");
    let object = describe_table(&driver, Some(&schema), "users")
        .await
        .expect("describe demo.users");
    let cols = object.columns.unwrap_or_default();
    assert!(!cols.is_empty(), "describe_table must return columns");
}

// --- HTTP bearer-token auth (SQL-SEC-05 / SQL-GAP-01 / SQL-GAP-04) ---

use strut_stack_sql::config::HttpToken;

async fn sqlite_config_with_tokens(
    write_mode: WriteMode,
    http_tokens: Vec<HttpToken>,
) -> AppConfig {
    let mut config = sqlite_config(write_mode).await;
    config.http_tokens = http_tokens;
    config
}

fn test_token(name: &str, secret: &str, mode: WriteMode) -> HttpToken {
    HttpToken {
        name: name.into(),
        token: secret.into(),
        mode,
        sources: vec![],
    }
}

fn mcp_tools_call_body(tool: &str, arguments: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    })
    .to_string()
}

async fn post_mcp(app: axum::Router, token: Option<&str>, body: String) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let response = app
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn http_mcp_without_token_is_401() {
    let config = sqlite_config_with_tokens(
        WriteMode::AllowDdl,
        vec![test_token("ro", "ro-secret", WriteMode::ReadOnly)],
    )
    .await;
    let (app, _ct) = build_http_router(config);
    let (status, _body) = post_mcp(
        app,
        None,
        mcp_tools_call_body("execute_sql", json!({ "sql": "SELECT 1" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_readonly_token_cannot_call_ddl_tool() {
    let config = sqlite_config_with_tokens(
        WriteMode::AllowDdl,
        vec![test_token("ro", "ro-secret", WriteMode::ReadOnly)],
    )
    .await;
    let (app, _ct) = build_http_router(config);
    let (status, body) = post_mcp(
        app,
        Some("ro-secret"),
        mcp_tools_call_body(
            "create_table",
            json!({ "ddl": "CREATE TABLE t (id INTEGER)" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert!(body.contains("may not call DDL tool"), "body: {body}");
}

#[tokio::test]
async fn http_readonly_token_cannot_insert_via_execute_sql() {
    let config = sqlite_config_with_tokens(
        WriteMode::AllowDdl,
        vec![test_token("ro", "ro-secret", WriteMode::ReadOnly)],
    )
    .await;
    let (app, _ct) = build_http_router(config);
    let (status, body) = post_mcp(
        app,
        Some("ro-secret"),
        mcp_tools_call_body("execute_sql", json!({ "sql": "INSERT INTO t VALUES (1)" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
}

#[tokio::test]
async fn http_token_source_allowlist_enforced() {
    let mut restricted = test_token("ro", "ro-secret", WriteMode::ReadOnly);
    restricted.sources = vec!["other".into()];
    let config = sqlite_config_with_tokens(WriteMode::AllowDdl, vec![restricted]).await;
    let (app, _ct) = build_http_router(config);
    // No `source` argument -> default source "default", not in allowlist.
    let (status, body) = post_mcp(
        app,
        Some("ro-secret"),
        mcp_tools_call_body("execute_sql", json!({ "sql": "SELECT 1" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert!(body.contains("not allowed to use source"), "body: {body}");
}

/// Full MCP handshake with a DDL token: initialize -> initialized ->
/// tools/call create_table must succeed end to end.
#[tokio::test]
async fn http_ddl_token_can_create_table_end_to_end() {
    let config = sqlite_config_with_tokens(
        WriteMode::AllowDdl,
        vec![test_token("admin", "admin-secret", WriteMode::AllowDdl)],
    )
    .await;
    let (app, _ct) = build_http_router(config);

    let init_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "auth-test", "version": "0" }
        }
    })
    .to_string();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("authorization", "Bearer admin-secret")
                .header("host", "localhost")
                .body(Body::from(init_body))
                .unwrap(),
        )
        .await
        .unwrap();
    if response.status() != StatusCode::OK {
        let st = response.status();
        let b = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        panic!(
            "initialize failed: {st} body: {}",
            String::from_utf8_lossy(&b)
        );
    }
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .expect("session id header")
        .to_string();

    // notifications/initialized (no response body expected)
    let _ = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("authorization", "Bearer admin-secret")
                .header("host", "localhost")
                .header("mcp-session-id", session_id.clone())
                .body(Body::from(
                    json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("authorization", "Bearer admin-secret")
                .header("host", "localhost")
                .header("mcp-session-id", session_id)
                .body(Body::from(mcp_tools_call_body(
                    "create_table",
                    json!({ "ddl": "CREATE TABLE auth_t (id INTEGER PRIMARY KEY, name TEXT)" }),
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("\"result\"") && !text.contains("\"error\""),
        "create_table via DDL token should succeed, got: {text}"
    );
}

#[tokio::test]
async fn livez_is_ok_without_touching_pools() {
    let config = sqlite_config(WriteMode::ReadOnly).await;
    let (app, _ct) = build_http_router(config);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/livez")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    // Liveness leaks nothing: no source names, no engine, no errors.
    assert!(text.contains("\"ok\""), "body: {text}");
    assert!(!text.contains("default"), "body: {text}");
}
