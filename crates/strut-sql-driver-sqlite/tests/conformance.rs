use async_trait::async_trait;
use strut_sql_driver_api::EngineDriver;
use strut_sql_driver_conformance::{run_conformance, DriverFactory};
use strut_sql_driver_sqlite::SqliteDriver;

struct SqliteTestFactory {
    url: String,
}

#[async_trait]
impl DriverFactory for SqliteTestFactory {
    async fn create(
        &self,
    ) -> Result<Box<dyn EngineDriver>, Box<dyn std::error::Error + Send + Sync>> {
        let driver = SqliteDriver::from_connection_string(&self.url).await?;
        Ok(driver)
    }
}

#[tokio::test]
async fn sqlite_conformance() {
    // A temp file (not `:memory:`) keeps every pool connection on the same
    // database, which is what the conformance routine assumes.
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("conformance.sqlite");
    let url = format!("sqlite://{}?mode=rwc", path.display());

    run_conformance(&SqliteTestFactory { url })
        .await
        .expect("conformance test failed");
}
