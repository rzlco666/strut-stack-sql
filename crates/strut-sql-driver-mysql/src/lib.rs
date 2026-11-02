use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use serde_json::Value;
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::collections::BTreeMap;
use std::time::Duration;
use strut_sql_driver_api::{
    BoundQuery, ColumnInfo, ColumnMeta, DriverError, EngineDriver, EngineKind, ForeignKeyInfo,
    IndexInfo, ObjectType, QueryMeta, QueryResult, RoutineInfo, SchemaObject, SqlValue, TableInfo,
};

pub struct MySqlDriver {
    pool: MySqlPool,
}

impl MySqlDriver {
    pub async fn new(connection_string: &str) -> Result<Self, DriverError> {
        let pool = MySqlPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(2))
            .connect(connection_string)
            .await
            .map_err(|e| DriverError::Connection(e.to_string()))?;
        Ok(Self { pool })
    }

    /// Current database of the connection, used as the default schema.
    async fn default_schema(&self) -> Result<String, DriverError> {
        let row = sqlx::query("SELECT DATABASE()")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
        row.try_get::<Option<String>, _>(0)
            .map_err(|e| DriverError::Introspection(e.to_string()))?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                DriverError::InvalidConfig(
                    "no default schema selected; connect with a database in the URL or pass schema"
                        .into(),
                )
            })
    }
}

#[async_trait]
impl EngineDriver for MySqlDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::MySql
    }

    async fn from_connection_string(connection_string: &str) -> Result<Box<Self>, DriverError> {
        Ok(Box::new(MySqlDriver::new(connection_string).await?))
    }

    async fn execute(&self, query: &BoundQuery) -> Result<QueryResult, DriverError> {
        let mut sqlx_query = sqlx::query(&query.sql);
        for param in &query.params {
            sqlx_query = bind_mysql_value(sqlx_query, param)?;
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

        mysql_rows_to_result(&rows, truncated)
    }

    async fn list_tables(&self) -> Result<Vec<SchemaObject>, DriverError> {
        // information_schema text columns arrive with a binary charset on
        // MySQL 8 under prepared statements, so names are decoded tolerantly.
        let rows = sqlx::query(
            r#"
            SELECT table_schema, table_name
            FROM information_schema.tables
            WHERE table_type = 'BASE TABLE'
              AND (
                table_schema = DATABASE()
                OR (
                  DATABASE() IS NULL
                  AND table_schema NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys')
                )
              )
            ORDER BY table_schema, table_name
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
                    schema: mysql_decode_text_by_index(r, 0),
                    name: mysql_decode_text_by_index(r, 1)?,
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
        let schema_name = match schema.filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => self.default_schema().await?,
        };
        let table = normalize_mysql_ident(table);

        // SHOW FULL COLUMNS is the authoritative per-table view; fall back to
        // information_schema when the table is not visible through SHOW.
        let mut columns = mysql_columns_from_show(&self.pool, &schema_name, &table).await?;
        if columns.is_empty() {
            columns =
                mysql_columns_from_information_schema(&self.pool, &schema_name, &table).await?;
        }

        Ok(TableInfo {
            schema: Some(schema_name),
            name: table,
            columns,
        })
    }

    async fn list_indexes(&self) -> Result<Vec<IndexInfo>, DriverError> {
        let rows = sqlx::query(
            r#"
            SELECT
                table_schema,
                table_name,
                index_name,
                column_name,
                non_unique
            FROM information_schema.statistics
            WHERE table_schema = DATABASE() OR DATABASE() IS NULL
            ORDER BY table_schema, table_name, index_name, seq_in_index
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        // Group by the composite (schema, table, index). Grouping on the index
        // name alone would merge same-named indexes across tables.
        let mut index_map: BTreeMap<(String, String, String), IndexInfo> = BTreeMap::new();
        for row in &rows {
            let Some(schema) = mysql_decode_text_by_index(row, 0) else {
                continue;
            };
            let Some(table) = mysql_decode_text_by_index(row, 1) else {
                continue;
            };
            let Some(index_name) = mysql_decode_text_by_index(row, 2) else {
                continue;
            };
            let Some(column_name) = mysql_decode_text_by_index(row, 3) else {
                continue;
            };
            let non_unique = mysql_int_by_index(row, 4).unwrap_or(1);

            let is_primary = index_name == "PRIMARY";
            let entry = index_map
                .entry((schema.clone(), table.clone(), index_name.clone()))
                .or_insert_with(|| IndexInfo {
                    schema: Some(schema),
                    table,
                    name: index_name,
                    columns: Vec::new(),
                    unique: non_unique == 0,
                    primary: is_primary,
                });
            entry.columns.push(column_name);
        }

        Ok(index_map.into_values().collect())
    }

    async fn list_foreign_keys(&self) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        // MySQL has no information_schema.constraint_column_usage; foreign
        // keys come from KEY_COLUMN_USAGE joined to REFERENTIAL_CONSTRAINTS.
        let rows = sqlx::query(
            r#"
            SELECT
                kcu.TABLE_SCHEMA,
                kcu.TABLE_NAME,
                kcu.CONSTRAINT_NAME,
                kcu.COLUMN_NAME,
                kcu.REFERENCED_TABLE_SCHEMA,
                kcu.REFERENCED_TABLE_NAME,
                kcu.REFERENCED_COLUMN_NAME,
                rc.DELETE_RULE,
                rc.UPDATE_RULE
            FROM information_schema.KEY_COLUMN_USAGE kcu
            JOIN information_schema.REFERENTIAL_CONSTRAINTS rc
              ON kcu.CONSTRAINT_NAME = rc.CONSTRAINT_NAME
             AND kcu.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA
            WHERE (kcu.TABLE_SCHEMA = DATABASE() OR DATABASE() IS NULL)
              AND kcu.REFERENCED_TABLE_NAME IS NOT NULL
            ORDER BY kcu.TABLE_SCHEMA, kcu.TABLE_NAME, kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let mut fk_map: BTreeMap<(String, String, String), ForeignKeyInfo> = BTreeMap::new();
        for row in &rows {
            let Some(schema) = mysql_decode_text_by_index(row, 0) else {
                continue;
            };
            let Some(table) = mysql_decode_text_by_index(row, 1) else {
                continue;
            };
            let Some(constraint) = mysql_decode_text_by_index(row, 2) else {
                continue;
            };
            let Some(column) = mysql_decode_text_by_index(row, 3) else {
                continue;
            };
            let ref_schema = mysql_decode_text_by_index(row, 4);
            let Some(ref_table) = mysql_decode_text_by_index(row, 5) else {
                continue;
            };
            let ref_column = mysql_decode_text_by_index(row, 6).unwrap_or_default();
            let on_delete = mysql_decode_text_by_index(row, 7);
            let on_update = mysql_decode_text_by_index(row, 8);

            let key = (schema.clone(), table.clone(), constraint.clone());
            let entry = fk_map.entry(key).or_insert_with(|| ForeignKeyInfo {
                schema: Some(schema),
                table,
                name: constraint,
                columns: Vec::new(),
                ref_schema,
                ref_table,
                ref_columns: Vec::new(),
                on_delete,
                on_update,
            });
            entry.columns.push(column);
            entry.ref_columns.push(ref_column);
        }

        Ok(fk_map.into_values().collect())
    }

    async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError> {
        let rows = sqlx::query(
            r#"
            SELECT
                routine_schema,
                routine_name,
                routine_type,
                dtd_identifier
            FROM information_schema.routines
            WHERE routine_schema = DATABASE() OR DATABASE() IS NULL
            ORDER BY routine_schema, routine_name
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(RoutineInfo {
                    schema: mysql_decode_text_by_index(r, 0),
                    name: mysql_decode_text_by_index(r, 1)?,
                    routine_type: mysql_decode_text_by_index(r, 2)
                        .unwrap_or_else(|| "FUNCTION".to_string()),
                    return_type: mysql_decode_text_by_index(r, 3),
                })
            })
            .collect())
    }

    async fn search_objects(
        &self,
        keyword: Option<&str>,
        object_type: Option<ObjectType>,
    ) -> Result<Vec<SchemaObject>, DriverError> {
        // Filter in Rust so the keyword never becomes SQL text.
        let needle = keyword.map(|k| k.to_ascii_lowercase());

        let objects = if object_type == Some(ObjectType::Schema) {
            let rows = sqlx::query(
                r#"
                SELECT schema_name
                FROM information_schema.schemata
                WHERE schema_name NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys')
                ORDER BY schema_name
                "#,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
            rows.iter()
                .filter_map(|r| {
                    Some(SchemaObject {
                        object_type: ObjectType::Schema,
                        schema: None,
                        name: mysql_decode_text_by_index(r, 0)?,
                        display_name: None,
                    })
                })
                .collect::<Vec<_>>()
        } else {
            let rows = sqlx::query(
                r#"
                SELECT table_schema, table_name
                FROM information_schema.tables
                WHERE table_type = 'BASE TABLE'
                  AND (
                    table_schema = DATABASE()
                    OR (
                      DATABASE() IS NULL
                      AND table_schema NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys')
                    )
                  )
                ORDER BY table_schema, table_name
                "#,
            )
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
            rows.iter()
                .filter_map(|r| {
                    Some(SchemaObject {
                        object_type: ObjectType::Table,
                        schema: mysql_decode_text_by_index(r, 0),
                        name: mysql_decode_text_by_index(r, 1)?,
                        display_name: None,
                    })
                })
                .collect::<Vec<_>>()
        };

        Ok(match needle {
            Some(needle) => objects
                .into_iter()
                .filter(|o| o.name.to_ascii_lowercase().contains(&needle))
                .collect(),
            None => objects,
        })
    }

    async fn ping(&self) -> Result<(), DriverError> {
        sqlx::query("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DriverError::Execution(e.to_string()))?;
        Ok(())
    }
}

async fn mysql_columns_from_show(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, DriverError> {
    let sql = format!(
        "SHOW FULL COLUMNS FROM {}.{}",
        mysql_quote_ident(schema),
        mysql_quote_ident(table)
    );
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

    Ok(rows
        .iter()
        .filter_map(|r| {
            let name = mysql_decode_text_by_name(r, "Field")
                .or_else(|| mysql_decode_text_by_index(r, 0))?;
            let data_type = mysql_decode_text_by_name(r, "Type")
                .or_else(|| mysql_decode_text_by_index(r, 1))?;
            let nullable = mysql_decode_text_by_name(r, "Null")
                .or_else(|| mysql_decode_text_by_index(r, 2))
                .is_some_and(|v| v.eq_ignore_ascii_case("YES"));
            let key =
                mysql_decode_text_by_name(r, "Key").or_else(|| mysql_decode_text_by_index(r, 3));
            let default_value = mysql_decode_text_by_name(r, "Default")
                .or_else(|| mysql_decode_text_by_index(r, 4));
            Some(ColumnInfo {
                name,
                data_type,
                nullable,
                default_value,
                is_primary_key: key.as_deref() == Some("PRI"),
            })
        })
        .collect())
}

async fn mysql_columns_from_information_schema(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, DriverError> {
    let rows = sqlx::query(
        r#"
        SELECT column_name, column_type, is_nullable, column_default, column_key
        FROM information_schema.columns
        WHERE table_schema = ? AND table_name = ?
        ORDER BY ordinal_position
        "#,
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .map_err(|e| DriverError::Introspection(e.to_string()))?;

    Ok(rows
        .iter()
        .filter_map(|r| {
            let name = mysql_decode_text_by_index(r, 0)?;
            let data_type = mysql_decode_text_by_index(r, 1)?;
            let nullable =
                mysql_decode_text_by_index(r, 2).is_some_and(|v| v.eq_ignore_ascii_case("YES"));
            Some(ColumnInfo {
                name,
                data_type,
                nullable,
                default_value: mysql_decode_text_by_index(r, 3),
                is_primary_key: mysql_decode_text_by_index(r, 4).as_deref() == Some("PRI"),
            })
        })
        .collect())
}

fn mysql_quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

/// Strip surrounding backticks and `` `schema`.`table` `` qualifiers.
fn normalize_mysql_ident(s: &str) -> String {
    let trimmed = s.trim();
    let trimmed = trimmed.trim_matches('`');
    match trimmed.rsplit_once('.') {
        Some((_, table)) if !table.trim().trim_matches('`').is_empty() => {
            table.trim().trim_matches('`').to_string()
        }
        _ => trimmed.to_string(),
    }
}

fn bind_mysql_value<'q>(
    query: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    param: &'q SqlValue,
) -> Result<sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>, DriverError> {
    match param {
        SqlValue::Null => Ok(query.bind(None::<String>)),
        SqlValue::Bool(b) => Ok(query.bind(*b)),
        SqlValue::I64(n) => Ok(query.bind(*n)),
        SqlValue::F64(f) => Ok(query.bind(*f)),
        SqlValue::String(s) => Ok(query.bind(s.as_str())),
        // MySQL has no native UUID type.
        SqlValue::Uuid(u) => Ok(query.bind(u.to_string())),
        SqlValue::BigDecimal(d) => Ok(query.bind(d)),
        SqlValue::Date(d) => Ok(query.bind(*d)),
        SqlValue::DateTime(dt) => Ok(query.bind(*dt)),
        SqlValue::Binary(b) => Ok(query.bind(b.as_slice())),
        // MySQL has no array type; the root engine rejects array parameters
        // rather than silently stringifying them.
        SqlValue::Array(_) => Err(DriverError::Unsupported(
            "array parameters are not supported on MySQL".into(),
        )),
        SqlValue::Json(j) => Ok(query.bind(j.clone())),
    }
}

fn mysql_rows_to_result(rows: &[MySqlRow], truncated: bool) -> Result<QueryResult, DriverError> {
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
            out_row.push(decode_mysql_cell(row, i)?);
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
        },
    })
}

fn mysql_int_by_index(row: &MySqlRow, index: usize) -> Option<i64> {
    if let Ok(v) = row.try_get::<i64, _>(index) {
        return Some(v);
    }
    if let Ok(v) = row.try_get::<u64, _>(index) {
        return Some(v as i64);
    }
    row.try_get::<i32, _>(index).ok().map(i64::from)
}

fn mysql_type_is_bool(type_name: &str) -> bool {
    type_name.eq_ignore_ascii_case("BOOL") || type_name.eq_ignore_ascii_case("BOOLEAN")
}

fn mysql_type_is_text(type_name: &str) -> bool {
    let t = type_name.to_ascii_uppercase();
    matches!(
        t.as_str(),
        "VARCHAR"
            | "CHAR"
            | "TEXT"
            | "TINYTEXT"
            | "MEDIUMTEXT"
            | "LONGTEXT"
            | "ENUM"
            | "SET"
            | "JSON"
            | "VAR_STRING"
            | "STRING"
    ) || t.contains("CHAR")
        || t.contains("TEXT")
}

fn mysql_type_is_binary(type_name: &str) -> bool {
    let t = type_name.to_ascii_uppercase();
    matches!(
        t.as_str(),
        "BLOB"
            | "TINYBLOB"
            | "MEDIUMBLOB"
            | "LONGBLOB"
            | "BINARY"
            | "VARBINARY"
            | "BIT"
            | "GEOMETRY"
            | "POINT"
            | "LINESTRING"
            | "POLYGON"
    ) || t.contains("BLOB")
        || t.contains("BINARY")
}

fn utf8_from_bytes(v: Vec<u8>) -> String {
    String::from_utf8(v).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Tolerant MySQL text decoder for information_schema columns that may have a
/// binary charset (String -> &str -> bytes, lossily decoded).
fn mysql_decode_text_by_index(row: &MySqlRow, index: usize) -> Option<String> {
    if let Ok(v) = row.try_get::<String, _>(index) {
        return Some(v);
    }
    if let Ok(v) = row.try_get::<&str, _>(index) {
        return Some(v.to_string());
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
        return Some(utf8_from_bytes(v));
    }
    None
}

fn mysql_decode_text_by_name(row: &MySqlRow, name: &str) -> Option<String> {
    if let Ok(v) = row.try_get::<String, _>(name) {
        return Some(v);
    }
    if let Ok(v) = row.try_get::<&str, _>(name) {
        return Some(v.to_string());
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(name) {
        return Some(utf8_from_bytes(v));
    }
    None
}

fn decode_mysql_cell(row: &MySqlRow, index: usize) -> Result<SqlValue, DriverError> {
    let col = row
        .columns()
        .get(index)
        .ok_or_else(|| DriverError::Execution(format!("column index {} out of range", index)))?;
    let type_name = col.type_info().name();

    let raw = row
        .try_get_raw(index)
        .map_err(|e| DriverError::Execution(e.to_string()))?;
    if raw.is_null() {
        return Ok(SqlValue::Null);
    }

    // BOOL/BOOLEAN must win before the numeric decoders, which would happily
    // turn 1/0 into an integer. A plain TINYINT(1) stays numeric by design
    // (indistinguishable from a boolean at the protocol level).
    if mysql_type_is_bool(type_name) {
        if let Ok(v) = row.try_get::<bool, _>(index) {
            return Ok(SqlValue::Bool(v));
        }
        if let Ok(v) = row.try_get::<i64, _>(index) {
            return Ok(SqlValue::Bool(v != 0));
        }
    }

    if let Ok(v) = row.try_get::<i64, _>(index) {
        return Ok(SqlValue::I64(v));
    }
    if let Ok(v) = row.try_get::<u64, _>(index) {
        if v <= (1u64 << 53) {
            return Ok(SqlValue::I64(v as i64));
        }
        return Ok(SqlValue::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<i32, _>(index) {
        return Ok(SqlValue::I64(i64::from(v)));
    }
    if let Ok(v) = row.try_get::<u32, _>(index) {
        return Ok(SqlValue::I64(i64::from(v)));
    }
    if let Ok(v) = row.try_get::<f64, _>(index) {
        return Ok(SqlValue::F64(v));
    }
    if let Ok(v) = row.try_get::<f32, _>(index) {
        return Ok(SqlValue::F64(f64::from(v)));
    }
    if let Ok(v) = row.try_get::<BigDecimal, _>(index) {
        return Ok(SqlValue::BigDecimal(v));
    }
    if let Ok(v) = row.try_get::<NaiveDate, _>(index) {
        return Ok(SqlValue::Date(v));
    }
    if let Ok(v) = row.try_get::<NaiveTime, _>(index) {
        return Ok(SqlValue::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<NaiveDateTime, _>(index) {
        return Ok(SqlValue::DateTime(v));
    }
    if type_name.to_ascii_lowercase().contains("json") {
        if let Ok(v) = row.try_get::<Value, _>(index) {
            return Ok(SqlValue::Json(v));
        }
    }

    if mysql_type_is_text(type_name) {
        if let Some(s) = mysql_decode_text_by_index(row, index) {
            return Ok(SqlValue::String(s));
        }
    }

    if mysql_type_is_binary(type_name) {
        if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
            return Ok(SqlValue::Binary(v));
        }
    }

    if let Ok(v) = row.try_get::<String, _>(index) {
        return Ok(SqlValue::String(v));
    }
    // Last resort: keep valid UTF-8 as text, otherwise keep the bytes.
    if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
        return Ok(match String::from_utf8(v) {
            Ok(s) => SqlValue::String(s),
            Err(e) => SqlValue::Binary(e.into_bytes()),
        });
    }
    Ok(SqlValue::Null)
}
