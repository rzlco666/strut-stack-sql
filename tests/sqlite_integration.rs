//! SQLite integration tests — in-memory, no external database required.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use strut_sql_driver_api::{ConnectOptions, EngineDriver, EngineKind};
use strut_sql_driver_sqlite::SqliteDriver;
use strut_stack_sql::config::WriteMode;
use strut_stack_sql::guard::validate_and_prepare;
use strut_stack_sql::tools::exec::{execute_query, ExecOptions};
use strut_stack_sql::tools::explain::analyze_query;
use strut_stack_sql::tools::introspect::{
    describe_table, invalidate_source, list_tables, schema_overview, search_objects, ObjectType,
};

async fn sqlite_pool() -> Arc<dyn EngineDriver> {
    let options = ConnectOptions {
        pool_size: 1,
        read_only: false,
        ..ConnectOptions::default()
    };
    let driver = SqliteDriver::from_connection_string_with_options("sqlite::memory:", &options)
        .await
        .expect("sqlite memory driver");
    Arc::new(*driver)
}

fn exec_opts(write_mode: WriteMode) -> ExecOptions {
    ExecOptions {
        write_mode,
        max_rows: 100,
        max_bytes: 64 * 1024,
        timeout: Duration::from_secs(10),
        limit_injected: false,
        page_offset: 0,
        page_size: None,
        deny_columns: vec![],
    }
}

#[tokio::test]
async fn sqlite_crud_and_parameterized_select() {
    let pool = sqlite_pool().await;
    let ddl_opts = exec_opts(WriteMode::AllowDdl);
    let write_opts = exec_opts(WriteMode::AllowWrites);
    let read_opts = exec_opts(WriteMode::ReadOnly);

    let created = execute_query(
        &pool,
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL)",
        &[],
        &ddl_opts,
    )
    .await
    .expect("create table");
    assert!(created.ok, "{:?}", created.error);

    let inserted = execute_query(
        &pool,
        "INSERT INTO users (id, name) VALUES (?, ?)",
        &[json!(1), json!("alice")],
        &write_opts,
    )
    .await
    .expect("insert");
    assert!(inserted.ok, "{:?}", inserted.error);

    let selected = execute_query(
        &pool,
        "SELECT id, name FROM users WHERE id = ?",
        &[json!(1)],
        &read_opts,
    )
    .await
    .expect("select");
    assert!(selected.ok, "{:?}", selected.error);
    let data = selected.data.expect("columnar data");
    assert_eq!(data.cols, vec!["id", "name"]);
    assert_eq!(data.rows.len(), 1);
    assert_eq!(data.rows[0][1], Value::String("alice".into()));
}

#[tokio::test]
async fn sqlite_list_tables_and_describe_table() {
    let pool = sqlite_pool().await;
    let ddl_opts = exec_opts(WriteMode::AllowDdl);

    execute_query(
        &pool,
        "CREATE TABLE items (id INTEGER PRIMARY KEY, sku TEXT)",
        &[],
        &ddl_opts,
    )
    .await
    .expect("create items");

    let tables = list_tables(&pool, None, Some("main"), None)
        .await
        .expect("list tables");
    assert!(tables.iter().any(|t| t.name == "items"));

    let described = describe_table(&pool, Some("main"), "items")
        .await
        .expect("describe items");
    let columns = described.columns.expect("columns");
    assert!(columns.iter().any(|c| c.name == "sku"));
    assert!(described.indexes.is_some());
}

#[tokio::test]
async fn sqlite_describe_table_unique_index_flag() {
    let pool = sqlite_pool().await;
    let ddl_opts = exec_opts(WriteMode::AllowDdl);

    execute_query(
        &pool,
        "CREATE TABLE uniq_demo (id INTEGER PRIMARY KEY, email TEXT, name TEXT)",
        &[],
        &ddl_opts,
    )
    .await
    .expect("create uniq_demo");
    execute_query(
        &pool,
        "CREATE UNIQUE INDEX uniq_demo_email ON uniq_demo(email)",
        &[],
        &ddl_opts,
    )
    .await
    .expect("unique index");
    execute_query(
        &pool,
        "CREATE INDEX uniq_demo_name ON uniq_demo(name)",
        &[],
        &ddl_opts,
    )
    .await
    .expect("non-unique index");

    let described = describe_table(&pool, Some("main"), "uniq_demo")
        .await
        .expect("describe uniq_demo");
    let indexes = described.indexes.expect("indexes");
    let email = indexes
        .iter()
        .find(|i| i.name == "uniq_demo_email")
        .expect("unique index present");
    assert!(email.unique, "UNIQUE index must report unique: true");
    let name_idx = indexes
        .iter()
        .find(|i| i.name == "uniq_demo_name")
        .expect("non-unique index present");
    assert!(!name_idx.unique, "plain index must report unique: false");
}

#[tokio::test]
async fn sqlite_schema_overview_collects_all_table_foreign_keys() {
    let pool = sqlite_pool().await;
    let ddl_opts = exec_opts(WriteMode::AllowDdl);
    for sql in [
        "CREATE TABLE parent (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        "CREATE TABLE child (x INTEGER, y INTEGER, \
         FOREIGN KEY (x, y) REFERENCES parent (a, b))",
        "CREATE TABLE grandchild (id INTEGER PRIMARY KEY, cx INTEGER, \
         FOREIGN KEY (cx) REFERENCES child (x))",
    ] {
        execute_query(&pool, sql, &[], &ddl_opts)
            .await
            .expect("ddl");
    }

    let overview = schema_overview(&pool, Some("overview-src"), Some("main"))
        .await
        .expect("schema overview");
    assert_eq!(overview.meta.n_tables, 3);
    assert_eq!(overview.meta.n_foreign_keys, 2, "{overview:?}");
    let composite = overview
        .foreign_keys
        .iter()
        .find(|fk| fk.table == "child")
        .expect("child FK");
    assert_eq!(composite.columns, vec!["x", "y"]);
    assert_eq!(composite.ref_columns, vec!["a", "b"]);
}

#[tokio::test]
async fn sqlite_search_objects_pushes_pagination_and_keyword() {
    let pool = sqlite_pool().await;
    let ddl_opts = exec_opts(WriteMode::AllowDdl);
    for name in ["alpha", "alpine", "beta"] {
        execute_query(
            &pool,
            &format!("CREATE TABLE {name} (id INTEGER)"),
            &[],
            &ddl_opts,
        )
        .await
        .expect("ddl");
    }

    let page = search_objects(
        &pool,
        Some("search-src"),
        ObjectType::Table,
        Some("alp"),
        Some("main"),
        0,
        1,
    )
    .await
    .expect("search");
    assert_eq!(page.meta.n, 1);
    assert!(page.meta.has_more);
    assert_eq!(page.objects[0].name, "alpha");

    let page = search_objects(
        &pool,
        Some("search-src"),
        ObjectType::Table,
        Some("alp"),
        Some("main"),
        1,
        1,
    )
    .await
    .expect("second page");
    assert_eq!(page.objects[0].name, "alpine");
    assert!(!page.meta.has_more);

    let beyond = search_objects(
        &pool,
        Some("search-src"),
        ObjectType::Table,
        Some("alp"),
        Some("main"),
        99,
        10,
    )
    .await
    .expect("offset past end");
    assert!(beyond.objects.is_empty());
    assert!(!beyond.meta.has_more);
}

#[tokio::test]
async fn sqlite_schema_cache_invalidation_reveals_new_table() {
    let pool = sqlite_pool().await;
    let ddl_opts = exec_opts(WriteMode::AllowDdl);
    execute_query(&pool, "CREATE TABLE first (id INTEGER)", &[], &ddl_opts)
        .await
        .expect("first");

    let before = schema_overview(&pool, Some("cache-src"), Some("main"))
        .await
        .expect("overview before");
    assert_eq!(before.meta.n_tables, 1);

    execute_query(&pool, "CREATE TABLE second (id INTEGER)", &[], &ddl_opts)
        .await
        .expect("second");
    // Without invalidation the cached overview is still served (TTL > 0).
    let cached = schema_overview(&pool, Some("cache-src"), Some("main"))
        .await
        .expect("cached overview");
    assert_eq!(cached.meta.n_tables, 1);

    invalidate_source("cache-src");
    let after = schema_overview(&pool, Some("cache-src"), Some("main"))
        .await
        .expect("overview after");
    assert_eq!(after.meta.n_tables, 2);
}

#[tokio::test]
async fn sqlite_guard_blocks_ddl_in_read_only() {
    let err = validate_and_prepare(
        "CREATE TABLE blocked (id INTEGER)",
        &[],
        EngineKind::Sqlite,
        WriteMode::ReadOnly,
        100,
    )
    .unwrap_err();
    assert!(err.to_string().contains("DDL blocked"));
}

#[tokio::test]
async fn sqlite_analyze_query_performance() {
    let pool = sqlite_pool().await;
    let ddl_opts = exec_opts(WriteMode::AllowDdl);

    execute_query(
        &pool,
        "CREATE TABLE metrics (id INTEGER PRIMARY KEY, value REAL)",
        &[],
        &ddl_opts,
    )
    .await
    .expect("create metrics");

    let summary = analyze_query(
        &pool,
        "SELECT * FROM metrics WHERE id = ?",
        &[json!(1)],
        WriteMode::ReadOnly,
        Duration::from_secs(5),
    )
    .await
    .expect("explain");
    assert_eq!(summary.engine, "sqlite");
    assert!(!summary.nodes.is_empty());
}

#[tokio::test]
async fn sqlite_execute_sql_pagination() {
    let pool = sqlite_pool().await;
    let ddl_opts = exec_opts(WriteMode::AllowDdl);
    let write_opts = exec_opts(WriteMode::AllowWrites);
    let mut read_opts = exec_opts(WriteMode::ReadOnly);

    execute_query(
        &pool,
        "CREATE TABLE pages (id INTEGER PRIMARY KEY)",
        &[],
        &ddl_opts,
    )
    .await
    .expect("create pages");

    for id in 1..=150 {
        execute_query(
            &pool,
            "INSERT INTO pages (id) VALUES (?)",
            &[json!(id)],
            &write_opts,
        )
        .await
        .expect("insert row");
    }

    read_opts.page_size = Some(50);
    read_opts.page_offset = 0;
    let page1 = execute_query(&pool, "SELECT id FROM pages ORDER BY id", &[], &read_opts)
        .await
        .expect("page 1");
    let data1 = page1.data.expect("page1 data");
    assert_eq!(data1.rows.len(), 50);
    assert_eq!(data1.meta.has_more, Some(true));
    assert_eq!(data1.meta.total_fetched, Some(51));

    read_opts.page_offset = 50;
    let page2 = execute_query(&pool, "SELECT id FROM pages ORDER BY id", &[], &read_opts)
        .await
        .expect("page 2");
    let data2 = page2.data.expect("page2 data");
    assert_eq!(data2.rows.len(), 50);
    assert_eq!(data2.meta.has_more, Some(true));
    assert_eq!(data2.meta.total_fetched, Some(100));

    read_opts.page_offset = 100;
    let page3 = execute_query(&pool, "SELECT id FROM pages ORDER BY id", &[], &read_opts)
        .await
        .expect("page 3");
    let data3 = page3.data.expect("page3 data");
    assert_eq!(data3.rows.len(), 50);
    assert_eq!(data3.meta.has_more, Some(false));
    assert_eq!(data3.meta.total_fetched, Some(50));
}
