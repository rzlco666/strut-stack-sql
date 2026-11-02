use serde::{Deserialize, Serialize};
use sqlx::{mysql::MySqlRow, postgres::PgRow, MySqlPool, PgPool, Row, SqlitePool};

use crate::db::value::{mysql_decode_text_by_index, mysql_decode_text_by_name};
use crate::db::{EngineKind, EnginePool};

/// SQL-BUG-07: pool-variant access used to `.expect()` and kill the whole MCP
/// process on an engine mismatch. Map the mismatch into a structured
/// `sqlx::Error` (the error type every caller here already propagates).
fn pool_mismatch(e: anyhow::Error) -> sqlx::Error {
    sqlx::Error::Configuration(format!("engine pool mismatch: {e}").into())
}

fn pg_pool(pool: &EnginePool) -> Result<&PgPool, sqlx::Error> {
    pool.postgres().map_err(pool_mismatch)
}

fn mysql_pool(pool: &EnginePool) -> Result<&MySqlPool, sqlx::Error> {
    pool.mysql().map_err(pool_mismatch)
}

fn sqlite_pool(pool: &EnginePool) -> Result<&SqlitePool, sqlx::Error> {
    pool.sqlite().map_err(pool_mismatch)
}

// ---------------------------------------------------------------------------
// Schema metadata cache (SQL-BUG-10)
//
// `list_tables` / `schema_overview` results are cached per source (engine +
// connection URL + schema [+ keyword]) for a short TTL so repeated agent
// introspection does not re-hit the catalogs every call. Freshness is
// TTL-only: a DDL change becomes visible after the TTL lapses at the latest.
// TTL comes from `MCP_SQL_SCHEMA_CACHE_TTL_SECS` (default 30; 0 disables).
// ---------------------------------------------------------------------------

use std::collections::HashMap as StdHashMap;
use std::sync::{Mutex as StdMutex, OnceLock};
use std::time::{Duration as StdDuration, Instant as StdInstant};

fn schema_cache_ttl() -> StdDuration {
    match std::env::var("MCP_SQL_SCHEMA_CACHE_TTL_SECS") {
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .map(StdDuration::from_secs)
            .unwrap_or(StdDuration::from_secs(30)),
        Err(_) => StdDuration::from_secs(30),
    }
}

/// One schema-cache map: lookup key -> (insertion instant, cached value).
type CacheMap<V> = StdHashMap<String, (StdInstant, V)>;

fn tables_cache() -> &'static StdMutex<CacheMap<Vec<SchemaObject>>> {
    static CACHE: OnceLock<StdMutex<CacheMap<Vec<SchemaObject>>>> = OnceLock::new();
    CACHE.get_or_init(|| StdMutex::new(StdHashMap::new()))
}

fn overview_cache() -> &'static StdMutex<CacheMap<SchemaOverview>> {
    static CACHE: OnceLock<StdMutex<CacheMap<SchemaOverview>>> = OnceLock::new();
    CACHE.get_or_init(|| StdMutex::new(StdHashMap::new()))
}

fn cache_lookup<V: Clone>(
    map: &StdMutex<StdHashMap<String, (StdInstant, V)>>,
    key: &str,
    ttl: StdDuration,
) -> Option<V> {
    if ttl.is_zero() {
        return None;
    }
    let mut guard = map.lock().ok()?;
    match guard.get(key) {
        Some((at, value)) if at.elapsed() < ttl => Some(value.clone()),
        Some(_) => {
            guard.remove(key);
            None
        }
        None => None,
    }
}

fn cache_store<V>(map: &StdMutex<StdHashMap<String, (StdInstant, V)>>, key: String, value: V) {
    if let Ok(mut guard) = map.lock() {
        guard.insert(key, (StdInstant::now(), value));
    }
}

fn tables_cache_key(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
    keyword: Option<&str>,
) -> String {
    format!(
        "tables|{:?}|{}|{}|{}",
        pool.engine(),
        connection_url.unwrap_or(""),
        schema.unwrap_or(""),
        keyword.unwrap_or("")
    )
}

fn overview_cache_key(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
) -> String {
    format!(
        "overview|{:?}|{}|{}",
        pool.engine(),
        connection_url.unwrap_or(""),
        schema.unwrap_or("")
    )
}

/// MySQL column row layout: `table_schema, table_name, column_name, data_type, is_nullable`.
const MYSQL_COL_NAME_IDX: usize = 2;
const MYSQL_COL_TYPE_IDX: usize = 3;
const MYSQL_COL_NULL_IDX: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ObjectType {
    Schema,
    Table,
    Column,
    Index,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaObject {
    pub object_type: ObjectType,
    pub schema: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<ColumnInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexes: Option<Vec<IndexInfo>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForeignKeyInfo {
    pub name: String,
    pub table: String,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_update: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexInfo {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// Flat index row for `list_indexes` (token-cheap; no nested `indexes[]` / indexdef).
#[derive(Debug, Clone, Serialize)]
pub struct ListedIndex {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableOverview {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approx_rows: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaOverviewMeta {
    pub n_tables: usize,
    pub n_foreign_keys: usize,
    pub truncated: bool,
    /// Always `"approx"` — planner/stats estimates; use `COUNT(*)` for exact.
    pub row_counts: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaOverview {
    pub schema: String,
    pub tables: Vec<TableOverview>,
    pub foreign_keys: Vec<ForeignKeyInfo>,
    pub meta: SchemaOverviewMeta,
}

const SCHEMA_OVERVIEW_MAX_TABLES: usize = 200;

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub objects: Vec<SchemaObject>,
    pub meta: SearchMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchMeta {
    pub n: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
}

pub async fn search_objects(
    pool: &EnginePool,
    connection_url: Option<&str>,
    object_type: ObjectType,
    keyword: Option<&str>,
    schema: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<SearchResult, sqlx::Error> {
    let objects = match object_type {
        ObjectType::Schema => list_schemas(pool, keyword).await?,
        ObjectType::Table => list_tables(pool, connection_url, schema, keyword).await?,
        ObjectType::Column => list_columns(pool, connection_url, schema, keyword).await?,
        ObjectType::Index => list_indexes(pool, connection_url, schema, None, keyword).await?,
    };

    let total = objects.len();
    let slice = paginate_objects(objects, offset, limit);

    Ok(SearchResult {
        meta: SearchMeta {
            n: slice.len(),
            offset,
            limit,
            has_more: offset + slice.len() < total,
        },
        objects: slice,
    })
}

/// Slice one page out of `objects` (SQL-BUG-01). The old code drained
/// `offset..total.min(offset + limit)`, which panics when `offset > total`
/// (invalid range). Clamp both bounds so an out-of-range offset simply
/// yields an empty page.
fn paginate_objects(
    mut objects: Vec<SchemaObject>,
    offset: usize,
    limit: usize,
) -> Vec<SchemaObject> {
    let total = objects.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    objects.drain(start..end).collect()
}

pub async fn list_schemas(
    pool: &EnginePool,
    keyword: Option<&str>,
) -> Result<Vec<SchemaObject>, sqlx::Error> {
    match pool.engine() {
        EngineKind::Postgres => {
            let pool = pg_pool(pool)?;
            let rows = sqlx::query(
                "SELECT schema_name FROM information_schema.schemata \
                 WHERE schema_name NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
                 ORDER BY schema_name",
            )
            .fetch_all(pool)
            .await?;
            Ok(filter_objects(
                rows.iter()
                    .filter_map(|r| {
                        let name: String = r.try_get(0).ok()?;
                        Some(SchemaObject {
                            object_type: ObjectType::Schema,
                            schema: None,
                            name,
                            table: None,
                            data_type: None,
                            nullable: None,
                            columns: None,
                            indexes: None,
                        })
                    })
                    .collect(),
                keyword,
            ))
        }
        EngineKind::Mysql => {
            let pool = mysql_pool(pool)?;
            let rows = sqlx::query("SHOW DATABASES").fetch_all(pool).await?;
            Ok(filter_objects(
                rows.iter()
                    .filter_map(|r| {
                        let name: String = r.try_get(0).ok()?;
                        Some(SchemaObject {
                            object_type: ObjectType::Schema,
                            schema: None,
                            name,
                            table: None,
                            data_type: None,
                            nullable: None,
                            columns: None,
                            indexes: None,
                        })
                    })
                    .collect(),
                keyword,
            ))
        }
        EngineKind::Sqlite => {
            let pool = sqlite_pool(pool)?;
            let rows = sqlx::query("PRAGMA database_list").fetch_all(pool).await?;
            Ok(filter_objects(
                rows.iter()
                    .filter_map(|r| {
                        let name: String = r.try_get(1).ok()?;
                        Some(SchemaObject {
                            object_type: ObjectType::Schema,
                            schema: None,
                            name,
                            table: None,
                            data_type: None,
                            nullable: None,
                            columns: None,
                            indexes: None,
                        })
                    })
                    .collect(),
                keyword,
            ))
        }
    }
}

pub async fn list_tables(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
    keyword: Option<&str>,
) -> Result<Vec<SchemaObject>, sqlx::Error> {
    let ttl = schema_cache_ttl();
    let key = tables_cache_key(pool, connection_url, schema, keyword);
    if let Some(hit) = cache_lookup(tables_cache(), &key, ttl) {
        return Ok(hit);
    }
    let fresh = list_tables_uncached(pool, connection_url, schema, keyword).await?;
    if !ttl.is_zero() {
        cache_store(tables_cache(), key, fresh.clone());
    }
    Ok(fresh)
}

async fn list_tables_uncached(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
    keyword: Option<&str>,
) -> Result<Vec<SchemaObject>, sqlx::Error> {
    match pool.engine() {
        EngineKind::Postgres => {
            let pool = pg_pool(pool)?;
            let schema = schema.unwrap_or("public");
            let rows = sqlx::query(
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = $1 AND table_type = 'BASE TABLE' \
                 ORDER BY table_name",
            )
            .bind(schema)
            .fetch_all(pool)
            .await?;
            Ok(filter_objects(
                rows.iter()
                    .filter_map(|r| {
                        let name: String = r.try_get(0).ok()?;
                        Some(SchemaObject {
                            object_type: ObjectType::Table,
                            schema: Some(schema.into()),
                            name,
                            table: None,
                            data_type: None,
                            nullable: None,
                            columns: None,
                            indexes: None,
                        })
                    })
                    .collect(),
                keyword,
            ))
        }
        EngineKind::Mysql => {
            let pool = mysql_pool(pool)?;
            let scope = resolve_mysql_schema(pool, schema, connection_url).await?;

            let rows = match &scope {
                MysqlSchemaScope::All => {
                    sqlx::query(
                        "SELECT table_schema, table_name FROM information_schema.tables \
                         WHERE table_type = 'BASE TABLE' ORDER BY table_schema, table_name",
                    )
                    .fetch_all(pool)
                    .await?
                }
                MysqlSchemaScope::One(schema_name) => {
                    sqlx::query(
                        "SELECT table_schema, table_name FROM information_schema.tables \
                         WHERE table_schema = ? AND table_type = 'BASE TABLE' ORDER BY table_name",
                    )
                    .bind(schema_name)
                    .fetch_all(pool)
                    .await?
                }
            };

            Ok(filter_objects(
                rows.iter()
                    .filter_map(|r| {
                        let sch: String = r.try_get(0).ok()?;
                        let name: String = r.try_get(1).ok()?;
                        Some(SchemaObject {
                            object_type: ObjectType::Table,
                            schema: Some(sch),
                            name,
                            table: None,
                            data_type: None,
                            nullable: None,
                            columns: None,
                            indexes: None,
                        })
                    })
                    .collect(),
                keyword,
            ))
        }
        EngineKind::Sqlite => {
            let pool = sqlite_pool(pool)?;
            let schema_name = resolve_sqlite_schema(schema);
            let rows = sqlite_list_table_rows(pool, schema_name).await?;
            Ok(filter_objects(
                rows.iter()
                    .filter_map(|r| {
                        let name: String = r.try_get(0).ok()?;
                        Some(SchemaObject {
                            object_type: ObjectType::Table,
                            schema: Some(schema_name.into()),
                            name,
                            table: None,
                            data_type: None,
                            nullable: None,
                            columns: None,
                            indexes: None,
                        })
                    })
                    .collect(),
                keyword,
            ))
        }
    }
}

async fn list_columns(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
    keyword: Option<&str>,
) -> Result<Vec<SchemaObject>, sqlx::Error> {
    match pool.engine() {
        EngineKind::Postgres => {
            let pool = pg_pool(pool)?;
            let schema = schema.unwrap_or("public");
            let rows = sqlx::query(
                "SELECT table_name, column_name, \
                 CASE WHEN data_type = 'USER-DEFINED' THEN udt_name ELSE data_type END, \
                 is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema = $1 ORDER BY table_name, ordinal_position",
            )
            .bind(schema)
            .fetch_all(pool)
            .await?;
            Ok(filter_objects(
                rows.iter()
                    .filter_map(|r| {
                        let table: String = r.try_get(0).ok()?;
                        let name: String = r.try_get(1).ok()?;
                        let data_type: String = r.try_get(2).ok()?;
                        let nullable: String = r.try_get(3).ok()?;
                        Some(SchemaObject {
                            object_type: ObjectType::Column,
                            schema: Some(schema.into()),
                            name,
                            table: Some(table),
                            data_type: Some(data_type),
                            nullable: Some(nullable == "YES"),
                            columns: None,
                            indexes: None,
                        })
                    })
                    .collect(),
                keyword,
            ))
        }
        EngineKind::Mysql => {
            let pool = mysql_pool(pool)?;
            let scope = resolve_mysql_schema(pool, schema, connection_url).await?;
            let rows = match &scope {
                MysqlSchemaScope::All => {
                    sqlx::query(
                        "SELECT table_schema, table_name, column_name, data_type, is_nullable \
                         FROM information_schema.columns \
                         ORDER BY table_schema, table_name, ordinal_position",
                    )
                    .fetch_all(pool)
                    .await?
                }
                MysqlSchemaScope::One(schema_name) => {
                    sqlx::query(
                        "SELECT table_schema, table_name, column_name, data_type, is_nullable \
                         FROM information_schema.columns WHERE table_schema = ? \
                         ORDER BY table_name, ordinal_position",
                    )
                    .bind(schema_name)
                    .fetch_all(pool)
                    .await?
                }
            };
            Ok(filter_objects(
                map_mysql_column_rows_to_schema_objects(&rows, None),
                keyword,
            ))
        }
        EngineKind::Sqlite => {
            let pool = sqlite_pool(pool)?;
            let schema_name = resolve_sqlite_schema(schema);
            let rows = sqlx::query(
                "SELECT m.name AS table_name, p.name, p.type, p.notnull \
                 FROM sqlite_master AS m, pragma_table_info(m.name) AS p \
                 WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' \
                 ORDER BY m.name, p.cid",
            )
            .fetch_all(pool)
            .await?;
            Ok(filter_objects(
                rows.iter()
                    .filter_map(|r| {
                        let table: String = r.try_get(0).ok()?;
                        let name: String = r.try_get(1).ok()?;
                        let data_type: String = r.try_get(2).ok()?;
                        let notnull: i64 = r.try_get(3).ok()?;
                        Some(SchemaObject {
                            object_type: ObjectType::Column,
                            schema: Some(schema_name.into()),
                            name,
                            table: Some(table),
                            data_type: Some(data_type),
                            nullable: Some(notnull == 0),
                            columns: None,
                            indexes: None,
                        })
                    })
                    .collect(),
                keyword,
            ))
        }
    }
}

pub async fn describe_table(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
    table: &str,
) -> Result<SchemaObject, sqlx::Error> {
    let engine = pool.engine();

    let (schema, table_name) = match engine {
        EngineKind::Postgres => (schema.unwrap_or("public").to_string(), table.to_string()),
        EngineKind::Mysql => {
            let pool = mysql_pool(pool)?;
            let (schema_from_table, table_only) = split_mysql_table(table);
            let table_name = normalize_mysql_ident(table_only);
            let resolved = resolve_mysql_schema(pool, schema, connection_url).await?;
            let schema = match resolved {
                MysqlSchemaScope::One(name) => normalize_mysql_ident(&name),
                MysqlSchemaScope::All => schema_from_table
                    .map(|s| normalize_mysql_ident(&s))
                    .ok_or_else(|| {
                        sqlx::Error::Configuration(
                            "describe_table requires schema when searching all databases".into(),
                        )
                    })?,
            };
            (schema, table_name)
        }
        EngineKind::Sqlite => {
            let (schema_name, table_only) = split_sqlite_table(schema, table);
            (schema_name, table_only)
        }
    };

    let columns = match engine {
        EngineKind::Postgres => {
            let pool = pg_pool(pool)?;
            let rows = sqlx::query(
                "SELECT column_name, \
                 CASE WHEN data_type = 'USER-DEFINED' THEN udt_name ELSE data_type END, \
                 is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema = $1 AND table_name = $2 \
                 ORDER BY ordinal_position",
            )
            .bind(&schema)
            .bind(&table_name)
            .fetch_all(pool)
            .await?;
            rows.iter()
                .filter_map(column_info_from_pg_info_schema_row)
                .collect()
        }
        EngineKind::Mysql => {
            let pool = mysql_pool(pool)?;
            let mut columns = mysql_columns_from_show(pool, &schema, &table_name).await?;
            if columns.is_empty() {
                columns = mysql_columns_from_information_schema(pool, &schema, &table_name).await?;
            }
            columns
        }
        EngineKind::Sqlite => {
            let pool = sqlite_pool(pool)?;
            sqlite_columns_from_pragma(pool, &table_name).await?
        }
    };

    let index_schema = if schema.is_empty() {
        None
    } else {
        Some(schema.as_str())
    };
    let indexes = list_indexes(pool, connection_url, index_schema, Some(&table_name), None)
        .await?
        .into_iter()
        .map(index_info_from_schema_object)
        .collect();

    Ok(SchemaObject {
        object_type: ObjectType::Table,
        schema: if schema.is_empty() {
            None
        } else {
            Some(schema)
        },
        name: table_name,
        table: None,
        data_type: None,
        nullable: None,
        columns: Some(columns),
        indexes: Some(indexes),
    })
}

pub async fn list_indexes(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    keyword: Option<&str>,
) -> Result<Vec<SchemaObject>, sqlx::Error> {
    match pool.engine() {
        EngineKind::Postgres => {
            let pool = pg_pool(pool)?;
            let schema = schema.unwrap_or("public");
            let rows = if let Some(table) = table {
                sqlx::query(
                    "SELECT indexname, indexdef, tablename FROM pg_indexes \
                     WHERE schemaname = $1 AND tablename = $2 ORDER BY indexname",
                )
                .bind(schema)
                .bind(table)
                .fetch_all(pool)
                .await?
            } else {
                sqlx::query(
                    "SELECT indexname, indexdef, tablename FROM pg_indexes \
                     WHERE schemaname = $1 ORDER BY indexname",
                )
                .bind(schema)
                .fetch_all(pool)
                .await?
            };
            Ok(filter_objects(
                rows.iter()
                    .filter_map(|r| {
                        let name: String = r.try_get(0).ok()?;
                        let def: String = r.try_get(1).ok()?;
                        let tablename: String = r.try_get(2).ok()?;
                        let unique = pg_indexdef_is_unique(&def);
                        let columns = pg_indexdef_columns(&def);
                        // Do not put full CREATE INDEX into data_type (token waste).
                        Some(SchemaObject {
                            object_type: ObjectType::Index,
                            schema: Some(schema.into()),
                            name: name.clone(),
                            table: Some(tablename),
                            data_type: None,
                            nullable: None,
                            columns: None,
                            indexes: Some(vec![IndexInfo {
                                name,
                                columns,
                                unique,
                            }]),
                        })
                    })
                    .collect(),
                keyword,
            ))
        }
        EngineKind::Mysql => {
            let pool = mysql_pool(pool)?;
            let scope = resolve_mysql_schema(pool, schema, connection_url).await?;
            // Uniform layout for every scope/table combination:
            // table_schema, table_name, index_name, column_name, non_unique.
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
                    .fetch_all(pool)
                    .await?
                }
                (MysqlSchemaScope::One(schema_name), None) => {
                    sqlx::query(
                        "SELECT table_schema, table_name, index_name, column_name, non_unique \
                         FROM information_schema.statistics \
                         WHERE table_schema = ? \
                         ORDER BY table_schema, table_name, index_name, seq_in_index",
                    )
                    .bind(schema_name)
                    .fetch_all(pool)
                    .await?
                }
                (MysqlSchemaScope::All, Some(table)) => {
                    sqlx::query(
                        "SELECT table_schema, table_name, index_name, column_name, non_unique \
                         FROM information_schema.statistics \
                         WHERE table_name = ? \
                         ORDER BY table_schema, table_name, index_name, seq_in_index",
                    )
                    .bind(table)
                    .fetch_all(pool)
                    .await?
                }
                (MysqlSchemaScope::All, None) => {
                    sqlx::query(
                        "SELECT table_schema, table_name, index_name, column_name, non_unique \
                         FROM information_schema.statistics \
                         ORDER BY table_schema, table_name, index_name, seq_in_index",
                    )
                    .fetch_all(pool)
                    .await?
                }
            };

            let objects = group_mysql_index_rows(extract_mysql_index_rows(&rows));
            Ok(filter_objects(objects, keyword))
        }
        EngineKind::Sqlite => {
            let pool = sqlite_pool(pool)?;
            let objects = sqlite_list_index_objects(pool, schema, table).await?;
            Ok(filter_objects(objects, keyword))
        }
    }
}

/// One raw MySQL index row: (table_schema, table_name, index_name,
/// column_name, non_unique) from `information_schema.statistics`.
#[derive(Debug, Clone)]
struct MysqlIndexRowData {
    schema: String,
    table: String,
    name: String,
    column: String,
    unique: bool,
}

fn extract_mysql_index_rows(rows: &[MySqlRow]) -> Vec<MysqlIndexRowData> {
    rows.iter()
        .map(|row| MysqlIndexRowData {
            schema: row.try_get(0).ok().unwrap_or_default(),
            table: row.try_get(1).ok().unwrap_or_default(),
            name: row.try_get(2).ok().unwrap_or_default(),
            column: row.try_get(3).ok().unwrap_or_default(),
            unique: row.try_get::<i32, _>(4).ok().unwrap_or(1) == 0,
        })
        .collect()
}

/// Group MySQL index rows into one `SchemaObject` per index.
///
/// SQL-BUG-03: grouping used to key on the index name alone (or
/// `schema.name` in the all-databases path), merging `PRIMARY` / `idx_status`
/// across tables into one ambiguous object. The key is now the composite
/// (schema, table, index name), and each object carries its real table.
fn group_mysql_index_rows(rows: Vec<MysqlIndexRowData>) -> Vec<SchemaObject> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<(String, String, String), (Vec<String>, bool)> = BTreeMap::new();
    for row in rows {
        let key = (row.schema.clone(), row.table.clone(), row.name.clone());
        let entry = map.entry(key).or_insert((Vec::new(), row.unique));
        entry.0.push(row.column);
    }
    map.into_iter()
        .map(|((schema, table, name), (columns, unique))| SchemaObject {
            object_type: ObjectType::Index,
            schema: Some(schema),
            name: name.clone(),
            table: Some(table),
            data_type: None,
            nullable: None,
            columns: None,
            indexes: Some(vec![IndexInfo {
                name,
                columns,
                unique,
            }]),
        })
        .collect()
}

fn resolve_sqlite_schema(schema: Option<&str>) -> &str {
    schema.filter(|s| !s.is_empty()).unwrap_or("main")
}

fn split_sqlite_table(schema: Option<&str>, table: &str) -> (String, String) {
    let trimmed = table.trim().trim_matches('"');
    if let Some((sch, tbl)) = trimmed.rsplit_once('.') {
        let sch = sch.trim().trim_matches('"');
        let tbl = tbl.trim().trim_matches('"');
        if !sch.is_empty() && !tbl.is_empty() {
            return (sch.to_string(), tbl.to_string());
        }
    }
    let schema_name = resolve_sqlite_schema(schema).to_string();
    (schema_name, trimmed.to_string())
}

async fn sqlite_list_table_rows(
    pool: &sqlx::SqlitePool,
    schema_name: &str,
) -> Result<Vec<sqlx::sqlite::SqliteRow>, sqlx::Error> {
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

fn sqlite_quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Quote a SQLite identifier (schema/table/index name) for interpolation into
/// a statement that cannot take bind parameters (e.g. `"<schema>".sqlite_master`).
/// Embedded double quotes are doubled, per SQL identifier rules.
fn sqlite_quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn sqlite_columns_from_pragma(
    pool: &sqlx::SqlitePool,
    table: &str,
) -> Result<Vec<ColumnInfo>, sqlx::Error> {
    let sql = format!(
        "SELECT name, type, \"notnull\" FROM pragma_table_info({})",
        sqlite_quote_literal(table)
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let name: String = r.try_get(0).ok()?;
            let data_type: String = r.try_get(1).ok()?;
            let notnull: i64 = r.try_get(2).ok()?;
            Some(ColumnInfo {
                name,
                data_type,
                nullable: notnull == 0,
                key: None,
                extra: None,
                comment: None,
                default: None,
            })
        })
        .collect())
}

async fn sqlite_list_index_objects(
    pool: &sqlx::SqlitePool,
    schema: Option<&str>,
    table: Option<&str>,
) -> Result<Vec<SchemaObject>, sqlx::Error> {
    let schema_name = resolve_sqlite_schema(schema).to_string();
    if let Some(table) = table {
        // Single table: per-table PRAGMAs touch exactly one table — no N+1.
        return sqlite_list_index_objects_legacy(pool, &schema_name, &[table.to_string()]).await;
    }
    // SQL-BUG-10: the all-tables path used to run PRAGMA index_list +
    // index_info per table/index (N+1 round-trips). The batched path uses a
    // constant number of statements with table-valued PRAGMA functions.
    // Table-valued PRAGMAs only address the main schema, so non-main
    // schemas (and any batched failure) fall back to the legacy loop.
    if schema_name == "main" {
        match sqlite_list_index_objects_batched(pool, &schema_name).await {
            Ok(objects) => return Ok(objects),
            Err(e) => {
                tracing::warn!(error = %e, "batched sqlite index listing failed; falling back to per-table pragmas");
            }
        }
    }
    let tables: Vec<String> = sqlite_list_table_rows(pool, &schema_name)
        .await?
        .into_iter()
        .filter_map(|r| r.try_get(0).ok())
        .collect();
    sqlite_list_index_objects_legacy(pool, &schema_name, &tables).await
}

/// Batched SQLite index listing: 2 statements total for any table count.
/// 1. (table, index, unique) for every table via `pragma_index_list` TVF.
/// 2. (index, column) for every index via `pragma_index_info` TVF.
async fn sqlite_list_index_objects_batched(
    pool: &sqlx::SqlitePool,
    schema_name: &str,
) -> Result<Vec<SchemaObject>, sqlx::Error> {
    let list_rows = sqlx::query(
        "SELECT t.name, il.name, il.\"unique\" \
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
        objects.push(SchemaObject {
            object_type: ObjectType::Index,
            schema: Some(schema_name.to_string()),
            name: idx_name.clone(),
            table: Some(table_name),
            data_type: None,
            nullable: None,
            columns: None,
            indexes: Some(vec![IndexInfo {
                columns: columns_by_index.remove(&idx_name).unwrap_or_default(),
                name: idx_name,
                unique: unique != 0,
            }]),
        });
    }
    Ok(objects)
}

async fn sqlite_list_index_objects_legacy(
    pool: &sqlx::SqlitePool,
    schema_name: &str,
    tables: &[String],
) -> Result<Vec<SchemaObject>, sqlx::Error> {
    let mut objects = Vec::new();
    for table_name in tables {
        let index_sql = format!("PRAGMA index_list({})", sqlite_quote_literal(table_name));
        let index_rows = sqlx::query(&index_sql).fetch_all(pool).await?;
        for row in index_rows {
            let idx_name: String = row.try_get(1)?;
            let unique: i64 = row.try_get(2)?;
            let info_sql = format!("PRAGMA index_info({})", sqlite_quote_literal(&idx_name));
            let info_rows = sqlx::query(&info_sql).fetch_all(pool).await?;
            let columns: Vec<String> = info_rows
                .iter()
                .filter_map(|r| r.try_get::<String, _>(2).ok())
                .collect();
            objects.push(SchemaObject {
                object_type: ObjectType::Index,
                schema: Some(schema_name.to_string()),
                name: idx_name.clone(),
                table: Some(table_name.clone()),
                data_type: None,
                nullable: None,
                columns: None,
                indexes: Some(vec![IndexInfo {
                    name: idx_name,
                    columns,
                    unique: unique != 0,
                }]),
            });
        }
    }
    Ok(objects)
}

/// All foreign keys of all (overview) tables in ONE statement via the
/// `pragma_foreign_key_list` table-valued function (SQL-BUG-10 — replaces a
/// per-table PRAGMA loop in `schema_overview`). Main schema only, like the
/// batched index path.
async fn sqlite_all_foreign_keys_batched(
    pool: &sqlx::SqlitePool,
) -> Result<Vec<ForeignKeyInfo>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT m.name, fk.id, fk.\"table\", fk.\"from\", fk.\"to\", \
                fk.on_delete, fk.on_update \
         FROM sqlite_master AS m, pragma_foreign_key_list(m.name) AS fk \
         WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' \
         ORDER BY m.name, fk.id, fk.seq",
    )
    .fetch_all(pool)
    .await?;

    use std::collections::BTreeMap;
    let mut map: BTreeMap<(String, i64), ForeignKeyInfo> = BTreeMap::new();
    for row in &rows {
        let table_name: String = row.try_get(0)?;
        let id: i64 = row.try_get(1)?;
        let ref_table: String = row.try_get(2)?;
        let from_col: String = row.try_get(3)?;
        let to_col: String = row.try_get(4)?;
        let on_delete: String = row.try_get(5)?;
        let on_update: String = row.try_get(6)?;
        map.entry((table_name.clone(), id))
            .and_modify(|fk| {
                fk.columns.push(from_col.clone());
                fk.ref_columns.push(to_col.clone());
            })
            .or_insert(ForeignKeyInfo {
                name: format!("fk_{id}"),
                table: table_name,
                columns: vec![from_col],
                ref_table,
                ref_columns: vec![to_col],
                on_delete: Some(on_delete),
                on_update: Some(on_update),
            });
    }
    Ok(map.into_values().collect())
}

fn index_info_from_schema_object(o: SchemaObject) -> IndexInfo {
    let nested = o.indexes.and_then(|i| i.into_iter().next());
    IndexInfo {
        name: o.name,
        columns: nested
            .as_ref()
            .map(|i| i.columns.clone())
            .unwrap_or_default(),
        unique: nested.map(|i| i.unique).unwrap_or(false),
    }
}

/// Flatten a schema index object into the compact `list_indexes` row shape.
pub fn listed_index_from_schema_object(o: SchemaObject) -> ListedIndex {
    let nested = o.indexes.and_then(|i| i.into_iter().next());
    ListedIndex {
        schema: o.schema,
        table: o.table,
        name: o.name,
        columns: nested
            .as_ref()
            .map(|i| i.columns.clone())
            .unwrap_or_default(),
        unique: nested.map(|i| i.unique).unwrap_or(false),
    }
}

/// Postgres `pg_indexes.indexdef` begins with `CREATE UNIQUE INDEX` for unique indexes.
fn pg_indexdef_is_unique(indexdef: &str) -> bool {
    let upper = indexdef.trim_start().to_ascii_uppercase();
    upper.starts_with("CREATE UNIQUE INDEX")
}

/// Extract indexed column / expression names from a `pg_indexes.indexdef` string.
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

fn filter_objects(mut objects: Vec<SchemaObject>, keyword: Option<&str>) -> Vec<SchemaObject> {
    if let Some(kw) = keyword {
        let kw = kw.to_lowercase();
        objects.retain(|o| {
            o.name.to_lowercase().contains(&kw)
                || o.table
                    .as_ref()
                    .is_some_and(|t| t.to_lowercase().contains(&kw))
                || o.schema
                    .as_ref()
                    .is_some_and(|s| s.to_lowercase().contains(&kw))
        });
    }
    objects
}

async fn resolve_mysql_schema(
    pool: &MySqlPool,
    schema: Option<&str>,
    connection_url: Option<&str>,
) -> Result<MysqlSchemaScope, sqlx::Error> {
    if let Some(s) = schema {
        if s == "*" {
            return Ok(MysqlSchemaScope::All);
        }
        if !s.is_empty() {
            return Ok(MysqlSchemaScope::One(s.to_string()));
        }
    }
    let row = sqlx::query("SELECT DATABASE()").fetch_one(pool).await?;
    let db: Option<String> = row.try_get(0)?;
    if let Some(db) = db.filter(|s| !s.is_empty()) {
        return Ok(MysqlSchemaScope::One(db));
    }
    if let Some(url) = connection_url {
        if let Some(db) = database_from_mysql_url(url) {
            return Ok(MysqlSchemaScope::One(db));
        }
    }
    Err(sqlx::Error::Configuration(
        "no default schema; pass schema or connect with database in URL".into(),
    ))
}

#[derive(Debug, Clone)]
enum MysqlSchemaScope {
    All,
    One(String),
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

fn parse_is_nullable(raw: &str) -> bool {
    raw.eq_ignore_ascii_case("YES")
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

fn parse_mysql_is_nullable_row(row: &MySqlRow, index: usize) -> bool {
    mysql_decode_text_by_index(row, index).is_some_and(|s| parse_is_nullable(&s))
}

fn column_info_from_mysql_column_row(r: &MySqlRow) -> Option<ColumnInfo> {
    let name = mysql_decode_text_by_index(r, MYSQL_COL_NAME_IDX)?;
    let data_type = mysql_decode_text_by_index(r, MYSQL_COL_TYPE_IDX)?;
    let nullable = parse_mysql_is_nullable_row(r, MYSQL_COL_NULL_IDX);
    Some(ColumnInfo {
        name,
        data_type,
        nullable,
        key: None,
        extra: None,
        comment: None,
        default: None,
    })
}

fn map_mysql_column_rows_to_schema_objects(
    rows: &[MySqlRow],
    context: Option<&str>,
) -> Vec<SchemaObject> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let sch = mysql_decode_text_by_index(r, 0);
        let table = mysql_decode_text_by_index(r, 1);
        let name = mysql_decode_text_by_index(r, MYSQL_COL_NAME_IDX);
        let data_type = mysql_decode_text_by_index(r, MYSQL_COL_TYPE_IDX);
        if sch.is_none() || table.is_none() || name.is_none() || data_type.is_none() {
            tracing::warn!(
                context = context.unwrap_or("mysql_columns"),
                "skipped information_schema row: decode failed"
            );
            continue;
        }
        out.push(SchemaObject {
            object_type: ObjectType::Column,
            schema: sch,
            name: name.unwrap(),
            table,
            data_type,
            nullable: Some(parse_mysql_is_nullable_row(r, MYSQL_COL_NULL_IDX)),
            columns: None,
            indexes: None,
        });
    }
    if !rows.is_empty() && out.is_empty() {
        tracing::warn!(
            context = context.unwrap_or("mysql_columns"),
            raw_rows = rows.len(),
            "all information_schema rows failed decode"
        );
    }
    out
}

fn map_mysql_column_rows_to_column_info(rows: &[MySqlRow], context: &str) -> Vec<ColumnInfo> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        match column_info_from_mysql_column_row(r) {
            Some(c) => out.push(c),
            None => tracing::warn!(context, "skipped column row: decode failed"),
        }
    }
    if !rows.is_empty() && out.is_empty() {
        tracing::warn!(
            context,
            raw_rows = rows.len(),
            "all column rows failed decode"
        );
    }
    out
}

fn column_info_from_pg_info_schema_row(r: &PgRow) -> Option<ColumnInfo> {
    let name: String = r.try_get(0).ok()?;
    let data_type: String = r.try_get(1).ok()?;
    let nullable = r
        .try_get::<String, _>(2)
        .ok()
        .is_some_and(|s| parse_is_nullable(&s));
    Some(ColumnInfo {
        name,
        data_type,
        nullable,
        key: None,
        extra: None,
        comment: None,
        default: None,
    })
}

fn mysql_quote_ident(name: &str) -> String {
    format!("`{}`", normalize_mysql_ident(name).replace('`', "``"))
}

async fn mysql_columns_from_information_schema(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, sqlx::Error> {
    let rows = if schema.is_empty() {
        sqlx::query(
            "SELECT table_schema, table_name, column_name, data_type, is_nullable \
             FROM information_schema.columns WHERE table_name = ? \
             ORDER BY ordinal_position",
        )
        .bind(table)
        .fetch_all(pool)
        .await?
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
        .await?
    };
    Ok(map_mysql_column_rows_to_column_info(
        &rows,
        "describe_table/information_schema",
    ))
}

async fn mysql_columns_from_show(
    pool: &MySqlPool,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnInfo>, sqlx::Error> {
    let sql = if schema.is_empty() {
        format!("SHOW FULL COLUMNS FROM {}", mysql_quote_ident(table))
    } else {
        format!(
            "SHOW FULL COLUMNS FROM {}.{}",
            mysql_quote_ident(schema),
            mysql_quote_ident(table)
        )
    };
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
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
                out.push(ColumnInfo {
                    name,
                    data_type,
                    nullable: parse_is_nullable(&null_raw),
                    key: mysql_decode_text_by_name(r, "Key"),
                    extra: mysql_decode_text_by_name(r, "Extra"),
                    comment: mysql_decode_text_by_name(r, "Comment"),
                    default: mysql_decode_text_by_name(r, "Default"),
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

/// Postgres planner/stats → optional approx row count for `schema_overview`.
///
/// - `reltuples < 0` (never ANALYZE'd): omit unless `n_live_tup > 0` (stale 0 is common).
/// - Otherwise prefer `n_live_tup`, else round non-negative `reltuples`.
fn pg_approx_rows(live: Option<i64>, reltuples: Option<f64>) -> Option<i64> {
    match reltuples {
        Some(t) if t < 0.0 => live.filter(|&n| n > 0),
        Some(t) => live.or(Some(t.round() as i64)),
        None => live,
    }
}

/// `pg_class.reltuples` is Postgres `real` (FLOAT4 / f32). Prefer f32 decode; accept f64
/// if the query casts to `float8`. Silent f64-only decode used to drop never-analyzed (-1).
fn pg_decode_reltuples(as_f32: Option<f32>, as_f64: Option<f64>) -> Option<f64> {
    as_f32.map(f64::from).or(as_f64)
}

/// One-shot schema survey: tables (+ approx row counts) and foreign keys.
pub async fn schema_overview(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
) -> Result<SchemaOverview, sqlx::Error> {
    let ttl = schema_cache_ttl();
    let key = overview_cache_key(pool, connection_url, schema);
    if let Some(hit) = cache_lookup(overview_cache(), &key, ttl) {
        return Ok(hit);
    }
    let fresh = schema_overview_uncached(pool, connection_url, schema).await?;
    if !ttl.is_zero() {
        cache_store(overview_cache(), key, fresh.clone());
    }
    Ok(fresh)
}

async fn schema_overview_uncached(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
) -> Result<SchemaOverview, sqlx::Error> {
    let schema_name = match pool.engine() {
        EngineKind::Postgres => schema.unwrap_or("public").to_string(),
        EngineKind::Mysql => {
            let mysql = mysql_pool(pool)?;
            match resolve_mysql_schema(mysql, schema, connection_url).await? {
                MysqlSchemaScope::One(name) => name,
                MysqlSchemaScope::All => {
                    return Err(sqlx::Error::Configuration(
                        "schema_overview requires schema on MySQL (or omit for current database)"
                            .into(),
                    ));
                }
            }
        }
        EngineKind::Sqlite => resolve_sqlite_schema(schema).to_string(),
    };

    let mut tables = match pool.engine() {
        EngineKind::Postgres => {
            let pg = pg_pool(pool)?;
            // Cast reltuples to float8 for sqlx; also decode f32 because native type is `real`.
            let rows = sqlx::query(
                "SELECT c.relname, s.n_live_tup, c.reltuples::float8 \
                 FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_stat_user_tables s \
                   ON s.relid = c.oid \
                 WHERE n.nspname = $1 AND c.relkind = 'r' \
                 ORDER BY c.relname",
            )
            .bind(&schema_name)
            .fetch_all(pg)
            .await?;
            rows.iter()
                .filter_map(|r| {
                    let name: String = r.try_get(0).ok()?;
                    let live: Option<i64> = r.try_get(1).ok().flatten();
                    let reltuples = pg_decode_reltuples(
                        r.try_get::<f32, _>(2).ok(),
                        r.try_get::<f64, _>(2).ok(),
                    );
                    let approx_rows = pg_approx_rows(live, reltuples);
                    Some(TableOverview { name, approx_rows })
                })
                .collect::<Vec<_>>()
        }
        EngineKind::Mysql => {
            let mysql = mysql_pool(pool)?;
            let rows = sqlx::query(
                "SELECT table_name, table_rows \
                 FROM information_schema.tables \
                 WHERE table_schema = ? AND table_type = 'BASE TABLE' \
                 ORDER BY table_name",
            )
            .bind(&schema_name)
            .fetch_all(mysql)
            .await?;
            rows.iter()
                .filter_map(|r| {
                    let name: String = r.try_get(0).ok()?;
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
                    Some(TableOverview {
                        name,
                        approx_rows: approx,
                    })
                })
                .collect::<Vec<_>>()
        }
        EngineKind::Sqlite => {
            let objects = list_tables(pool, connection_url, Some(&schema_name), None).await?;
            objects
                .into_iter()
                .map(|o| TableOverview {
                    name: o.name,
                    approx_rows: None,
                })
                .collect()
        }
    };

    let truncated = tables.len() > SCHEMA_OVERVIEW_MAX_TABLES;
    if truncated {
        tables.truncate(SCHEMA_OVERVIEW_MAX_TABLES);
    }

    let foreign_keys = match pool.engine() {
        EngineKind::Sqlite => {
            // SQL-BUG-10: one batched statement for all tables (main schema);
            // fall back to the per-table PRAGMA loop when unavailable.
            let batched = if schema_name == "main" {
                let sqlite = sqlite_pool(pool)?;
                match sqlite_all_foreign_keys_batched(sqlite).await {
                    Ok(all) => Some(
                        all.into_iter()
                            .filter(|fk| tables.iter().any(|t| t.name == fk.table))
                            .collect::<Vec<_>>(),
                    ),
                    Err(e) => {
                        tracing::warn!(error = %e, "batched sqlite FK listing failed; falling back to per-table pragmas");
                        None
                    }
                }
            } else {
                None
            };
            match batched {
                Some(fks) => fks,
                None => {
                    let mut all = Vec::new();
                    for t in &tables {
                        let fks = list_foreign_keys(
                            pool,
                            connection_url,
                            Some(&schema_name),
                            Some(&t.name),
                        )
                        .await?;
                        all.extend(fks);
                    }
                    all
                }
            }
        }
        _ => list_foreign_keys(pool, connection_url, Some(&schema_name), None).await?,
    };

    let n_tables = tables.len();
    let n_foreign_keys = foreign_keys.len();
    Ok(SchemaOverview {
        schema: schema_name,
        tables,
        foreign_keys,
        meta: SchemaOverviewMeta {
            n_tables,
            n_foreign_keys,
            truncated,
            row_counts: "approx",
        },
    })
}

pub async fn list_foreign_keys(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
) -> Result<Vec<ForeignKeyInfo>, sqlx::Error> {
    match pool.engine() {
        EngineKind::Postgres => {
            let pool = pg_pool(pool)?;
            let schema_name = schema.unwrap_or("public");
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
                .fetch_all(pool)
                .await?
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
                .fetch_all(pool)
                .await?
            };
            Ok(group_fk_rows(extract_fk_rows_pg(&rows), schema_name))
        }
        EngineKind::Mysql => {
            let pool = mysql_pool(pool)?;
            let scope = resolve_mysql_schema(pool, schema, connection_url).await?;
            let schema_name =
                match scope {
                    MysqlSchemaScope::One(name) => name,
                    MysqlSchemaScope::All => return Err(sqlx::Error::Configuration(
                        "list_foreign_keys requires schema on MySQL (or omit for current database)"
                            .into(),
                    )),
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
                .fetch_all(pool)
                .await?
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
                .fetch_all(pool)
                .await?
            };
            Ok(group_fk_rows(extract_fk_rows_mysql(&rows), &schema_name))
        }
        EngineKind::Sqlite => {
            let pool = sqlite_pool(pool)?;
            let table_name = table.ok_or_else(|| {
                sqlx::Error::Configuration("list_foreign_keys on SQLite requires table".into())
            })?;
            let pragma = format!(
                "PRAGMA foreign_key_list({})",
                sqlite_quote_literal(table_name)
            );
            let rows = sqlx::query(&pragma).fetch_all(pool).await?;
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
                        name: format!("fk_{id}"),
                        table: table_name.to_string(),
                        columns: vec![from_col],
                        ref_table,
                        ref_columns: vec![to_col],
                        on_delete: Some(on_delete),
                        on_update: Some(on_update),
                    })
                })
                .collect())
        }
    }
}

/// One raw foreign-key row as selected from information_schema (PG & MySQL
/// share the column layout: constraint, table, column, ref_table, ref_column,
/// delete_rule, update_rule).
#[derive(Debug, Clone)]
struct FkRowData {
    name: String,
    table: String,
    column: String,
    ref_table: String,
    ref_column: String,
    on_delete: String,
    on_update: String,
}

fn extract_fk_rows_pg(rows: &[PgRow]) -> Vec<FkRowData> {
    rows.iter()
        .map(|row| FkRowData {
            name: row.try_get(0).ok().unwrap_or_default(),
            table: row.try_get(1).ok().unwrap_or_default(),
            column: row.try_get(2).ok().unwrap_or_default(),
            ref_table: row.try_get(3).ok().unwrap_or_default(),
            ref_column: row.try_get(4).ok().unwrap_or_default(),
            on_delete: row.try_get(5).ok().unwrap_or_default(),
            on_update: row.try_get(6).ok().unwrap_or_default(),
        })
        .collect()
}

fn extract_fk_rows_mysql(rows: &[MySqlRow]) -> Vec<FkRowData> {
    rows.iter()
        .map(|row| FkRowData {
            name: row.try_get(0).ok().unwrap_or_default(),
            table: row.try_get(1).ok().unwrap_or_default(),
            column: row.try_get(2).ok().unwrap_or_default(),
            ref_table: row.try_get(3).ok().unwrap_or_default(),
            ref_column: row.try_get(4).ok().unwrap_or_default(),
            on_delete: row.try_get(5).ok().unwrap_or_default(),
            on_update: row.try_get(6).ok().unwrap_or_default(),
        })
        .collect()
}

/// Group FK rows into one `ForeignKeyInfo` per constraint.
///
/// SQL-BUG-02: the grouping key used to be the constraint name alone, so two
/// same-named FKs on different tables (very common with generated names)
/// merged into one factually wrong object with mixed columns. The key is
/// now the composite (schema, table, constraint name).
fn group_fk_rows(rows: Vec<FkRowData>, schema: &str) -> Vec<ForeignKeyInfo> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<(String, String, String), ForeignKeyInfo> = BTreeMap::new();
    for row in rows {
        let key = (schema.to_string(), row.table.clone(), row.name.clone());
        map.entry(key)
            .and_modify(|fk| {
                fk.columns.push(row.column.clone());
                fk.ref_columns.push(row.ref_column.clone());
            })
            .or_insert(ForeignKeyInfo {
                name: row.name,
                table: row.table,
                columns: vec![row.column],
                ref_table: row.ref_table,
                ref_columns: vec![row.ref_column],
                on_delete: Some(row.on_delete),
                on_update: Some(row.on_update),
            });
    }
    map.into_values().collect()
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
    fn split_mysql_table_handles_qualified_names() {
        let (schema, table) = split_mysql_table("hris_ksei_prod_5.fw_users");
        assert_eq!(schema.as_deref(), Some("hris_ksei_prod_5"));
        assert_eq!(table, "fw_users");
        let (schema, table) = split_mysql_table("`db`.`tbl`");
        assert_eq!(schema.as_deref(), Some("db"));
        assert_eq!(table, "tbl");
        let (schema, table) = split_mysql_table("fw_users");
        assert!(schema.is_none());
        assert_eq!(table, "fw_users");
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
        assert_eq!(
            pg_indexdef_columns(
                "CREATE INDEX idx_expr ON public.users USING btree (lower((email)::text))"
            ),
            vec!["lower((email)::text)"]
        );
    }

    #[test]
    fn index_info_from_schema_object_preserves_unique() {
        let o = SchemaObject {
            object_type: ObjectType::Index,
            schema: Some("public".into()),
            name: "users_username_key".into(),
            table: Some("users".into()),
            data_type: None,
            nullable: None,
            columns: None,
            indexes: Some(vec![IndexInfo {
                name: "users_username_key".into(),
                columns: vec!["username".into()],
                unique: true,
            }]),
        };
        let info = index_info_from_schema_object(o);
        assert_eq!(info.name, "users_username_key");
        assert_eq!(info.columns, vec!["username"]);
        assert!(info.unique);
    }

    #[test]
    fn listed_index_flattens_without_indexdef() {
        let o = SchemaObject {
            object_type: ObjectType::Index,
            schema: Some("public".into()),
            name: "users_email_key".into(),
            table: Some("users".into()),
            data_type: None,
            nullable: None,
            columns: None,
            indexes: Some(vec![IndexInfo {
                name: "users_email_key".into(),
                columns: vec!["email".into()],
                unique: true,
            }]),
        };
        let listed = listed_index_from_schema_object(o);
        assert_eq!(listed.name, "users_email_key");
        assert_eq!(listed.table.as_deref(), Some("users"));
        assert_eq!(listed.columns, vec!["email"]);
        assert!(listed.unique);
        let json = serde_json::to_string(&listed).unwrap();
        assert!(!json.contains("CREATE INDEX"));
        assert!(!json.contains("\"indexes\""));
        assert!(!json.contains("data_type"));
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

    fn obj(name: &str) -> SchemaObject {
        SchemaObject {
            object_type: ObjectType::Table,
            schema: Some("public".into()),
            name: name.into(),
            table: None,
            data_type: None,
            nullable: None,
            columns: None,
            indexes: None,
        }
    }

    #[test]
    fn paginate_objects_offset_beyond_total_is_empty_not_panic() {
        // SQL-BUG-01 regression: offset > total used to panic in drain().
        let objects = vec![obj("a"), obj("b")];
        let page = paginate_objects(objects, 10, 5);
        assert!(page.is_empty());
    }

    #[test]
    fn paginate_objects_normal_and_partial_pages() {
        let objects = (0..5).map(|i| obj(&format!("t{i}"))).collect::<Vec<_>>();
        let page = paginate_objects(objects, 1, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].name, "t1");
        let objects = (0..5).map(|i| obj(&format!("t{i}"))).collect::<Vec<_>>();
        let page = paginate_objects(objects, 4, 10);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].name, "t4");
        let objects = vec![obj("x")];
        assert!(paginate_objects(objects, 0, 0).is_empty());
    }

    #[test]
    fn sqlite_quote_ident_doubles_embedded_quotes() {
        assert_eq!(sqlite_quote_ident("main"), "\"main\"");
        assert_eq!(sqlite_quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    fn fk_row(table: &str, name: &str, col: &str) -> FkRowData {
        FkRowData {
            name: name.into(),
            table: table.into(),
            column: col.into(),
            ref_table: "parents".into(),
            ref_column: "id".into(),
            on_delete: "CASCADE".into(),
            on_update: "NO ACTION".into(),
        }
    }

    #[test]
    fn group_fk_rows_same_name_different_tables_stay_separate() {
        // SQL-BUG-02 regression.
        let rows = vec![
            fk_row("orders", "fk_parent", "parent_id"),
            fk_row("invoices", "fk_parent", "parent_id"),
            fk_row("orders", "fk_parent", "parent2_id"),
        ];
        let grouped = group_fk_rows(rows, "public");
        assert_eq!(grouped.len(), 2, "same constraint name on 2 tables = 2 FKs");
        let orders = grouped.iter().find(|f| f.table == "orders").unwrap();
        assert_eq!(orders.columns.len(), 2, "composite FK columns merge");
    }

    #[test]
    fn group_mysql_index_rows_same_name_different_tables_stay_separate() {
        // SQL-BUG-03 regression.
        let mk = |table: &str, col: &str| MysqlIndexRowData {
            schema: "app".into(),
            table: table.into(),
            name: "PRIMARY".into(),
            column: col.into(),
            unique: true,
        };
        let grouped = group_mysql_index_rows(vec![mk("a", "id"), mk("b", "id"), mk("a", "tenant")]);
        assert_eq!(grouped.len(), 2);
        let a = grouped
            .iter()
            .find(|o| o.table.as_deref() == Some("a"))
            .unwrap();
        assert_eq!(a.indexes.as_ref().unwrap()[0].columns.len(), 2);
    }

    #[test]
    fn schema_cache_roundtrip_and_expiry() {
        use std::time::Duration;
        let map: StdMutex<StdHashMap<String, (StdInstant, Vec<SchemaObject>)>> =
            StdMutex::new(StdHashMap::new());
        cache_store(&map, "k".into(), vec![obj("cached")]);
        let hit = cache_lookup(&map, "k", Duration::from_secs(30)).expect("fresh hit");
        assert_eq!(hit[0].name, "cached");
        assert!(
            cache_lookup(&map, "k", Duration::ZERO).is_none(),
            "TTL 0 disables the cache"
        );
        // Expired entries are misses: age the entry past a tiny TTL.
        std::thread::sleep(Duration::from_millis(3));
        assert!(
            cache_lookup(&map, "k", Duration::from_millis(1)).is_none(),
            "entry older than TTL must miss"
        );
    }

    #[test]
    fn schema_overview_meta_marks_approx_row_counts() {
        let meta = SchemaOverviewMeta {
            n_tables: 1,
            n_foreign_keys: 0,
            truncated: false,
            row_counts: "approx",
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"row_counts\":\"approx\""));
    }
}
