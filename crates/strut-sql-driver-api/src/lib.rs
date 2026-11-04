use std::time::Duration;

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod placeholders;

/// Engine identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Postgres,
    MySql,
    Sqlite,
    DuckDb,
    ClickHouse,
    #[serde(rename = "sqlserver")]
    SqlServer,
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
    /// Catalog comment for the object (column/table), where the engine
    /// exposes one. Used by ranked search (PRD-F3-01) and omitted otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
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

/// One page of schema objects plus whether more rows exist after it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaPage {
    pub objects: Vec<SchemaObject>,
    pub has_more: bool,
}

/// One page of indexes plus whether more indexes exist after it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexPage {
    pub indexes: Vec<IndexInfo>,
    pub has_more: bool,
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

/// How an approximate row count was obtained (PRD-F3-07). The label is
/// deliberately explicit because MySQL/SQLite catalog values are not
/// planner-quality estimates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowEstimateKind {
    /// Postgres planner statistics (`pg_class.reltuples` / live tuples).
    PlannerStats,
    /// Engine catalog estimate (MySQL `table_rows`, SQLite `sqlite_stat1`).
    CatalogStats,
    /// The engine has no usable estimate; do not infer one.
    Unavailable,
}

/// Uniform per-table statistics for the performance advisor (PRD-F3-07).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableStats {
    /// Table name.
    pub table: String,
    /// Approximate row count; `None` when unavailable.
    pub approx_rows: Option<i64>,
    /// Provenance of `approx_rows`.
    pub row_estimate: RowEstimateKind,
    /// Total table bytes (heap/data + auxiliary structures), when reported.
    pub table_bytes: Option<i64>,
    /// Total index bytes, when reported.
    pub index_bytes: Option<i64>,
    /// Indexes on the table, in catalog order.
    pub indexes: Vec<IndexInfo>,
}

/// Case-insensitive substring match over name/table/schema, matching the
/// historical in-memory filter semantics.
pub fn schema_object_matches(object: &SchemaObject, keyword: &str) -> bool {
    let kw = keyword.to_lowercase();
    object.name.to_lowercase().contains(&kw)
        || object
            .table
            .as_ref()
            .is_some_and(|t| t.to_lowercase().contains(&kw))
        || object
            .schema
            .as_ref()
            .is_some_and(|s| s.to_lowercase().contains(&kw))
}

/// Filter, then slice one page (offset clamped; never panics).
pub fn filter_and_page_schema_objects(
    objects: Vec<SchemaObject>,
    keyword: Option<&str>,
    offset: usize,
    limit: usize,
) -> SchemaPage {
    let objects: Vec<SchemaObject> = match keyword {
        Some(kw) => objects
            .into_iter()
            .filter(|o| schema_object_matches(o, kw))
            .collect(),
        None => objects,
    };
    page_schema_objects(objects, offset, limit)
}

/// Slice one page out of already-filtered objects.
pub fn page_schema_objects(objects: Vec<SchemaObject>, offset: usize, limit: usize) -> SchemaPage {
    let total = objects.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    SchemaPage {
        objects: objects[start..end].to_vec(),
        has_more: end < total,
    }
}

pub fn index_matches(index: &IndexInfo, keyword: &str) -> bool {
    let kw = keyword.to_lowercase();
    index.name.to_lowercase().contains(&kw)
        || index.table.to_lowercase().contains(&kw)
        || index
            .schema
            .as_ref()
            .is_some_and(|s| s.to_lowercase().contains(&kw))
}

/// Filter, then slice one page of indexes (offset clamped; never panics).
pub fn filter_and_page_indexes(
    indexes: Vec<IndexInfo>,
    keyword: Option<&str>,
    offset: usize,
    limit: usize,
) -> IndexPage {
    let indexes: Vec<IndexInfo> = match keyword {
        Some(kw) => indexes
            .into_iter()
            .filter(|i| index_matches(i, kw))
            .collect(),
        None => indexes,
    };
    let total = indexes.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    IndexPage {
        indexes: indexes[start..end].to_vec(),
        has_more: end < total,
    }
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

/// Non-fatal driver capability degradation (e.g. a server-side statement
/// timeout could not be installed). Surfaced in diagnostic logs and result
/// meta so callers never silently assume a capability they do not have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityNote {
    pub code: String,
    pub message: String,
}

/// Engine driver trait - engine-neutral database operations.
// `async_trait` (0.1.89) synthesizes a bare `#[must_use]` on every generated
// method, whose return type (`Pin<Box<dyn Future<Output = Result<..>>>>`) is
// already `must_use`. Clippy 1.99's `double_must_use` flags that as an error
// and suggests adding a reason, but the attribute is emitted by the macro and
// cannot be given one here. Suppress locally; behaviour is unchanged.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait EngineDriver: Send + Sync {
    /// Return the engine kind.
    fn kind(&self) -> EngineKind;

    /// Capabilities that could not be established for this session.
    /// Empty when everything requested is effective.
    fn capability_notes(&self) -> Vec<CapabilityNote> {
        Vec::new()
    }

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

    /// Obtain a session-scoped execution plan for an already-prepared query.
    ///
    /// Drivers override this only when they can manage the required session
    /// state safely. The default reports a real lack of this driver capability;
    /// it does not imply generic execution-plan support across engines.
    async fn explain(
        &self,
        _query: &BoundQuery,
        _analyze: bool,
    ) -> Result<QueryResult, DriverError> {
        Err(DriverError::Unsupported(
            "execution plans are not supported by this driver".into(),
        ))
    }

    /// List all schemas/databases.
    async fn list_schemas(&self) -> Result<Vec<SchemaObject>, DriverError>;

    /// List tables, optionally scoped to a schema/database.
    async fn list_tables(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError>;

    /// List columns, optionally scoped to a schema/database.
    async fn list_columns(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError>;

    /// One page of schemas/databases after keyword filtering (PRD-F2-05).
    /// Drivers that can push both down override this; the default keeps the
    /// old fetch-all-then-filter behaviour only as a fallback.
    async fn list_schemas_page(
        &self,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let objects = self.list_schemas().await?;
        Ok(filter_and_page_schema_objects(
            objects, keyword, offset, limit,
        ))
    }

    /// One page of tables after keyword filtering (PRD-F2-05).
    async fn list_tables_page(
        &self,
        schema: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let objects = self.list_tables(schema).await?;
        Ok(filter_and_page_schema_objects(
            objects, keyword, offset, limit,
        ))
    }

    /// One page of columns after keyword filtering (PRD-F2-05).
    async fn list_columns_page(
        &self,
        schema: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let objects = self.list_columns(schema).await?;
        Ok(filter_and_page_schema_objects(
            objects, keyword, offset, limit,
        ))
    }

    /// One page of indexes after keyword filtering (PRD-F2-05).
    async fn list_indexes_page(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<IndexPage, DriverError> {
        let indexes = self.list_indexes(schema, table).await?;
        Ok(filter_and_page_indexes(indexes, keyword, offset, limit))
    }

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

    /// Uniform per-table stats for the advisor (PRD-F3-07). Drivers override
    /// this to report sizes and honest estimate provenance; the default only
    /// combines row counts with the index catalog.
    async fn table_stats(&self, schema: Option<&str>) -> Result<Vec<TableStats>, DriverError> {
        let counts = self.approximate_row_counts(schema).await?;
        let indexes = self.list_indexes(schema, None).await?;
        Ok(counts
            .into_iter()
            .map(|c| {
                let table_indexes: Vec<IndexInfo> = indexes
                    .iter()
                    .filter(|i| i.table == c.name)
                    .cloned()
                    .collect();
                TableStats {
                    table: c.name,
                    approx_rows: c.approx_rows,
                    row_estimate: if c.approx_rows.is_some() {
                        RowEstimateKind::CatalogStats
                    } else {
                        RowEstimateKind::Unavailable
                    },
                    table_bytes: None,
                    index_bytes: None,
                    indexes: table_indexes,
                }
            })
            .collect())
    }

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
                    comment: None,
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
                            || o.comment
                                .as_ref()
                                .is_some_and(|c| c.to_lowercase().contains(&kw))
                    })
                    .collect()
            }
            None => objects,
        })
    }

    /// Ping the database to check connectivity.
    async fn ping(&self) -> Result<(), DriverError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(name: &str, table: Option<&str>) -> SchemaObject {
        SchemaObject {
            object_type: ObjectType::Table,
            schema: Some("public".into()),
            name: name.into(),
            table: table.map(str::to_string),
            data_type: None,
            nullable: None,
            comment: None,
        }
    }

    #[test]
    fn page_clamps_offset_and_reports_has_more() {
        let objects = (0..5).map(|i| obj(&format!("t{i}"), None)).collect();
        let page = page_schema_objects(objects, 1, 2);
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].name, "t1");
        assert!(page.has_more);

        let objects = (0..5).map(|i| obj(&format!("t{i}"), None)).collect();
        let page = page_schema_objects(objects, 4, 10);
        assert_eq!(page.objects.len(), 1);
        assert!(!page.has_more);

        let page = page_schema_objects(vec![obj("x", None)], 99, 10);
        assert!(page.objects.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn filter_matches_name_table_and_schema() {
        let objects = vec![obj("users", Some("accounts"))];
        assert_eq!(
            filter_and_page_schema_objects(objects.clone(), Some("user"), 0, 10)
                .objects
                .len(),
            1
        );
        assert_eq!(
            filter_and_page_schema_objects(objects.clone(), Some("acc"), 0, 10)
                .objects
                .len(),
            1
        );
        assert_eq!(
            filter_and_page_schema_objects(objects.clone(), Some("pub"), 0, 10)
                .objects
                .len(),
            1
        );
        assert!(filter_and_page_schema_objects(objects, Some("nope"), 0, 10)
            .objects
            .is_empty());
    }

    #[test]
    fn index_page_filters_and_limits() {
        let index = |name: &str| IndexInfo {
            schema: Some("public".into()),
            table: "users".into(),
            name: name.into(),
            columns: vec!["id".into()],
            unique: false,
            primary: false,
        };
        let indexes = vec![index("idx_a"), index("idx_b"), index("other")];
        let page = filter_and_page_indexes(indexes, Some("idx"), 1, 1);
        assert_eq!(page.indexes.len(), 1);
        assert_eq!(page.indexes[0].name, "idx_b");
        assert!(!page.has_more);
    }
}
