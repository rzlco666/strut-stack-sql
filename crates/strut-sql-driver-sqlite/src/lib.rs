use async_trait::async_trait;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::time::Duration;
use strut_sql_driver_api::{
    BoundQuery, ColumnInfo, ColumnMeta, ConnectOptions, DriverError, EngineDriver, EngineKind,
    ForeignKeyInfo, IndexInfo, ObjectType, QueryMeta, QueryResult, RoutineInfo, SchemaObject,
    SqlValue, TableInfo, TableRowCount,
};

pub struct SqliteDriver {
    pool: SqlitePool,
}

impl SqliteDriver {
    pub async fn new(connection_string: &str) -> Result<Self, DriverError> {
        Self::new_with_options(connection_string, &ConnectOptions::default()).await
    }

    pub async fn new_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Self, DriverError> {
        let connect_url = sqlite_connect_url(connection_string, options.read_only);
        let pool = SqlitePoolOptions::new()
            .max_connections(options.pool_size)
            .acquire_timeout(options.connect_timeout)
            .idle_timeout(Duration::from_secs(45))
            .max_lifetime(Duration::from_secs(300))
            .test_before_acquire(true)
            .connect(&connect_url)
            .await
            .map_err(|e| DriverError::Connection(e.to_string()))?;
        Ok(Self { pool })
    }
}

fn sqlite_connect_url(url: &str, read_only: bool) -> String {
    if !read_only {
        return url.to_string();
    }
    let lower = url.to_lowercase();
    if lower.contains("mode=ro") || lower.contains("mode=readonly") {
        return url.to_string();
    }
    if url.contains('?') {
        format!("{url}&mode=ro")
    } else {
        format!("{url}?mode=ro")
    }
}

#[async_trait]
impl EngineDriver for SqliteDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::Sqlite
    }

    async fn from_connection_string_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Box<Self>, DriverError> {
        Ok(Box::new(
            SqliteDriver::new_with_options(connection_string, options).await?,
        ))
    }

    async fn execute(&self, query: &BoundQuery) -> Result<QueryResult, DriverError> {
        let mut sqlx_query = sqlx::query(&query.sql);
        for param in &query.params {
            sqlx_query = bind_sqlite_value(sqlx_query, param)?;
        }

        if !query.fetch_rows {
            let result = sqlx_query
                .execute(&self.pool)
                .await
                .map_err(|e| DriverError::Execution(e.to_string()))?;
            return Ok(command_result(result.rows_affected()));
        }

        let (rows, truncated) = if let Some(cap) = query.row_cap {
            let mut stream = sqlx_query.fetch(&self.pool);
            let mut rows = Vec::new();
            let mut overflow = false;
            use futures_util::StreamExt;
            while let Some(item) = stream.next().await {
                let row = item.map_err(|e| DriverError::Execution(e.to_string()))?;
                if rows.len() >= cap {
                    overflow = true;
                    break;
                }
                rows.push(row);
            }
            (rows, overflow)
        } else {
            let rows = sqlx_query
                .fetch_all(&self.pool)
                .await
                .map_err(|e| DriverError::Execution(e.to_string()))?;
            (rows, false)
        };

        sqlite_rows_to_result(&rows, truncated)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaObject>, DriverError> {
        let rows = sqlx::query("PRAGMA database_list")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let name: String = r.try_get(1).ok()?;
                Some(SchemaObject {
                    object_type: ObjectType::Schema,
                    schema: None,
                    name,
                    table: None,
                    data_type: None,
                    nullable: None,
                })
            })
            .collect())
    }

    async fn resolve_schema(&self, schema: Option<&str>) -> Result<String, DriverError> {
        Ok(resolve_sqlite_schema(schema).to_string())
    }

    async fn list_tables(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let schema_name = resolve_sqlite_schema(schema);
        let rows = sqlite_list_table_rows(&self.pool, schema_name)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let name: String = r.try_get(0).ok()?;
                Some(SchemaObject {
                    object_type: ObjectType::Table,
                    schema: Some(schema_name.to_string()),
                    name,
                    table: None,
                    data_type: None,
                    nullable: None,
                })
            })
            .collect())
    }

    async fn list_columns(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let schema_name = resolve_sqlite_schema(schema);
        let rows = sqlx::query(
            "SELECT m.name AS table_name, p.name, p.type, p.notnull \
             FROM sqlite_master AS m, pragma_table_info(m.name) AS p \
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' \
             ORDER BY m.name, p.cid",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let table: String = r.try_get(0).ok()?;
                let name: String = r.try_get(1).ok()?;
                let data_type: String = r.try_get(2).ok()?;
                let notnull: i64 = r.try_get(3).ok()?;
                Some(SchemaObject {
                    object_type: ObjectType::Column,
                    schema: Some(schema_name.to_string()),
                    name,
                    table: Some(table),
                    data_type: Some(data_type),
                    nullable: Some(notnull == 0),
                })
            })
            .collect())
    }

    async fn describe_table(
        &self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<TableInfo, DriverError> {
        let (schema_name, table_name) = split_sqlite_table(schema, table);
        let sql = format!(
            "SELECT name, type, \"notnull\", pk FROM pragma_table_info({})",
            sqlite_quote_literal(&table_name)
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let columns = rows
            .iter()
            .filter_map(|r| {
                let name: String = r.try_get(0).ok()?;
                let data_type: String = r.try_get(1).ok()?;
                let notnull: i64 = r.try_get(2).ok()?;
                let pk: i64 = r.try_get(3).unwrap_or(0);
                Some(ColumnInfo {
                    name,
                    data_type,
                    nullable: notnull == 0,
                    default_value: None,
                    is_primary_key: pk > 0,
                    key: None,
                    extra: None,
                    comment: None,
                })
            })
            .collect();

        Ok(TableInfo {
            schema: Some(schema_name),
            name: table_name,
            columns,
        })
    }

    async fn list_indexes(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<IndexInfo>, DriverError> {
        let schema_name = resolve_sqlite_schema(schema).to_string();
        if let Some(table) = table {
            return sqlite_indexes_for_tables(&self.pool, &schema_name, &[table.to_string()])
                .await
                .map_err(|e| DriverError::Introspection(e.to_string()));
        }
        if schema_name == "main" {
            match sqlite_indexes_batched(&self.pool, &schema_name).await {
                Ok(objects) => return Ok(objects),
                Err(e) => {
                    tracing::warn!(error = %e, "batched sqlite index listing failed; falling back to per-table pragmas");
                }
            }
        }
        let tables: Vec<String> = sqlite_list_table_rows(&self.pool, &schema_name)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?
            .into_iter()
            .filter_map(|r| r.try_get(0).ok())
            .collect();
        sqlite_indexes_for_tables(&self.pool, &schema_name, &tables)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))
    }

    async fn list_foreign_keys(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        let schema_name = resolve_sqlite_schema(schema).to_string();
        let table = table.ok_or_else(|| {
            DriverError::InvalidConfig("list_foreign_keys on SQLite requires table".into())
        })?;
        let pragma = format!("PRAGMA foreign_key_list({})", sqlite_quote_literal(table));
        let rows = sqlx::query(&pragma)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let id: i64 = r.try_get("id").ok()?;
                let ref_table: String = r.try_get("table").ok()?;
                let from_col: String = r.try_get("from").ok()?;
                let to_col: String = r.try_get("to").ok()?;
                let on_delete: String = r.try_get("on_delete").ok()?;
                let on_update: String = r.try_get("on_update").ok()?;
                Some(ForeignKeyInfo {
                    schema: Some(schema_name.clone()),
                    table: table.to_string(),
                    name: format!("fk_{id}"),
                    columns: vec![from_col],
                    ref_schema: None,
                    ref_table,
                    ref_columns: vec![to_col],
                    on_delete: Some(on_delete),
                    on_update: Some(on_update),
                })
            })
            .collect())
    }

    async fn approximate_row_counts(
        &self,
        schema: Option<&str>,
    ) -> Result<Vec<TableRowCount>, DriverError> {
        Ok(self
            .list_tables(schema)
            .await?
            .into_iter()
            .map(|o| TableRowCount {
                name: o.name,
                approx_rows: None,
            })
            .collect())
    }

    async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError> {
        // SQLite has no stored procedures/functions in the catalog.
        Ok(Vec::new())
    }

    async fn ping(&self) -> Result<(), DriverError> {
        sqlx::query("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DriverError::Execution(e.to_string()))?;
        Ok(())
    }
}

fn command_result(rows_affected: u64) -> QueryResult {
    QueryResult {
        cols: Vec::new(),
        rows: Vec::new(),
        meta: QueryMeta {
            row_count: 0,
            truncated: false,
            rows_affected: Some(rows_affected),
        },
    }
}

fn bind_sqlite_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    param: &'q SqlValue,
) -> Result<sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>, DriverError> {
    match param {
        SqlValue::Null => Ok(query.bind(None::<String>)),
        SqlValue::Bool(b) => Ok(query.bind(*b)),
        SqlValue::I64(n) => Ok(query.bind(*n)),
        SqlValue::F64(f) => Ok(query.bind(*f)),
        SqlValue::String(s) => Ok(query.bind(s.as_str())),
        SqlValue::Uuid(u) => Ok(query.bind(*u)),
        // sqlx SQLite has no DECIMAL type: bind the exact decimal text.
        SqlValue::BigDecimal(d) => Ok(query.bind(d.to_string())),
        SqlValue::Date(d) => Ok(query.bind(*d)),
        SqlValue::DateTime(dt) => Ok(query.bind(*dt)),
        SqlValue::Binary(b) => Ok(query.bind(b.as_slice())),
        // SQLite has no array type; the root engine rejects array parameters.
        SqlValue::Array(_) => Err(DriverError::Unsupported(
            "array parameters are not supported on SQLite".into(),
        )),
        SqlValue::Json(j) => Ok(query.bind(j.to_string())),
    }
}

fn sqlite_rows_to_result(rows: &[SqliteRow], truncated: bool) -> Result<QueryResult, DriverError> {
    if rows.is_empty() {
        return Ok(QueryResult {
            cols: Vec::new(),
            rows: Vec::new(),
            meta: QueryMeta {
                row_count: 0,
                truncated,
                rows_affected: None,
            },
        });
    }

    let cols = rows[0]
        .columns()
        .iter()
        .map(|c| ColumnMeta {
            name: c.name().to_string(),
            type_name: c.type_info().name().to_string(),
            nullable: true,
        })
        .collect();

    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut out_row = Vec::with_capacity(row.columns().len());
        for (i, _col) in row.columns().iter().enumerate() {
            out_row.push(decode_sqlite_cell(row, i)?);
        }
        out_rows.push(out_row);
    }

    let row_count = out_rows.len();
    Ok(QueryResult {
        cols,
        rows: out_rows,
        meta: QueryMeta {
            row_count,
            truncated,
            rows_affected: None,
        },
    })
}

fn decode_sqlite_cell(row: &SqliteRow, index: usize) -> Result<SqlValue, DriverError> {
    if row.columns().get(index).is_none() {
        return Err(DriverError::Execution(format!(
            "column index {index} out of range"
        )));
    }

    let raw = row
        .try_get_raw(index)
        .map_err(|e| DriverError::Execution(e.to_string()))?;
    if raw.is_null() {
        return Ok(SqlValue::Null);
    }

    // SQLite storage classes, in the same order the root engine uses:
    // INTEGER -> REAL -> TEXT -> BLOB. No value is ever silently dropped.
    if let Ok(v) = row.try_get::<i64, _>(index) {
        return Ok(SqlValue::I64(v));
    }
    if let Ok(v) = row.try_get::<f64, _>(index) {
        return Ok(SqlValue::F64(v));
    }
    if let Ok(v) = row.try_get::<String, _>(index) {
        return Ok(SqlValue::String(v));
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
        return Ok(SqlValue::Binary(v));
    }
    Ok(SqlValue::Null)
}

fn resolve_sqlite_schema(schema: Option<&str>) -> &str {
    schema.filter(|s| !s.is_empty()).unwrap_or("main")
}

/// Split `schema.table` or `"schema"."table"` into (schema, bare table).
fn split_sqlite_table(schema: Option<&str>, table: &str) -> (String, String) {
    let trimmed = table.trim().trim_matches('"');
    if let Some((sch, tbl)) = trimmed.rsplit_once('.') {
        let sch = sch.trim().trim_matches('"');
        let tbl = tbl.trim().trim_matches('"');
        if !sch.is_empty() && !tbl.is_empty() {
            return (sch.to_string(), tbl.to_string());
        }
    }
    (
        resolve_sqlite_schema(schema).to_string(),
        trimmed.to_string(),
    )
}

fn sqlite_quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn sqlite_quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn sqlite_list_table_rows(
    pool: &SqlitePool,
    schema_name: &str,
) -> Result<Vec<SqliteRow>, sqlx::Error> {
    if schema_name == "main" {
        sqlx::query(
            "SELECT name FROM sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .fetch_all(pool)
        .await
    } else {
        // SQL-SEC-11: quote the schema identifier properly (double any
        // embedded `"`) instead of interpolating the raw caller input.
        let sql = format!(
            "SELECT name FROM {}.sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
            sqlite_quote_ident(schema_name)
        );
        sqlx::query(&sql).fetch_all(pool).await
    }
}

/// Batched SQLite index listing: 2 statements total for any table count.
async fn sqlite_indexes_batched(
    pool: &SqlitePool,
    schema_name: &str,
) -> Result<Vec<IndexInfo>, sqlx::Error> {
    let list_rows = sqlx::query(
        "SELECT t.name, il.name, il.\"unique\", il.origin \
         FROM sqlite_master AS t, pragma_index_list(t.name) AS il \
         WHERE t.type = 'table' AND t.name NOT LIKE 'sqlite_%' \
         ORDER BY t.name, il.seq",
    )
    .fetch_all(pool)
    .await?;

    let info_rows = sqlx::query(
        "SELECT m.name, p.name \
         FROM sqlite_master AS m, pragma_index_info(m.name) AS p \
         WHERE m.type = 'index' AND m.name NOT LIKE 'sqlite_%' \
         ORDER BY m.name, p.seqno",
    )
    .fetch_all(pool)
    .await?;

    let mut columns_by_index: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for row in &info_rows {
        let idx: String = row.try_get(0)?;
        let col: String = row.try_get(1)?;
        columns_by_index.entry(idx).or_default().push(col);
    }

    let mut objects = Vec::with_capacity(list_rows.len());
    for row in &list_rows {
        let table_name: String = row.try_get(0)?;
        let idx_name: String = row.try_get(1)?;
        let unique: i64 = row.try_get(2)?;
        let origin: String = row.try_get(3).unwrap_or_default();
        objects.push(IndexInfo {
            schema: Some(schema_name.to_string()),
            table: table_name,
            name: idx_name.clone(),
            columns: columns_by_index.remove(&idx_name).unwrap_or_default(),
            unique: unique != 0,
            primary: origin == "pk",
        });
    }
    Ok(objects)
}

async fn sqlite_indexes_for_tables(
    pool: &SqlitePool,
    schema_name: &str,
    tables: &[String],
) -> Result<Vec<IndexInfo>, sqlx::Error> {
    let mut objects = Vec::new();
    for table_name in tables {
        let index_sql = format!("PRAGMA index_list({})", sqlite_quote_literal(table_name));
        let index_rows = sqlx::query(&index_sql).fetch_all(pool).await?;
        for row in index_rows {
            let idx_name: String = row.try_get(1)?;
            let unique: i64 = row.try_get(2)?;
            let origin: String = row.try_get(3).unwrap_or_default();
            let info_sql = format!("PRAGMA index_info({})", sqlite_quote_literal(&idx_name));
            let info_rows = sqlx::query(&info_sql).fetch_all(pool).await?;
            let columns: Vec<String> = info_rows
                .iter()
                .filter_map(|r| r.try_get::<String, _>(2).ok())
                .collect();
            objects.push(IndexInfo {
                schema: Some(schema_name.to_string()),
                table: table_name.clone(),
                name: idx_name,
                columns,
                unique: unique != 0,
                primary: origin == "pk",
            });
        }
    }
    Ok(objects)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_literal_doubles_embedded_quotes() {
        assert_eq!(sqlite_quote_literal("o'brien"), "'o''brien'");
    }

    #[test]
    fn sqlite_quote_ident_doubles_embedded_quotes() {
        assert_eq!(sqlite_quote_ident("main"), "\"main\"");
        assert_eq!(sqlite_quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn split_table_splits_qualified_names() {
        assert_eq!(
            split_sqlite_table(Some("main"), "main.t"),
            ("main".to_string(), "t".to_string())
        );
        assert_eq!(
            split_sqlite_table(None, "\"main\".\"t\""),
            ("main".to_string(), "t".to_string())
        );
        assert_eq!(
            split_sqlite_table(None, "t"),
            ("main".to_string(), "t".to_string())
        );
    }
}
