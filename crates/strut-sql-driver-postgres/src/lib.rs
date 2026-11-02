use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::collections::BTreeMap;
use std::time::Duration;
use strut_sql_driver_api::{
    BoundQuery, ColumnInfo, ColumnMeta, DriverError, EngineDriver, EngineKind, ForeignKeyInfo,
    IndexInfo, ObjectType, QueryMeta, QueryResult, RoutineInfo, SchemaObject, SqlValue, TableInfo,
};
use uuid::Uuid;

pub struct PostgresDriver {
    pool: PgPool,
}

impl PostgresDriver {
    pub async fn new(connection_string: &str) -> Result<Self, DriverError> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(Duration::from_secs(2))
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

    async fn from_connection_string(connection_string: &str) -> Result<Box<Self>, DriverError> {
        Ok(Box::new(PostgresDriver::new(connection_string).await?))
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

        pg_rows_to_result(&rows, truncated)
    }

    async fn list_tables(&self) -> Result<Vec<SchemaObject>, DriverError> {
        let rows = sqlx::query(
            r#"
            SELECT table_schema, table_name
            FROM information_schema.tables
            WHERE table_type = 'BASE TABLE'
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
                    schema: r.try_get("table_schema").ok(),
                    name: r.try_get("table_name").ok()?,
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
        // Resolve the schema of the table when the caller did not name
        // one: an unqualified CREATE lands in the first search_path schema
        // (often the per-user schema), not necessarily "public".
        let schema_name: String = match schema.filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => sqlx::query_scalar(
                "SELECT table_schema FROM information_schema.tables \
                 WHERE table_name = $1 \
                   AND table_schema NOT IN ('pg_catalog', 'information_schema') \
                 ORDER BY (table_schema = current_schema()) DESC, table_schema \
                 LIMIT 1",
            )
            .bind(table)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| DriverError::Introspection(e.to_string()))?
            .unwrap_or_else(|| "public".to_string()),
        };

        // Bind the schema (never interpolate caller input). The EXISTS
        // subquery marks primary-key columns from the catalog.
        let rows = sqlx::query(
            r#"
            SELECT
                c.column_name,
                CASE WHEN c.data_type = 'USER-DEFINED' THEN c.udt_name ELSE c.data_type END
                    AS data_type,
                c.is_nullable,
                c.column_default,
                EXISTS (
                    SELECT 1
                    FROM information_schema.table_constraints tc
                    JOIN information_schema.key_column_usage kcu
                      ON kcu.constraint_name = tc.constraint_name
                     AND kcu.constraint_schema = tc.constraint_schema
                    WHERE tc.constraint_type = 'PRIMARY KEY'
                      AND tc.table_schema = c.table_schema
                      AND tc.table_name = c.table_name
                      AND kcu.column_name = c.column_name
                ) AS is_primary_key
            FROM information_schema.columns c
            WHERE c.table_schema = $1 AND c.table_name = $2
            ORDER BY c.ordinal_position
            "#,
        )
        .bind(&schema_name)
        .bind(table)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        let columns = rows
            .iter()
            .filter_map(|r| {
                Some(ColumnInfo {
                    name: r.try_get("column_name").ok()?,
                    data_type: r.try_get("data_type").ok()?,
                    nullable: r
                        .try_get::<String, _>("is_nullable")
                        .map(|v| v == "YES")
                        .unwrap_or(false),
                    default_value: r.try_get("column_default").ok(),
                    is_primary_key: r.try_get("is_primary_key").unwrap_or(false),
                })
            })
            .collect();

        Ok(TableInfo {
            schema: Some(schema_name),
            name: table.to_string(),
            columns,
        })
    }

    async fn list_indexes(&self) -> Result<Vec<IndexInfo>, DriverError> {
        let rows = sqlx::query(
            r#"
            SELECT
                schemaname as schema,
                tablename as table,
                indexname as name,
                indexdef
            FROM pg_indexes
            ORDER BY schemaname, tablename, indexname
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        Ok(rows
            .iter()
            .filter_map(|r| {
                let indexdef: String = r.try_get("indexdef").ok()?;
                let table: String = r.try_get("table").ok()?;
                let name: String = r.try_get("name").ok()?;
                Some(IndexInfo {
                    schema: r.try_get("schema").ok(),
                    primary: name == format!("{table}_pkey"),
                    columns: pg_indexdef_columns(&indexdef),
                    unique: pg_indexdef_is_unique(&indexdef),
                    table,
                    name,
                })
            })
            .collect())
    }

    async fn list_foreign_keys(&self) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        let rows = sqlx::query(
            r#"
            SELECT
                tc.table_schema,
                tc.table_name,
                tc.constraint_name,
                kcu.column_name,
                ccu.table_schema AS foreign_table_schema,
                ccu.table_name AS foreign_table_name,
                ccu.column_name AS foreign_column_name,
                rc.delete_rule,
                rc.update_rule
            FROM information_schema.table_constraints AS tc
            JOIN information_schema.key_column_usage AS kcu
                ON tc.constraint_name = kcu.constraint_name
                AND tc.table_schema = kcu.table_schema
            JOIN information_schema.constraint_column_usage AS ccu
                ON ccu.constraint_name = tc.constraint_name
                AND ccu.table_schema = tc.table_schema
            JOIN information_schema.referential_constraints AS rc
                ON tc.constraint_name = rc.constraint_name
                AND tc.table_schema = rc.constraint_schema
            WHERE tc.constraint_type = 'FOREIGN KEY'
            ORDER BY tc.table_schema, tc.table_name, tc.constraint_name, kcu.ordinal_position
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DriverError::Introspection(e.to_string()))?;

        // Group by the composite (schema, table, constraint): same-named
        // constraints on different tables are distinct facts.
        let mut fk_map: BTreeMap<(String, String, String), ForeignKeyInfo> = BTreeMap::new();
        for row in &rows {
            let Ok(schema) = row.try_get::<String, _>("table_schema") else {
                continue;
            };
            let Ok(table) = row.try_get::<String, _>("table_name") else {
                continue;
            };
            let Ok(constraint) = row.try_get::<String, _>("constraint_name") else {
                continue;
            };
            let Ok(column) = row.try_get::<String, _>("column_name") else {
                continue;
            };
            let Ok(ref_schema) = row.try_get::<String, _>("foreign_table_schema") else {
                continue;
            };
            let Ok(ref_table) = row.try_get::<String, _>("foreign_table_name") else {
                continue;
            };
            let ref_column = row.try_get::<String, _>("foreign_column_name").ok();
            let on_delete = row.try_get::<String, _>("delete_rule").ok();
            let on_update = row.try_get::<String, _>("update_rule").ok();

            let key = (schema.clone(), table.clone(), constraint.clone());
            let entry = fk_map.entry(key).or_insert_with(|| ForeignKeyInfo {
                schema: Some(schema),
                table,
                name: constraint,
                columns: Vec::new(),
                ref_schema: Some(ref_schema),
                ref_table,
                ref_columns: Vec::new(),
                on_delete,
                on_update,
            });
            entry.columns.push(column);
            entry.ref_columns.push(ref_column.unwrap_or_default());
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
                data_type
            FROM information_schema.routines
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
                    schema: r.try_get("routine_schema").ok(),
                    name: r.try_get("routine_name").ok()?,
                    routine_type: r
                        .try_get("routine_type")
                        .unwrap_or_else(|_| "FUNCTION".to_string()),
                    return_type: r.try_get("data_type").ok(),
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

        let objects: Vec<SchemaObject> = if object_type == Some(ObjectType::Schema) {
            let rows = sqlx::query(
                r#"
                SELECT schema_name
                FROM information_schema.schemata
                WHERE schema_name NOT LIKE 'pg\_%'
                  AND schema_name != 'information_schema'
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
                        name: r.try_get("schema_name").ok()?,
                        display_name: None,
                    })
                })
                .collect()
        } else {
            let rows = sqlx::query(
                r#"
                SELECT table_schema, table_name
                FROM information_schema.tables
                WHERE table_type = 'BASE TABLE'
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
                        schema: r.try_get("table_schema").ok(),
                        name: r.try_get("table_name").ok()?,
                        display_name: None,
                    })
                })
                .collect()
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
        },
    })
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
    if let Ok(v) = row.try_get::<DateTime<Utc>, _>(index) {
        return Ok(SqlValue::DateTime(v.naive_utc()));
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
            return Ok(SqlValue::Array(v));
        }
        if let Ok(bytes) = raw.as_bytes() {
            return Ok(SqlValue::Binary(bytes.to_vec()));
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
            Err(e) => SqlValue::Binary(e.into_bytes()),
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
fn decode_pg_array(row: &PgRow, index: usize) -> Option<Vec<SqlValue>> {
    if let Ok(v) = row.try_get::<Vec<String>, _>(index) {
        return Some(v.into_iter().map(SqlValue::String).collect());
    }
    if let Ok(v) = row.try_get::<Vec<i64>, _>(index) {
        return Some(v.into_iter().map(SqlValue::I64).collect());
    }
    if let Ok(v) = row.try_get::<Vec<i32>, _>(index) {
        return Some(v.into_iter().map(|n| SqlValue::I64(i64::from(n))).collect());
    }
    if let Ok(v) = row.try_get::<Vec<f64>, _>(index) {
        return Some(v.into_iter().map(SqlValue::F64).collect());
    }
    if let Ok(v) = row.try_get::<Vec<bool>, _>(index) {
        return Some(v.into_iter().map(SqlValue::Bool).collect());
    }
    if let Ok(v) = row.try_get::<Vec<Uuid>, _>(index) {
        return Some(v.into_iter().map(SqlValue::Uuid).collect());
    }
    if let Ok(v) = row.try_get::<Vec<BigDecimal>, _>(index) {
        return Some(v.into_iter().map(SqlValue::BigDecimal).collect());
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
