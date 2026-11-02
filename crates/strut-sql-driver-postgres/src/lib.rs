use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::collections::BTreeMap;
use std::time::Duration;
use strut_sql_driver_api::{
    BoundQuery, ColumnInfo, ColumnMeta, ConnectOptions, DriverError, EngineDriver, EngineKind,
    ForeignKeyInfo, IndexInfo, IndexPage, ObjectType, QueryMeta, QueryResult, RoutineInfo,
    SchemaObject, SchemaPage, SqlValue, TableInfo, TableRowCount,
};
use uuid::Uuid;

pub struct PostgresDriver {
    pool: PgPool,
}

impl PostgresDriver {
    pub async fn new(connection_string: &str) -> Result<Self, DriverError> {
        Self::new_with_options(connection_string, &ConnectOptions::default()).await
    }

    pub async fn new_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Self, DriverError> {
        let stmt_ms = options.statement_timeout.as_millis().max(1) as u64;
        let mut pool_options = PgPoolOptions::new()
            .max_connections(options.pool_size)
            .acquire_timeout(options.connect_timeout)
            .idle_timeout(Duration::from_secs(45))
            .max_lifetime(Duration::from_secs(300))
            .test_before_acquire(true);
        let read_only = options.read_only;
        pool_options = pool_options.after_connect(move |conn, _meta| {
            Box::pin(async move {
                sqlx::query(&format!("SET statement_timeout = '{stmt_ms}ms'"))
                    .execute(&mut *conn)
                    .await?;
                if read_only {
                    sqlx::query("SET default_transaction_read_only = on")
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
        Ok(Self { pool })
    }
}

#[async_trait]
impl EngineDriver for PostgresDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::Postgres
    }

    async fn from_connection_string_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Box<Self>, DriverError> {
        Ok(Box::new(
            PostgresDriver::new_with_options(connection_string, options).await?,
        ))
    }

    async fn execute(&self, query: &BoundQuery) -> Result<QueryResult, DriverError> {
        // The neutral API accepts `?` placeholders on every engine; PostgreSQL
        // needs `$N`, so rewrite exactly the `?` characters outside literals
        // and comments (same single scanner the guard uses).
        let sql = rewrite_placeholders_for_postgres(&query.sql);
        let mut sqlx_query = sqlx::query(&sql);
        for param in &query.params {
            sqlx_query = bind_pg_value(sqlx_query, param)?;
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

        pg_rows_to_result(&rows, truncated)
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaObject>, DriverError> {
        let rows = sqlx::query(
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
             ORDER BY schema_name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(SchemaObject {
                    object_type: ObjectType::Schema,
                    schema: None,
                    name: r.try_get(0).ok()?,
                    table: None,
                    data_type: None,
                    nullable: None,
                })
            })
            .collect())
    }

    async fn resolve_schema(&self, schema: Option<&str>) -> Result<String, DriverError> {
        Ok(schema
            .filter(|s| !s.is_empty())
            .unwrap_or("public")
            .to_string())
    }

    async fn list_tables(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let schema = schema.filter(|s| !s.is_empty()).unwrap_or("public");
        let rows = sqlx::query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = $1 AND table_type = 'BASE TABLE' \
             ORDER BY table_name",
        )
        .bind(schema)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(SchemaObject {
                    object_type: ObjectType::Table,
                    schema: Some(schema.to_string()),
                    name: r.try_get(0).ok()?,
                    table: None,
                    data_type: None,
                    nullable: None,
                })
            })
            .collect())
    }

    async fn list_columns(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let schema = schema.filter(|s| !s.is_empty()).unwrap_or("public");
        let rows = sqlx::query(
            "SELECT table_name, column_name, \
             CASE WHEN data_type = 'USER-DEFINED' THEN udt_name ELSE data_type END, \
             is_nullable \
             FROM information_schema.columns \
             WHERE table_schema = $1 ORDER BY table_name, ordinal_position",
        )
        .bind(schema)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let table: String = r.try_get(0).ok()?;
                let name: String = r.try_get(1).ok()?;
                let data_type: String = r.try_get(2).ok()?;
                let nullable: String = r.try_get(3).ok()?;
                Some(SchemaObject {
                    object_type: ObjectType::Column,
                    schema: Some(schema.to_string()),
                    name,
                    table: Some(table),
                    data_type: Some(data_type),
                    nullable: Some(nullable == "YES"),
                })
            })
            .collect())
    }

    async fn list_schemas_page(
        &self,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let rows = sqlx::query(
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
               AND ($1::text IS NULL OR strpos(lower(schema_name), lower($1)) > 0) \
             ORDER BY schema_name LIMIT $2 OFFSET $3",
        )
        .bind(keyword)
        .bind((limit.saturating_add(1)) as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(pg_page_from_rows(rows, limit, |r| {
            Some(SchemaObject {
                object_type: ObjectType::Schema,
                schema: None,
                name: r.try_get(0).ok()?,
                table: None,
                data_type: None,
                nullable: None,
            })
        }))
    }

    async fn list_tables_page(
        &self,
        schema: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let schema = schema.filter(|s| !s.is_empty()).unwrap_or("public");
        let rows = sqlx::query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = $1 AND table_type = 'BASE TABLE' \
               AND ($2::text IS NULL OR strpos(lower(table_name), lower($2)) > 0) \
             ORDER BY table_name LIMIT $3 OFFSET $4",
        )
        .bind(schema)
        .bind(keyword)
        .bind((limit.saturating_add(1)) as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(pg_page_from_rows(rows, limit, |r| {
            Some(SchemaObject {
                object_type: ObjectType::Table,
                schema: Some(schema.to_string()),
                name: r.try_get(0).ok()?,
                table: None,
                data_type: None,
                nullable: None,
            })
        }))
    }

    async fn list_columns_page(
        &self,
        schema: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let schema = schema.filter(|s| !s.is_empty()).unwrap_or("public");
        let rows = sqlx::query(
            "SELECT table_name, column_name, \
             CASE WHEN data_type = 'USER-DEFINED' THEN udt_name ELSE data_type END, \
             is_nullable \
             FROM information_schema.columns \
             WHERE table_schema = $1 \
               AND ($2::text IS NULL \
                    OR strpos(lower(column_name), lower($2)) > 0 \
                    OR strpos(lower(table_name), lower($2)) > 0) \
             ORDER BY table_name, ordinal_position LIMIT $3 OFFSET $4",
        )
        .bind(schema)
        .bind(keyword)
        .bind((limit.saturating_add(1)) as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(pg_page_from_rows(rows, limit, |r| {
            let table: String = r.try_get(0).ok()?;
            let name: String = r.try_get(1).ok()?;
            let data_type: String = r.try_get(2).ok()?;
            let nullable: String = r.try_get(3).ok()?;
            Some(SchemaObject {
                object_type: ObjectType::Column,
                schema: Some(schema.to_string()),
                name,
                table: Some(table),
                data_type: Some(data_type),
                nullable: Some(nullable == "YES"),
            })
        }))
    }

    async fn list_indexes_page(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<IndexPage, DriverError> {
        let schema = schema.filter(|s| !s.is_empty()).unwrap_or("public");
        let rows = sqlx::query(
            "SELECT indexname, indexdef, tablename FROM pg_indexes \
             WHERE schemaname = $1 \
               AND ($2::text IS NULL OR tablename = $2) \
               AND ($3::text IS NULL \
                    OR strpos(lower(indexname), lower($3)) > 0 \
                    OR strpos(lower(tablename), lower($3)) > 0) \
             ORDER BY indexname LIMIT $4 OFFSET $5",
        )
        .bind(schema)
        .bind(table)
        .bind(keyword)
        .bind((limit.saturating_add(1)) as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;
        let mut indexes: Vec<IndexInfo> = rows
            .iter()
            .filter_map(|r| {
                let name: String = r.try_get(0).ok()?;
                let def: String = r.try_get(1).ok()?;
                let table: String = r.try_get(2).ok()?;
                Some(IndexInfo {
                    schema: Some(schema.to_string()),
                    table,
                    name,
                    columns: pg_indexdef_columns(&def),
                    unique: pg_indexdef_is_unique(&def),
                    primary: false,
                })
            })
            .collect();
        let has_more = indexes.len() > limit;
        indexes.truncate(limit);
        Ok(IndexPage { indexes, has_more })
    }

    async fn describe_table(
        &self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<TableInfo, DriverError> {
        let schema_name = schema.filter(|s| !s.is_empty()).unwrap_or("public");
        let rows = sqlx::query(
            "SELECT column_name, \
             CASE WHEN data_type = 'USER-DEFINED' THEN udt_name ELSE data_type END, \
             is_nullable, column_default, \
             EXISTS ( \
                 SELECT 1 FROM information_schema.table_constraints tc \
                 JOIN information_schema.key_column_usage kcu \
                   ON kcu.constraint_name = tc.constraint_name \
                  AND kcu.constraint_schema = tc.constraint_schema \
                 WHERE tc.constraint_type = 'PRIMARY KEY' \
                   AND tc.table_schema = c.table_schema \
                   AND tc.table_name = c.table_name \
                   AND kcu.column_name = c.column_name \
             ) AS is_primary_key \
             FROM information_schema.columns c \
             WHERE c.table_schema = $1 AND c.table_name = $2 \
             ORDER BY c.ordinal_position",
        )
        .bind(schema_name)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let columns = rows
            .iter()
            .filter_map(|r| {
                Some(ColumnInfo {
                    name: r.try_get(0).ok()?,
                    data_type: r.try_get(1).ok()?,
                    nullable: r
                        .try_get::<String, _>(2)
                        .map(|v| v == "YES")
                        .unwrap_or(false),
                    default_value: r.try_get(3).ok(),
                    is_primary_key: r.try_get(4).unwrap_or(false),
                    key: None,
                    extra: None,
                    comment: None,
                })
            })
            .collect();

        Ok(TableInfo {
            schema: Some(schema_name.to_string()),
            name: table.to_string(),
            columns,
        })
    }

    async fn list_indexes(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<IndexInfo>, DriverError> {
        let schema = schema.filter(|s| !s.is_empty()).unwrap_or("public");
        let rows = if let Some(table) = table {
            sqlx::query(
                "SELECT indexname, indexdef, tablename FROM pg_indexes \
                 WHERE schemaname = $1 AND tablename = $2 ORDER BY indexname",
            )
            .bind(schema)
            .bind(table)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                "SELECT indexname, indexdef, tablename FROM pg_indexes \
                 WHERE schemaname = $1 ORDER BY indexname",
            )
            .bind(schema)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        Ok(rows
            .iter()
            .filter_map(|r| {
                let name: String = r.try_get(0).ok()?;
                let def: String = r.try_get(1).ok()?;
                let table: String = r.try_get(2).ok()?;
                Some(IndexInfo {
                    schema: Some(schema.to_string()),
                    table,
                    name,
                    columns: pg_indexdef_columns(&def),
                    unique: pg_indexdef_is_unique(&def),
                    primary: false,
                })
            })
            .collect())
    }

    async fn list_foreign_keys(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        let schema_name = schema.filter(|s| !s.is_empty()).unwrap_or("public");
        let rows = if let Some(table) = table {
            sqlx::query(
                "SELECT tc.constraint_name, kcu.table_name, kcu.column_name, \
                 ccu.table_name AS foreign_table_name, ccu.column_name AS foreign_column_name, \
                 rc.delete_rule, rc.update_rule \
                 FROM information_schema.table_constraints AS tc \
                 JOIN information_schema.key_column_usage AS kcu \
                   ON tc.constraint_name = kcu.constraint_name \
                  AND tc.table_schema = kcu.table_schema \
                 JOIN information_schema.constraint_column_usage AS ccu \
                   ON ccu.constraint_name = tc.constraint_name \
                  AND ccu.table_schema = tc.table_schema \
                 JOIN information_schema.referential_constraints AS rc \
                   ON rc.constraint_name = tc.constraint_name \
                  AND rc.constraint_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'FOREIGN KEY' \
                   AND tc.table_schema = $1 AND kcu.table_name = $2 \
                 ORDER BY tc.constraint_name, kcu.ordinal_position",
            )
            .bind(schema_name)
            .bind(table)
            .fetch_all(&self.pool)
            .await
        } else {
            sqlx::query(
                "SELECT tc.constraint_name, kcu.table_name, kcu.column_name, \
                 ccu.table_name AS foreign_table_name, ccu.column_name AS foreign_column_name, \
                 rc.delete_rule, rc.update_rule \
                 FROM information_schema.table_constraints AS tc \
                 JOIN information_schema.key_column_usage AS kcu \
                   ON tc.constraint_name = kcu.constraint_name \
                  AND tc.table_schema = kcu.table_schema \
                 JOIN information_schema.constraint_column_usage AS ccu \
                   ON ccu.constraint_name = tc.constraint_name \
                  AND ccu.table_schema = tc.table_schema \
                 JOIN information_schema.referential_constraints AS rc \
                   ON rc.constraint_name = tc.constraint_name \
                  AND rc.constraint_schema = tc.table_schema \
                 WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = $1 \
                 ORDER BY tc.constraint_name, kcu.ordinal_position",
            )
            .bind(schema_name)
            .fetch_all(&self.pool)
            .await
        }
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let mut map: BTreeMap<(String, String, String), ForeignKeyInfo> = BTreeMap::new();
        for row in &rows {
            let name: String = row.try_get(0).unwrap_or_default();
            let table: String = row.try_get(1).unwrap_or_default();
            let column: String = row.try_get(2).unwrap_or_default();
            let ref_table: String = row.try_get(3).unwrap_or_default();
            let ref_column: String = row.try_get(4).unwrap_or_default();
            let on_delete: String = row.try_get(5).unwrap_or_default();
            let on_update: String = row.try_get(6).unwrap_or_default();
            let key = fk_key(schema_name, &table, &name);
            map.entry(key)
                .and_modify(|fk| {
                    fk.columns.push(column.clone());
                    fk.ref_columns.push(ref_column.clone());
                })
                .or_insert(ForeignKeyInfo {
                    schema: Some(schema_name.to_string()),
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
        let schema_name = schema.filter(|s| !s.is_empty()).unwrap_or("public");
        let rows = sqlx::query(
            "SELECT c.relname, s.n_live_tup, c.reltuples::float8 \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid \
             WHERE n.nspname = $1 AND c.relkind = 'r' \
             ORDER BY c.relname",
        )
        .bind(schema_name)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        Ok(rows
            .iter()
            .filter_map(|r| {
                let name: String = r.try_get(0).ok()?;
                let live: Option<i64> = r.try_get(1).ok().flatten();
                let reltuples =
                    pg_decode_reltuples(r.try_get::<f32, _>(2).ok(), r.try_get::<f64, _>(2).ok());
                Some(TableRowCount {
                    name,
                    approx_rows: pg_approx_rows(live, reltuples),
                })
            })
            .collect())
    }

    async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError> {
        let rows = sqlx::query(
            "SELECT routine_schema, routine_name, routine_type, data_type \
             FROM information_schema.routines \
             ORDER BY routine_schema, routine_name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(RoutineInfo {
                    schema: r.try_get(0).ok(),
                    name: r.try_get(1).ok()?,
                    routine_type: r.try_get(2).unwrap_or_else(|_| "FUNCTION".to_string()),
                    return_type: r.try_get(3).ok(),
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

fn pg_page_from_rows(
    rows: Vec<PgRow>,
    limit: usize,
    map: impl Fn(&PgRow) -> Option<SchemaObject>,
) -> SchemaPage {
    let mut objects: Vec<SchemaObject> = rows.iter().filter_map(map).collect();
    let has_more = objects.len() > limit;
    objects.truncate(limit);
    SchemaPage { objects, has_more }
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

/// Postgres planner/stats → optional approx row count for `schema_overview`.
/// `pg_class.reltuples` is Postgres `real` (FLOAT4 / f32). Prefer f32 decode; accept f64
/// if the query casts to `float8`. Silent f64-only decode used to drop never-analyzed (-1).
fn pg_decode_reltuples(as_f32: Option<f32>, as_f64: Option<f64>) -> Option<f64> {
    as_f32.map(f64::from).or(as_f64)
}

/// Composite key for FK grouping: same-named constraints on different tables
/// are distinct facts (SQL-BUG-02).
fn fk_key(schema: &str, table: &str, name: &str) -> (String, String, String) {
    (schema.to_string(), table.to_string(), name.to_string())
}

fn pg_approx_rows(live: Option<i64>, reltuples: Option<f64>) -> Option<i64> {
    match reltuples {
        Some(t) if t < 0.0 => live.filter(|&n| n > 0),
        Some(t) => live.or(Some(t.round() as i64)),
        None => live,
    }
}

fn bind_pg_value<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    param: &'q SqlValue,
) -> Result<sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>, DriverError> {
    match param {
        SqlValue::Null => Ok(query.bind(None::<String>)),
        SqlValue::Bool(b) => Ok(query.bind(*b)),
        SqlValue::I64(n) => Ok(query.bind(*n)),
        SqlValue::F64(f) => Ok(query.bind(*f)),
        SqlValue::String(s) => Ok(query.bind(s.as_str())),
        SqlValue::Uuid(u) => Ok(query.bind(*u)),
        SqlValue::BigDecimal(d) => Ok(query.bind(d)),
        SqlValue::Date(d) => Ok(query.bind(*d)),
        SqlValue::DateTime(dt) => Ok(query.bind(*dt)),
        SqlValue::Binary(b) => Ok(query.bind(b.as_slice())),
        // Arrays need a typed element encoding sqlx cannot infer from the
        // neutral `SqlValue`; the root engine rejects them too.
        SqlValue::Array(_) => Err(DriverError::Unsupported(
            "array parameters are not supported; pass a JSON value instead".into(),
        )),
        SqlValue::Json(j) => Ok(query.bind(j.clone())),
    }
}

fn pg_rows_to_result(rows: &[PgRow], truncated: bool) -> Result<QueryResult, DriverError> {
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
            // The wire protocol does not carry result nullability.
            nullable: true,
        })
        .collect();

    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut out_row = Vec::with_capacity(row.columns().len());
        for (i, _col) in row.columns().iter().enumerate() {
            out_row.push(decode_pg_cell(row, i)?);
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

fn bytes_to_hex(v: &[u8]) -> String {
    v.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_pg_cell(row: &PgRow, index: usize) -> Result<SqlValue, DriverError> {
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

    if let Ok(v) = row.try_get::<i64, _>(index) {
        return Ok(SqlValue::I64(v));
    }
    if let Ok(v) = row.try_get::<i32, _>(index) {
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
    if let Ok(v) = row.try_get::<bool, _>(index) {
        return Ok(SqlValue::Bool(v));
    }
    if let Ok(v) = row.try_get::<Uuid, _>(index) {
        return Ok(SqlValue::Uuid(v));
    }
    // timestamptz surfaces as RFC 3339 text (legacy behavior); timestamp
    // without time zone stays a naive datetime.
    if let Ok(v) = row.try_get::<DateTime<Utc>, _>(index) {
        return Ok(SqlValue::String(v.to_rfc3339()));
    }
    if let Ok(v) = row.try_get::<NaiveDateTime, _>(index) {
        return Ok(SqlValue::DateTime(v));
    }
    if let Ok(v) = row.try_get::<NaiveDate, _>(index) {
        return Ok(SqlValue::Date(v));
    }
    if let Ok(v) = row.try_get::<NaiveTime, _>(index) {
        return Ok(SqlValue::String(v.to_string()));
    }

    // bytea and arrays must never silently fall through to NULL.
    if type_name.eq_ignore_ascii_case("bytea") {
        if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
            return Ok(SqlValue::Binary(v));
        }
    }
    if pg_type_is_array(type_name) {
        if let Some(v) = decode_pg_array(row, index) {
            return Ok(v);
        }
        if let Ok(bytes) = raw.as_bytes() {
            return Ok(SqlValue::Json(serde_json::json!({
                "$raw_hex": bytes_to_hex(bytes)
            })));
        }
        return Ok(SqlValue::String(format!("<undecoded {type_name} value>")));
    }

    if type_name.to_ascii_lowercase().contains("json") {
        if let Ok(v) = row.try_get::<Value, _>(index) {
            return Ok(SqlValue::Json(v));
        }
    }
    if let Ok(v) = row.try_get::<String, _>(index) {
        return Ok(SqlValue::String(v));
    }
    if pg_type_is_textish(type_name) {
        if let Ok(v) = row.try_get::<&str, _>(index) {
            return Ok(SqlValue::String(v.to_string()));
        }
        if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
            return Ok(SqlValue::String(String::from_utf8_lossy(&v).into_owned()));
        }
    }

    // Last resort: never silently NULL a non-NULL cell when bytes exist.
    if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
        return Ok(match String::from_utf8(v) {
            Ok(s) => SqlValue::String(s),
            Err(e) => SqlValue::String(bytes_to_hex(e.as_bytes())),
        });
    }
    Ok(SqlValue::Null)
}

/// PostgreSQL types that always surface as text for agents.
fn pg_type_is_textish(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    matches!(
        t.as_str(),
        "varchar"
            | "bpchar"
            | "text"
            | "name"
            | "citext"
            | "xml"
            | "uuid"
            | "inet"
            | "cidr"
            | "macaddr"
            | "macaddr8"
            | "date"
            | "time"
            | "timetz"
            | "timestamp"
            | "timestamptz"
            | "interval"
            | "json"
            | "jsonb"
            | "ltree"
            | "lquery"
            | "ltxtquery"
    ) || t.contains("char")
        || t.contains("text")
}

/// sqlx reports array types as `text[]` or as the internal `_text` form.
fn pg_type_is_array(type_name: &str) -> bool {
    type_name.ends_with("[]") || (type_name.starts_with('_') && type_name.len() > 1)
}

/// Element-wise array decode for the common element types.
fn decode_pg_array(row: &PgRow, index: usize) -> Option<SqlValue> {
    if let Ok(v) = row.try_get::<Vec<String>, _>(index) {
        return Some(SqlValue::Array(
            v.into_iter().map(SqlValue::String).collect(),
        ));
    }
    if let Ok(v) = row.try_get::<Vec<i64>, _>(index) {
        return Some(SqlValue::Array(v.into_iter().map(SqlValue::I64).collect()));
    }
    if let Ok(v) = row.try_get::<Vec<i32>, _>(index) {
        return Some(SqlValue::Array(
            v.into_iter().map(|n| SqlValue::I64(i64::from(n))).collect(),
        ));
    }
    if let Ok(v) = row.try_get::<Vec<f64>, _>(index) {
        return Some(SqlValue::Array(v.into_iter().map(SqlValue::F64).collect()));
    }
    if let Ok(v) = row.try_get::<Vec<bool>, _>(index) {
        return Some(SqlValue::Array(v.into_iter().map(SqlValue::Bool).collect()));
    }
    if let Ok(v) = row.try_get::<Vec<Uuid>, _>(index) {
        return Some(SqlValue::Array(v.into_iter().map(SqlValue::Uuid).collect()));
    }
    if let Ok(v) = row.try_get::<Vec<BigDecimal>, _>(index) {
        return Some(SqlValue::Array(
            v.into_iter().map(SqlValue::BigDecimal).collect(),
        ));
    }
    None
}

fn pg_indexdef_is_unique(indexdef: &str) -> bool {
    indexdef
        .trim_start()
        .to_ascii_uppercase()
        .starts_with("CREATE UNIQUE INDEX")
}

/// Extract indexed column/expression names from `pg_indexes.indexdef`.
fn pg_indexdef_columns(indexdef: &str) -> Vec<String> {
    let upper = indexdef.to_ascii_uppercase();
    let open = if let Some(pos) = upper.find(" USING ") {
        indexdef[pos..].find('(').map(|i| pos + i)
    } else if let Some(pos) = upper.find(" ON ") {
        indexdef[pos..].find('(').map(|i| pos + i)
    } else {
        None
    };
    let Some(open) = open else {
        return Vec::new();
    };
    let rest = &indexdef[open + 1..];
    let mut depth = 1i32;
    let mut end = None;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    split_index_col_list(&rest[..end])
}

fn split_index_col_list(inner: &str) -> Vec<String> {
    let mut cols = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                if let Some(name) = normalize_index_col(&inner[start..i]) {
                    cols.push(name);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    if let Some(name) = normalize_index_col(&inner[start..]) {
        cols.push(name);
    }
    cols
}

fn normalize_index_col(part: &str) -> Option<String> {
    let mut s = part.trim();
    loop {
        let upper = s.to_ascii_uppercase();
        let stripped = if upper.ends_with(" NULLS FIRST") {
            Some(&s[..s.len() - " NULLS FIRST".len()])
        } else if upper.ends_with(" NULLS LAST") {
            Some(&s[..s.len() - " NULLS LAST".len()])
        } else if upper.ends_with(" DESC") {
            Some(&s[..s.len() - " DESC".len()])
        } else if upper.ends_with(" ASC") {
            Some(&s[..s.len() - " ASC".len()])
        } else {
            None
        };
        match stripped {
            Some(next) => s = next.trim_end(),
            None => break,
        }
    }
    let s = s.trim().trim_matches('"').trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

// --- `?` -> `$N` rewrite -------------------------------------------------
//
// One scanner for literals, identifiers, comments and dollar-quoted strings,
// so a `?` inside a string/comment is never rewritten. Adapted from the root
// engine's guard scanner.

#[derive(Debug, Default)]
struct ScanOutcome {
    qmark_positions: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Normal,
    SingleQuote,
    DoubleQuote,
    Backtick,
    LineComment,
    BlockComment(u32),
    DollarQuote,
}

fn scan_sql(sql: &str) -> ScanOutcome {
    let bytes = sql.as_bytes();
    let mut out = ScanOutcome::default();
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut dollar_tag: Vec<u8> = Vec::new();

    while i < bytes.len() {
        let b = bytes[i];
        match state {
            ScanState::Normal => match b {
                b'\'' => state = ScanState::SingleQuote,
                b'"' => state = ScanState::DoubleQuote,
                b'`' => state = ScanState::Backtick,
                b'?' => out.qmark_positions.push(i),
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    state = ScanState::LineComment;
                    i += 1;
                }
                b'#' => state = ScanState::LineComment,
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    state = ScanState::BlockComment(1);
                    i += 1;
                }
                b'$' => {
                    if let Some((tag, end)) = parse_dollar_tag(bytes, i) {
                        dollar_tag = tag;
                        state = ScanState::DollarQuote;
                        i = end;
                        continue;
                    }
                }
                b'E' | b'e' | b'B' | b'b' | b'X' | b'x' | b'U' | b'u'
                    if bytes.get(i + 1) == Some(&b'\'') && !prev_is_ident(bytes, i) =>
                {
                    state = ScanState::SingleQuote;
                    i += 1;
                }
                _ => {}
            },
            ScanState::SingleQuote => match b {
                b'\\' => i += 1,
                b'\'' => {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                }
                _ => {}
            },
            ScanState::DoubleQuote => {
                if b == b'"' {
                    if bytes.get(i + 1) == Some(&b'"') {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::Backtick => {
                if b == b'`' {
                    if bytes.get(i + 1) == Some(&b'`') {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::LineComment => {
                if b == b'\n' {
                    state = ScanState::Normal;
                }
            }
            ScanState::BlockComment(depth) => {
                if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    state = ScanState::BlockComment(depth + 1);
                    i += 1;
                } else if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    i += 1;
                    state = if depth <= 1 {
                        ScanState::Normal
                    } else {
                        ScanState::BlockComment(depth - 1)
                    };
                }
            }
            ScanState::DollarQuote => {
                if b == b'$' && dollar_close_matches(bytes, i, &dollar_tag) {
                    i += dollar_tag.len() + 1;
                    dollar_tag = Vec::new();
                    state = ScanState::Normal;
                }
            }
        }
        i += 1;
    }
    out
}

fn prev_is_ident(bytes: &[u8], i: usize) -> bool {
    i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_' || bytes[i - 1] == b'$')
}

/// True when the bytes at `start` are exactly `$<tag>$`.
fn dollar_close_matches(bytes: &[u8], start: usize, tag: &[u8]) -> bool {
    let after = start + 1;
    bytes.len() > after + tag.len()
        && &bytes[after..after + tag.len()] == tag
        && bytes[after + tag.len()] == b'$'
}

/// Returns `(tag, end_index_exclusive)` for an opening `$tag$` or `$$`.
/// A tag may not start with a digit (those are `$N` placeholders).
fn parse_dollar_tag(bytes: &[u8], start: usize) -> Option<(Vec<u8>, usize)> {
    if bytes.get(start) != Some(&b'$') {
        return None;
    }
    let mut j = start + 1;
    while j < bytes.len() && bytes[j] != b'$' {
        let ch = bytes[j];
        if !ch.is_ascii_alphanumeric() && ch != b'_' {
            return None;
        }
        if j == start + 1 && ch.is_ascii_digit() {
            return None;
        }
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    if bytes[j] != b'$' {
        return None;
    }
    Some((bytes[start + 1..j].to_vec(), j + 1))
}

/// Replace `?` with `$1`, `$2`, … outside literals and comments.
fn rewrite_placeholders_for_postgres(sql: &str) -> String {
    let scan = scan_sql(sql);
    if scan.qmark_positions.is_empty() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + scan.qmark_positions.len() * 2);
    let mut last = 0usize;
    for (idx, &pos) in scan.qmark_positions.iter().enumerate() {
        out.push_str(&sql[last..pos]);
        out.push('$');
        out.push_str(&(idx + 1).to_string());
        last = pos + 1;
    }
    out.push_str(&sql[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_qmark_outside_literals() {
        assert_eq!(
            rewrite_placeholders_for_postgres("SELECT * FROM t WHERE id = ? AND x = ?"),
            "SELECT * FROM t WHERE id = $1 AND x = $2"
        );
        assert_eq!(
            rewrite_placeholders_for_postgres("SELECT '?' AS q WHERE id = ?"),
            "SELECT '?' AS q WHERE id = $1"
        );
    }

    #[test]
    fn leaves_numbered_placeholders_alone() {
        assert_eq!(
            rewrite_placeholders_for_postgres("SELECT * FROM t WHERE id = $1"),
            "SELECT * FROM t WHERE id = $1"
        );
    }

    #[test]
    fn ignores_qmark_in_comments_and_dollar_quotes() {
        assert_eq!(
            rewrite_placeholders_for_postgres("SELECT 1 -- ?\n, $$??$$, ?"),
            "SELECT 1 -- ?\n, $$??$$, $1"
        );
    }

    #[test]
    fn pg_indexdef_detects_unique() {
        assert!(pg_indexdef_is_unique(
            "CREATE UNIQUE INDEX users_username_key ON public.users USING btree (username)"
        ));
        assert!(!pg_indexdef_is_unique(
            "CREATE INDEX users_email_idx ON public.users USING btree (email)"
        ));
    }

    #[test]
    fn pg_indexdef_columns_parses_simple_and_composite() {
        assert_eq!(
            pg_indexdef_columns(
                "CREATE UNIQUE INDEX users_email_key ON public.users USING btree (email)"
            ),
            vec!["email"]
        );
        assert_eq!(
            pg_indexdef_columns(
                "CREATE INDEX idx_u ON public.users USING btree (email, username DESC)"
            ),
            vec!["email", "username"]
        );
    }

    #[test]
    fn pg_approx_rows_never_analyzed_omits_stale_zero() {
        assert_eq!(pg_approx_rows(Some(0), Some(-1.0)), None);
        assert_eq!(pg_approx_rows(None, Some(-1.0)), None);
        assert_eq!(pg_approx_rows(Some(2), Some(-1.0)), Some(2));
    }

    #[test]
    fn pg_approx_rows_analyzed_prefers_live_then_reltuples() {
        assert_eq!(pg_approx_rows(Some(0), Some(0.0)), Some(0));
        assert_eq!(pg_approx_rows(None, Some(100.0)), Some(100));
        assert_eq!(pg_approx_rows(Some(5), Some(100.0)), Some(5));
        assert_eq!(pg_approx_rows(Some(3), None), Some(3));
    }

    #[test]
    fn pg_decode_reltuples_real_f32_feeds_omit_path() {
        // Production bug: try_get::<f64> on `real` failed → None → live Some(0) kept.
        let decoded = pg_decode_reltuples(Some(-1.0_f32), None);
        assert_eq!(decoded, Some(-1.0));
        assert_eq!(pg_approx_rows(Some(0), decoded), None);
        assert_eq!(
            pg_decode_reltuples(None, Some(42.0_f64)),
            Some(42.0),
            "float8 cast path"
        );
    }

    #[test]
    fn pg_type_is_array_detects_forms() {
        assert!(pg_type_is_array("text[]"));
        assert!(pg_type_is_array("int4[]"));
        assert!(pg_type_is_array("_text"));
        assert!(!pg_type_is_array("text"));
        assert!(!pg_type_is_array("bytea"));
    }

    #[test]
    fn pg_type_is_textish_covers_agent_pain_types() {
        for t in [
            "uuid",
            "timestamptz",
            "timestamp",
            "inet",
            "cidr",
            "citext",
            "text",
            "varchar",
            "macaddr",
        ] {
            assert!(pg_type_is_textish(t), "{t} should be textish");
        }
        assert!(!pg_type_is_textish("int4"));
        assert!(!pg_type_is_textish("bytea"));
    }

    #[test]
    fn fk_group_key_separates_same_name_on_different_tables() {
        assert_ne!(
            fk_key("public", "orders", "fk_parent"),
            fk_key("public", "invoices", "fk_parent")
        );
        assert_eq!(
            fk_key("public", "orders", "fk_parent"),
            ("public".into(), "orders".into(), "fk_parent".into())
        );
    }

    #[test]
    fn indexdef_parsing_handles_composite_and_expressions() {
        assert!(pg_indexdef_is_unique(
            "CREATE UNIQUE INDEX u ON public.t USING btree (a, b)"
        ));
        assert_eq!(
            pg_indexdef_columns(
                "CREATE INDEX i ON public.t USING btree (a, lower((b)::text) DESC)"
            ),
            vec!["a".to_string(), "lower((b)::text)".to_string()]
        );
    }
}
