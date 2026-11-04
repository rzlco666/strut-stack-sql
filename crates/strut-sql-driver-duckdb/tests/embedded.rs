use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bigdecimal::BigDecimal;
use chrono::{NaiveDate, NaiveDateTime};
use strut_sql_driver_api::{BoundQuery, ConnectOptions, DriverError, EngineDriver, SqlValue};
use strut_sql_driver_duckdb::DuckDbDriver;

fn query(sql: &str) -> BoundQuery {
    BoundQuery {
        sql: sql.into(),
        params: vec![],
        row_cap: None,
        fetch_rows: true,
    }
}

async fn memory() -> DuckDbDriver {
    DuckDbDriver::new("duckdb::memory:")
        .await
        .expect("memory database")
}

async fn command(driver: &DuckDbDriver, sql: &str) -> u64 {
    let mut q = query(sql);
    q.fetch_rows = false;
    let result = driver.execute(&q).await.expect("command");
    assert!(result.rows.is_empty());
    assert!(result.cols.is_empty());
    result.meta.rows_affected.expect("command affected count")
}

#[tokio::test]
async fn caps_metadata_and_command_counts() {
    let driver = memory().await;
    assert_eq!(
        command(
            &driver,
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name VARCHAR)"
        )
        .await,
        0
    );
    assert_eq!(
        command(&driver, "INSERT INTO items VALUES (1, 'one'), (2, 'two')").await,
        2
    );
    for (cap, count, truncated) in [(0, 0, true), (1, 1, true), (2, 2, false), (3, 2, false)] {
        let mut q = query("SELECT id, name FROM items ORDER BY id");
        q.row_cap = Some(cap);
        let result = driver.execute(&q).await.expect("capped query");
        assert_eq!(result.meta.row_count, count);
        assert_eq!(result.rows.len(), count);
        assert_eq!(result.meta.truncated, truncated);
        assert_eq!(result.cols[0].name, "id");
        assert_eq!(result.cols[0].type_name, "INTEGER");
        assert_eq!(result.cols[1].type_name, "VARCHAR");
    }
    for cap in [None, Some(0), Some(3)] {
        let mut q = query("SELECT id, name FROM items WHERE FALSE");
        q.row_cap = cap;
        let result = driver.execute(&q).await.expect("empty metadata");
        assert!(result.rows.is_empty());
        assert_eq!(result.cols.len(), 2);
        assert_eq!(result.cols[0].name, "id");
        assert_eq!(result.cols[0].type_name, "INTEGER");
        assert!(!result.meta.truncated);
    }
    assert_eq!(
        command(&driver, "UPDATE items SET name = 'updated' WHERE id = 1").await,
        1
    );
    assert_eq!(command(&driver, "DELETE FROM items WHERE id = 99").await, 0);
    assert_eq!(command(&driver, "DELETE FROM items").await, 2);
    assert_eq!(command(&driver, "DROP TABLE items").await, 0);
    driver.ping().await.expect("persistent connection");
}

#[tokio::test]
async fn typed_parameters_and_lossless_results() {
    let driver = memory().await;
    let decimal = BigDecimal::from_str("12345678901234567890.123456789012345678").unwrap();
    let date = NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
    let timestamp =
        NaiveDateTime::parse_from_str("2024-02-29 12:34:56.123456", "%Y-%m-%d %H:%M:%S%.f")
            .unwrap();
    let params = vec![
        SqlValue::Null,
        SqlValue::Bool(true),
        SqlValue::I64(i64::MIN),
        SqlValue::F64(1.25),
        SqlValue::String("quotes ' \" ; -- and Unicode λ\0 bytes".into()),
        SqlValue::Binary(vec![0, 255, 39, 92]),
        SqlValue::Date(date),
        SqlValue::DateTime(timestamp),
        SqlValue::BigDecimal(decimal.clone()),
        SqlValue::Array(vec![SqlValue::I64(7), SqlValue::Null, SqlValue::I64(8)]),
    ];
    let mut q = query("SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?");
    q.params = params.clone();
    let result = driver.execute(&q).await.expect("typed parameter roundtrip");
    assert_eq!(result.rows, vec![params]);
    assert_eq!(result.cols[8].type_name, "DECIMAL(38,18)");
    let result = driver
        .execute(&query(
            "SELECT \
        170141183460469231731687303715884105727::HUGEINT, \
        340282366920938463463374607431768211455::UHUGEINT, \
        18446744073709551615::UBIGINT, \
        TIME '12:34:56.123456', TIMESTAMP_NS '2024-01-01 00:00:00.123456789', \
        [1::DECIMAL(38,18), 2::DECIMAL(38,18)], [1, 2]::INTEGER[2], \
        '550e8400-e29b-41d4-a716-446655440000'::UUID",
        ))
        .await
        .expect("wide and nested results");
    assert_eq!(
        result.rows[0][0],
        SqlValue::BigDecimal(
            BigDecimal::from_str("170141183460469231731687303715884105727").unwrap()
        )
    );
    assert_eq!(
        result.rows[0][1],
        SqlValue::BigDecimal(
            BigDecimal::from_str("340282366920938463463374607431768211455").unwrap()
        )
    );
    assert_eq!(
        result.rows[0][2],
        SqlValue::BigDecimal(BigDecimal::from_str("18446744073709551615").unwrap())
    );
    assert_eq!(
        result.rows[0][3],
        SqlValue::String("12:34:56.123456".into())
    );
    assert_eq!(
        result.rows[0][4],
        SqlValue::DateTime(
            NaiveDateTime::parse_from_str("2024-01-01 00:00:00.123456789", "%Y-%m-%d %H:%M:%S%.f")
                .unwrap()
        )
    );
    assert_eq!(
        result.rows[0][5],
        SqlValue::Array(vec![
            SqlValue::BigDecimal(1.into()),
            SqlValue::BigDecimal(2.into())
        ])
    );
    assert_eq!(result.cols[6].type_name, "INTEGER[2]");
    assert_eq!(
        result.rows[0][7],
        SqlValue::Uuid(uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap())
    );
}

#[tokio::test]
async fn unsupported_values_are_explicit() {
    let driver = memory().await;
    for sql in [
        "SELECT INTERVAL '1 month'",
        "SELECT {'a': 1}",
        "SELECT map([1], [2])",
        "SELECT TIMESTAMPTZ '2024-01-01 00:00:00+00'",
        "SELECT [1::UHUGEINT]",
        "SELECT 'Infinity'::DOUBLE",
        "SELECT 'infinity'::DATE",
        "SELECT [NULL]",
    ] {
        assert!(
            matches!(
                driver.execute(&query(sql)).await,
                Err(DriverError::Unsupported(_))
            ),
            "{sql}"
        );
    }
    let nulls = driver
        .execute(&query("SELECT NULL, CAST(NULL AS INTEGER)"))
        .await
        .unwrap();
    assert_eq!(nulls.rows, vec![vec![SqlValue::Null, SqlValue::Null]]);
    assert_eq!(nulls.cols[0].type_name, "NULL");
    for value in [
        SqlValue::Uuid(uuid::Uuid::nil()),
        SqlValue::Json(Default::default()),
        SqlValue::Array(vec![]),
        SqlValue::Array(vec![SqlValue::Null]),
        SqlValue::Array(vec![SqlValue::I64(1), SqlValue::F64(1.5)]),
        SqlValue::BigDecimal(BigDecimal::from_str("1e100").unwrap()),
        SqlValue::BigDecimal(BigDecimal::from_str("1e-100").unwrap()),
        SqlValue::DateTime(
            NaiveDateTime::parse_from_str("2024-01-01 00:00:00.123456789", "%Y-%m-%d %H:%M:%S%.f")
                .unwrap(),
        ),
    ] {
        let mut q = query("SELECT ?");
        q.params = vec![value];
        assert!(matches!(
            driver.execute(&q).await,
            Err(DriverError::Unsupported(_))
        ));
    }
    driver.ping().await.unwrap();
}

#[tokio::test]
async fn catalogs_preserve_scope_defaults_and_composite_order() {
    let driver = memory().await;
    command(&driver, "CREATE SCHEMA reporting").await;
    command(
        &driver,
        "CREATE TABLE reporting.parent (b INTEGER, a INTEGER, PRIMARY KEY (b, a))",
    )
    .await;
    command(&driver, "CREATE TABLE reporting.child (x INTEGER, y INTEGER DEFAULT 17, label VARCHAR DEFAULT 'a''b', FOREIGN KEY (y, x) REFERENCES reporting.parent (b, a))").await;
    command(
        &driver,
        "CREATE INDEX child_order ON reporting.child (y, x)",
    )
    .await;
    command(&driver, "CREATE MACRO reporting.twice(v) AS v * 2").await;
    assert!(driver
        .list_schemas()
        .await
        .unwrap()
        .iter()
        .any(|s| s.name == "reporting"));
    assert!(driver.list_tables(None).await.unwrap().is_empty());
    assert_eq!(
        driver.list_tables(Some("reporting")).await.unwrap().len(),
        2
    );
    assert!(driver
        .list_tables(Some("reporting' OR TRUE --"))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        driver.list_columns(Some("reporting")).await.unwrap().len(),
        5
    );
    let table = driver
        .describe_table(Some("reporting"), "child")
        .await
        .unwrap();
    assert_eq!(table.columns[1].default_value.as_deref(), Some("17"));
    assert!(table.columns[2]
        .default_value
        .as_deref()
        .unwrap()
        .contains("a''b"));
    let parent = driver
        .describe_table(Some("reporting"), "parent")
        .await
        .unwrap();
    assert!(parent
        .columns
        .iter()
        .all(|c| c.is_primary_key && !c.nullable));
    let pk = driver
        .list_indexes(Some("reporting"), Some("parent"))
        .await
        .unwrap();
    assert!(pk
        .iter()
        .any(|i| i.primary && i.unique && i.columns == ["b", "a"]));
    let indexes = driver
        .list_indexes(Some("reporting"), Some("child"))
        .await
        .unwrap();
    assert!(indexes
        .iter()
        .any(|i| i.name == "child_order" && i.columns == ["y", "x"]));
    let fks = driver
        .list_foreign_keys(Some("reporting"), Some("child"))
        .await
        .unwrap();
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0].columns, ["y", "x"]);
    assert_eq!(fks[0].ref_columns, ["b", "a"]);
    assert_eq!(fks[0].ref_schema.as_deref(), Some("reporting"));
    assert_eq!(fks[0].ref_table, "parent");
    assert!(driver
        .list_routines()
        .await
        .unwrap()
        .iter()
        .any(|r| r.name == "twice" && r.schema.as_deref() == Some("reporting")));
    assert_eq!(
        driver
            .approximate_row_counts(Some("reporting"))
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        driver.table_stats(Some("reporting")).await.unwrap().len(),
        2
    );
    command(&driver, "SET schema = 'reporting'").await;
    assert_eq!(driver.resolve_schema(None).await.unwrap(), "reporting");
    assert_eq!(driver.resolve_schema(Some("main")).await.unwrap(), "main");
}

#[tokio::test]
async fn quoted_identifiers_and_expression_indexes() {
    let driver = memory().await;
    command(
        &driver,
        "CREATE TABLE \"odd'table\" (\"a,b\" INTEGER, \"say\"\"hi\" INTEGER)",
    )
    .await;
    command(
        &driver,
        "CREATE INDEX \"odd index\" ON \"odd'table\" (\"say\"\"hi\", \"a,b\")",
    )
    .await;
    let indexes = driver.list_indexes(None, Some("odd'table")).await.unwrap();
    assert_eq!(indexes[0].columns, ["say\"hi", "a,b"]);
    assert_eq!(
        driver
            .describe_table(None, "odd'table")
            .await
            .unwrap()
            .columns
            .len(),
        2
    );
    assert!(driver
        .describe_table(None, "odd'table' OR TRUE --")
        .await
        .unwrap()
        .columns
        .is_empty());
    command(
        &driver,
        "CREATE INDEX expression_key ON \"odd'table\" ((\"a,b\" + 1))",
    )
    .await;
    assert!(matches!(
        driver.list_indexes(None, None).await,
        Err(DriverError::Unsupported(_))
    ));
}

#[tokio::test]
async fn read_only_files_and_external_access_restrictions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("embedded.duckdb");
    let url = format!("duckdb://{}", path.display());
    let driver = DuckDbDriver::new(&url).await.unwrap();
    command(&driver, "CREATE TABLE persisted (id INTEGER)").await;
    command(&driver, "INSERT INTO persisted VALUES (1)").await;
    drop(driver);
    let read_only = DuckDbDriver::new_with_options(
        &url,
        &ConnectOptions {
            read_only: true,
            ..ConnectOptions::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        read_only
            .execute(&query("SELECT id FROM persisted"))
            .await
            .unwrap()
            .rows,
        vec![vec![SqlValue::I64(1)]]
    );
    assert!(read_only
        .execute(&query("INSERT INTO persisted VALUES (2)"))
        .await
        .is_err());
    let guarded_memory = DuckDbDriver::new_with_options(
        "duckdb::memory:",
        &ConnectOptions {
            read_only: true,
            ..ConnectOptions::default()
        },
    )
    .await
    .unwrap();
    assert!(guarded_memory
        .capability_notes()
        .iter()
        .any(|n| n.code == "duckdb_memory_read_only"));
    let driver = memory().await;
    let settings = driver.execute(&query("SELECT current_setting('enable_external_access'), current_setting('autoload_known_extensions'), current_setting('autoinstall_known_extensions')")).await.unwrap();
    assert_eq!(
        settings.rows[0],
        vec![
            SqlValue::Bool(false),
            SqlValue::Bool(false),
            SqlValue::Bool(false)
        ]
    );
    let csv = dir.path().join("data.csv");
    std::fs::write(&csv, "id\n1\n").unwrap();
    let mut q = query("SELECT * FROM read_csv(?)");
    q.params = vec![SqlValue::String(csv.display().to_string())];
    assert!(driver.execute(&q).await.is_err());
    assert!(driver
        .execute(&query("SET enable_external_access = true"))
        .await
        .is_err());
    assert!(driver.execute(&query("INSTALL httpfs")).await.is_err());
    assert!(driver.execute(&query("LOAD httpfs")).await.is_err());
    for invalid in [
        "duckdb:",
        "duckdb:file?access_mode=read_write",
        "duckdb:file#fragment",
        "sqlite::memory:",
    ] {
        assert!(matches!(
            DuckDbDriver::new(invalid).await,
            Err(DriverError::InvalidConfig(_))
        ));
    }
}

#[tokio::test]
async fn engine_timeout_recovers_without_stale_interrupts() {
    let driver = DuckDbDriver::new_with_options(
        "duckdb::memory:",
        &ConnectOptions {
            statement_timeout: Duration::from_millis(50),
            ..ConnectOptions::default()
        },
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        driver.execute(&query(
            "SELECT sum(sin(i::DOUBLE)) FROM range(1000000000000) t(i)",
        )),
    )
    .await
    .expect("native interruption should finish promptly");
    assert!(matches!(result, Err(DriverError::Timeout)));
    for _ in 0..5 {
        driver.ping().await.expect("no stale interrupt");
    }
}

#[tokio::test]
async fn dropped_future_interrupts_and_acquire_timeout_is_honored() {
    // connect_timeout also bounds the initial native open, so keep the default
    // (2s) instead of a sub-millisecond budget that cannot cover DuckDB startup.
    let driver = Arc::new(
        DuckDbDriver::new_with_options(
            "duckdb::memory:",
            &ConnectOptions {
                statement_timeout: Duration::from_secs(30),
                ..ConnectOptions::default()
            },
        )
        .await
        .unwrap(),
    );
    let running = driver.clone();
    let task = tokio::spawn(async move {
        running
            .execute(&query(
                "SELECT sum(sin(i::DOUBLE)) FROM range(1000000000000) t(i)",
            ))
            .await
    });
    // Observe ownership instead of assuming the blocking query was scheduled:
    // short pings succeed until the long statement acquires the connection.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(driver.ping().await, Err(DriverError::Timeout)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "long query did not acquire connection"
        );
        tokio::task::yield_now().await;
    }
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match driver.ping().await {
                Ok(()) => break,
                Err(DriverError::Timeout) => tokio::task::yield_now().await,
                Err(error) => panic!("recovery failed: {error}"),
            }
        }
    })
    .await
    .expect("cancelled native query must stop");
    driver.ping().await.unwrap();
}
