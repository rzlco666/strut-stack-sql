//! ClickHouse integration tests.
//!
//! Gated on `CLICKHOUSE_URL` (for example
//! `clickhouse://default:@127.0.0.1:8123/default`) and marked `#[ignore]`,
//! matching the PostgreSQL/MySQL integration conventions.
//!
//! The shared `strut-sql-driver-conformance` harness is deliberately NOT used:
//! its fixture is `CREATE TABLE ... (id INTEGER PRIMARY KEY, name TEXT,
//! value REAL)`, which ClickHouse rejects (no `PRIMARY KEY` constraint, no
//! `REAL`, and every table needs an `ENGINE`). The assertions below mirror
//! that harness with ClickHouse-legal DDL instead of weakening either one.

use std::env;

use strut_sql_driver_api::{
    BoundQuery, ConnectOptions, DriverError, EngineDriver, EngineKind, SqlValue,
};
use strut_sql_driver_clickhouse::ClickHouseDriver;

fn url() -> Option<String> {
    env::var("CLICKHOUSE_URL").ok().filter(|u| !u.is_empty())
}

fn bound(sql: &str, params: Vec<SqlValue>, row_cap: Option<usize>) -> BoundQuery {
    BoundQuery {
        sql: sql.to_string(),
        params,
        row_cap,
        fetch_rows: true,
    }
}

async fn connect() -> ClickHouseDriver {
    let url = url().expect("CLICKHOUSE_URL must be set");
    ClickHouseDriver::new(&url).await.expect("connect")
}

#[tokio::test]
#[ignore = "requires CLICKHOUSE_URL pointing at a ClickHouse HTTP endpoint"]
async fn clickhouse_conformance() {
    let Some(_) = url() else {
        eprintln!("skipping: CLICKHOUSE_URL not set");
        return;
    };
    let driver = connect().await;
    driver.ping().await.expect("ping");

    driver
        .execute(&bound(
            "DROP TABLE IF EXISTS conformance_test",
            vec![],
            None,
        ))
        .await
        .expect("drop leftovers");
    driver
        .execute(&bound(
            "CREATE TABLE conformance_test (id Int64, name String, value Float64) \
             ENGINE = MergeTree ORDER BY id",
            vec![],
            None,
        ))
        .await
        .expect("create table");

    let insert_sql = "INSERT INTO conformance_test (id, name, value) VALUES (?, ?, ?)";
    driver
        .execute(&bound(
            insert_sql,
            vec![
                SqlValue::I64(1),
                SqlValue::String("test".to_string()),
                SqlValue::F64(2.75),
            ],
            None,
        ))
        .await
        .expect("insert first row");
    driver
        .execute(&bound(
            insert_sql,
            vec![
                SqlValue::I64(2),
                SqlValue::String("second".to_string()),
                SqlValue::F64(2.5),
            ],
            None,
        ))
        .await
        .expect("insert second row");

    let result = driver
        .execute(&bound(
            "SELECT id, name, value FROM conformance_test WHERE id = ? ORDER BY id",
            vec![SqlValue::I64(1)],
            None,
        ))
        .await
        .expect("select back");
    assert_eq!(result.rows.len(), 1, "expected exactly one row");
    assert_eq!(result.meta.row_count, 1);
    assert_eq!(
        result.rows[0],
        vec![
            SqlValue::I64(1),
            SqlValue::String("test".to_string()),
            SqlValue::F64(2.75)
        ]
    );
    assert_eq!(result.cols[0].type_name, "Int64");

    // Row cap: two rows exist, a cap of one must truncate and say so.
    let capped = driver
        .execute(&bound(
            "SELECT id FROM conformance_test ORDER BY id",
            vec![],
            Some(1),
        ))
        .await
        .expect("capped select");
    assert_eq!(capped.rows.len(), 1);
    assert_eq!(capped.meta.row_count, 1);
    assert!(
        capped.meta.truncated,
        "row cap must mark the result truncated"
    );

    let tables = driver.list_tables(None).await.expect("list tables");
    assert!(
        tables.iter().any(|t| t.name == "conformance_test"),
        "list_tables should contain conformance_test: {tables:?}"
    );

    let found = driver
        .search_objects(
            Some("conformance"),
            Some(strut_sql_driver_api::ObjectType::Table),
        )
        .await
        .expect("search");
    assert!(found.iter().any(|t| t.name == "conformance_test"));

    let info = driver
        .describe_table(None, "conformance_test")
        .await
        .expect("describe table");
    assert_eq!(info.name, "conformance_test");
    assert_eq!(info.columns.len(), 3);
    let id = info
        .columns
        .iter()
        .find(|c| c.name == "id")
        .expect("id column");
    assert!(id.is_primary_key, "id is the sorting key");
    assert_eq!(id.data_type, "Int64");
    let name = info
        .columns
        .iter()
        .find(|c| c.name == "name")
        .expect("name column");
    assert!(!name.is_primary_key);

    // ClickHouse has no unique indexes: the primary key is reported, with
    // `unique = false`.
    let indexes = driver
        .list_indexes(None, Some("conformance_test"))
        .await
        .expect("list indexes");
    let primary = indexes
        .iter()
        .find(|i| i.name == "PRIMARY")
        .expect("PRIMARY index");
    assert_eq!(primary.columns, vec!["id".to_string()]);
    assert!(primary.primary);
    assert!(!primary.unique);

    let stats = driver.table_stats(None).await.expect("table stats");
    let table = stats
        .iter()
        .find(|s| s.table == "conformance_test")
        .expect("stats for conformance_test");
    assert!(table.approx_rows.is_some());

    assert!(driver
        .list_foreign_keys(None, None)
        .await
        .expect("foreign keys")
        .is_empty());

    driver
        .execute(&bound("DROP TABLE conformance_test", vec![], None))
        .await
        .expect("drop table");
}

#[tokio::test]
#[ignore = "requires CLICKHOUSE_URL pointing at a ClickHouse HTTP endpoint"]
async fn clickhouse_typed_values_round_trip() {
    let Some(_) = url() else {
        eprintln!("skipping: CLICKHOUSE_URL not set");
        return;
    };
    let driver = connect().await;
    driver
        .execute(&bound("DROP TABLE IF EXISTS types_test", vec![], None))
        .await
        .expect("drop leftovers");
    driver
        .execute(&bound(
            "CREATE TABLE types_test (\
               id Int64, d Date, ts DateTime64(3), dec Decimal(18, 2), u UUID, \
               arr Array(Int64), n Nullable(String)\
             ) ENGINE = MergeTree ORDER BY id",
            vec![],
            None,
        ))
        .await
        .expect("create table");

    let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
    let ts = chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
        .unwrap()
        .and_hms_milli_opt(3, 4, 5, 123)
        .unwrap();
    let uuid = uuid::Uuid::parse_str("61f0c404-5cb3-11e7-907b-a6006ad3dba0").unwrap();
    driver
        .execute(&bound(
            "INSERT INTO types_test (id, d, ts, dec, u, arr, n) VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                SqlValue::I64(1),
                SqlValue::Date(date),
                SqlValue::DateTime(ts),
                SqlValue::BigDecimal("1.25".parse().unwrap()),
                SqlValue::Uuid(uuid),
                SqlValue::Array(vec![SqlValue::I64(1), SqlValue::I64(2)]),
                SqlValue::Null,
            ],
            None,
        ))
        .await
        .expect("insert typed row");

    let result = driver
        .execute(&bound(
            "SELECT id, d, ts, dec, u, arr, n FROM types_test WHERE id = ?",
            vec![SqlValue::I64(1)],
            None,
        ))
        .await
        .expect("select typed row");
    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    assert_eq!(row[0], SqlValue::I64(1));
    assert_eq!(row[1], SqlValue::Date(date));
    assert_eq!(row[2], SqlValue::DateTime(ts));
    assert_eq!(
        row[3],
        SqlValue::BigDecimal("1.25".parse().unwrap()),
        "decimal must stay exact"
    );
    assert_eq!(row[4], SqlValue::Uuid(uuid));
    assert_eq!(
        row[5],
        SqlValue::Array(vec![SqlValue::I64(1), SqlValue::I64(2)])
    );
    assert_eq!(row[6], SqlValue::Null);

    driver
        .execute(&bound("DROP TABLE types_test", vec![], None))
        .await
        .expect("drop table");
}

#[tokio::test]
#[ignore = "requires CLICKHOUSE_URL pointing at a ClickHouse HTTP endpoint"]
async fn clickhouse_read_only_is_enforced_by_the_server() {
    let Some(url) = url() else {
        eprintln!("skipping: CLICKHOUSE_URL not set");
        return;
    };
    let options = ConnectOptions {
        read_only: true,
        ..ConnectOptions::default()
    };
    let driver = ClickHouseDriver::new_with_options(&url, &options)
        .await
        .expect("connect read-only");
    assert_eq!(driver.kind(), EngineKind::ClickHouse);

    // The write never reaches the table: ClickHouse rejects it because the
    // session carries `readonly=1`.
    let error = driver
        .execute(&bound(
            "CREATE TABLE readonly_probe (id Int64) ENGINE = Memory",
            vec![],
            None,
        ))
        .await
        .expect_err("write must fail on a read-only source");
    let message = error.to_string();
    assert!(
        message.to_lowercase().contains("readonly"),
        "expected a READONLY error, got: {message}"
    );

    // Reads still work on the same connection.
    driver.ping().await.expect("ping read-only");
}

#[tokio::test]
#[ignore = "requires CLICKHOUSE_URL pointing at a ClickHouse HTTP endpoint"]
async fn clickhouse_statement_timeout_maps_to_timeout() {
    let Some(url) = url() else {
        eprintln!("skipping: CLICKHOUSE_URL not set");
        return;
    };
    let options = ConnectOptions {
        statement_timeout: std::time::Duration::from_secs(1),
        ..ConnectOptions::default()
    };
    let driver = ClickHouseDriver::new_with_options(&url, &options)
        .await
        .expect("connect");
    let error = driver
        .execute(&bound("SELECT sleep(3)", vec![], None))
        .await
        .expect_err("sleep must hit max_execution_time");
    assert!(
        matches!(error, DriverError::Timeout),
        "expected Timeout, got: {error:?}"
    );
}

#[tokio::test]
#[ignore = "requires CLICKHOUSE_URL pointing at a ClickHouse HTTP endpoint"]
async fn clickhouse_error_maps_to_execution() {
    let Some(_) = url() else {
        eprintln!("skipping: CLICKHOUSE_URL not set");
        return;
    };
    let driver = connect().await;
    let error = driver
        .execute(&bound("SELECT * FROM no_such_table_r3", vec![], None))
        .await
        .expect_err("unknown table must fail");
    assert!(
        matches!(error, DriverError::Execution(ref m) if m.contains("UNKNOWN_TABLE") || m.contains("Unknown table")),
        "expected an execution error, got: {error:?}"
    );
}
