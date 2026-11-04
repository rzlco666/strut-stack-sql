//! ClickHouse driver over the HTTP interface.
//!
//! ## Surface
//!
//! Every `EngineDriver` method is implemented: bound execution with the
//! neutral `?` placeholder syntax, schema/column/index/routine introspection,
//! row-count statistics, and connectivity checks.
//!
//! ## Deliberate ClickHouse semantics
//!
//! * **Read-only** is enforced by the server (`readonly=1`), not just by the
//!   client: writes and setting changes fail inside ClickHouse.
//! * **Statement timeout** maps to `max_execution_time`, so an over-long
//!   statement is aborted server-side (HTTP 408 / code 159 → `Timeout`).
//! * **Row caps** are applied while streaming the response: the driver stops
//!   reading after `cap` rows and marks the result truncated. ClickHouse has
//!   already computed the extra row server-side, so a cap bounds transfer and
//!   memory, not server work.
//! * **Primary keys are not unique.** `IndexInfo.primary` reports the table's
//!   sorting key and `unique` is always false; `CREATE INDEX` does not exist
//!   (data-skipping indexes are created with `ALTER TABLE ... ADD INDEX`).
//! * **No foreign keys and no stored procedures** exist in the catalog, so
//!   `list_foreign_keys` is empty and `list_routines` returns SQL UDFs only.
//! * **Value fidelity**: 128/256-bit integers and `UInt64` values above
//!   `i64::MAX` are returned as exact decimal strings (the neutral
//!   `SqlValue` model has no unsigned/128-bit variant); `Map`, named tuples
//!   and unknown/complex types are returned as JSON.
//! * **Parameter binding**: scalars travel as typed `{pN:Type}` HTTP
//!   parameters; `NULL` and arrays are inlined as escaped literals because
//!   ClickHouse has no generic array parameter type. Binary parameters are
//!   sent as UTF-8 (lossy) for the same reason.

mod config;
mod introspect;
mod transport;
mod value;

use std::sync::Arc;

use async_trait::async_trait;
use strut_sql_driver_api::{
    BoundQuery, CapabilityNote, ConnectOptions, DriverError, EngineDriver, EngineKind,
    ForeignKeyInfo, IndexInfo, IndexPage, ObjectType, QueryResult, RoutineInfo, SchemaObject,
    SchemaPage, SqlValue, TableInfo, TableRowCount, TableStats,
};

use transport::Transport;

pub struct ClickHouseDriver {
    transport: Arc<Transport>,
    notes: Vec<CapabilityNote>,
}

impl ClickHouseDriver {
    pub async fn new(connection_string: &str) -> Result<Self, DriverError> {
        Self::new_with_options(connection_string, &ConnectOptions::default()).await
    }

    pub async fn new_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Self, DriverError> {
        let endpoint = config::Endpoint::parse(connection_string)?;
        let transport = Arc::new(Transport::new(endpoint, options)?);
        let driver = Self {
            transport,
            notes: Vec::new(),
        };
        // Fail fast, like the pool-based drivers: a source that cannot answer
        // `SELECT 1` must not be cached as connected.
        driver.ping().await?;
        Ok(driver)
    }

    fn schema(&self, schema: Option<&str>) -> String {
        schema
            .filter(|s| !s.is_empty())
            .unwrap_or(self.transport.database())
            .to_string()
    }

    async fn fetch(&self, sql: &str, params: &[SqlValue]) -> Result<QueryResult, DriverError> {
        self.transport.fetch(sql, params).await
    }
}

#[async_trait]
impl EngineDriver for ClickHouseDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::ClickHouse
    }

    fn capability_notes(&self) -> Vec<CapabilityNote> {
        self.notes.clone()
    }

    async fn from_connection_string_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Box<Self>, DriverError> {
        Ok(Box::new(
            ClickHouseDriver::new_with_options(connection_string, options).await?,
        ))
    }

    async fn execute(&self, query: &BoundQuery) -> Result<QueryResult, DriverError> {
        self.transport
            .execute(&query.sql, &query.params, query.fetch_rows, query.row_cap)
            .await
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaObject>, DriverError> {
        introspect::list_schemas(&self.transport).await
    }

    async fn list_schemas_page(
        &self,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let (objects, has_more) =
            introspect::list_schemas_page(&self.transport, keyword, offset, limit).await?;
        Ok(SchemaPage { objects, has_more })
    }

    async fn list_tables(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let schema = self.schema(schema);
        introspect::list_tables(&self.transport, &schema).await
    }

    async fn list_tables_page(
        &self,
        schema: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let schema = self.schema(schema);
        let (objects, has_more) =
            introspect::list_tables_page(&self.transport, &schema, keyword, offset, limit).await?;
        Ok(SchemaPage { objects, has_more })
    }

    async fn list_columns(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let schema = self.schema(schema);
        introspect::list_columns(&self.transport, &schema).await
    }

    async fn list_columns_page(
        &self,
        schema: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let schema = self.schema(schema);
        let (objects, has_more) =
            introspect::list_columns_page(&self.transport, &schema, keyword, offset, limit).await?;
        Ok(SchemaPage { objects, has_more })
    }

    async fn describe_table(
        &self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<TableInfo, DriverError> {
        let schema = self.schema(schema);
        introspect::describe_table(&self.transport, &schema, table).await
    }

    async fn list_indexes(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<IndexInfo>, DriverError> {
        let schema = self.schema(schema);
        introspect::list_indexes(&self.transport, &schema, table).await
    }

    async fn list_indexes_page(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<IndexPage, DriverError> {
        let mut indexes = self.list_indexes(schema, table).await?;
        if let Some(keyword) = keyword {
            let keyword = keyword.to_lowercase();
            indexes.retain(|index| {
                index.name.to_lowercase().contains(&keyword)
                    || index.table.to_lowercase().contains(&keyword)
            });
        }
        let has_more = indexes.len() > offset.saturating_add(limit);
        let page = indexes
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();
        Ok(IndexPage {
            indexes: page,
            has_more,
        })
    }

    async fn list_foreign_keys(
        &self,
        _schema: Option<&str>,
        _table: Option<&str>,
    ) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        Ok(introspect::list_foreign_keys().await)
    }

    async fn resolve_schema(&self, schema: Option<&str>) -> Result<String, DriverError> {
        Ok(self.schema(schema))
    }

    async fn approximate_row_counts(
        &self,
        schema: Option<&str>,
    ) -> Result<Vec<TableRowCount>, DriverError> {
        let schema = self.schema(schema);
        introspect::table_row_counts(&self.transport, &schema).await
    }

    async fn table_stats(&self, schema: Option<&str>) -> Result<Vec<TableStats>, DriverError> {
        let schema = self.schema(schema);
        introspect::table_stats(&self.transport, &schema).await
    }

    async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError> {
        introspect::list_routines(&self.transport).await
    }

    async fn search_objects(
        &self,
        keyword: Option<&str>,
        object_type: Option<ObjectType>,
    ) -> Result<Vec<SchemaObject>, DriverError> {
        // Routines live in a catalog without a database scope in ClickHouse.
        if matches!(object_type, Some(ObjectType::Routine)) {
            let routines = self.list_routines().await?;
            let keyword = keyword.map(str::to_lowercase);
            return Ok(routines
                .into_iter()
                .filter(|r| {
                    keyword
                        .as_ref()
                        .is_none_or(|k| r.name.to_lowercase().contains(k))
                })
                .map(|r| SchemaObject {
                    object_type: ObjectType::Routine,
                    schema: r.schema,
                    name: r.name,
                    table: None,
                    data_type: r.return_type,
                    nullable: None,
                    comment: None,
                })
                .collect());
        }
        let objects = match object_type {
            Some(ObjectType::Schema) => self.list_schemas().await?,
            Some(ObjectType::Table) => self.list_tables(None).await?,
            Some(ObjectType::Column) => self.list_columns(None).await?,
            Some(ObjectType::Index) => self
                .list_indexes(None, None)
                .await?
                .into_iter()
                .map(|i| SchemaObject {
                    object_type: ObjectType::Index,
                    schema: i.schema,
                    name: i.name,
                    table: Some(i.table),
                    data_type: None,
                    nullable: None,
                    comment: None,
                })
                .collect(),
            Some(ObjectType::ForeignKey) | Some(ObjectType::Routine) | None => Vec::new(),
        };
        let keyword = keyword.map(str::to_lowercase);
        Ok(objects
            .into_iter()
            .filter(|o| {
                keyword.as_ref().is_none_or(|kw| {
                    o.name.to_lowercase().contains(kw)
                        || o.table
                            .as_ref()
                            .is_some_and(|t| t.to_lowercase().contains(kw))
                        || o.schema
                            .as_ref()
                            .is_some_and(|s| s.to_lowercase().contains(kw))
                        || o.comment
                            .as_ref()
                            .is_some_and(|c| c.to_lowercase().contains(kw))
                })
            })
            .collect())
    }

    async fn ping(&self) -> Result<(), DriverError> {
        self.fetch("SELECT 1", &[]).await.map(|_| ())
    }
}
