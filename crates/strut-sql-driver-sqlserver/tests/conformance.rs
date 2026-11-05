use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{NaiveDate, NaiveDateTime};
use std::env;
use std::str::FromStr;
use std::time::Duration;
use strut_sql_driver_api::{
    BoundQuery, ConnectOptions, DriverError, EngineDriver, ObjectType, SqlValue,
};
use strut_sql_driver_conformance::{run_conformance, DriverFactory};
use strut_sql_driver_sqlserver::SqlServerDriver;

struct SqlServerTestFactory {
    url: String,
}

#[async_trait]
impl DriverFactory for SqlServerTestFactory {
    async fn create(
        &self,
    ) -> Result<Box<dyn EngineDriver>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Box::new(SqlServerDriver::new(&self.url).await?))
    }
}

fn url() -> String {
    let url = env::var("SQLSERVER_DATABASE_URL")
        .or_else(|_| env::var("DATABASE_URL"))
        .expect("SQLSERVER_DATABASE_URL or DATABASE_URL must be set");
    assert!(
        url.starts_with("sqlserver://") || url.starts_with("mssql://"),
        "SQL Server conformance requires a sqlserver:// or mssql:// URL"
    );
    url
}

fn bound(sql: &str, params: Vec<SqlValue>, row_cap: Option<usize>) -> BoundQuery {
    BoundQuery {
        sql: sql.into(),
        params,
        row_cap,
        fetch_rows: true,
    }
}

async fn command(driver: &SqlServerDriver, sql: &str) {
    driver
        .execute(&BoundQuery {
            fetch_rows: false,
            ..bound(sql, vec![], None)
        })
        .await
        .unwrap_or_else(|error| panic!("fixture command failed: {sql}: {error}"));
}

#[tokio::test]
#[ignore = "requires disposable SQLSERVER_DATABASE_URL"]
async fn sqlserver_conformance() {
    run_conformance(&SqlServerTestFactory { url: url() })
        .await
        .expect("SQL Server conformance");
}

#[tokio::test]
#[ignore = "requires disposable SQLSERVER_DATABASE_URL"]
async fn empty_metadata_caps_and_codecs() {
    let driver = SqlServerDriver::new(&url()).await.unwrap();
    let empty = driver
        .execute(&bound(
            "SELECT CAST(NULL AS bigint) AS id, CAST(NULL AS nvarchar(20)) AS label WHERE 1=0",
            vec![],
            Some(0),
        ))
        .await
        .unwrap();
    assert_eq!(empty.cols.len(), 2);
    assert_eq!(empty.cols[0].name, "id");
    assert_eq!(empty.cols[0].type_name, "bigint");
    assert_eq!(empty.cols[1].type_name, "nvarchar");
    assert!(empty.rows.is_empty());
    assert!(!empty.meta.truncated);
    for (cap, rows, truncated) in [(0, 0, true), (1, 1, true), (2, 2, false), (3, 2, false)] {
        let result = driver
            .execute(&bound(
                "SELECT n FROM (VALUES (1), (2)) AS numbers(n) ORDER BY n",
                vec![],
                Some(cap),
            ))
            .await
            .unwrap();
        assert_eq!(result.rows.len(), rows);
        assert_eq!(result.meta.row_count, rows);
        assert_eq!(result.meta.truncated, truncated);
        driver.ping().await.unwrap();
    }
    let decimal = BigDecimal::from_str("12345678901234567890.12345678").unwrap();
    let date = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
    let datetime =
        NaiveDateTime::parse_from_str("2026-11-04 12:34:56.1234567", "%Y-%m-%d %H:%M:%S%.f")
            .unwrap();
    let params = vec![
        SqlValue::Null,
        SqlValue::Bool(true),
        SqlValue::I64(i64::MAX),
        SqlValue::F64(2.75),
        SqlValue::String("猫 ' ? -- /* */".into()),
        SqlValue::BigDecimal(decimal),
        SqlValue::Date(date),
        SqlValue::DateTime(datetime),
        SqlValue::Binary(vec![0, 1, 255]),
    ];
    let result = driver
        .execute(&bound(
            "SELECT CAST(? AS int), ?, ?, ?, ?, ?, ?, ?, ?",
            params.clone(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(result.rows, vec![params]);
    let scale38 = SqlValue::BigDecimal(
        BigDecimal::from_str("0.00000000000000000000000000000000000001").unwrap(),
    );
    let high_precision = driver
        .execute(&bound("SELECT ? AS precise", vec![scale38.clone()], None))
        .await
        .unwrap();
    assert_eq!(high_precision.rows, vec![vec![scale38]]);
    let uuid =
        SqlValue::Uuid(tiberius::Uuid::parse_str("12345678-1234-5678-9012-123456789abc").unwrap());
    let json = serde_json::json!({"message": "猫", "count": 2});
    let extra = driver
        .execute(&bound(
            "SELECT ?, ?",
            vec![uuid.clone(), SqlValue::Json(json.clone())],
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        extra.rows,
        vec![vec![uuid, SqlValue::String(json.to_string())]]
    );
    let lexical = driver
        .execute(&bound(
            "SELECT N'it''s ?' AS [literal?], ? AS [bracket]]?] /* ? /* ? */ */ -- ?\n",
            vec![SqlValue::I64(42)],
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        lexical.rows[0],
        vec![SqlValue::String("it's ?".into()), SqlValue::I64(42)]
    );
    let offset = driver.execute(&bound("SELECT CAST('12:34:56.1234567' AS time(7)), CAST('2026-11-04T12:34:56.1234567+05:30' AS datetimeoffset(7)), CAST('<root />' AS xml)", vec![], None)).await.unwrap();
    assert!(
        matches!(&offset.rows[0][0], SqlValue::String(value) if value.starts_with("12:34:56.1234567"))
    );
    assert!(matches!(&offset.rows[0][1], SqlValue::String(value) if value.ends_with("+05:30")));
    assert!(matches!(&offset.rows[0][2], SqlValue::String(value) if value.contains("root")));
}

#[tokio::test]
#[ignore = "requires disposable SQLSERVER_DATABASE_URL"]
async fn commands_catalogs_composite_keys_and_search() {
    let driver = SqlServerDriver::new(&url()).await.unwrap();
    command(&driver, "DROP TABLE IF EXISTS dbo.sqlserver_driver_child").await;
    command(&driver, "DROP TABLE IF EXISTS dbo.sqlserver_driver_parent").await;
    command(
        &driver,
        "DROP PROCEDURE IF EXISTS dbo.sqlserver_driver_routine",
    )
    .await;
    command(&driver, "CREATE TABLE dbo.sqlserver_driver_parent (a int NOT NULL, b int NOT NULL, label nvarchar(40) NULL DEFAULT N'empty', CONSTRAINT sqlserver_driver_pk PRIMARY KEY (a, b))").await;
    command(&driver, "CREATE UNIQUE INDEX sqlserver_driver_label_index ON dbo.sqlserver_driver_parent (label, b)").await;
    command(&driver, "CREATE TABLE dbo.sqlserver_driver_child (a int NOT NULL, b int NOT NULL, CONSTRAINT sqlserver_driver_fk FOREIGN KEY (a, b) REFERENCES dbo.sqlserver_driver_parent(a, b) ON DELETE CASCADE)").await;
    command(
        &driver,
        "CREATE PROCEDURE dbo.sqlserver_driver_routine AS SELECT 1 AS value",
    )
    .await;
    let inserted = driver
        .execute(&BoundQuery {
            fetch_rows: false,
            ..bound(
                "INSERT INTO dbo.sqlserver_driver_parent(a,b,label) VALUES (?, ?, ?)",
                vec![
                    SqlValue::I64(1),
                    SqlValue::I64(2),
                    SqlValue::String("one".into()),
                ],
                None,
            )
        })
        .await
        .unwrap();
    assert_eq!(inserted.meta.rows_affected, Some(1));
    assert!(inserted.rows.is_empty());
    let table = driver
        .describe_table(Some("dbo"), "sqlserver_driver_parent")
        .await
        .unwrap();
    assert_eq!(
        table
            .columns
            .iter()
            .filter(|column| column.is_primary_key)
            .count(),
        2
    );
    assert!(
        table
            .columns
            .iter()
            .find(|column| column.name == "label")
            .unwrap()
            .nullable
    );
    assert!(table
        .columns
        .iter()
        .find(|column| column.name == "label")
        .unwrap()
        .default_value
        .is_some());
    let indexes = driver
        .list_indexes(Some("dbo"), Some("sqlserver_driver_parent"))
        .await
        .unwrap();
    assert!(indexes
        .iter()
        .any(|index| index.primary && index.unique && index.columns == ["a", "b"]));
    assert!(indexes.iter().any(
        |index| index.name == "sqlserver_driver_label_index" && index.columns == ["label", "b"]
    ));
    let keys = driver
        .list_foreign_keys(Some("dbo"), Some("sqlserver_driver_child"))
        .await
        .unwrap();
    let key = keys
        .iter()
        .find(|key| key.name == "sqlserver_driver_fk")
        .unwrap();
    assert_eq!(key.columns, ["a", "b"]);
    assert_eq!(key.ref_columns, ["a", "b"]);
    assert_eq!(key.ref_schema.as_deref(), Some("dbo"));
    assert_eq!(key.ref_table, "sqlserver_driver_parent");
    assert_eq!(key.on_delete.as_deref(), Some("CASCADE"));
    assert!(driver
        .list_routines()
        .await
        .unwrap()
        .iter()
        .any(|routine| routine.name == "sqlserver_driver_routine"));
    for kind in [
        ObjectType::Table,
        ObjectType::Column,
        ObjectType::Index,
        ObjectType::ForeignKey,
        ObjectType::Routine,
    ] {
        assert!(
            !driver
                .search_objects(Some("sqlserver_driver"), Some(kind))
                .await
                .unwrap()
                .is_empty(),
            "{kind:?}"
        );
    }
    let stats = driver.table_stats(Some("dbo")).await.unwrap();
    let stats = stats
        .iter()
        .find(|stats| stats.table == "sqlserver_driver_parent")
        .unwrap();
    assert_eq!(stats.approx_rows, Some(1));
    assert!(!stats.indexes.is_empty());
    assert!(driver
        .list_tables(Some("dbo' OR 1=1 --"))
        .await
        .unwrap()
        .is_empty());
    // Parameterless commands travel as batches; their affected-row count must
    // still be exact (DML) and not a stale count from an earlier statement (DDL).
    let literal_insert = driver
        .execute(&BoundQuery {
            fetch_rows: false,
            ..bound(
                "INSERT INTO dbo.sqlserver_driver_parent(a,b,label) VALUES (3, 4, N'literal')",
                vec![],
                None,
            )
        })
        .await
        .unwrap();
    assert_eq!(literal_insert.meta.rows_affected, Some(1));
    let literal_delete = driver
        .execute(&BoundQuery {
            fetch_rows: false,
            ..bound(
                "DELETE FROM dbo.sqlserver_driver_parent WHERE a = 3 AND b = 4",
                vec![],
                None,
            )
        })
        .await
        .unwrap();
    assert_eq!(literal_delete.meta.rows_affected, Some(1));
    let ddl = driver
        .execute(&BoundQuery {
            fetch_rows: false,
            ..bound(
                "CREATE OR ALTER PROCEDURE dbo.sqlserver_driver_routine AS SELECT 1 AS value",
                vec![],
                None,
            )
        })
        .await
        .unwrap();
    assert_eq!(ddl.meta.rows_affected, Some(0));
    command(&driver, "DROP PROCEDURE dbo.sqlserver_driver_routine").await;
    command(&driver, "DROP TABLE dbo.sqlserver_driver_child").await;
    command(&driver, "DROP TABLE dbo.sqlserver_driver_parent").await;
}

#[tokio::test]
#[ignore = "requires disposable SQLSERVER_DATABASE_URL"]
async fn timeout_cancellation_and_execution_errors_discard_connections() {
    let options = ConnectOptions {
        pool_size: 1,
        statement_timeout: Duration::from_millis(150),
        connect_timeout: Duration::from_secs(5),
        ..ConnectOptions::default()
    };
    let driver = SqlServerDriver::new_with_options(&url(), &options)
        .await
        .unwrap();
    let slow = bound("WAITFOR DELAY '00:00:02'; SELECT 1", vec![], None);
    assert!(matches!(
        driver.execute(&slow).await,
        Err(DriverError::Timeout)
    ));
    driver.ping().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), driver.execute(&slow))
            .await
            .is_err()
    );
    driver.ping().await.unwrap();
    let error = driver
        .execute(&bound(
            "SELECT N'sensitive_sql_literal' FROM dbo.sqlserver_missing_secret_table",
            vec![],
            None,
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("sensitive_sql_literal"));
    assert!(!error.contains("sqlserver_missing_secret_table"));
    driver.ping().await.unwrap();
}

#[tokio::test]
#[ignore = "requires disposable SQLSERVER_DATABASE_URL"]
async fn explain_returns_only_plan_and_cleans_session_mode() {
    let options = ConnectOptions {
        pool_size: 1,
        ..ConnectOptions::default()
    };
    let driver = SqlServerDriver::new_with_options(&url(), &options)
        .await
        .unwrap();
    // Estimated plans exist even for a scalar statement; only the plan is returned.
    let estimated = driver
        .explain(
            &bound("SELECT ? AS secret_data", vec![SqlValue::I64(17)], None),
            false,
        )
        .await
        .unwrap();
    assert_eq!(estimated.rows.len(), 1);
    assert_eq!(estimated.rows[0].len(), 1);
    assert!(matches!(&estimated.rows[0][0], SqlValue::String(xml) if xml.contains("<ShowPlanXML")));
    // Rich bind types must compile as batch declarations, and no bound value may
    // leak into the generated batch or its plan.
    const SECRET: &str = "UTC8-secret-marker-猫";
    let rich = vec![
        SqlValue::String(SECRET.into()),
        SqlValue::Date(NaiveDate::from_ymd_opt(2026, 11, 4).unwrap()),
        SqlValue::BigDecimal(
            BigDecimal::from_str("0.00000000000000000000000000000000000001").unwrap(),
        ),
        SqlValue::Binary(vec![0, 1, 255]),
        SqlValue::Json(serde_json::json!({ "secret": SECRET })),
        SqlValue::Null,
    ];
    let rich_plan = driver
        .explain(
            &bound(
                "SELECT ? AS marker, ? AS day, ? AS precise, ? AS payload, ? AS doc, ? AS missing",
                rich,
                None,
            ),
            false,
        )
        .await
        .unwrap();
    assert_eq!(rich_plan.rows.len(), 1);
    assert_eq!(rich_plan.rows[0].len(), 1);
    let SqlValue::String(xml) = &rich_plan.rows[0][0] else {
        panic!("estimated plan must be XML text");
    };
    assert!(xml.contains("<ShowPlanXML"));
    assert!(!xml.contains(SECRET), "bound value leaked into the plan");
    assert!(
        !xml.contains("0.00000000000000000000000000000000000001"),
        "bound value leaked into the plan"
    );
    // SQL Server emits no actual plan for a trivial scalar SELECT. That must be a
    // truthful error, never the estimated plan relabelled as an actual plan.
    let unavailable = driver
        .explain(
            &bound("SELECT ? AS secret_data", vec![SqlValue::I64(17)], None),
            true,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(unavailable, DriverError::Execution(_)),
        "{unavailable}"
    );
    // Actual plans require a statement with a real access path, and none of the
    // analyzed query's data rows may be returned.
    command(&driver, "DROP TABLE IF EXISTS dbo.sqlserver_driver_plan").await;
    command(
        &driver,
        "CREATE TABLE dbo.sqlserver_driver_plan (id int PRIMARY KEY, label nvarchar(30) NULL)",
    )
    .await;
    command(
        &driver,
        "INSERT INTO dbo.sqlserver_driver_plan VALUES (1, N'one'), (2, N'two')",
    )
    .await;
    for analyze in [false, true] {
        let result = driver
            .explain(
                &bound(
                    "SELECT label FROM dbo.sqlserver_driver_plan WHERE id = ?",
                    vec![SqlValue::I64(1)],
                    None,
                ),
                analyze,
            )
            .await
            .unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].len(), 1);
        assert!(
            matches!(&result.rows[0][0], SqlValue::String(xml) if xml.contains("<ShowPlanXML")),
            "{analyze}"
        );
        let normal = driver
            .execute(&bound("SELECT 19 AS value", vec![], None))
            .await
            .unwrap();
        assert_eq!(normal.rows, vec![vec![SqlValue::I64(19)]]);
    }
    command(&driver, "DROP TABLE dbo.sqlserver_driver_plan").await;
}

#[tokio::test]
#[ignore = "requires a disposable SQL Server with KILL permission (admin)"]
async fn checkout_probe_replaces_a_killed_idle_connection() {
    let options = ConnectOptions {
        pool_size: 1,
        ..ConnectOptions::default()
    };
    let driver = SqlServerDriver::new_with_options(&url(), &options)
        .await
        .unwrap();
    let session = driver
        .execute(&bound("SELECT @@SPID AS spid", vec![], None))
        .await
        .unwrap();
    let Some(SqlValue::I64(spid)) = session.rows.first().and_then(|row| row.first()) else {
        panic!("SQL Server did not report a session id");
    };
    // An independent connection kills the idle session held by `driver`.
    let admin = SqlServerDriver::new(&url()).await.unwrap();
    admin
        .execute(&BoundQuery {
            fetch_rows: false,
            ..bound(&format!("KILL {spid}"), vec![], None)
        })
        .await
        .expect("KILL requires admin rights on a disposable fixture");
    // The checkout probe must discard the killed connection and run this bound
    // query on a fresh socket, without retrying the caller's statement.
    let alive = driver
        .execute(&bound("SELECT ? AS alive", vec![SqlValue::I64(17)], None))
        .await
        .expect("checkout probe must replace a killed idle connection");
    assert_eq!(alive.rows, vec![vec![SqlValue::I64(17)]]);
}
