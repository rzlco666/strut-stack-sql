//! Cursor (keyset) pagination tests (R2-01).
//!
//! The interesting property is not "page 2 works" but "a row inserted or
//! deleted between pages cannot duplicate or skip another row", which offset
//! pagination cannot promise. These tests walk pages while mutating the table.

use std::collections::HashSet;
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
use strut_stack_sql::tools::keyset;

fn policy(write_mode: WriteMode) -> Policy {
    Policy::new(Tier::from_write_mode(write_mode), true, false).with_local_default()
}

async fn config(write_mode: WriteMode) -> (Arc<AppConfig>, Arc<dyn EngineDriver>) {
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
    let config = AppConfig {
        write_mode,
        full_tools: false,
        max_rows: 100,
        max_bytes: 64 * 1024,
        query_timeout: Duration::from_secs(10),
        batch_concurrency: 4,
        batch_max_statements: 50,
        batch_timeout: Duration::from_secs(60),
        batch_max_bytes: 1024 * 1024,
        fail_fast: false,
        deny_columns: vec![],
        require_approval: false,
        approvals: std::sync::Arc::new(strut_stack_sql::core::approvals::ApprovalQueue::new()),
        plans: std::sync::Arc::new(strut_sql_core::migrations::PlanStore::new()),
        trace_sql: false,
        tables: strut_stack_sql::core::auth::TablePolicy::default(),
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
        policy: policy(write_mode),
        ephemeral: false,
        hide_endpoints: None,
    };
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
        principal: String::new(),
        source: String::new(),
        trace_sql: false,
    }
}

async fn seed(driver: &Arc<dyn EngineDriver>, rows: i64) {
    execute_query(
        driver,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)",
        &[],
        &opts(),
    )
    .await
    .expect("create");
    for id in 1..=rows {
        execute_query(
            driver,
            "INSERT INTO t (id, name) VALUES (?, ?)",
            &[json!(id), json!(format!("row{id}"))],
            &opts(),
        )
        .await
        .expect("insert");
    }
}

fn body(result: CallToolResult) -> Value {
    let text = result
        .content
        .first()
        .and_then(|b| b.as_text())
        .map(|t| t.text.as_str())
        .expect("tool text");
    serde_json::from_str(text).expect("json body")
}

async fn page(
    config: &Arc<AppConfig>,
    sql: &str,
    page_size: Option<usize>,
    cursor: Option<String>,
) -> Result<Value, rmcp::ErrorData> {
    let result = handle_execute_sql(
        config,
        ExecuteSqlParams {
            sql: Some(sql.into()),
            params: None,
            queries: None,
            page_offset: None,
            page_size,
            cursor,
            source: None,
            format: None,
            summary: None,
            approval_id: None,
            dry_run: None,
        },
    )
    .await?;
    Ok(body(result))
}

fn ids(payload: &Value) -> Vec<i64> {
    payload["data"]["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row[0].as_i64().expect("int id"))
        .collect()
}

#[tokio::test]
async fn cursor_walk_survives_inserts_and_deletes_between_pages() {
    let (config, driver) = config(WriteMode::AllowDdl).await;
    seed(&driver, 30).await;

    let sql = "SELECT id, name FROM t ORDER BY id";
    let mut seen: Vec<i64> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut inserted = 0i64;
    let mut deleted: HashSet<i64> = HashSet::new();

    for round in 0..10 {
        let payload = page(&config, sql, Some(10), cursor.clone())
            .await
            .expect("page")
            .clone();
        assert_eq!(payload["ok"], json!(true), "{payload}");
        let page_ids = ids(&payload);
        assert!(page_ids.len() <= 10, "page too large: {page_ids:?}");

        // Mutate between pages: insert a row beyond the cursor and delete an
        // already-returned row. Neither may corrupt the walk.
        if let Some(last) = page_ids.last().copied() {
            inserted += 1;
            let new_id = 1000 + inserted;
            execute_query(
                &driver,
                "INSERT INTO t (id, name) VALUES (?, ?)",
                &[json!(new_id), json!("inserted")],
                &opts(),
            )
            .await
            .expect("insert between pages");
            let victim = page_ids[0];
            execute_query(
                &driver,
                "DELETE FROM t WHERE id = ?",
                &[json!(victim)],
                &opts(),
            )
            .await
            .expect("delete between pages");
            deleted.insert(victim);
            let _ = last;
        }

        seen.extend(page_ids);
        cursor = payload["data"]["meta"]["next_cursor"]
            .as_str()
            .map(str::to_string);
        if cursor.is_none() {
            break;
        }
        assert!(round < 9, "pagination did not terminate");
    }

    // No duplicate ids anywhere in the walk.
    let unique: HashSet<i64> = seen.iter().copied().collect();
    assert_eq!(
        unique.len(),
        seen.len(),
        "duplicate rows across pages: {seen:?}"
    );
    // Every original row appears, in ascending order, regardless of the
    // deletes (a deleted row that was already returned stays in the result
    // stream; one deleted before its page simply never appears).
    let originals: Vec<i64> = seen.iter().copied().filter(|id| *id <= 30).collect();
    let original_set: HashSet<i64> = originals.iter().copied().collect();
    for id in 1..=30 {
        if deleted.contains(&id) {
            continue;
        }
        assert!(original_set.contains(&id), "row {id} was skipped: {seen:?}");
    }
    assert!(
        originals.windows(2).all(|w| w[0] < w[1]),
        "pages are not ordered: {seen:?}"
    );
}

#[tokio::test]
async fn cursor_is_bound_to_the_exact_sql_and_params() {
    let (config, driver) = config(WriteMode::AllowDdl).await;
    seed(&driver, 5).await;
    let sql = "SELECT id, name FROM t ORDER BY id";
    let first = page(&config, sql, Some(2), None).await.expect("first page");
    let cursor = first["data"]["meta"]["next_cursor"]
        .as_str()
        .expect("first page has a cursor")
        .to_string();

    let err = page(
        &config,
        "SELECT id, name FROM t ORDER BY id DESC",
        Some(2),
        Some(cursor.clone()),
    )
    .await
    .expect_err("different SQL must be rejected");
    let data = err.data.expect("structured error data");
    assert_eq!(data["code"], json!("cursor_invalid"), "{data}");

    let err = page(&config, sql, Some(2), Some("not-a-cursor".into()))
        .await
        .expect_err("garbage cursor");
    assert_eq!(err.data.expect("data")["code"], json!("cursor_invalid"));
}

#[tokio::test]
async fn expired_cursor_reports_cursor_expired() {
    let (config, driver) = config(WriteMode::AllowDdl).await;
    seed(&driver, 5).await;
    let sql = "SELECT id, name FROM t ORDER BY id";
    let token = keyset::codec().encode(&strut_stack_sql::tools::cursor::CursorPayload {
        v: 1,
        exp: 1,
        source: "default".into(),
        sql_hash: strut_stack_sql::tools::cursor::CursorPayload::sql_hash(sql),
        params_hash: strut_stack_sql::tools::cursor::CursorPayload::params_hash(&[]),
        after: json!(2),
        descending: false,
        page_size: 2,
    });
    let err = page(&config, sql, Some(2), Some(token))
        .await
        .expect_err("expired cursor");
    assert_eq!(err.data.expect("data")["code"], json!("cursor_expired"));
}

#[tokio::test]
async fn non_unique_order_column_falls_back_to_offset_pagination() {
    let (config, driver) = config(WriteMode::AllowDdl).await;
    seed(&driver, 5).await;
    let payload = page(
        &config,
        "SELECT id, name FROM t ORDER BY name",
        Some(2),
        None,
    )
    .await
    .expect("first page");
    assert_eq!(payload["ok"], json!(true));
    assert!(
        payload["data"]["meta"].get("next_cursor").is_none(),
        "no cursor may be issued for a non-unique order key: {payload}"
    );
    assert_eq!(ids(&payload), vec![1, 2], "fallback still pages by offset");
}

#[tokio::test]
async fn order_key_must_be_projected_for_a_cursor() {
    let (config, driver) = config(WriteMode::AllowDdl).await;
    seed(&driver, 5).await;
    let payload = page(&config, "SELECT name FROM t ORDER BY id", Some(2), None)
        .await
        .expect("first page");
    assert!(
        payload["data"]["meta"].get("next_cursor").is_none(),
        "cursor requires the key column in the projection: {payload}"
    );
}

#[tokio::test]
async fn descending_cursor_walks_down() {
    let (config, driver) = config(WriteMode::AllowDdl).await;
    seed(&driver, 7).await;
    let sql = "SELECT id FROM t ORDER BY id DESC";
    let first = page(&config, sql, Some(3), None).await.expect("page 1");
    assert_eq!(ids(&first), vec![7, 6, 5]);
    let cursor = first["data"]["meta"]["next_cursor"]
        .as_str()
        .expect("cursor")
        .to_string();
    let second = page(&config, sql, Some(3), Some(cursor))
        .await
        .expect("page 2");
    assert_eq!(ids(&second), vec![4, 3, 2]);
}
