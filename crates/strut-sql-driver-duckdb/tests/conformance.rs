use async_trait::async_trait;
use strut_sql_driver_api::EngineDriver;
use strut_sql_driver_conformance::{run_conformance, DriverFactory};
use strut_sql_driver_duckdb::DuckDbDriver;

struct DuckDbFactory;

#[allow(clippy::double_must_use)]
#[async_trait]
impl DriverFactory for DuckDbFactory {
    async fn create(
        &self,
    ) -> Result<Box<dyn EngineDriver>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Box::new(DuckDbDriver::new("duckdb::memory:").await?))
    }
}

#[tokio::test]
async fn duckdb_conformance() {
    run_conformance(&DuckDbFactory)
        .await
        .expect("DuckDB conformance");
}
