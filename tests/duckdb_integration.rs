//! DuckDB through the existing config, policy, tools and result contracts.
//! All databases are embedded and isolated per test.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rmcp::model::CallToolResult;
use serde_json::{json, Value};
use strut_sql_driver_api::{BoundQuery, EngineDriver, EngineKind};
use strut_sql_guard::{ColumnMask, MaskMode, RowFilter, TablePolicy, TableRule};
use strut_stack_sql::config::{load_config, source_endpoint, AppConfig, Cli, WriteMode};
use strut_stack_sql::tools::core::{handle_execute_sql, ExecuteSqlParams};
use strut_stack_sql::tools::explain::{analyze_query_with_options, AnalyzeOptions};
use strut_stack_sql::tools::full::{
    handle_describe_table, handle_list_tables, DescribeTableParams, ListTablesParams,
};

async fn config() -> Arc<AppConfig> {
    let cli = Cli::try_parse_from([
        "strut-stack-sql",
        "--url",
        "duckdb::memory:",
        "--allow-ddl",
        "--full-tools",
    ])
    .expect("cli");
    Arc::new(load_config(&cli).await.expect("DuckDB config"))
}

fn params(sql: &str, values: Vec<Value>) -> ExecuteSqlParams {
    serde_json::from_value(json!({"sql": sql, "params": values})).expect("tool params")
}

fn body(result: CallToolResult) -> Value {
    let text = result.content[0].as_text().expect("text result");
    serde_json::from_str(&text.text).expect("tool JSON")
}

async fn execute(config: &Arc<AppConfig>, sql: &str, values: Vec<Value>) -> Value {
    body(
        handle_execute_sql(config, params(sql, values))
            .await
            .expect(sql),
    )
}

async fn seed(config: &Arc<AppConfig>) {
    execute(
        config,
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, tenant INTEGER, email VARCHAR)",
        vec![],
    )
    .await;
    execute(config, "CREATE TABLE payroll (salary INTEGER)", vec![]).await;
    execute(
        config,
        "INSERT INTO orders VALUES (1, 7, 'alice@example.com'), (2, 9, 'bob@example.com')",
        vec![],
    )
    .await;
}

#[tokio::test]
async fn duckdb_lazy_registry_crud_and_error_contract() {
    let config = config().await;
    assert_eq!(config.source(None).unwrap().engine, EngineKind::DuckDb);
    let endpoint = source_endpoint("duckdb::memory:", EngineKind::DuckDb);
    assert_eq!(endpoint.engine, "duckdb");
    assert_eq!(endpoint.database.as_deref(), Some(":memory:"));
    assert!(endpoint.host.is_none());
    assert!(endpoint.port.is_none());
    execute(
        &config,
        "CREATE TABLE values_test (id INTEGER, note VARCHAR, flag BOOLEAN)",
        vec![],
    )
    .await;
    execute(
        &config,
        "INSERT INTO values_test VALUES (?, ?, ?)",
        vec![json!(42), json!("O'Reilly"), json!(true)],
    )
    .await;
    let selected = execute(
        &config,
        "SELECT id, note, flag FROM values_test WHERE id = ?",
        vec![json!(42)],
    )
    .await;
    assert_eq!(
        selected["data"]["rows"],
        json!([[42, "O'Reilly", true]]),
        "{selected}"
    );
    assert_eq!(selected["data"]["cols"], json!(["id", "note", "flag"]));

    let missing = handle_execute_sql(&config, params("SELECT * FROM nonexistent_table", vec![]))
        .await
        .expect_err("catalog error");
    assert_eq!(missing.data.unwrap()["code"], "execution_error");
    let mismatch = handle_execute_sql(&config, params("SELECT ?", vec![]))
        .await
        .expect_err("parameter count");
    assert_eq!(mismatch.data.unwrap()["code"], "guard_denied");
    let selected = execute(&config, "SELECT count(*) AS n FROM values_test", vec![]).await;
    assert_eq!(selected["data"]["rows"], json!([[1]]));
}

#[tokio::test]
async fn duckdb_table_policy_masks_filters_and_discovery() {
    let mut config = config().await;
    seed(&config).await;
    Arc::make_mut(&mut config).tables = TablePolicy {
        deny: vec![TableRule {
            schema: None,
            table: "payroll".into(),
        }],
        row_filters: vec![RowFilter {
            schema: None,
            table: "orders".into(),
            predicate: "tenant = 7".into(),
        }],
        masks: vec![ColumnMask {
            table: None,
            column: "email".into(),
            mode: MaskMode::Null,
        }],
        ..TablePolicy::default()
    };
    let selected = execute(
        &config,
        "SELECT o.id, o.email FROM orders o WHERE o.id > ? ORDER BY o.id",
        vec![json!(0)],
    )
    .await;
    assert_eq!(selected["data"]["rows"], json!([[1, null]]), "{selected}");
    assert_eq!(
        selected["data"]["meta"]["redacted_columns"],
        json!(["email"])
    );
    for sql in [
        "SELECT * FROM payroll",
        "DELETE FROM orders",
        "SELECT * FROM query('SELECT * FROM payroll')",
        "SELECT * FROM query_table('payroll')",
    ] {
        let denied = handle_execute_sql(&config, params(sql, vec![]))
            .await
            .expect_err(sql);
        assert_eq!(denied.data.unwrap()["code"], "guard_denied", "{sql}");
    }
    let listed = body(
        handle_list_tables(
            &config,
            ListTablesParams {
                schema: Some("main".into()),
                keyword: None,
                offset: None,
                limit: None,
                source: None,
            },
        )
        .await
        .expect("list tables"),
    );
    assert!(!listed.to_string().contains("payroll"), "{listed}");
    let denied = handle_describe_table(
        &config,
        DescribeTableParams {
            schema: Some("main".into()),
            table: "payroll".into(),
            source: None,
        },
    )
    .await
    .expect_err("denied catalog access");
    assert_eq!(denied.data.unwrap()["code"], "invalid_params");
}

#[tokio::test]
async fn duckdb_catalog_constraints_and_real_explain() {
    let config = config().await;
    for sql in [
        "CREATE SCHEMA analytics",
        "CREATE TABLE analytics.parent (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        "CREATE TABLE analytics.child (x INTEGER DEFAULT 7, y INTEGER, note VARCHAR, FOREIGN KEY (x, y) REFERENCES analytics.parent (a, b))",
        "CREATE INDEX child_note ON analytics.child(note)",
        "INSERT INTO analytics.parent VALUES (7, 8)",
        "INSERT INTO analytics.child VALUES (7, 8, 'hello')",
    ] {
        execute(&config, sql, vec![]).await;
    }
    let driver = config.source(None).unwrap().driver().await.expect("driver");
    // DuckDB marks the default `main` schema internal; it must still surface,
    // while system catalogs must not.
    let schemas: Vec<_> = driver
        .list_schemas()
        .await
        .expect("schemas")
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    assert!(schemas.iter().any(|s| s == "main"), "{schemas:?}");
    assert!(schemas.iter().any(|s| s == "analytics"), "{schemas:?}");
    assert!(
        !schemas.iter().any(|s| s == "information_schema"),
        "{schemas:?}"
    );
    let child = driver
        .describe_table(Some("analytics"), "child")
        .await
        .expect("describe");
    assert_eq!(
        child
            .columns
            .iter()
            .find(|c| c.name == "x")
            .unwrap()
            .default_value
            .as_deref(),
        Some("7")
    );
    let fks = driver
        .list_foreign_keys(Some("analytics"), Some("child"))
        .await
        .expect("FKs");
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0].columns, ["x", "y"]);
    assert_eq!(fks[0].ref_columns, ["a", "b"]);
    assert_eq!(fks[0].ref_schema.as_deref(), Some("analytics"));
    let indexes = driver
        .list_indexes(Some("analytics"), Some("child"))
        .await
        .expect("indexes");
    let index = indexes
        .iter()
        .find(|i| i.name == "child_note")
        .expect("explicit index");
    assert_eq!(index.columns, ["note"]);
    assert!(!index.unique);
    assert!(driver
        .list_tables(Some("main"))
        .await
        .unwrap()
        .iter()
        .all(|t| t.name != "child"));
    for analyze in [false, true] {
        let plan = analyze_query_with_options(
            &driver,
            "SELECT note FROM analytics.child WHERE x = ?",
            &[json!(7)],
            WriteMode::ReadOnly,
            Duration::from_secs(10),
            AnalyzeOptions {
                analyze,
                ..Default::default()
            },
        )
        .await
        .expect("real DuckDB plan");
        assert_eq!(plan.engine, "duckdb");
        assert_eq!(plan.analyze, analyze);
        assert!(
            plan.nodes
                .iter()
                .any(|n| n.relation.as_deref().is_some_and(|r| r.ends_with("child"))),
            "{plan:?}"
        );
        if analyze {
            assert!(
                plan.nodes.iter().any(|n| n.actual_rows == Some(1)),
                "{plan:?}"
            );
        }
    }
}

#[tokio::test]
async fn duckdb_filesystem_allowlist_rejects_outside_and_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let allowed = dir.path().join("allowed");
    std::fs::create_dir(&allowed).unwrap();
    let outside = dir.path().join("outside.duckdb");
    for path in [outside.clone(), allowed.join("..").join("outside.duckdb")] {
        let cli = Cli::try_parse_from([
            "strut-stack-sql",
            "--url",
            &format!("duckdb://{}", path.display()),
            "--duckdb-allowlist",
            allowed.to_str().unwrap(),
        ])
        .unwrap();
        let error = load_config(&cli).await.expect_err("outside allowlist");
        assert!(
            error.to_string().contains("outside the allowlist"),
            "{error}"
        );
    }
    #[cfg(unix)]
    {
        let existing = dir.path().join("existing.duckdb");
        std::fs::write(&existing, []).unwrap();
        let link = allowed.join("link.duckdb");
        std::os::unix::fs::symlink(existing, &link).unwrap();
        let cli = Cli::try_parse_from([
            "strut-stack-sql",
            "--url",
            &format!("duckdb://{}", link.display()),
            "--duckdb-allowlist",
            allowed.to_str().unwrap(),
        ])
        .unwrap();
        assert!(load_config(&cli)
            .await
            .expect_err("symlink escapes allowlist")
            .to_string()
            .contains("outside the allowlist"));
    }
    let inside = allowed.join("inside.duckdb");
    let cli = Cli::try_parse_from([
        "strut-stack-sql",
        "--url",
        &format!("duckdb://{}", inside.display()),
        "--allow-ddl",
        "--duckdb-allowlist",
        allowed.to_str().unwrap(),
    ])
    .unwrap();
    let config = Arc::new(load_config(&cli).await.expect("inside allowlist"));
    execute(&config, "CREATE TABLE persisted (id INTEGER)", vec![]).await;
    execute(&config, "INSERT INTO persisted VALUES (9)", vec![]).await;
    drop(config);
    let cli = Cli::try_parse_from([
        "strut-stack-sql",
        "--url",
        &format!("duckdb://{}", inside.display()),
    ])
    .unwrap();
    let config = Arc::new(load_config(&cli).await.expect("readonly config"));
    let selected = execute(&config, "SELECT id FROM persisted", vec![]).await;
    assert_eq!(selected["data"]["rows"], json!([[9]]));
    let denied = handle_execute_sql(&config, params("DELETE FROM persisted", vec![]))
        .await
        .expect_err("readonly guard");
    assert_eq!(denied.data.unwrap()["code"], "guard_denied");
}

#[tokio::test]
async fn duckdb_and_sqlite_sources_do_not_share_state() {
    let mut config = config().await;
    let sqlite: Arc<dyn EngineDriver> = Arc::new(
        *strut_sql_driver_sqlite::SqliteDriver::from_connection_string("sqlite::memory:")
            .await
            .unwrap(),
    );
    Arc::make_mut(&mut config).sources.insert(
        "sqlite".into(),
        strut_stack_sql::config::ResolvedSource::with_driver(
            "sqlite",
            "sqlite::memory:".into(),
            EngineKind::Sqlite,
            sqlite.clone(),
            false,
        ),
    );
    execute(&config, "CREATE TABLE isolated (id INTEGER)", vec![]).await;
    execute(&config, "INSERT INTO isolated VALUES (17)", vec![]).await;
    sqlite
        .execute(&BoundQuery {
            sql: "CREATE TABLE isolated (id INTEGER)".into(),
            params: vec![],
            row_cap: None,
            fetch_rows: false,
        })
        .await
        .unwrap();
    let mut select = params("SELECT count(*) AS n FROM isolated", vec![]);
    select.source = Some("sqlite".into());
    let sqlite_rows = body(handle_execute_sql(&config, select).await.unwrap());
    assert_eq!(sqlite_rows["data"]["rows"], json!([[0]]));
    assert_eq!(
        execute(&config, "SELECT id FROM isolated", vec![]).await["data"]["rows"],
        json!([[17]])
    );
}
