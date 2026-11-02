use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::{Duration, Instant};
use strut_sql_driver_api::{
    BoundQuery, ColumnInfo, ColumnMeta, ConnectOptions, DriverError, EngineDriver, EngineKind,
    ForeignKeyInfo, IndexInfo, IndexPage, ObjectType, QueryMeta, QueryResult, RoutineInfo,
    SchemaObject, SchemaPage, SqlValue, TableInfo, TableRowCount,
};

pub struct SqliteDriver {
    pool: SqlitePool,
    /// PRD-F2-11: enforced at the driver level through a SQLite progress
    /// handler (plus busy_timeout for lock waits), not only a client timeout.
    statement_timeout: Duration,
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
        let connect_options = SqliteConnectOptions::from_str(&connect_url)
            .map_err(|e| DriverError::Connection(e.to_string()))?
            .busy_timeout(options.statement_timeout);
        let pool = SqlitePoolOptions::new()
            .max_connections(options.pool_size)
            .acquire_timeout(options.connect_timeout)
            .idle_timeout(Duration::from_secs(45))
            .max_lifetime(Duration::from_secs(300))
            .test_before_acquire(true)
            .connect_with(connect_options)
            .await
            .map_err(|e| DriverError::Connection(e.to_string()))?;
        Ok(Self {
            pool,
            statement_timeout: options.statement_timeout,
        })
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

        // Hold one connection for the whole statement so the progress
        // handler races the statement itself (PRD-F2-11), not the tool layer.
        let mut conn = self
            .pool
            .acquire()
            .await
            .map_err(|e| DriverError::Connection(e.to_string()))?;
        let deadline = Instant::now() + self.statement_timeout;
        {
            let mut handle = conn
                .lock_handle()
                .await
                .map_err(|e| DriverError::Execution(e.to_string()))?;
            handle.set_progress_handler(1_000, move || Instant::now() < deadline);
        }

        let outcome: Result<QueryResult, DriverError> = async {
            if !query.fetch_rows {
                let result = sqlx_query
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| map_sqlite_exec_error(e, deadline))?;
                return Ok(command_result(result.rows_affected()));
            }

            let (rows, truncated) = if let Some(cap) = query.row_cap {
                let mut stream = sqlx_query.fetch(&mut *conn);
                let mut rows = Vec::new();
                let mut overflow = false;
                use futures_util::StreamExt;
                while let Some(item) = stream.next().await {
                    let row = item.map_err(|e| map_sqlite_exec_error(e, deadline))?;
                    if rows.len() >= cap {
                        overflow = true;
                        break;
                    }
                    rows.push(row);
                }
                (rows, overflow)
            } else {
                let rows = sqlx_query
                    .fetch_all(&mut *conn)
                    .await
                    .map_err(|e| map_sqlite_exec_error(e, deadline))?;
                (rows, false)
            };

            sqlite_rows_to_result(&rows, truncated)
        }
        .await;
        if let Ok(mut handle) = conn.lock_handle().await {
            handle.remove_progress_handler();
        }

        outcome
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
        sqlite_list_indexes(&self.pool, &schema_name, table, None, None, None)
            .await
            .map(|page| page.indexes)
            .map_err(|e| DriverError::Introspection(e.to_string()))
    }

    async fn list_foreign_keys(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        let schema_name = resolve_sqlite_schema(schema).to_string();
        sqlite_list_foreign_keys(&self.pool, &schema_name, table)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))
    }

    async fn list_tables_page(
        &self,
        schema: Option<&str>,
        keyword: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<SchemaPage, DriverError> {
        let schema_name = resolve_sqlite_schema(schema);
        let sql = format!(
            "SELECT name FROM {}.sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
               AND (? IS NULL OR instr(lower(name), lower(?)) > 0) \
             ORDER BY name LIMIT ? OFFSET ?",
            sqlite_quote_ident(schema_name)
        );
        let rows = sqlx::query(&sql)
            .bind(keyword)
            .bind(keyword)
            .bind((limit.saturating_add(1)) as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(sqlite_page_from_rows(rows, limit, |r| {
            Some(SchemaObject {
                object_type: ObjectType::Table,
                schema: Some(schema_name.to_string()),
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
        let schema_name = resolve_sqlite_schema(schema);
        let sql = format!(
            "SELECT m.name AS table_name, p.name, p.type, p.notnull \
             FROM {}.sqlite_master AS m, pragma_table_info(m.name) AS p \
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' \
               AND (? IS NULL \
                    OR instr(lower(p.name), lower(?)) > 0 \
                    OR instr(lower(m.name), lower(?)) > 0) \
             ORDER BY m.name, p.cid LIMIT ? OFFSET ?",
            sqlite_quote_ident(schema_name)
        );
        let rows = sqlx::query(&sql)
            .bind(keyword)
            .bind(keyword)
            .bind(keyword)
            .bind((limit.saturating_add(1)) as i64)
            .bind(offset as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?;
        Ok(sqlite_page_from_rows(rows, limit, |r| {
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
        let schema_name = resolve_sqlite_schema(schema).to_string();
        sqlite_list_indexes(
            &self.pool,
            &schema_name,
            table,
            keyword,
            Some(offset),
            Some(limit),
        )
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))
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

fn sqlite_page_from_rows(
    rows: Vec<SqliteRow>,
    limit: usize,
    map: impl Fn(&SqliteRow) -> Option<SchemaObject>,
) -> SchemaPage {
    let mut objects: Vec<SchemaObject> = rows.iter().filter_map(map).collect();
    let has_more = objects.len() > limit;
    objects.truncate(limit);
    SchemaPage { objects, has_more }
}

fn map_sqlite_exec_error(error: sqlx::Error, deadline: Instant) -> DriverError {
    let message = error.to_string();
    if Instant::now() >= deadline || message.contains("interrupted") {
        DriverError::Timeout
    } else {
        DriverError::Execution(message)
    }
}

/// Set-based SQLite index listing: two statements for any table count.
/// The table metadata query carries keyword + LIMIT/OFFSET (PRD-F2-05);
/// index columns come from a single joined pragma query (PRD-F2-06).
async fn sqlite_list_indexes(
    pool: &SqlitePool,
    schema_name: &str,
    table: Option<&str>,
    keyword: Option<&str>,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<IndexPage, sqlx::Error> {
    let master = format!("{}.sqlite_master", sqlite_quote_ident(schema_name));
    let schema_arg = pragma_schema_arg(schema_name);
    let mut list_sql = format!(
        "SELECT m.name, il.name, il.\"unique\", il.origin \
         FROM {master} AS m, pragma_index_list(m.name{schema_arg}) AS il \
         WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%'"
    );
    if table.is_some() {
        list_sql.push_str(" AND m.name = ?");
    }
    list_sql.push_str(
        " AND (? IS NULL \
             OR instr(lower(il.name), lower(?)) > 0 \
             OR instr(lower(m.name), lower(?)) > 0) \
         ORDER BY m.name, il.seq",
    );
    if limit.is_some() {
        list_sql.push_str(" LIMIT ? OFFSET ?");
    }
    let mut query = sqlx::query(&list_sql);
    if let Some(table) = table {
        query = query.bind(table);
    }
    query = query.bind(keyword).bind(keyword).bind(keyword);
    if let Some(limit) = limit {
        query = query
            .bind((limit.saturating_add(1)) as i64)
            .bind(offset.unwrap_or(0) as i64);
    }
    let list_rows = query.fetch_all(pool).await?;

    let mut has_more = false;
    let mut selected = list_rows;
    if let Some(limit) = limit {
        has_more = selected.len() > limit;
        selected.truncate(limit);
    }
    if selected.is_empty() {
        return Ok(IndexPage {
            indexes: Vec::new(),
            has_more,
        });
    }

    let info_sql = format!(
        "SELECT il.name, p.name \
         FROM {master} AS m, pragma_index_list(m.name{schema_arg}) AS il, \
              pragma_index_info(il.name{schema_arg}) AS p \
         WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' \
         ORDER BY il.name, p.seqno"
    );
    let info_rows = sqlx::query(&info_sql).fetch_all(pool).await?;
    let mut columns_by_index: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for row in &info_rows {
        let idx: String = row.try_get(0)?;
        let col: String = row.try_get(1)?;
        columns_by_index.entry(idx).or_default().push(col);
    }

    let mut indexes = Vec::with_capacity(selected.len());
    for row in &selected {
        let table_name: String = row.try_get(0)?;
        let idx_name: String = row.try_get(1)?;
        let unique: i64 = row.try_get(2)?;
        let origin: String = row.try_get(3).unwrap_or_default();
        indexes.push(IndexInfo {
            schema: Some(schema_name.to_string()),
            table: table_name,
            name: idx_name.clone(),
            columns: columns_by_index.remove(&idx_name).unwrap_or_default(),
            unique: unique != 0,
            primary: origin == "pk",
        });
    }
    Ok(IndexPage { indexes, has_more })
}

fn pragma_schema_arg(schema_name: &str) -> String {
    if schema_name == "main" {
        String::new()
    } else {
        format!(", {}", sqlite_quote_literal(schema_name))
    }
}

/// Set-based SQLite foreign-key listing: one statement for every table,
/// grouping composite keys by `(table, id)` (PRD-F2-06).
async fn sqlite_list_foreign_keys(
    pool: &SqlitePool,
    schema_name: &str,
    table: Option<&str>,
) -> Result<Vec<ForeignKeyInfo>, sqlx::Error> {
    let master = format!("{}.sqlite_master", sqlite_quote_ident(schema_name));
    let schema_arg = pragma_schema_arg(schema_name);
    let mut sql = format!(
        "SELECT m.name AS tbl, fk.id AS fk_id, fk.seq AS fk_seq, \
                fk.\"table\" AS ref_table, fk.\"from\" AS from_col, \
                fk.\"to\" AS to_col, fk.on_delete, fk.on_update \
         FROM {master} AS m, pragma_foreign_key_list(m.name{schema_arg}) AS fk \
         WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%'"
    );
    if table.is_some() {
        sql.push_str(" AND m.name = ?");
    }
    sql.push_str(" ORDER BY m.name, fk.id, fk.seq");
    let mut query = sqlx::query(&sql);
    if let Some(table) = table {
        query = query.bind(table);
    }
    let rows = query.fetch_all(pool).await?;

    let mut map: BTreeMap<(String, i64), ForeignKeyInfo> = BTreeMap::new();
    for row in &rows {
        let table_name: String = row.try_get("tbl")?;
        let id: i64 = row.try_get("fk_id")?;
        let ref_table: String = row.try_get("ref_table")?;
        let from_col: String = row.try_get("from_col")?;
        let to_col: Option<String> = row.try_get("to_col").ok();
        let on_delete: String = row.try_get("on_delete")?;
        let on_update: String = row.try_get("on_update")?;
        map.entry((table_name.clone(), id))
            .and_modify(|fk| {
                fk.columns.push(from_col.clone());
                fk.ref_columns.push(to_col.clone().unwrap_or_default());
            })
            .or_insert(ForeignKeyInfo {
                schema: Some(schema_name.to_string()),
                table: table_name,
                name: format!("fk_{id}"),
                columns: vec![from_col],
                ref_schema: None,
                ref_table,
                ref_columns: vec![to_col.unwrap_or_default()],
                on_delete: Some(on_delete),
                on_update: Some(on_update),
            });
    }
    Ok(map.into_values().collect())
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

    async fn in_memory_driver() -> SqliteDriver {
        SqliteDriver::new("sqlite::memory:").await.expect("driver")
    }

    async fn exec(driver: &SqliteDriver, sql: &str) {
        driver
            .execute(&BoundQuery {
                sql: sql.to_string(),
                params: Vec::new(),
                row_cap: None,
                fetch_rows: false,
            })
            .await
            .expect(sql);
    }

    #[tokio::test]
    async fn foreign_keys_are_set_based_and_group_composite_keys() {
        let driver = in_memory_driver().await;
        exec(
            &driver,
            "CREATE TABLE parent (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        )
        .await;
        exec(
            &driver,
            "CREATE TABLE child (x INTEGER, y INTEGER, \
                 FOREIGN KEY (x, y) REFERENCES parent (a, b))",
        )
        .await;
        exec(
            &driver,
            "CREATE TABLE other (id INTEGER PRIMARY KEY, cid INTEGER REFERENCES child (x))",
        )
        .await;

        let fks = driver
            .list_foreign_keys(None, None)
            .await
            .expect("all-table foreign keys");
        assert_eq!(fks.len(), 2, "one FK per constraint: {fks:?}");

        let composite = fks.iter().find(|f| f.table == "child").expect("child FK");
        assert_eq!(composite.name, "fk_0");
        assert_eq!(composite.columns, vec!["x", "y"]);
        assert_eq!(composite.ref_table, "parent");
        assert_eq!(composite.ref_columns, vec!["a", "b"]);

        let single = fks.iter().find(|f| f.table == "other").expect("other FK");
        assert_eq!(single.columns, vec!["cid"]);
        assert_eq!(single.ref_columns, vec!["x"]);
    }

    #[tokio::test]
    async fn indexes_are_set_based_and_include_autoindex_columns() {
        let driver = in_memory_driver().await;
        exec(
            &driver,
            "CREATE TABLE parent (a INTEGER, b INTEGER, PRIMARY KEY (a, b))",
        )
        .await;
        exec(&driver, "CREATE TABLE child (x INTEGER, y INTEGER)").await;
        exec(&driver, "CREATE INDEX idx_child_x ON child (x)").await;

        let indexes = driver.list_indexes(None, None).await.expect("indexes");
        let pk = indexes
            .iter()
            .find(|i| i.name == "sqlite_autoindex_parent_1")
            .expect("PK autoindex");
        assert_eq!(pk.columns, vec!["a", "b"]);
        assert!(pk.unique);
        assert!(pk.primary);

        let idx = indexes
            .iter()
            .find(|i| i.name == "idx_child_x")
            .expect("explicit index");
        assert_eq!(idx.columns, vec!["x"]);
        assert!(!idx.unique);
    }

    #[tokio::test]
    async fn table_page_pushes_keyword_and_limit() {
        let driver = in_memory_driver().await;
        for name in ["alpha", "alpine", "beta", "gamma"] {
            exec(&driver, &format!("CREATE TABLE {name} (id INTEGER)")).await;
        }
        let page = driver
            .list_tables_page(None, Some("alp"), 0, 1)
            .await
            .expect("page");
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].name, "alpha");
        assert!(page.has_more, "second match must be visible via has_more");

        let page = driver
            .list_tables_page(None, Some("alp"), 1, 1)
            .await
            .expect("second page");
        assert_eq!(page.objects[0].name, "alpine");
        assert!(!page.has_more);
    }

    #[tokio::test]
    async fn driver_level_timeout_interrupts_runaway_query() {
        let driver = SqliteDriver::new_with_options(
            "sqlite::memory:",
            &ConnectOptions {
                statement_timeout: Duration::from_millis(25),
                ..ConnectOptions::default()
            },
        )
        .await
        .expect("driver");
        // A recursive CTE with no row cap runs for a long time; the progress
        // handler must abort it rather than waiting for the tool-layer timeout.
        let result = driver
            .execute(&BoundQuery {
                sql: "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
                      SELECT count(*) FROM c"
                    .into(),
                params: Vec::new(),
                row_cap: None,
                fetch_rows: true,
            })
            .await;
        assert!(
            matches!(result, Err(DriverError::Timeout)),
            "expected driver timeout, got {result:?}"
        );
    }
}
