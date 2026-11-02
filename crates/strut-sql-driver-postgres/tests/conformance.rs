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
