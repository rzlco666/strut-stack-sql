//! Live SQL Server behavior tests. Use only a disposable database.
//! SQLSERVER_DATABASE_URL=sqlserver://... cargo +1.99 test --test sqlserver_integration -- --ignored

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use strut_sql_driver_api::{BoundQuery, EngineDriver, ObjectType, SqlValue};
use strut_sql_driver_sqlserver::SqlServerDriver;
use strut_sql_guard::{MaskMode, RowFilter, TablePolicy, TableRule, WriteMode};
use strut_stack_sql::tools::exec::{execute_keyset_page, execute_query, ExecError, ExecOptions};
use strut_stack_sql::tools::explain::{analyze_query_with_options, AnalyzeOptions};

async fn driver() -> Arc<dyn EngineDriver> {
    let url = std::env::var("SQLSERVER_DATABASE_URL")
        .expect("SQLSERVER_DATABASE_URL must point to a disposable SQL Server database");
    Arc::new(
        *SqlServerDriver::from_connection_string(&url)
            .await
            .expect("SQL Server connection"),
    )
}

async fn raw(driver: &Arc<dyn EngineDriver>, sql: &str) {
    driver
        .execute(&BoundQuery {
            sql: sql.into(),
            params: vec![],
            row_cap: None,
            fetch_rows: false,
        })
        .await
        .expect("fixture statement");
}

fn options() -> ExecOptions {
    ExecOptions {
        write_mode: WriteMode::ReadOnly,
        max_rows: 100,
        max_bytes: 64 * 1024,
        timeout: Duration::from_secs(10),
        limit_injected: false,
        page_offset: 0,
        page_size: None,
        deny_columns: vec![],
        masks: vec![],
        table_policy: None,
        principal: "sqlserver-test".into(),
        source: "default".into(),
        trace_sql: false,
        limiter: None,
    }
}

#[tokio::test]
#[ignore = "requires SQLSERVER_DATABASE_URL on a disposable SQL Server"]
async fn sqlserver_policy_masks_and_pages_preserve_bound_predicates() {
    let driver = driver().await;
    raw(&driver, "DROP TABLE IF EXISTS dbo.r3_policy").await;
    raw(
        &driver,
        "CREATE TABLE dbo.r3_policy (id int PRIMARY KEY, tenant int, email nvarchar(100))",
    )
    .await;
    raw(&driver, "INSERT INTO dbo.r3_policy VALUES (1,7,N'alice@example.com'),(2,9,N'other@example.com'),(3,7,N'bob@example.com'),(4,7,N'carol@example.com')").await;
    let mut opts = options();
    opts.table_policy = Some(TablePolicy {
        row_filters: vec![RowFilter {
            schema: Some("dbo".into()),
            table: "r3_policy".into(),
            predicate: "tenant = 7".into(),
        }],
        ..Default::default()
    });
    opts.masks = vec![("email".into(), MaskMode::Partial)];
    opts.page_offset = 1;
    opts.page_size = Some(1);
    let result = execute_query(
        &driver,
        "SELECT p.id, p.email FROM dbo.r3_policy AS p WHERE p.id > ? ORDER BY p.id",
        &[json!(0)],
        &opts,
    )
    .await
    .expect("filtered offset page")
    .data
    .expect("data");
    assert_eq!(result.rows, vec![vec![json!(3), json!("b***@example.com")]]);
    assert_eq!(result.meta.has_more, Some(true));

    opts.page_offset = 0;
    opts.page_size = None;
    let empty = execute_query(
        &driver,
        "SELECT id FROM dbo.r3_policy WHERE tenant = ?",
        &[json!(9)],
        &opts,
    )
    .await
    .expect("AND semantics")
    .data
    .expect("data");
    assert!(
        empty.rows.is_empty(),
        "caller predicate must not replace tenant policy"
    );

    opts.table_policy = Some(TablePolicy {
        deny: vec![TableRule {
            schema: Some("dbo".into()),
            table: "r3_policy".into(),
        }],
        ..Default::default()
    });
    assert!(
        execute_query(&driver, "SELECT * FROM dbo.r3_policy", &[], &opts)
            .await
            .is_err()
    );
    let mut cursor_opts = options();
    cursor_opts.masks = opts.masks.clone();
    let cursor_sql = "SELECT id, email FROM dbo.r3_policy WHERE tenant = ? ORDER BY id";
    let first = execute_keyset_page(&driver, cursor_sql, &[json!(7)], &cursor_opts, None, 1)
        .await
        .expect("first cursor page");
    assert_eq!(first.next_key, Some(json!(1)));
    raw(&driver, "DELETE FROM dbo.r3_policy WHERE id = 1").await;
    raw(
        &driver,
        "INSERT INTO dbo.r3_policy VALUES (5,7,N'dana@example.com')",
    )
    .await;
    let second = execute_keyset_page(
        &driver,
        cursor_sql,
        &[json!(7)],
        &cursor_opts,
        first.next_key,
        1,
    )
    .await
    .expect("next cursor page retains original parameter");
    assert_eq!(
        second.result.data.expect("page data").rows,
        vec![vec![json!(3), json!("b***@example.com")]]
    );
    assert!(second.has_more);
    assert!(
        execute_query(&driver, "DELETE FROM dbo.r3_policy", &[], &options())
            .await
            .is_err()
    );
    raw(&driver, "DROP TABLE dbo.r3_policy").await;
}

#[tokio::test]
#[ignore = "requires SQLSERVER_DATABASE_URL on a disposable SQL Server"]
async fn sqlserver_catalog_preserves_composite_keys_and_literal_filters() {
    let driver = driver().await;
    raw(&driver, "DROP TABLE IF EXISTS dbo.r3_child").await;
    raw(&driver, "DROP TABLE IF EXISTS dbo.r3_parent").await;
    raw(&driver, "CREATE TABLE dbo.r3_parent (a int NOT NULL, b int NOT NULL, CONSTRAINT PK_r3_parent PRIMARY KEY (b,a))").await;
    raw(&driver, "CREATE TABLE dbo.r3_child (id int PRIMARY KEY, parent_a int, parent_b int, label nvarchar(80) NULL DEFAULT N'new', CONSTRAINT FK_r3_parent FOREIGN KEY (parent_b,parent_a) REFERENCES dbo.r3_parent(b,a) ON DELETE CASCADE)").await;
    raw(
        &driver,
        "CREATE INDEX IX_r3_child_pair ON dbo.r3_child(parent_b,parent_a) INCLUDE(label)",
    )
    .await;
    raw(&driver, "INSERT INTO dbo.r3_parent VALUES (1,2)").await;
    raw(
        &driver,
        "INSERT INTO dbo.r3_child (id,parent_a,parent_b) VALUES (1,1,2)",
    )
    .await;
    let mut merge_options = options();
    merge_options.write_mode = WriteMode::AllowWrites;
    let merged = execute_query(
        &driver,
        "MERGE INTO dbo.r3_child AS t USING dbo.r3_parent AS s ON t.parent_a=s.a AND t.parent_b=s.b WHEN MATCHED THEN UPDATE SET label=N'merged' -- sync",
        &[],
        &merge_options,
    ).await.expect("trailing comment cannot swallow the MERGE terminator");
    assert_eq!(
        merged.data.expect("MERGE metadata").meta.rows_affected,
        Some(1)
    );
    let label = execute_query(
        &driver,
        "SELECT label FROM dbo.r3_child WHERE id=1",
        &[],
        &options(),
    )
    .await
    .expect("merged row")
    .data
    .expect("data");
    assert_eq!(label.rows, vec![vec![json!("merged")]]);
    let table = driver
        .describe_table(Some("dbo"), "r3_child")
        .await
        .expect("describe");
    let id = table.columns.iter().find(|c| c.name == "id").expect("id");
    assert!(id.is_primary_key);
    assert!(!id.nullable);
    let label = table
        .columns
        .iter()
        .find(|c| c.name == "label")
        .expect("label");
    assert!(label.nullable);
    assert!(label
        .default_value
        .as_deref()
        .is_some_and(|v| v.contains("new")));
    let indexes = driver
        .list_indexes(Some("dbo"), Some("r3_child"))
        .await
        .expect("indexes");
    let pair = indexes
        .iter()
        .find(|i| i.name == "IX_r3_child_pair")
        .expect("pair index");
    assert_eq!(pair.columns, ["parent_b", "parent_a"]);
    let fks = driver
        .list_foreign_keys(Some("dbo"), Some("r3_child"))
        .await
        .expect("foreign keys");
    let fk = fks
        .iter()
        .find(|fk| fk.name == "FK_r3_parent")
        .expect("composite FK");
    assert_eq!(fk.columns, ["parent_b", "parent_a"]);
    assert_eq!(fk.ref_columns, ["b", "a"]);
    assert_eq!(fk.ref_schema.as_deref(), Some("dbo"));
    assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
    assert!(driver
        .list_tables(Some("dbo' OR 1=1--"))
        .await
        .expect("literal schema filter")
        .is_empty());
    assert!(driver
        .list_indexes(Some("dbo"), Some("r3_child' OR 1=1--"))
        .await
        .expect("literal table filter")
        .is_empty());
    let found = driver
        .search_objects(Some("r3_child"), Some(ObjectType::Table))
        .await
        .expect("search");
    assert!(found
        .iter()
        .any(|o| o.name == "r3_child" && o.schema.as_deref() == Some("dbo")));
    raw(&driver, "DROP TABLE dbo.r3_child").await;
    raw(&driver, "DROP TABLE dbo.r3_parent").await;
}

#[tokio::test]
#[ignore = "requires SQLSERVER_DATABASE_URL on a disposable SQL Server"]
async fn sqlserver_estimated_and_actual_plans_leave_pool_usable() {
    let driver = driver().await;
    raw(&driver, "DROP TABLE IF EXISTS dbo.r3_plan").await;
    raw(
        &driver,
        "CREATE TABLE dbo.r3_plan (id int PRIMARY KEY, label nvarchar(30))",
    )
    .await;
    raw(
        &driver,
        "INSERT INTO dbo.r3_plan VALUES (1,N'one'),(2,N'two')",
    )
    .await;
    for analyze in [false, true] {
        let summary = analyze_query_with_options(
            &driver,
            "SELECT label FROM dbo.r3_plan WHERE id = ?",
            &[json!(1)],
            WriteMode::AllowWrites,
            Duration::from_secs(10),
            AnalyzeOptions {
                analyze,
                ..Default::default()
            },
        )
        .await
        .expect("SQL Server XML plan");
        assert_eq!(summary.engine, "sqlserver");
        assert_eq!(summary.analyze, analyze);
        assert!(
            summary.nodes.iter().any(|n| n.node_type.contains("Seek")),
            "{summary:?}"
        );
        if analyze {
            assert!(
                summary.nodes.iter().any(|n| n.actual_rows == Some(1)),
                "{summary:?}"
            );
        }
        let after = driver
            .execute(&BoundQuery {
                sql: "SELECT CAST(17 AS int) AS alive".into(),
                params: vec![],
                row_cap: None,
                fetch_rows: true,
            })
            .await
            .expect("plan options reset");
        assert_eq!(after.rows, vec![vec![SqlValue::I64(17)]]);
    }
    assert!(
        analyze_query_with_options(
            &driver,
            "SELECT id FROM dbo.r3_plan",
            &[],
            WriteMode::ReadOnly,
            Duration::from_secs(10),
            AnalyzeOptions {
                analyze: true,
                ..Default::default()
            },
        )
        .await
        .is_err(),
        "actual plans must not execute in read-only mode"
    );
    raw(&driver, "DROP TABLE dbo.r3_plan").await;
}

#[tokio::test]
#[ignore = "requires SQLSERVER_DATABASE_URL on a disposable SQL Server"]
async fn sqlserver_quoted_names_cannot_bypass_table_policy() {
    let driver = driver().await;
    raw(&driver, "DROP TABLE IF EXISTS dbo.[r3.ledger]]raw]").await;
    raw(
        &driver,
        "CREATE TABLE dbo.[r3.ledger]]raw] (id int PRIMARY KEY, tenant int)",
    )
    .await;
    raw(
        &driver,
        "INSERT INTO dbo.[r3.ledger]]raw] VALUES (1,7),(2,9)",
    )
    .await;
    let mut opts = options();
    opts.table_policy = Some(TablePolicy {
        deny: vec![TableRule {
            schema: Some("dbo".into()),
            table: "r3.ledger]raw".into(),
        }],
        ..Default::default()
    });
    let denied = execute_query(&driver, "SELECT id FROM dbo.[r3.ledger]]raw]", &[], &opts).await;
    assert!(matches!(denied, Err(ExecError::Guard(_))), "{denied:?}");

    let database = driver
        .execute(&BoundQuery {
            sql: "SELECT DB_NAME()".into(),
            params: vec![],
            row_cap: None,
            fetch_rows: true,
        })
        .await
        .expect("database name");
    let SqlValue::String(database) = &database.rows[0][0] else {
        panic!("database name must be text");
    };
    let qualified = format!(
        "SELECT id FROM [{}].[dbo].[r3.ledger]]raw]",
        database.replace(']', "]]"),
    );
    let denied = execute_query(&driver, &qualified, &[], &opts).await;
    assert!(matches!(denied, Err(ExecError::Guard(_))), "{denied:?}");

    opts.table_policy = Some(TablePolicy {
        row_filters: vec![RowFilter {
            schema: Some("dbo".into()),
            table: "r3.ledger]raw".into(),
            predicate: "tenant = 7".into(),
        }],
        ..Default::default()
    });
    let filtered = execute_query(
        &driver,
        "SELECT id FROM dbo.[r3.ledger]]raw] ORDER BY id",
        &[],
        &opts,
    )
    .await
    .expect("quoted row policy")
    .data
    .expect("data");
    assert_eq!(filtered.rows, vec![vec![json!(1)]]);
    let cursor_sql = "SELECT id FROM dbo.[r3.ledger]]raw] ORDER BY id";
    let shape =
        strut_sql_guard::keyset_shape(cursor_sql, strut_sql_driver_api::EngineKind::SqlServer)
            .expect("quoted cursor shape");
    strut_stack_sql::tools::keyset::ensure_unique_order_column(&driver, &shape)
        .await
        .expect("catalog lookup uses the exact quoted table name");
    let page = execute_keyset_page(&driver, cursor_sql, &[], &opts, None, 1)
        .await
        .expect("quoted policy cursor page");
    assert_eq!(
        page.result.data.expect("cursor data").rows,
        vec![vec![json!(1)]]
    );
    assert!(!page.has_more);
    raw(&driver, "DROP TABLE dbo.[r3.ledger]]raw]").await;
}

#[tokio::test]
#[ignore = "requires SQLSERVER_DATABASE_URL on a disposable SQL Server"]
async fn sqlserver_xml_set_bounds_and_money_literals_preserve_values() {
    let driver = driver().await;
    let mut opts = options();
    opts.max_rows = 1;
    let xml = execute_query(
        &driver,
        "SELECT CAST(N'<a/>' AS xml) AS payload UNION ALL SELECT CAST(N'<b/>' AS xml)",
        &[],
        &opts,
    )
    .await
    .expect("XML set query needs no sortable result column")
    .data
    .expect("data");
    assert_eq!(xml.rows.len(), 1);
    let payload = xml.rows[0][0]
        .as_str()
        .expect("XML string")
        .replace(' ', "");
    assert!(["<a/>", "<b/>"].contains(&payload.as_str()), "{payload}");
    let money = execute_query(&driver, "SELECT $12.34 AS amount", &[], &options())
        .await
        .expect("money literal")
        .data
        .expect("data");
    assert!((money.rows[0][0].as_f64().expect("money number") - 12.34).abs() < 0.000001);
}
