use async_trait::async_trait;
use std::env;
use strut_sql_driver_api::EngineDriver;
use strut_sql_driver_conformance::{run_conformance, DriverFactory};
use strut_sql_driver_postgres::PostgresDriver;

struct PostgresTestFactory {
    url: String,
}

#[async_trait]
impl DriverFactory for PostgresTestFactory {
    async fn create(
        &self,
    ) -> Result<Box<dyn EngineDriver>, Box<dyn std::error::Error + Send + Sync>> {
        let driver = PostgresDriver::new(&self.url).await?;
        Ok(Box::new(driver) as Box<dyn EngineDriver>)
    }
}

#[tokio::test]
#[ignore = "requires POSTGRES_DATABASE_URL or postgres:// DATABASE_URL"]
async fn postgres_conformance() {
    let url = env::var("POSTGRES_DATABASE_URL")
        .or_else(|_| env::var("DATABASE_URL"))
        .expect("POSTGRES_DATABASE_URL or DATABASE_URL must be set");

    // Ensure it's a postgres URL
    if !url.starts_with("postgres://") && !url.starts_with("postgresql://") {
        panic!("DATABASE_URL must be a postgres:// URL for this test");
    }

    let factory = PostgresTestFactory { url };
    run_conformance(&factory)
        .await
        .expect("conformance test failed");
}

/// R4-03: `vector` is a data type. When the pgvector extension is present, a
/// vector column reads as an array of numbers; there is no dedicated
/// vector-search tool.
#[tokio::test]
#[ignore = "requires POSTGRES_DATABASE_URL on a pgvector-enabled server"]
async fn vector_columns_read_as_arrays_of_numbers() {
    use strut_sql_driver_api::{BoundQuery, EngineDriver, SqlValue};

    let url = std::env::var("POSTGRES_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .expect("POSTGRES_DATABASE_URL or DATABASE_URL");
    if !url.starts_with("postgres") {
        eprintln!("skipping: not a PostgreSQL URL");
        return;
    }
    let driver = PostgresDriver::from_connection_string(&url)
        .await
        .expect("connect");
    let execute = |sql: &str| {
        let sql = sql.to_string();
        async {
            driver
                .execute(&BoundQuery {
                    sql,
                    params: vec![],
                    row_cap: None,
                    fetch_rows: true,
                })
                .await
        }
    };

    execute("CREATE EXTENSION IF NOT EXISTS vector")
        .await
        .expect("extension");
    execute("DROP TABLE IF EXISTS vector_conformance")
        .await
        .expect("drop");
    execute("CREATE TABLE vector_conformance (id INTEGER PRIMARY KEY, embedding vector(3))")
        .await
        .expect("create");
    execute("INSERT INTO vector_conformance (id, embedding) VALUES (1, '[1,2.5,3]')")
        .await
        .expect("insert");

    let result = execute("SELECT embedding FROM vector_conformance WHERE id = 1")
        .await
        .expect("select");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0][0] {
        SqlValue::Json(value) => {
            let numbers = value.as_array().expect("array of numbers");
            assert_eq!(numbers.len(), 3, "{value}");
            assert_eq!(numbers[0].as_f64(), Some(1.0));
            assert_eq!(numbers[1].as_f64(), Some(2.5));
            assert_eq!(numbers[2].as_f64(), Some(3.0));
        }
        other => panic!("vector must decode as a JSON array, got {other:?}"),
    }

    execute("DROP TABLE vector_conformance")
        .await
        .expect("cleanup");
}
