use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strut_sql_driver_api::{
    ColumnInfo as DriverColumnInfo, EngineDriver, EngineKind,
    ForeignKeyInfo as DriverForeignKeyInfo, IndexInfo as DriverIndexInfo, IndexPage,
    ObjectType as DriverObjectType, SchemaObject as DriverSchemaObject,
};

/// Object kind exposed to agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<u32>,
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
    /// Foreign keys this table points at (outgoing edges).
    #[serde(skip_serializing_if = "is_zero")]
    pub fk_out: usize,
    /// Foreign keys other tables point at this table (incoming edges).
    #[serde(skip_serializing_if = "is_zero")]
    pub fk_in: usize,
    /// Highest-degree table(s) in the schema join graph.
    #[serde(skip_serializing_if = "is_false")]
    pub hub: bool,
    /// No incoming or outgoing foreign keys.
    #[serde(skip_serializing_if = "is_false")]
    pub orphan: bool,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaOverviewMeta {
    pub n_tables: usize,
    pub n_foreign_keys: usize,
    pub truncated: bool,
    /// Always `"approx"` — planner/stats estimates; use `COUNT(*)` for exact.
    pub row_counts: &'static str,
    /// Tables at the maximum join-graph degree.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hubs: Vec<String>,
    /// Tables with no foreign-key relation in either direction.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub orphans: Vec<String>,
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

// ---------------------------------------------------------------------------
// Schema metadata cache (SQL-BUG-10, PRD-F2-04)
//
// Introspection results are cached per `(source, engine, schema)` — the key
// never omits the source, so two sources with the same engine/schema cannot
// share an entry. The TTL is configurable via
// `MCP_SQL_SCHEMA_CACHE_TTL_SECS` (default 30; 0 disables). A successful DDL
// statement routed through this server invalidates every entry for that
// source immediately. Hit/miss counters are emitted in diagnostic logs.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex as StdMutex};

use std::collections::HashMap as StdHashMap;
use std::time::{Duration as StdDuration, Instant as StdInstant};

fn schema_cache_ttl() -> StdDuration {
    match std::env::var("MCP_SQL_SCHEMA_CACHE_TTL_SECS") {
        Ok(v) => v
            .trim()
            .parse::<u64>()
            .map(StdDuration::from_secs)
            .unwrap_or_else(|_| StdDuration::from_secs(30)),
        Err(_) => StdDuration::from_secs(30),
    }
}

#[derive(Debug, Clone)]
struct CachedObjects {
    objects: Vec<SchemaObject>,
    has_more: bool,
}

/// Cache key separator that cannot appear in a source name.
const KEY_SEP: char = '\u{1f}';

type CacheMap<V> = StdHashMap<String, (StdInstant, V)>;

static OBJECTS_CACHE: LazyLock<StdMutex<CacheMap<CachedObjects>>> =
    LazyLock::new(|| StdMutex::new(StdHashMap::new()));

static INDEXES_CACHE: LazyLock<StdMutex<CacheMap<IndexPage>>> =
    LazyLock::new(|| StdMutex::new(StdHashMap::new()));

static OVERVIEW_CACHE: LazyLock<StdMutex<CacheMap<SchemaOverview>>> =
    LazyLock::new(|| StdMutex::new(StdHashMap::new()));

static CACHE_HITS: AtomicU64 = AtomicU64::new(0);
static CACHE_MISSES: AtomicU64 = AtomicU64::new(0);

fn objects_cache() -> &'static StdMutex<CacheMap<CachedObjects>> {
    &OBJECTS_CACHE
}

fn indexes_cache() -> &'static StdMutex<CacheMap<IndexPage>> {
    &INDEXES_CACHE
}

fn overview_cache() -> &'static StdMutex<CacheMap<SchemaOverview>> {
    &OVERVIEW_CACHE
}

fn cache_lookup<V: Clone>(
    map_name: &str,
    map: &StdMutex<StdHashMap<String, (StdInstant, V)>>,
    key: &str,
    ttl: StdDuration,
) -> Option<V> {
    if ttl.is_zero() {
        return None;
    }
    let mut guard = map.lock().ok()?;
    let hit = match guard.get(key) {
        Some((at, value)) if at.elapsed() < ttl => Some(value.clone()),
        Some(_) => {
            guard.remove(key);
            None
        }
        None => None,
    };
    if hit.is_some() {
        CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    } else {
        CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }
    tracing::debug!(
        cache = "schema",
        map = map_name,
        key,
        hit = hit.is_some(),
        hits = CACHE_HITS.load(Ordering::Relaxed),
        misses = CACHE_MISSES.load(Ordering::Relaxed),
        "schema cache lookup"
    );
    hit
}

fn cache_store<V>(map: &StdMutex<StdHashMap<String, (StdInstant, V)>>, key: String, value: V) {
    if let Ok(mut guard) = map.lock() {
        guard.insert(key, (StdInstant::now(), value));
    }
}

/// Diagnostic hit/miss counters (tests and operators).
pub fn cache_stats() -> (u64, u64) {
    (
        CACHE_HITS.load(Ordering::Relaxed),
        CACHE_MISSES.load(Ordering::Relaxed),
    )
}

/// Drop every cached introspection entry for one source. Called after a
/// successful DDL statement routed through this server (PRD-F2-04).
pub fn invalidate_source(source: &str) {
    let prefix = format!("{source}{KEY_SEP}");
    let mut removed = 0usize;
    if let Ok(mut guard) = objects_cache().lock() {
        let before = guard.len();
        guard.retain(|k, _| !k.starts_with(&prefix));
        removed += before - guard.len();
    }
    if let Ok(mut guard) = indexes_cache().lock() {
        let before = guard.len();
        guard.retain(|k, _| !k.starts_with(&prefix));
        removed += before - guard.len();
    }
    if let Ok(mut guard) = overview_cache().lock() {
        let before = guard.len();
        guard.retain(|k, _| !k.starts_with(&prefix));
        removed += before - guard.len();
    }
    tracing::info!(
        source,
        invalidated = removed,
        "schema cache invalidated after DDL"
    );
}

fn schema_cache_key(
    source: &str,
    engine: EngineKind,
    schema: Option<&str>,
    variant: &str,
    rest: &[String],
) -> String {
    let mut key = format!(
        "{source}{KEY_SEP}{engine:?}{KEY_SEP}{}{KEY_SEP}{variant}",
        schema.unwrap_or("")
    );
    for part in rest {
        key.push(KEY_SEP);
        key.push_str(part);
    }
    key
}

fn map_object_type(t: DriverObjectType) -> ObjectType {
    match t {
        DriverObjectType::Schema => ObjectType::Schema,
        DriverObjectType::Table => ObjectType::Table,
        DriverObjectType::Column => ObjectType::Column,
        DriverObjectType::Index => ObjectType::Index,
        DriverObjectType::ForeignKey => ObjectType::Table,
        DriverObjectType::Routine => ObjectType::Table,
    }
}

fn map_schema_object(o: DriverSchemaObject) -> SchemaObject {
    SchemaObject {
        object_type: map_object_type(o.object_type),
        schema: o.schema,
        name: o.name,
        table: o.table,
        data_type: o.data_type,
        nullable: o.nullable,
        comment: o.comment,
        score: None,
        columns: None,
        indexes: None,
    }
}

fn map_column(engine: EngineKind, c: DriverColumnInfo) -> ColumnInfo {
    // Legacy MySQL describe_table exposed Key/Extra/Comment/Default; PG and
    // SQLite exposed only name/type/nullability.
    let mysql_extras = engine == EngineKind::MySql;
    ColumnInfo {
        name: c.name,
        data_type: c.data_type,
        nullable: c.nullable,
        key: if mysql_extras { c.key } else { None },
        extra: if mysql_extras { c.extra } else { None },
        comment: if mysql_extras { c.comment } else { None },
        default: if mysql_extras { c.default_value } else { None },
    }
}

fn map_fk(fk: DriverForeignKeyInfo) -> ForeignKeyInfo {
    ForeignKeyInfo {
        name: fk.name,
        table: fk.table,
        columns: fk.columns,
        ref_table: fk.ref_table,
        ref_columns: fk.ref_columns,
        on_delete: fk.on_delete,
        on_update: fk.on_update,
    }
}

fn map_index(idx: DriverIndexInfo) -> IndexInfo {
    IndexInfo {
        name: idx.name,
        columns: idx.columns,
        unique: idx.unique,
    }
}

pub fn filter_objects(mut objects: Vec<SchemaObject>, keyword: Option<&str>) -> Vec<SchemaObject> {
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

/// Slice one page out of `objects` (SQL-BUG-01). The old code drained
/// `offset..total.min(offset + limit)`, which panics when `offset > total`
/// (invalid range). Clamp both bounds so an out-of-range offset simply
/// yields an empty page.
pub fn paginate_objects(
    mut objects: Vec<SchemaObject>,
    offset: usize,
    limit: usize,
) -> Vec<SchemaObject> {
    let total = objects.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    objects.drain(start..end).collect()
}

const CATALOG_PAGE_MAX: usize = 100_000;

fn page_rest(keyword: Option<&str>, offset: usize, limit: usize) -> Vec<String> {
    vec![
        keyword.unwrap_or("").to_string(),
        offset.to_string(),
        limit.to_string(),
    ]
}

/// One page of schemas with keyword + LIMIT/OFFSET pushed to the driver
/// (PRD-F2-05), cached per source/engine/schema (PRD-F2-04).
pub async fn list_schemas_page(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    keyword: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<SearchResult, String> {
    let source = source.unwrap_or("");
    let ttl = schema_cache_ttl();
    let rest = page_rest(keyword, offset, limit);
    let key = schema_cache_key(source, driver.kind(), None, "schemas", &rest);
    let page = match cache_lookup("schemas", objects_cache(), &key, ttl) {
        Some(hit) => hit,
        None => {
            let fresh = driver
                .list_schemas_page(keyword, offset, limit)
                .await
                .map_err(|e| e.to_string())?;
            let fresh = CachedObjects {
                objects: fresh.objects.into_iter().map(map_schema_object).collect(),
                has_more: fresh.has_more,
            };
            if !ttl.is_zero() {
                cache_store(objects_cache(), key, fresh.clone());
            }
            fresh
        }
    };
    Ok(SearchResult {
        meta: SearchMeta {
            n: page.objects.len(),
            offset,
            limit,
            has_more: page.has_more,
        },
        objects: page.objects,
    })
}

/// One page of tables with keyword + LIMIT/OFFSET pushed to the driver
/// (PRD-F2-05), cached per source/engine/schema (PRD-F2-04).
pub async fn list_tables_page(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
    keyword: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<SearchResult, String> {
    let source = source.unwrap_or("");
    let ttl = schema_cache_ttl();
    let rest = page_rest(keyword, offset, limit);
    let key = schema_cache_key(source, driver.kind(), schema, "tables", &rest);
    let page = match cache_lookup("tables", objects_cache(), &key, ttl) {
        Some(hit) => hit,
        None => {
            let fresh = driver
                .list_tables_page(schema, keyword, offset, limit)
                .await
                .map_err(|e| e.to_string())?;
            let fresh = CachedObjects {
                objects: fresh.objects.into_iter().map(map_schema_object).collect(),
                has_more: fresh.has_more,
            };
            if !ttl.is_zero() {
                cache_store(objects_cache(), key, fresh.clone());
            }
            fresh
        }
    };
    Ok(SearchResult {
        meta: SearchMeta {
            n: page.objects.len(),
            offset,
            limit,
            has_more: page.has_more,
        },
        objects: page.objects,
    })
}

/// One page of columns with keyword + LIMIT/OFFSET pushed to the driver
/// (PRD-F2-05), cached per source/engine/schema (PRD-F2-04).
pub async fn list_columns_page(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
    keyword: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<SearchResult, String> {
    let source = source.unwrap_or("");
    let ttl = schema_cache_ttl();
    let rest = page_rest(keyword, offset, limit);
    let key = schema_cache_key(source, driver.kind(), schema, "columns", &rest);
    let page = match cache_lookup("columns", objects_cache(), &key, ttl) {
        Some(hit) => hit,
        None => {
            let fresh = driver
                .list_columns_page(schema, keyword, offset, limit)
                .await
                .map_err(|e| e.to_string())?;
            let fresh = CachedObjects {
                objects: fresh.objects.into_iter().map(map_schema_object).collect(),
                has_more: fresh.has_more,
            };
            if !ttl.is_zero() {
                cache_store(objects_cache(), key, fresh.clone());
            }
            fresh
        }
    };
    Ok(SearchResult {
        meta: SearchMeta {
            n: page.objects.len(),
            offset,
            limit,
            has_more: page.has_more,
        },
        objects: page.objects,
    })
}

/// One page of indexes with keyword + LIMIT/OFFSET pushed to the driver
/// (PRD-F2-05), cached per source/engine/schema (PRD-F2-04).
async fn driver_indexes_page(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    keyword: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<IndexPage, String> {
    let source = source.unwrap_or("");
    let ttl = schema_cache_ttl();
    let mut rest = page_rest(keyword, offset, limit);
    rest.push(table.unwrap_or("").to_string());
    let key = schema_cache_key(source, driver.kind(), schema, "indexes", &rest);
    if let Some(hit) = cache_lookup("indexes", indexes_cache(), &key, ttl) {
        return Ok(hit);
    }
    let fresh = driver
        .list_indexes_page(schema, table, keyword, offset, limit)
        .await
        .map_err(|e| e.to_string())?;
    if !ttl.is_zero() {
        cache_store(indexes_cache(), key, fresh.clone());
    }
    Ok(fresh)
}

/// Backwards-compatible unpaginated schema listing.
pub async fn list_schemas(
    driver: &Arc<dyn EngineDriver>,
    keyword: Option<&str>,
) -> Result<Vec<SchemaObject>, String> {
    Ok(
        list_schemas_page(driver, None, keyword, 0, CATALOG_PAGE_MAX)
            .await?
            .objects,
    )
}

/// Backwards-compatible unpaginated table listing. `source` is the configured
/// source name (or `None` for callers that do not have one).
pub async fn list_tables(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
    keyword: Option<&str>,
) -> Result<Vec<SchemaObject>, String> {
    Ok(
        list_tables_page(driver, source, schema, keyword, 0, CATALOG_PAGE_MAX)
            .await?
            .objects,
    )
}

/// Backwards-compatible unpaginated column listing.
pub async fn list_columns(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
    keyword: Option<&str>,
) -> Result<Vec<SchemaObject>, String> {
    Ok(
        list_columns_page(driver, source, schema, keyword, 0, CATALOG_PAGE_MAX)
            .await?
            .objects,
    )
}

pub async fn search_objects(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    object_type: ObjectType,
    keyword: Option<&str>,
    schema: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<SearchResult, String> {
    search_objects_ranked(
        driver,
        source,
        object_type,
        keyword,
        schema,
        offset,
        limit,
        false,
    )
    .await
}

/// Ranked search (PRD-F3-01): exact > prefix > substring > comment match.
///
/// Ranking is global, so the catalog page is fetched wide and sliced only
/// after ordering — a page-local sort would rank the wrong candidates.
#[allow(clippy::too_many_arguments)]
pub async fn search_objects_ranked(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    object_type: ObjectType,
    keyword: Option<&str>,
    schema: Option<&str>,
    offset: usize,
    limit: usize,
    include_score: bool,
) -> Result<SearchResult, String> {
    let mut objects: Vec<SchemaObject> = match object_type {
        ObjectType::Schema => {
            let page = driver
                .list_schemas_page(keyword, 0, CATALOG_PAGE_MAX)
                .await
                .map_err(|e| e.to_string())?;
            page.objects.into_iter().map(map_schema_object).collect()
        }
        ObjectType::Table => {
            let page =
                list_tables_page(driver, source, schema, keyword, 0, CATALOG_PAGE_MAX).await?;
            page.objects
        }
        ObjectType::Column => {
            let page =
                list_columns_page(driver, source, schema, keyword, 0, CATALOG_PAGE_MAX).await?;
            page.objects
        }
        ObjectType::Index => {
            let page =
                driver_indexes_page(driver, source, schema, None, keyword, 0, CATALOG_PAGE_MAX)
                    .await?;
            page.indexes
                .into_iter()
                .map(|idx| SchemaObject {
                    object_type: ObjectType::Index,
                    schema: idx.schema.clone(),
                    name: idx.name.clone(),
                    table: Some(idx.table.clone()),
                    data_type: None,
                    nullable: None,
                    comment: None,
                    score: None,
                    columns: None,
                    indexes: Some(vec![map_index(idx)]),
                })
                .collect()
        }
    };

    if let Some(kw) = keyword {
        objects.retain(|o| object_matches(o, kw));
        for object in &mut objects {
            object.score = score_object(object, kw);
        }
        objects.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.table.cmp(&b.table))
                .then_with(|| a.schema.cmp(&b.schema))
        });
    } else {
        for object in &mut objects {
            object.score = Some(0);
        }
    }
    if !include_score {
        for object in &mut objects {
            object.score = None;
        }
    } else if keyword.is_none() {
        for object in &mut objects {
            object.score = Some(0);
        }
    }

    let total = objects.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let page = objects[start..end].to_vec();
    Ok(SearchResult {
        meta: SearchMeta {
            n: page.len(),
            offset,
            limit,
            has_more: end < total,
        },
        objects: page,
    })
}

fn object_matches(object: &SchemaObject, keyword: &str) -> bool {
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
        || object
            .comment
            .as_ref()
            .is_some_and(|c| c.to_lowercase().contains(&kw))
}

fn score_object(object: &SchemaObject, keyword: &str) -> Option<u32> {
    let kw = keyword.to_lowercase();
    let name = object.name.to_lowercase();
    if name == kw {
        Some(1000)
    } else if name.starts_with(&kw) {
        Some(800)
    } else if name.contains(&kw) {
        Some(600)
    } else if object
        .table
        .as_ref()
        .is_some_and(|t| t.to_lowercase().contains(&kw))
    {
        Some(400)
    } else if object
        .schema
        .as_ref()
        .is_some_and(|s| s.to_lowercase().contains(&kw))
    {
        Some(350)
    } else if object
        .comment
        .as_ref()
        .is_some_and(|c| c.to_lowercase().contains(&kw))
    {
        Some(200)
    } else {
        Some(0)
    }
}

pub async fn describe_table(
    driver: &Arc<dyn EngineDriver>,
    schema: Option<&str>,
    table: &str,
) -> Result<SchemaObject, String> {
    let engine = driver.kind();
    let info = driver
        .describe_table(schema, table)
        .await
        .map_err(|e| e.to_string())?;
    let columns: Vec<ColumnInfo> = info
        .columns
        .into_iter()
        .map(|c| map_column(engine, c))
        .collect();
    let indexes = driver
        .list_indexes(info.schema.as_deref(), Some(&info.name))
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(map_index)
        .collect();

    Ok(SchemaObject {
        object_type: ObjectType::Table,
        schema: info.schema,
        name: info.name,
        table: None,
        data_type: None,
        nullable: None,
        comment: None,
        score: None,
        columns: Some(columns),
        indexes: Some(indexes),
    })
}

pub async fn list_indexes(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
    table: Option<&str>,
    keyword: Option<&str>,
) -> Result<Vec<ListedIndex>, String> {
    let page =
        driver_indexes_page(driver, source, schema, table, keyword, 0, CATALOG_PAGE_MAX).await?;
    Ok(page
        .indexes
        .into_iter()
        .map(|idx| ListedIndex {
            schema: idx.schema,
            table: Some(idx.table),
            name: idx.name,
            columns: idx.columns,
            unique: idx.unique,
        })
        .collect())
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

pub async fn list_foreign_keys(
    driver: &Arc<dyn EngineDriver>,
    schema: Option<&str>,
    table: Option<&str>,
) -> Result<Vec<ForeignKeyInfo>, String> {
    Ok(driver
        .list_foreign_keys(schema, table)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(map_fk)
        .collect())
}

/// One-shot schema survey: tables (+ approx row counts) and foreign keys.
pub async fn schema_overview(
    driver: &Arc<dyn EngineDriver>,
    source: Option<&str>,
    schema: Option<&str>,
) -> Result<SchemaOverview, String> {
    let ttl = schema_cache_ttl();
    let key = schema_cache_key(source.unwrap_or(""), driver.kind(), schema, "overview", &[]);
    if let Some(hit) = cache_lookup("overview", overview_cache(), &key, ttl) {
        return Ok(hit);
    }
    let fresh = schema_overview_uncached(driver, schema).await?;
    if !ttl.is_zero() {
        cache_store(overview_cache(), key, fresh.clone());
    }
    Ok(fresh)
}

async fn schema_overview_uncached(
    driver: &Arc<dyn EngineDriver>,
    schema: Option<&str>,
) -> Result<SchemaOverview, String> {
    let engine = driver.kind();
    let schema_name = match (engine, schema) {
        (EngineKind::MySql, Some("*")) => {
            return Err(
                "schema_overview requires schema on MySQL (or omit for current database)".into(),
            )
        }
        _ => driver
            .resolve_schema(schema)
            .await
            .map_err(|e| e.to_string())?,
    };

    let mut tables: Vec<TableOverview> = driver
        .approximate_row_counts(Some(&schema_name))
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|t| TableOverview {
            name: t.name,
            approx_rows: t.approx_rows,
            fk_out: 0,
            fk_in: 0,
            hub: false,
            orphan: false,
        })
        .collect();

    let truncated = tables.len() > SCHEMA_OVERVIEW_MAX_TABLES;
    if truncated {
        tables.truncate(SCHEMA_OVERVIEW_MAX_TABLES);
    }

    // Every engine now exposes a set-based all-tables FK query (SQLite
    // included, PRD-F2-06): no per-table round-trip loop.
    let foreign_keys: Vec<ForeignKeyInfo> = driver
        .list_foreign_keys(Some(&schema_name), None)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(map_fk)
        .collect();

    // Per-table FK in/out counts plus hub/orphan markers (PRD-F3-03).
    for fk in &foreign_keys {
        if let Some(parent) = tables.iter_mut().find(|t| t.name == fk.ref_table) {
            parent.fk_in += 1;
        }
        if let Some(child) = tables.iter_mut().find(|t| t.name == fk.table) {
            child.fk_out += 1;
        }
    }
    let max_degree = tables.iter().map(|t| t.fk_in + t.fk_out).max().unwrap_or(0);
    let mut hubs = Vec::new();
    let mut orphans = Vec::new();
    for table in &mut tables {
        let degree = table.fk_in + table.fk_out;
        table.hub = max_degree > 0 && degree == max_degree;
        table.orphan = degree == 0;
        if table.hub {
            hubs.push(table.name.clone());
        }
        if table.orphan {
            orphans.push(table.name.clone());
        }
    }
    hubs.sort();
    orphans.sort();

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
            hubs,
            orphans,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(name: &str, schema: Option<&str>) -> SchemaObject {
        SchemaObject {
            object_type: ObjectType::Table,
            schema: schema.map(str::to_string),
            name: name.into(),
            table: None,
            data_type: None,
            nullable: None,
            comment: None,
            score: None,
            columns: None,
            indexes: None,
        }
    }

    #[test]
    fn pagination_clamps_out_of_range_offset() {
        let objects = vec![obj("a", None), obj("b", None)];
        assert!(paginate_objects(objects.clone(), 5, 10).is_empty());
        assert_eq!(paginate_objects(objects, 1, 10).len(), 1);
    }

    #[test]
    fn paginate_objects_normal_and_partial_pages() {
        let objects = (0..5)
            .map(|i| obj(&format!("t{i}"), None))
            .collect::<Vec<_>>();
        let page = paginate_objects(objects, 1, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].name, "t1");
        let objects = (0..5)
            .map(|i| obj(&format!("t{i}"), None))
            .collect::<Vec<_>>();
        let page = paginate_objects(objects, 4, 10);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].name, "t4");
        assert!(paginate_objects(vec![obj("x", None)], 0, 0).is_empty());
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
            comment: None,
            score: None,
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
    fn map_index_preserves_unique_and_columns() {
        let mapped = map_index(DriverIndexInfo {
            schema: Some("public".into()),
            table: "users".into(),
            name: "users_username_key".into(),
            columns: vec!["username".into()],
            unique: true,
            primary: false,
        });
        assert_eq!(mapped.name, "users_username_key");
        assert_eq!(mapped.columns, vec!["username"]);
        assert!(mapped.unique);
    }

    #[test]
    fn schema_cache_roundtrip_and_expiry() {
        use std::time::Duration;
        let map: StdMutex<StdHashMap<String, (StdInstant, Vec<SchemaObject>)>> =
            StdMutex::new(StdHashMap::new());
        cache_store(&map, "k".into(), vec![obj("cached", None)]);
        let hit = cache_lookup("test", &map, "k", Duration::from_secs(30)).expect("fresh hit");
        assert_eq!(hit[0].name, "cached");
        assert!(
            cache_lookup("test", &map, "k", Duration::ZERO).is_none(),
            "TTL 0 disables the cache"
        );
        // Expired entries are misses: age the entry past a tiny TTL.
        std::thread::sleep(Duration::from_millis(3));
        assert!(
            cache_lookup("test", &map, "k", Duration::from_millis(1)).is_none(),
            "entry older than TTL must miss"
        );
    }

    #[test]
    fn cache_keys_are_scoped_per_source() {
        let key_a = schema_cache_key("src_a", EngineKind::Sqlite, Some("main"), "tables", &[]);
        let key_b = schema_cache_key("src_b", EngineKind::Sqlite, Some("main"), "tables", &[]);
        assert_ne!(key_a, key_b);
        assert!(key_a.starts_with("src_a\u{1f}"));
        assert!(key_b.starts_with("src_b\u{1f}"));
    }

    #[test]
    fn invalidate_source_drops_only_that_source() {
        let key_a = schema_cache_key("src_a", EngineKind::Sqlite, Some("main"), "tables", &[]);
        let key_b = schema_cache_key("src_b", EngineKind::Sqlite, Some("main"), "tables", &[]);
        cache_store(
            objects_cache(),
            key_a.clone(),
            CachedObjects {
                objects: vec![obj("a", None)],
                has_more: false,
            },
        );
        cache_store(
            objects_cache(),
            key_b.clone(),
            CachedObjects {
                objects: vec![obj("b", None)],
                has_more: false,
            },
        );
        invalidate_source("src_a");
        assert!(
            objects_cache().lock().unwrap().get(&key_a).is_none(),
            "source A must be invalidated"
        );
        assert!(
            objects_cache().lock().unwrap().get(&key_b).is_some(),
            "source B must survive"
        );
        if let Ok(mut guard) = objects_cache().lock() {
            guard.remove(&key_b);
        }
    }

    #[test]
    fn cache_hit_miss_counters_advance() {
        let before = cache_stats();
        let map: StdMutex<StdHashMap<String, (StdInstant, Vec<SchemaObject>)>> =
            StdMutex::new(StdHashMap::new());
        cache_store(&map, "counted".into(), vec![obj("x", None)]);
        let _ = cache_lookup("test", &map, "counted", StdDuration::from_secs(30));
        let _ = cache_lookup("test", &map, "missing", StdDuration::from_secs(30));
        let after = cache_stats();
        assert!(after.0 > before.0, "hit counter must advance");
        assert!(after.1 > before.1, "miss counter must advance");
    }

    #[test]
    fn schema_overview_meta_marks_approx_row_counts() {
        let meta = SchemaOverviewMeta {
            n_tables: 1,
            n_foreign_keys: 0,
            truncated: false,
            row_counts: "approx",
            hubs: vec![],
            orphans: vec![],
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"row_counts\":\"approx\""));
    }

    #[test]
    fn relevance_scores_exact_prefix_substring_and_comment() {
        let mut exact = obj("users", None);
        exact.name = "users".into();
        assert_eq!(score_object(&exact, "users"), Some(1000));
        exact.name = "users_archive".into();
        assert_eq!(score_object(&exact, "users"), Some(800));
        exact.name = "old_users_archive".into();
        assert_eq!(score_object(&exact, "users"), Some(600));
        exact.name = "legacy".into();
        exact.comment = Some("user lookup table".into());
        assert_eq!(score_object(&exact, "user"), Some(200));
    }

    #[test]
    fn keyword_filters_name_table_and_schema() {
        let mut o = obj("users", Some("public"));
        o.table = Some("accounts".into());
        assert_eq!(filter_objects(vec![o.clone()], Some("user")).len(), 1);
        assert_eq!(filter_objects(vec![o.clone()], Some("acc")).len(), 1);
        assert_eq!(filter_objects(vec![o], Some("pub")).len(), 1);
    }
}
