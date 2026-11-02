use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::collections::BTreeMap;
use std::time::Duration;
use strut_sql_driver_api::{
    BoundQuery, ColumnInfo, ColumnMeta, ConnectOptions, DriverError, EngineDriver, EngineKind,
    ForeignKeyInfo, IndexInfo, ObjectType, QueryMeta, QueryResult, RoutineInfo, SchemaObject,
    SqlValue, TableInfo, TableRowCount,
};

pub struct MySqlDriver {
    pool: MySqlPool,
    url: String,
}

impl MySqlDriver {
    pub async fn new(connection_string: &str) -> Result<Self, DriverError> {
        Self::new_with_options(connection_string, &ConnectOptions::default()).await
    }

    pub async fn new_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Self, DriverError> {
        let stmt_ms = options.statement_timeout.as_millis().max(1) as u64;
        let mut pool_options = MySqlPoolOptions::new()
            .max_connections(options.pool_size)
            .acquire_timeout(options.connect_timeout)
            .idle_timeout(Duration::from_secs(45))
            .max_lifetime(Duration::from_secs(300))
            .test_before_acquire(true);
        let read_only = options.read_only;
        pool_options = pool_options.after_connect(move |conn, _meta| {
            Box::pin(async move {
                // max_execution_time is SELECT-only (ms); still helps agent runaway reads.
                let _ = sqlx::query(&format!("SET SESSION max_execution_time = {stmt_ms}"))
                    .execute(&mut *conn)
                    .await;
                if read_only {
                    sqlx::query("SET SESSION TRANSACTION READ ONLY")
                        .execute(&mut *conn)
                        .await?;
                }
                Ok(())
            })
        });
        let pool = pool_options
            .connect(connection_string)
            .await
            .map_err(|e| DriverError::Connection(e.to_string()))?;
        Ok(Self {
            pool,
            url: connection_string.to_string(),
        })
    }

    /// Resolve the schema scope: explicit name, `*` for all databases, or the
    /// connection's default database (catalog `DATABASE()`, then URL).
    async fn resolve_scope(&self, schema: Option<&str>) -> Result<MysqlSchemaScope, DriverError> {
        if let Some(s) = schema {
            if s == "*" {
                return Ok(MysqlSchemaScope::All);
            }
            if !s.is_empty() {
                return Ok(MysqlSchemaScope::One(s.to_string()));
            }
        }
        let row = sqlx::query("SELECT DATABASE()")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
        let db: Option<String> = row.try_get(0).unwrap_or(None);
        if let Some(db) = db.filter(|s| !s.is_empty()) {
            return Ok(MysqlSchemaScope::One(db));
        }
        if let Some(db) = database_from_mysql_url(&self.url) {
            return Ok(MysqlSchemaScope::One(db));
        }
        Err(DriverError::InvalidConfig(
            "no default schema; pass schema or connect with database in URL".into(),
        ))
    }
}

#[derive(Debug, Clone)]
enum MysqlSchemaScope {
    All,
    One(String),
}

#[async_trait]
impl EngineDriver for MySqlDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::MySql
    }

    async fn from_connection_string_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Box<Self>, DriverError> {
        Ok(Box::new(
            MySqlDriver::new_with_options(connection_string, options).await?,
        ))
    }

    async fn execute(&self, query: &BoundQuery) -> Result<QueryResult, DriverError> {
        let mut sqlx_query = sqlx::query(&query.sql);
        for param in &query.params {
            sqlx_query = bind_mysql_value(sqlx_query, param)?;
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

        mysql_rows_to_result(&rows, truncated)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaObject>, DriverError> {
        let rows = sqlx::query("SHOW DATABASES")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(SchemaObject {
                    object_type: ObjectType::Schema,
                    schema: None,
                    name: mysql_decode_text_by_index(r, 0)?,
                    table: None,
                    data_type: None,
                    nullable: None,
                })
            })
            .collect())
    }

    async fn resolve_schema(&self, schema: Option<&str>) -> Result<String, DriverError> {
        match self.resolve_scope(schema).await? {
            MysqlSchemaScope::One(name) => Ok(name),
            MysqlSchemaScope::All => Err(DriverError::InvalidConfig(
                "schema is required on MySQL (omit for the current database)".into(),
            )),
        }
    }

    async fn list_tables(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let scope = self.resolve_scope(schema).await?;
        let rows = match &scope {
            MysqlSchemaScope::All => {
                sqlx::query(
                    "SELECT table_schema, table_name FROM information_schema.tables \
                     WHERE table_type = 'BASE TABLE' ORDER BY table_schema, table_name",
                )
                .fetch_all(&self.pool)
                .await
            }
            MysqlSchemaScope::One(schema_name) => {
                sqlx::query(
                    "SELECT table_schema, table_name FROM information_schema.tables \
                     WHERE table_schema = ? AND table_type = 'BASE TABLE' ORDER BY table_name",
                )
                .bind(schema_name)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        Ok(rows
            .iter()
            .filter_map(|r| {
                let schema = mysql_decode_text_by_index(r, 0)?;
                let name = mysql_decode_text_by_index(r, 1)?;
                Some(SchemaObject {
                    object_type: ObjectType::Table,
                    schema: Some(schema),
                    name,
                    table: None,
                    data_type: None,
                    nullable: None,
                })
            })
            .collect())
    }

    async fn list_columns(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let scope = self.resolve_scope(schema).await?;
        let rows = match &scope {
            MysqlSchemaScope::All => {
                sqlx::query(
                    "SELECT table_schema, table_name, column_name, data_type, is_nullable \
                     FROM information_schema.columns \
                     ORDER BY table_schema, table_name, ordinal_position",
                )
                .fetch_all(&self.pool)
                .await
            }
            MysqlSchemaScope::One(schema_name) => {
                sqlx::query(
                    "SELECT table_schema, table_name, column_name, data_type, is_nullable \
                     FROM information_schema.columns WHERE table_schema = ? \
                     ORDER BY table_name, ordinal_position",
                )
                .bind(schema_name)
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let Some(schema) = mysql_decode_text_by_index(r, 0) else {
                continue;
            };
            let Some(table) = mysql_decode_text_by_index(r, 1) else {
                continue;
            };
            let Some(name) = mysql_decode_text_by_index(r, 2) else {
                continue;
            };
            let Some(data_type) = mysql_decode_text_by_index(r, 3) else {
                continue;
            };
            let nullable =
                mysql_decode_text_by_index(r, 4).is_some_and(|v| v.eq_ignore_ascii_case("YES"));
            out.push(SchemaObject {
                object_type: ObjectType::Column,
                schema: Some(schema),
                name,
                table: Some(table),
                data_type: Some(data_type),
                nullable: Some(nullable),
            });
        }
        Ok(out)
    }

    async fn describe_table(
        &self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<TableInfo, DriverError> {
        let (schema_from_table, table_only) = split_mysql_table(table);
        let table_name = normalize_mysql_ident(table_only);
        let resolved = self.resolve_scope(schema).await?;
        let schema_name = match resolved {
            MysqlSchemaScope::One(name) => normalize_mysql_ident(&name),
            MysqlSchemaScope::All => schema_from_table
                .map(|s| normalize_mysql_ident(&s))
                .ok_or_else(|| {
                    DriverError::InvalidConfig(
                        "describe_table requires schema when searching all databases".into(),
                    )
                })?,
        };

        // SHOW FULL COLUMNS is the authoritative per-table view; fall back to
        // information_schema when the table is not visible through SHOW.
        let mut columns = mysql_columns_from_show(&self.pool, &schema_name, &table_name).await?;
        if columns.is_empty() {
            columns = mysql_columns_from_information_schema(&self.pool, &schema_name, &table_name)
                .await?;
        }

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
        let scope = self.resolve_scope(schema).await?;
        let rows = match (&scope, table) {
            (MysqlSchemaScope::One(schema_name), Some(table)) => {
                sqlx::query(
                    "SELECT table_schema, table_name, index_name, column_name, non_unique \
                     FROM information_schema.statistics \
                     WHERE table_schema = ? AND table_name = ? \
                     ORDER BY table_schema, table_name, index_name, seq_in_index",
                )
                .bind(schema_name)
                .bind(table)
                .fetch_all(&self.pool)
                .await
            }
            (MysqlSchemaScope::One(schema_name), None) => {
                sqlx::query(
                    "SELECT table_schema, table_name, index_name, column_name, non_unique \
                     FROM information_schema.statistics \
                     WHERE table_schema = ? \
                     ORDER BY table_schema, table_name, index_name, seq_in_index",
                )
                .bind(schema_name)
                .fetch_all(&self.pool)
                .await
            }
            (MysqlSchemaScope::All, Some(table)) => {
                sqlx::query(
                    "SELECT table_schema, table_name, index_name, column_name, non_unique \
                     FROM information_schema.statistics \
                     WHERE table_name = ? \
                     ORDER BY table_schema, table_name, index_name, seq_in_index",
                )
                .bind(table)
                .fetch_all(&self.pool)
                .await
            }
            (MysqlSchemaScope::All, None) => {
                sqlx::query(
                    "SELECT table_schema, table_name, index_name, column_name, non_unique \
                     FROM information_schema.statistics \
                     ORDER BY table_schema, table_name, index_name, seq_in_index",
                )
                .fetch_all(&self.pool)
                .await
            }
        }
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let mut map: BTreeMap<(String, String, String), IndexInfo> = BTreeMap::new();
        for row in &rows {
            let Some(schema) = mysql_decode_text_by_index(row, 0) else {
                continue;
            };
            let Some(table) = mysql_decode_text_by_index(row, 1) else {
                continue;
            };
            let Some(name) = mysql_decode_text_by_index(row, 2) else {
                continue;
            };
            let Some(column) = mysql_decode_text_by_index(row, 3) else {
                continue;
            };
            let unique = mysql_int_by_index(row, 4).unwrap_or(1) == 0;
            let key = index_key(&schema, &table, &name);
            map.entry(key)
                .and_modify(|i| i.columns.push(column.clone()))
                .or_insert(IndexInfo {
                    primary: name == "PRIMARY",
                    schema: Some(schema),
                    table,
                    name,
                    columns: vec![column],
                    unique,
                });
        }
        Ok(map.into_values().collect())
    }

    async fn list_foreign_keys(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        let schema_name = match self.resolve_scope(schema).await? {
            MysqlSchemaScope::One(name) => name,
            MysqlSchemaScope::All => {
                return Err(DriverError::InvalidConfig(
                    "list_foreign_keys requires schema on MySQL (or omit for current database)"
                        .into(),
                ))
            }
        };
        let rows = if let Some(table) = table {
            sqlx::query(
                "SELECT kcu.CONSTRAINT_NAME, kcu.TABLE_NAME, kcu.COLUMN_NAME, \
                 kcu.REFERENCED_TABLE_NAME, kcu.REFERENCED_COLUMN_NAME, \
                 rc.DELETE_RULE, rc.UPDATE_RULE \
                 FROM information_schema.KEY_COLUMN_USAGE kcu \
                 JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
                   ON kcu.CONSTRAINT_NAME = rc.CONSTRAINT_NAME \
                  AND kcu.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA \
                 WHERE kcu.TABLE_SCHEMA = ? AND kcu.TABLE_NAME = ? \
                   AND kcu.REFERENCED_TABLE_NAME IS NOT NULL \
                 ORDER BY kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
            )
            .bind(&schema_name)
            .bind(table)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                "SELECT kcu.CONSTRAINT_NAME, kcu.TABLE_NAME, kcu.COLUMN_NAME, \
                 kcu.REFERENCED_TABLE_NAME, kcu.REFERENCED_COLUMN_NAME, \
                 rc.DELETE_RULE, rc.UPDATE_RULE \
                 FROM information_schema.KEY_COLUMN_USAGE kcu \
                 JOIN information_schema.REFERENTIAL_CONSTRAINTS rc \
                   ON kcu.CONSTRAINT_NAME = rc.CONSTRAINT_NAME \
                  AND kcu.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA \
                 WHERE kcu.TABLE_SCHEMA = ? AND kcu.REFERENCED_TABLE_NAME IS NOT NULL \
                 ORDER BY kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
            )
            .bind(&schema_name)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let mut map: BTreeMap<(String, String, String), ForeignKeyInfo> = BTreeMap::new();
        for row in &rows {
            let name = mysql_decode_text_by_index(row, 0).unwrap_or_default();
            let table = mysql_decode_text_by_index(row, 1).unwrap_or_default();
            let column = mysql_decode_text_by_index(row, 2).unwrap_or_default();
            let ref_table = mysql_decode_text_by_index(row, 3).unwrap_or_default();
            let ref_column = mysql_decode_text_by_index(row, 4).unwrap_or_default();
            let on_delete = mysql_decode_text_by_index(row, 5).unwrap_or_default();
            let on_update = mysql_decode_text_by_index(row, 6).unwrap_or_default();
            let key = (schema_name.clone(), table.clone(), name.clone());
            map.entry(key)
                .and_modify(|fk| {
                    fk.columns.push(column.clone());
                    fk.ref_columns.push(ref_column.clone());
                })
                .or_insert(ForeignKeyInfo {
                    schema: Some(schema_name.clone()),
                    table,
                    name,
                    columns: vec![column],
                    ref_schema: None,
                    ref_table,
                    ref_columns: vec![ref_column],
                    on_delete: Some(on_delete),
                    on_update: Some(on_update),
                });
        }
        Ok(map.into_values().collect())
    }

    async fn approximate_row_counts(
        &self,
        schema: Option<&str>,
    ) -> Result<Vec<TableRowCount>, DriverError> {
        let schema_name = match self.resolve_scope(schema).await? {
            MysqlSchemaScope::One(name) => name,
            MysqlSchemaScope::All => {
                return Err(DriverError::InvalidConfig(
                    "schema_overview requires schema on MySQL (or omit for current database)"
                        .into(),
                ))
            }
        };
        let rows = sqlx::query(
            "SELECT table_name, table_rows \
             FROM information_schema.tables \
             WHERE table_schema = ? AND table_type = 'BASE TABLE' \
             ORDER BY table_name",
        )
        .bind(&schema_name)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        Ok(rows
            .iter()
            .filter_map(|r| {
                let name = mysql_decode_text_by_index(r, 0)?;
                let approx = r
                    .try_get::<Option<i64>, _>(1)
                    .ok()
                    .flatten()
                    .or_else(|| {
                        r.try_get::<Option<u64>, _>(1)
                            .ok()
                            .flatten()
                            .and_then(|v| i64::try_from(v).ok())
                    })
                    .or_else(|| {
                        r.try_get::<i64, _>(1).ok().or_else(|| {
                            r.try_get::<u64, _>(1)
                                .ok()
                                .and_then(|v| i64::try_from(v).ok())
                        })
                    });
                Some(TableRowCount {
                    name,
                    approx_rows: approx,
                })
            })
            .collect())
    }

    async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError> {
        let rows = sqlx::query(
            "SELECT routine_schema, routine_name, routine_type, dtd_identifier \
             FROM information_schema.routines \
             WHERE routine_schema = DATABASE() OR DATABASE() IS NULL \
             ORDER BY routine_schema, routine_name",
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

fn database_from_mysql_url(url: &str) -> Option<String> {
    let rest = url.strip_prefix("mysql://")?;
    let after_host = rest.split('@').next_back()?;
    let db = after_host.split('/').nth(1)?;
    let db = db.split('?').next()?.trim();
    if db.is_empty() {
        None
    } else {
        Some(db.to_string())
    }
}

async fn mysql_columns_from_show(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, DriverError> {
    let sql = if schema.is_empty() {
        format!("SHOW FULL COLUMNS FROM {}", mysql_quote_ident(table))
    } else {
        format!(
            "SHOW FULL COLUMNS FROM {}.{}",
            mysql_quote_ident(schema),
            mysql_quote_ident(table)
        )
    };
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let name =
            mysql_decode_text_by_name(r, "Field").or_else(|| mysql_decode_text_by_index(r, 0));
        let data_type =
            mysql_decode_text_by_name(r, "Type").or_else(|| mysql_decode_text_by_index(r, 1));
        let null_raw =
            mysql_decode_text_by_name(r, "Null").or_else(|| mysql_decode_text_by_index(r, 2));
        match (name, data_type, null_raw) {
            (Some(name), Some(data_type), Some(null_raw)) => {
                let key = mysql_decode_text_by_name(r, "Key");
                out.push(ColumnInfo {
                    name,
                    data_type,
                    nullable: parse_is_nullable(&null_raw),
                    default_value: mysql_decode_text_by_name(r, "Default"),
                    is_primary_key: key.as_deref() == Some("PRI"),
                    key,
                    extra: mysql_decode_text_by_name(r, "Extra"),
                    comment: mysql_decode_text_by_name(r, "Comment"),
                });
            }
            _ => tracing::warn!("describe_table/show: skipped row decode failed"),
        }
    }
    if !rows.is_empty() && out.is_empty() {
        tracing::warn!(
            raw_rows = rows.len(),
            "describe_table/show: all rows failed decode"
        );
    }
    Ok(out)
}

async fn mysql_columns_from_information_schema(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, DriverError> {
    let rows = if schema.is_empty() {
        sqlx::query(
            "SELECT table_schema, table_name, column_name, data_type, is_nullable \
             FROM information_schema.columns WHERE table_name = ? \
             ORDER BY ordinal_position",
        )
        .bind(table)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query(
            "SELECT table_schema, table_name, column_name, data_type, is_nullable \
             FROM information_schema.columns \
             WHERE table_schema = ? AND table_name = ? \
             ORDER BY ordinal_position",
        )
        .bind(schema)
        .bind(table)
        .fetch_all(pool)
        .await
    }
    .map_err(|e| DriverError::Introspection(e.to_string()))?;

    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let Some(name) = mysql_decode_text_by_index(r, 2) else {
            tracing::warn!("describe_table/information_schema: skipped column row decode failed");
            continue;
        };
        let Some(data_type) = mysql_decode_text_by_index(r, 3) else {
            continue;
        };
        let nullable =
            mysql_decode_text_by_index(r, 4).is_some_and(|v| v.eq_ignore_ascii_case("YES"));
        out.push(ColumnInfo {
            name,
            data_type,
            nullable,
            default_value: None,
            is_primary_key: false,
            key: None,
            extra: None,
            comment: None,
        });
    }
    if !rows.is_empty() && out.is_empty() {
        tracing::warn!(
            raw_rows = rows.len(),
            "describe_table/information_schema: all rows failed decode"
        );
    }
    Ok(out)
}

fn parse_is_nullable(raw: &str) -> bool {
    raw.eq_ignore_ascii_case("YES")
}

/// Composite key for index grouping: same-named indexes on different tables
/// are distinct facts (SQL-BUG-03).
fn index_key(schema: &str, table: &str, name: &str) -> (String, String, String) {
    (schema.to_string(), table.to_string(), name.to_string())
}

fn mysql_quote_ident(name: &str) -> String {
    format!("`{}`", normalize_mysql_ident(name).replace('`', "``"))
}

fn normalize_mysql_ident(s: &str) -> String {
    s.trim().trim_matches('`').to_string()
}

/// Split `schema.table` or `` `schema`.`table` `` into optional schema + bare table name.
fn split_mysql_table(table: &str) -> (Option<String>, &str) {
    let trimmed = table.trim().trim_matches('`');
    if let Some((schema, table)) = trimmed.rsplit_once('.') {
        let schema = schema.trim().trim_matches('`');
        let table = table.trim().trim_matches('`');
        if !schema.is_empty() && !table.is_empty() {
            return (Some(schema.to_string()), table);
        }
    }
    (None, trimmed)
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
        // MySQL has no array type; the root engine rejects array parameters.
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
            rows_affected: None,
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

fn bytes_to_hex(v: &[u8]) -> String {
    v.iter().map(|b| format!("{b:02x}")).collect()
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
        .ok_or_else(|| DriverError::Execution(format!("column index {index} out of range")))?;
    let type_name = col.type_info().name();

    let raw = row
        .try_get_raw(index)
        .map_err(|e| DriverError::Execution(e.to_string()))?;
    if raw.is_null() {
        return Ok(SqlValue::Null);
    }

    // BOOL/BOOLEAN must win before the numeric decoders, which would happily
    // turn 1/0 into an integer. A plain TINYINT(1) stays numeric by design.
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
    if let Ok(v) = row.try_get::<DateTime<Utc>, _>(index) {
        return Ok(SqlValue::String(v.to_rfc3339()));
    }
    if mysql_type_is_text(type_name) {
        if let Some(s) = mysql_decode_text_by_index(row, index) {
            return Ok(SqlValue::String(s));
        }
    }

    if mysql_type_is_binary(type_name) {
        if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
            return Ok(SqlValue::String(bytes_to_hex(&v)));
        }
    }

    if let Ok(v) = row.try_get::<String, _>(index) {
        return Ok(SqlValue::String(v));
    }
    // Fallback: prefer UTF-8 when bytes are valid text; otherwise hex.
    if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
        return Ok(match String::from_utf8(v) {
            Ok(s) => SqlValue::String(s),
            Err(e) => SqlValue::String(bytes_to_hex(e.as_bytes())),
        });
    }
    Ok(SqlValue::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_nullable_accepts_yes_no_case_insensitive() {
        assert!(parse_is_nullable("YES"));
        assert!(parse_is_nullable("yes"));
        assert!(!parse_is_nullable("NO"));
        assert!(!parse_is_nullable("no"));
    }

    #[test]
    fn mysql_quote_ident_escapes_backticks() {
        assert_eq!(mysql_quote_ident("fw_users"), "`fw_users`");
        assert_eq!(mysql_quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn normalize_mysql_ident_strips_backticks_and_whitespace() {
        assert_eq!(normalize_mysql_ident("  `fw_users`  "), "fw_users");
    }

    #[test]
    fn mysql_type_is_bool_recognizes_boolean_types() {
        assert!(mysql_type_is_bool("BOOL"));
        assert!(mysql_type_is_bool("BOOLEAN"));
        assert!(!mysql_type_is_bool("BIGINT"));
        assert!(!mysql_type_is_bool("TINYINT"));
    }

    #[test]
    fn mysql_type_is_text_recognizes_string_types() {
        for t in [
            "VARCHAR",
            "CHAR",
            "TEXT",
            "TINYTEXT",
            "MEDIUMTEXT",
            "LONGTEXT",
            "ENUM",
            "SET",
            "JSON",
            "VAR_STRING",
            "varchar",
        ] {
            assert!(mysql_type_is_text(t), "{t} should be text");
            assert!(!mysql_type_is_binary(t), "{t} should not be binary");
        }
    }

    #[test]
    fn mysql_type_is_binary_recognizes_blob_types() {
        for t in ["BLOB", "TINYBLOB", "VARBINARY", "BINARY", "BIT", "LONGBLOB"] {
            assert!(mysql_type_is_binary(t), "{t} should be binary");
            assert!(!mysql_type_is_text(t), "{t} should not be text");
        }
    }

    #[test]
    fn bytes_to_hex_encodes_ascii() {
        assert_eq!(bytes_to_hex(b"edoc"), "65646f63");
    }

    #[test]
    fn utf8_from_bytes_valid_and_lossy() {
        assert_eq!(utf8_from_bytes(b"edoc_pid_dev".to_vec()), "edoc_pid_dev");
        let bad = vec![0xff, 0xfe, b'a'];
        let s = utf8_from_bytes(bad);
        assert!(s.contains('a'));
    }

    #[test]
    fn index_group_key_separates_same_name_on_different_tables() {
        assert_ne!(
            index_key("app", "a", "PRIMARY"),
            index_key("app", "b", "PRIMARY")
        );
        assert_eq!(
            index_key("app", "a", "PRIMARY"),
            ("app".to_string(), "a".to_string(), "PRIMARY".to_string())
        );
    }

    #[test]
    fn splits_qualified_table_names() {
        assert_eq!(
            split_mysql_table("`demo`.`users`"),
            (Some("demo".to_string()), "users")
        );
        assert_eq!(split_mysql_table("users"), (None, "users"));
    }

    #[test]
    fn normalizes_identifiers() {
        assert_eq!(normalize_mysql_ident("`users`"), "users");
        assert_eq!(normalize_mysql_ident(" users "), "users");
    }
}
