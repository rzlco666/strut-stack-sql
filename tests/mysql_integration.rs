//! MySQL integration tests — require a live database.
//!
//! Run with: `MYSQL_DATABASE_URL=mysql://... cargo test --test mysql_integration -- --ignored`

use std::time::Duration;

use sqlx::mysql::MySqlPoolOptions;
use sqlx::Row;
use strut_stack_sql::config::WriteMode;
use strut_stack_sql::db::EngineKind;
use strut_stack_sql::db::{
    describe_table, execute_query, list_foreign_keys, list_indexes, list_tables, EnginePool,
    ExecOptions,
};
use strut_stack_sql::guard::validate_and_prepare;

async fn mysql_pool() -> Option<EnginePool> {
    let url = std::env::var("MYSQL_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()?;
    if !url.to_lowercase().starts_with("mysql://") {
        return None;
    }
    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()?;
    Some(EnginePool::Mysql(pool))
}

fn exec_opts() -> ExecOptions {
    ExecOptions {
        write_mode: WriteMode::ReadOnly,
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
#[ignore = "requires MYSQL_DATABASE_URL or mysql:// DATABASE_URL"]
async fn mysql_describe_table_returns_columns() {
    let pool = mysql_pool()
        .await
        .expect("set MYSQL_DATABASE_URL to a mysql:// DSN");
    let mysql = pool.mysql().unwrap();

    let row = sqlx::query("SELECT DATABASE()")
        .fetch_one(mysql)
        .await
        .expect("DATABASE()");
    let schema: String = row.try_get(0).expect("schema name");

    // CAST to CHAR: on MySQL 8, information_schema name columns are
    // reported with a binary charset under prepared statements, so a typed
    // String decode of the raw column fails with a VARBINARY mismatch.
    let tables = sqlx::query(
        "SELECT CAST(table_name AS CHAR) FROM information_schema.tables \
         WHERE table_schema = ? AND table_type = 'BASE TABLE' LIMIT 1",
    )
    .bind(&schema)
    .fetch_all(mysql)
    .await
    .expect("list tables");
    let table: String = tables[0].try_get(0).expect("table name");

    let raw_rows = sqlx::query(
        "SELECT table_schema, table_name, column_name, data_type, is_nullable \
         FROM information_schema.columns \
         WHERE table_schema = ? AND table_name = ?",
    )
    .bind(&schema)
    .bind(&table)
    .fetch_all(mysql)
    .await
    .expect("raw column query");
    assert!(
        !raw_rows.is_empty(),
        "binding bug: information_schema returned 0 rows for {schema}.{table}"
    );

    let object = describe_table(&pool, None, Some(&schema), &table)
        .await
        .expect("describe_table");
    let columns = object.columns.expect("columns field");
    assert!(
        !columns.is_empty(),
        "describe_table should return columns for {schema}.{table}"
    );
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL or mysql:// DATABASE_URL"]
async fn mysql_describe_table_fw_users_if_present() {
    let pool = mysql_pool()
        .await
        .expect("set MYSQL_DATABASE_URL to a mysql:// DSN");
    let mysql = pool.mysql().unwrap();

    let schema_row = sqlx::query("SELECT DATABASE()")
        .fetch_one(mysql)
        .await
        .expect("DATABASE()");
    let schema: String = schema_row.try_get(0).expect("schema");

    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = ? AND table_name = 'fw_users'",
    )
    .bind(&schema)
    .fetch_one(mysql)
    .await
    .expect("table exists check");

    if exists == 0 {
        return;
    }

    let object = describe_table(&pool, None, Some(&schema), "fw_users")
        .await
        .expect("describe fw_users");
    let columns = object.columns.expect("columns");
    assert!(!columns.is_empty(), "fw_users should have columns");

    if let Some(indexes) = &object.indexes {
        for idx in indexes {
            let has_uni_col = columns.iter().any(|c| {
                c.key.as_deref() == Some("UNI")
                    && idx
                        .columns
                        .iter()
                        .any(|ic| ic.eq_ignore_ascii_case(&c.name))
            });
            if has_uni_col {
                assert!(
                    idx.unique,
                    "index {} covering UNI column must report unique: true",
                    idx.name
                );
            }
        }
    }

    let qualified = describe_table(&pool, None, None, &format!("{schema}.fw_users"))
        .await
        .expect("describe qualified table");
    assert!(
        !qualified.columns.unwrap().is_empty(),
        "schema.table form should work"
    );
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL or mysql:// DATABASE_URL"]
async fn mysql_count_serializes_as_number_not_bool() {
    let pool = mysql_pool()
        .await
        .expect("set MYSQL_DATABASE_URL to a mysql:// DSN");

    for sql in [
        "SELECT COUNT(*) AS c FROM (SELECT 1 AS x) t",
        "SELECT 0 AS c",
    ] {
        let result = execute_query(&pool, sql, &[], &exec_opts())
            .await
            .expect("execute")
            .data
            .expect("data");
        let value = &result.rows[0][0];
        assert!(
            value.is_number(),
            "expected numeric JSON for `{sql}`, got {value}"
        );
    }
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL or mysql:// DATABASE_URL"]
async fn mysql_show_processlist_allowed_and_runs() {
    validate_and_prepare(
        "SHOW PROCESSLIST",
        &[],
        EngineKind::Mysql,
        WriteMode::ReadOnly,
        100,
    )
    .expect("guard should allow SHOW PROCESSLIST");

    let pool = mysql_pool()
        .await
        .expect("set MYSQL_DATABASE_URL to a mysql:// DSN");

    let result = execute_query(&pool, "SHOW PROCESSLIST", &[], &exec_opts())
        .await
        .expect("execute");
    assert!(result.ok, "SHOW PROCESSLIST should succeed");
    assert!(result.data.is_some());
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL or mysql:// DATABASE_URL"]
async fn mysql_information_schema_aggregates_serialize_as_numbers() {
    let pool = mysql_pool()
        .await
        .expect("set MYSQL_DATABASE_URL to a mysql:// DSN");

    let sql = "SELECT COUNT(*) AS total_tables, SUM(table_rows) AS approx_rows, \
               ROUND(SUM(data_length)/1024/1024,1) AS data_mb \
               FROM information_schema.tables \
               WHERE table_schema = DATABASE() AND table_type='BASE TABLE'";

    let result = execute_query(&pool, sql, &[], &exec_opts())
        .await
        .expect("execute")
        .data
        .expect("data");

    assert_eq!(result.rows.len(), 1);
    let row = &result.rows[0];
    assert!(
        row[0].is_number(),
        "COUNT(*) should be number, got {}",
        row[0]
    );
    assert!(
        row[1].is_number(),
        "SUM(table_rows) should be number, got {}",
        row[1]
    );
    assert!(
        row[2].is_number(),
        "ROUND(...) should be number, got {}",
        row[2]
    );

    let top_sql = "SELECT table_name, table_rows, \
                   ROUND((data_length+index_length)/1024/1024,1) AS size_mb \
                   FROM information_schema.tables WHERE table_schema = DATABASE() \
                   ORDER BY (data_length+index_length) DESC LIMIT 5";

    let top = execute_query(&pool, top_sql, &[], &exec_opts())
        .await
        .expect("execute top tables")
        .data
        .expect("data");

    for (i, row) in top.rows.iter().enumerate() {
        assert!(
            row[1].is_number() || row[1].is_null(),
            "row {i} table_rows: {}",
            row[1]
        );
        assert!(
            row[2].is_number() || row[2].is_null(),
            "row {i} size_mb: {}",
            row[2]
        );
    }
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL or mysql:// DATABASE_URL"]
async fn mysql_parameterized_select_binds_value() {
    let pool = mysql_pool()
        .await
        .expect("set MYSQL_DATABASE_URL to a mysql:// DSN");

    let result = execute_query(
        &pool,
        "SELECT ? AS v",
        &[serde_json::json!(42)],
        &exec_opts(),
    )
    .await
    .expect("execute")
    .data
    .expect("data");
    assert_eq!(result.rows[0][0], serde_json::json!(42));
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL or mysql:// DATABASE_URL"]
async fn mysql_text_values_decode_as_readable_strings_not_hex() {
    let pool = mysql_pool()
        .await
        .expect("set MYSQL_DATABASE_URL to a mysql:// DSN");

    let result = execute_query(
        &pool,
        "SELECT DATABASE() AS db, 'edoc_pid_dev' AS literal, CAST('hello' AS CHAR) AS as_char",
        &[],
        &exec_opts(),
    )
    .await
    .expect("execute")
    .data
    .expect("data");

    let row = &result.rows[0];
    let db = row[0].as_str().expect("DATABASE() string");
    assert!(
        !db.is_empty() && !db.chars().all(|c| c.is_ascii_hexdigit()),
        "DATABASE() must be readable UTF-8, not hex: {db}"
    );
    assert_eq!(row[1].as_str(), Some("edoc_pid_dev"));
    assert_eq!(row[2].as_str(), Some("hello"));
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL or mysql:// DATABASE_URL"]
async fn mysql_binary_values_stay_hex_encoded() {
    let pool = mysql_pool()
        .await
        .expect("set MYSQL_DATABASE_URL to a mysql:// DSN");

    let result = execute_query(
        &pool,
        "SELECT CAST('xy' AS BINARY(2)) AS b, UNHEX('65646f63') AS hx",
        &[],
        &exec_opts(),
    )
    .await
    .expect("execute")
    .data
    .expect("data");

    let row = &result.rows[0];
    assert_eq!(row[0].as_str(), Some("7879"));
    assert_eq!(row[1].as_str(), Some("65646f63"));
}

/// Regression: on MySQL 8, information_schema name columns are reported
/// with a binary charset under prepared statements, so typed String
/// decoding fails. The product schema listing must decode them tolerantly
/// (bytes -> UTF-8) — a typed decode used to make list_tables return an
/// empty list and index/FK extraction return empty names, silently.
#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL"]
async fn mysql_schema_names_decode_from_information_schema() {
    let pool = mysql_pool()
        .await
        .expect("set MYSQL_DATABASE_URL to a mysql:// URL");
    let mysql = pool.mysql().unwrap();

    let row = sqlx::query("SELECT DATABASE()")
        .fetch_one(mysql)
        .await
        .expect("DATABASE()");
    let schema: String = row.try_get(0).expect("schema name");

    let tables = list_tables(&pool, None, Some(&schema), None)
        .await
        .expect("list_tables");
    assert!(
        tables.len() >= 3,
        "seed defines users/products/orders, got {} tables",
        tables.len()
    );
    assert!(
        tables.iter().all(|t| !t.name.is_empty()),
        "table names must decode, not come back empty: {tables:?}"
    );
    assert!(
        tables.iter().any(|t| t.name == "orders"),
        "orders table must be listed: {tables:?}"
    );

    let indexes = list_indexes(&pool, None, Some(&schema), Some("orders"), None)
        .await
        .expect("list_indexes");
    assert!(
        !indexes.is_empty(),
        "orders has a PRIMARY key and two named indexes"
    );
    assert!(
        indexes.iter().all(|i| !i.name.is_empty()),
        "index names must decode, not come back empty: {indexes:?}"
    );
    assert!(
        indexes
            .iter()
            .any(|i| i.name.eq_ignore_ascii_case("idx_orders_user_id")),
        "named index must be listed: {indexes:?}"
    );

    let fks = list_foreign_keys(&pool, None, Some(&schema), Some("orders"))
        .await
        .expect("list_foreign_keys");
    assert_eq!(fks.len(), 2, "orders has two foreign keys: {fks:?}");
    assert!(
        fks.iter()
            .all(|fk| !fk.name.is_empty() && !fk.columns.iter().any(|c| c.is_empty())),
        "FK names/columns must decode, not come back empty: {fks:?}"
    );
    assert!(
        fks.iter().any(|fk| fk.ref_table == "users"),
        "orders.user_id must reference users: {fks:?}"
    );
}
