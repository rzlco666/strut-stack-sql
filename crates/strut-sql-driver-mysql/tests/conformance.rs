use async_trait::async_trait;
use std::env;
use strut_sql_driver_api::EngineDriver;
use strut_sql_driver_conformance::{run_conformance, DriverFactory};
use strut_sql_driver_mysql::MySqlDriver;

struct MySqlTestFactory {
    url: String,
}

#[async_trait]
impl DriverFactory for MySqlTestFactory {
    async fn create(
        &self,
    ) -> Result<Box<dyn EngineDriver>, Box<dyn std::error::Error + Send + Sync>> {
        let driver = MySqlDriver::new(&self.url).await?;
        Ok(Box::new(driver) as Box<dyn EngineDriver>)
    }
}

#[tokio::test]
#[ignore = "requires MYSQL_DATABASE_URL or mysql:// DATABASE_URL"]
async fn mysql_conformance() {
    let url = env::var("MYSQL_DATABASE_URL")
        .or_else(|_| env::var("DATABASE_URL"))
        .expect("MYSQL_DATABASE_URL or DATABASE_URL must be set");

    // Ensure it's a mysql URL
    if !url.starts_with("mysql://") {
        panic!("DATABASE_URL must be a mysql:// URL for this test");
    }

    let factory = MySqlTestFactory { url };
    run_conformance(&factory)
        .await
        .expect("conformance test failed");
}
