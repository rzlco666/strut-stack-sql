use async_trait::async_trait;
use serde_json::Value;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::time::Duration;
use strut_sql_driver_api::{
    BoundQuery, ColumnInfo, ColumnMeta, DriverError, EngineDriver, EngineKind, ForeignKeyInfo,
    IndexInfo, ObjectType, QueryMeta, QueryResult, RoutineInfo, SchemaObject, SqlValue, TableInfo,
};

pub struct SqliteDriver {
    pool: SqlitePool,
}

impl SqliteDriver {
    pub async fn new(connection_string: &str) -> Result<Self, DriverError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(2))
            .connect(connection_string)
            .await
            .map_err(|e| DriverError::Connection(e.to_string()))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl EngineDriver for SqliteDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::Sqlite
    }

    async fn from_connection_string(connection_string: &str) -> Result<Box<Self>, DriverError> {
        Ok(Box::new(SqliteDriver::new(connection_string).await?))
    }

    async fn execute(&self, query: &BoundQuery) -> Result<QueryResult, DriverError> {
        let mut sqlx_query = sqlx::query(&query.sql);
        for param in &query.params {
            sqlx_query = bind_sqlite_value(sqlx_query, param)?;
        }

        let mut rows = sqlx_query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Execution(e.to_string()))?;

        let mut truncated = false;
        if let Some(cap) = query.row_cap {
            if rows.len() > cap {
                rows.truncate(cap);
                truncated = true;
            }
        }

        sqlite_rows_to_result(&rows, truncated)
    }

    async fn list_tables(&self) -> Result<Vec<SchemaObject>, DriverError> {
        let rows = sqlx::query(
            r#"
            SELECT name
            FROM sqlite_master
            WHERE type = 'table'
            AND name NOT LIKE 'sqlite_%'
            ORDER BY name
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(SchemaObject {
                    object_type: ObjectType::Table,
                    schema: None, // SQLite has no schemas; `main` is implicit
                    name: r.try_get("name").ok()?,
                    display_name: None,
                })
            })
            .collect())
    }

    async fn describe_table(
        &self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<TableInfo, DriverError> {
        let table = split_sqlite_table(schema, table);
        let rows = sqlx::query(&format!(
            "PRAGMA table_info({})",
            sqlite_quote_literal(&table)
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let columns = rows
            .iter()
            .filter_map(|r| {
                Some(ColumnInfo {
                    name: r.try_get("name").ok()?,
                    data_type: r.try_get("type").unwrap_or_default(),
                    nullable: r.try_get::<i64, _>("notnull").unwrap_or(1) == 0,
                    default_value: r.try_get("dflt_value").ok(),
                    is_primary_key: r.try_get::<i64, _>("pk").unwrap_or(0) > 0,
                })
            })
            .collect();

        Ok(TableInfo {
            schema: schema.map(|s| s.to_string()),
            name: table,
            columns,
        })
    }

    async fn list_indexes(&self) -> Result<Vec<IndexInfo>, DriverError> {
        let tables = self.list_tables().await?;
        let mut indexes = Vec::new();

        for table_obj in tables {
            let table = &table_obj.name;
            let index_rows = sqlx::query(&format!(
                "PRAGMA index_list({})",
                sqlite_quote_literal(table)
            ))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;

            for idx_row in &index_rows {
                let Ok(index_name) = idx_row.try_get::<String, _>("name") else {
                    continue;
                };
                let unique = idx_row
                    .try_get::<i64, _>("unique")
                    .map(|v| v != 0)
                    .unwrap_or(false);
                let origin: String = idx_row.try_get("origin").unwrap_or_default();

                let info_rows = sqlx::query(&format!(
                    "PRAGMA index_info({})",
                    sqlite_quote_literal(&index_name)
                ))
                .fetch_all(&self.pool)
                .await
                .map_err(|e| DriverError::Introspection(e.to_string()))?;

                let columns = info_rows
                    .iter()
                    .filter_map(|r| r.try_get::<String, _>("name").ok())
                    .collect();

                indexes.push(IndexInfo {
                    schema: None,
                    table: table.clone(),
                    name: index_name,
                    columns,
                    unique,
                    primary: origin == "pk",
                });
            }
        }

        indexes.sort_by(|a, b| (&a.table, &a.name).cmp(&(&b.table, &b.name)));
        Ok(indexes)
    }

    async fn list_foreign_keys(&self) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        let tables = self.list_tables().await?;
        let mut fks: Vec<ForeignKeyInfo> = Vec::new();

        for table_obj in tables {
            let table = &table_obj.name;
            let fk_rows = sqlx::query(&format!(
                "PRAGMA foreign_key_list({})",
                sqlite_quote_literal(table)
            ))
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;

            for fk_row in &fk_rows {
                let Ok(id) = fk_row.try_get::<i64, _>("id") else {
                    continue;
                };
                let Ok(column) = fk_row.try_get::<String, _>("from") else {
                    continue;
                };
                let Ok(ref_table) = fk_row.try_get::<String, _>("table") else {
                    continue;
                };
                // `to` is NULL when the FK targets the referenced table's PK.
                let ref_column: String = fk_row.try_get("to").unwrap_or_default();
                let on_update: Option<String> = fk_row.try_get("on_update").ok();
                let on_delete: Option<String> = fk_row.try_get("on_delete").ok();

                if let Some(fk) = fks
                    .iter_mut()
                    .find(|fk| fk.table == *table && fk.name == format!("fk_{id}"))
                {
                    fk.columns.push(column);
                    fk.ref_columns.push(ref_column);
                } else {
                    fks.push(ForeignKeyInfo {
                        schema: None,
                        table: table.clone(),
                        name: format!("fk_{id}"),
                        columns: vec![column],
                        ref_schema: None,
                        ref_table,
                        ref_columns: vec![ref_column],
                        on_delete,
                        on_update,
                    });
                }
            }
        }

        Ok(fks)
    }

    async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError> {
        // SQLite has no stored procedures/functions in the catalog.
        Ok(Vec::new())
    }

    async fn search_objects(
        &self,
        keyword: Option<&str>,
        object_type: Option<ObjectType>,
    ) -> Result<Vec<SchemaObject>, DriverError> {
        // Filter in Rust so the keyword never becomes SQL text.
        let rows = sqlx::query(
            r#"
            SELECT name, type
            FROM sqlite_master
            WHERE name NOT LIKE 'sqlite_%'
            AND type IN ('table', 'index')
            ORDER BY name
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let needle = keyword.map(|k| k.to_ascii_lowercase());
        Ok(rows
            .iter()
            .filter_map(|r| {
                let raw_type: String = r.try_get("type").ok()?;
                let name: String = r.try_get("name").ok()?;
                let parsed_type = match raw_type.as_str() {
                    "index" => ObjectType::Index,
                    _ => ObjectType::Table,
                };
                if let Some(want) = object_type {
                    if parsed_type != want {
                        return None;
                    }
                }
                if let Some(needle) = &needle {
                    if !name.to_ascii_lowercase().contains(needle) {
                        return None;
                    }
                }
                Some(SchemaObject {
                    object_type: parsed_type,
                    schema: None,
                    name,
                    display_name: None,
                })
            })
            .collect())
    }

    async fn ping(&self) -> Result<(), DriverError> {
        sqlx::query("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DriverError::Execution(e.to_string()))?;
        Ok(())
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
        // SQLite has no UUID/DECIMAL storage class: store their exact text.
        SqlValue::Uuid(u) => Ok(query.bind(u.to_string())),
        SqlValue::BigDecimal(d) => Ok(query.bind(d.to_string())),
        SqlValue::Date(d) => Ok(query.bind(d.to_string())),
        SqlValue::DateTime(dt) => Ok(query.bind(dt.to_string())),
        SqlValue::Binary(b) => Ok(query.bind(b.as_slice())),
        // SQLite has no array type: store compact JSON text.
        SqlValue::Array(arr) => {
            let json: Value = serde_json::to_value(arr)
                .map_err(|e| DriverError::Execution(format!("array serialization: {e}")))?;
            Ok(query.bind(json.to_string()))
        }
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

    // Compute before the move: `out_rows.len()` is not available afterwards.
    let row_count = out_rows.len();
    Ok(QueryResult {
        cols,
        rows: out_rows,
        meta: QueryMeta {
            row_count,
            truncated,
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

/// Quote a SQLite string literal for PRAGMA argument positions.
fn sqlite_quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Strip a `schema.` qualifier from a table name (SQLite schemas are implicit).
fn split_sqlite_table(schema: Option<&str>, table: &str) -> String {
    let _ = schema;
    let trimmed = table.trim().trim_matches('"');
    match trimmed.rsplit_once('.') {
        Some((_, bare)) if !bare.trim().trim_matches('"').is_empty() => {
            bare.trim().trim_matches('"').to_string()
        }
        _ => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_literal_doubles_embedded_quotes() {
        assert_eq!(sqlite_quote_literal("o'brien"), "'o''brien'");
    }

    #[test]
    fn split_table_strips_schema_qualifier() {
        assert_eq!(split_sqlite_table(Some("main"), "main.t"), "t");
        assert_eq!(split_sqlite_table(None, "\"main\".\"t\""), "t");
        assert_eq!(split_sqlite_table(None, "t"), "t");
    }
}
