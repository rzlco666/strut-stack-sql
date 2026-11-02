use std::time::Duration;

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Engine identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Postgres,
    MySql,
    Sqlite,
}

/// Connection-pool options shared by every driver.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub pool_size: u32,
    pub read_only: bool,
    pub connect_timeout: Duration,
    /// Server-side statement timeout applied on each new connection (PG/MySQL).
    pub statement_timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            pool_size: 5,
            read_only: false,
            connect_timeout: Duration::from_secs(2),
            statement_timeout: Duration::from_secs(10),
        }
    }
}

/// Neutral SQL value representation for cross-driver compatibility.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SqlValue {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
    Uuid(Uuid),
    BigDecimal(BigDecimal),
    Date(NaiveDate),
    DateTime(NaiveDateTime),
    Binary(Vec<u8>),
    Array(Vec<SqlValue>),
    Json(serde_json::Value),
}

/// Column metadata from query results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnMeta {
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
}

/// Query result with columnar data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub cols: Vec<ColumnMeta>,
    pub rows: Vec<Vec<SqlValue>>,
    pub meta: QueryMeta,
}

/// Query metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryMeta {
    pub row_count: usize,
    pub truncated: bool,
    /// Rows affected by a non-row-returning statement.
    pub rows_affected: Option<u64>,
}

/// Schema object (table, column, index, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaObject {
    pub object_type: ObjectType,
    pub schema: Option<String>,
    pub name: String,
    pub table: Option<String>,
    pub data_type: Option<String>,
    pub nullable: Option<bool>,
}

/// Object type in schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectType {
    Schema,
    Table,
    Column,
    Index,
    ForeignKey,
    Routine,
}

/// Column information from schema introspection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default_value: Option<String>,
    pub is_primary_key: bool,
    /// MySQL `SHOW FULL COLUMNS.Key` (PRI/UNI/MUL); other engines leave `None`.
    pub key: Option<String>,
    /// MySQL `SHOW FULL COLUMNS.Extra`; other engines leave `None`.
    pub extra: Option<String>,
    /// MySQL `SHOW FULL COLUMNS.Comment`; other engines leave `None`.
    pub comment: Option<String>,
}

/// Table information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableInfo {
    pub schema: Option<String>,
    pub name: String,
    pub columns: Vec<ColumnInfo>,
}

/// Index information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    pub schema: Option<String>,
    pub table: String,
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

/// Foreign key information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignKeyInfo {
    pub schema: Option<String>,
    pub table: String,
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: Option<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
}

/// Routine (function/procedure) information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineInfo {
    pub schema: Option<String>,
    pub name: String,
    pub routine_type: String,
    pub return_type: Option<String>,
}

/// Approximate row count for one table (planner/stats estimates).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRowCount {
    pub name: String,
    pub approx_rows: Option<i64>,
}

/// Bound query with SQL and parameters.
#[derive(Debug, Clone)]
pub struct BoundQuery {
    pub sql: String,
    pub params: Vec<SqlValue>,
    pub row_cap: Option<usize>,
    /// Fetch rows (`true`) or run as a command and report rows affected.
    pub fetch_rows: bool,
}

/// Driver-specific error.
#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("connection error: {0}")]
    Connection(String),

    #[error("execution error: {0}")]
    Execution(String),

    #[error("introspection error: {0}")]
    Introspection(String),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("unsupported operation: {0}")]
    Unsupported(String),

    #[error("timeout")]
    Timeout,

    #[error("closed")]
    Closed,
}

/// Engine driver trait - engine-neutral database operations.
#[async_trait]
pub trait EngineDriver: Send + Sync {
    /// Return the engine kind.
    fn kind(&self) -> EngineKind;

    /// Create a driver instance from a connection string with pool options.
    ///
    /// The connection string format is engine-specific (e.g., postgres://, mysql://, sqlite:).
    async fn from_connection_string_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Box<Self>, DriverError>
    where
        Self: Sized;

    /// Create a driver instance with default options.
    async fn from_connection_string(connection_string: &str) -> Result<Box<Self>, DriverError>
    where
        Self: Sized,
    {
        Self::from_connection_string_with_options(connection_string, &ConnectOptions::default())
            .await
    }

    /// Execute a bound query.
    async fn execute(&self, query: &BoundQuery) -> Result<QueryResult, DriverError>;

    /// List all schemas/databases.
    async fn list_schemas(&self) -> Result<Vec<SchemaObject>, DriverError>;

    /// List tables, optionally scoped to a schema/database.
    async fn list_tables(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError>;

    /// List columns, optionally scoped to a schema/database.
    async fn list_columns(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError>;

    /// Describe a table's schema.
    async fn describe_table(
        &self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<TableInfo, DriverError>;

    /// List indexes, optionally scoped to a schema/database and table.
    async fn list_indexes(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<IndexInfo>, DriverError>;

    /// List foreign keys, optionally scoped to a schema/database and table.
    async fn list_foreign_keys(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<ForeignKeyInfo>, DriverError>;

    /// Resolve the default schema/database name for this driver.
    async fn resolve_schema(&self, schema: Option<&str>) -> Result<String, DriverError>;

    /// Approximate row counts per table from planner/catalog statistics.
    async fn approximate_row_counts(
        &self,
        schema: Option<&str>,
    ) -> Result<Vec<TableRowCount>, DriverError>;

    /// List all routines (functions/procedures).
    async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError>;

    /// Search for schema objects by keyword.
    async fn search_objects(
        &self,
        keyword: Option<&str>,
        object_type: Option<ObjectType>,
    ) -> Result<Vec<SchemaObject>, DriverError> {
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
                })
                .collect(),
            Some(ObjectType::ForeignKey) | Some(ObjectType::Routine) | None => Vec::new(),
        };
        Ok(match keyword {
            Some(kw) => {
                let kw = kw.to_lowercase();
                objects
                    .into_iter()
                    .filter(|o| {
                        o.name.to_lowercase().contains(&kw)
                            || o.table
                                .as_ref()
                                .is_some_and(|t| t.to_lowercase().contains(&kw))
                            || o.schema
                                .as_ref()
                                .is_some_and(|s| s.to_lowercase().contains(&kw))
                    })
                    .collect()
            }
            None => objects,
        })
    }

    /// Ping the database to check connectivity.
    async fn ping(&self) -> Result<(), DriverError>;
}
