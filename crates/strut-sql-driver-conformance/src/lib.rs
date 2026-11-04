use async_trait::async_trait;
use strut_sql_driver_api::TableInfo;
use strut_sql_driver_api::{BoundQuery, EngineDriver, ObjectType, SchemaObject, SqlValue};

/// Generic driver factory for conformance testing.
// Same `async_trait` (0.1.89) synthesis as `EngineDriver`: the generated
// `#[must_use]` on the boxed-future method cannot carry a reason, and clippy
// 1.99's `double_must_use` flags it as an error. Suppress locally; behaviour
// is unchanged.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait DriverFactory: Send + Sync {
    async fn create(
        &self,
    ) -> Result<Box<dyn EngineDriver>, Box<dyn std::error::Error + Send + Sync>>;
}

fn bound(sql: &str, params: Vec<SqlValue>, row_cap: Option<usize>) -> BoundQuery {
    BoundQuery {
        sql: sql.to_string(),
        params,
        row_cap,
        fetch_rows: true,
    }
}

/// Run basic conformance checks against a driver.
///
/// This routine:
/// 1. Connects to the database
/// 2. Creates a test table
/// 3. Inserts data
/// 4. Selects it back
/// 5. Verifies list_tables contains the test table
/// 6. Verifies describe_table returns the table's columns
/// 7. Verifies the row cap truncates and marks the result
/// 8. Drops the test table
pub async fn run_conformance<F>(factory: &F) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: DriverFactory,
{
    let driver = factory.create().await?;

    // Ping to verify connectivity
    driver.ping().await?;

    // Drop left-overs so the routine is idempotent.
    driver
        .execute(&bound(
            "DROP TABLE IF EXISTS conformance_test",
            vec![],
            None,
        ))
        .await?;

    // Create test table
    driver
        .execute(&bound(
            "CREATE TABLE conformance_test (id INTEGER PRIMARY KEY, name TEXT, value REAL)",
            vec![],
            None,
        ))
        .await?;

    // Insert data (two rows: the second exercises the row cap)
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
        .await?;
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
        .await?;

    // Select back
    let select_sql = "SELECT id, name, value FROM conformance_test WHERE id = ?";
    let result = driver
        .execute(&bound(select_sql, vec![SqlValue::I64(1)], None))
        .await?;

    // Verify we got one row
    assert_eq!(result.rows.len(), 1, "Expected exactly one row");
    assert_eq!(result.meta.row_count, 1);

    // Verify column values
    let row = &result.rows[0];
    assert_eq!(row.len(), 3);
    assert_eq!(row[0], SqlValue::I64(1));
    assert_eq!(row[1], SqlValue::String("test".to_string()));
    // Float comparison may vary by engine
    match &row[2] {
        SqlValue::F64(f) => assert!((f - 2.75).abs() < 0.001),
        other => panic!("Expected F64 for value column, got {other:?}"),
    }

    // Row cap: two rows exist, cap of one must truncate and say so.
    let capped = driver
        .execute(&bound(
            "SELECT id FROM conformance_test ORDER BY id",
            vec![],
            Some(1),
        ))
        .await?;
    assert_eq!(capped.rows.len(), 1, "row cap should keep one row");
    assert_eq!(capped.meta.row_count, 1);
    assert!(
        capped.meta.truncated,
        "row cap must mark the result truncated"
    );

    // Verify list_tables contains our table
    let tables = driver.list_tables(None).await?;
    assert!(
        tables.iter().any(|t| t.name == "conformance_test"),
        "list_tables should contain conformance_test"
    );

    // Verify search_objects finds it
    let found: Vec<SchemaObject> = driver
        .search_objects(Some("conformance"), Some(ObjectType::Table))
        .await?;
    assert!(
        found.iter().any(|t| t.name == "conformance_test"),
        "search_objects should find conformance_test"
    );

    // Verify describe_table returns columns
    let table_info: TableInfo = driver.describe_table(None, "conformance_test").await?;
    assert_eq!(table_info.name, "conformance_test");
    assert_eq!(table_info.columns.len(), 3);

    // Verify column names and the primary-key flag
    let col_names: Vec<_> = table_info.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(col_names.contains(&"id"));
    assert!(col_names.contains(&"name"));
    assert!(col_names.contains(&"value"));
    assert!(
        table_info
            .columns
            .iter()
            .any(|c| c.name == "id" && c.is_primary_key),
        "describe_table should mark id as the primary key"
    );

    // Clean up: drop table
    driver
        .execute(&bound("DROP TABLE conformance_test", vec![], None))
        .await?;

    Ok(())
}
