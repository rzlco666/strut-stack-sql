use serde::{Deserialize, Serialize};
use sqlx::{mysql::MySqlRow, postgres::PgRow, MySqlPool, Row};

use crate::db::value::{mysql_decode_text_by_index, mysql_decode_text_by_name};
use crate::db::{EngineKind, EnginePool};

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
    let mut objects = match object_type {
        ObjectType::Schema => list_schemas(pool, keyword).await?,
        ObjectType::Table => list_tables(pool, connection_url, schema, keyword).await?,
        ObjectType::Column => list_columns(pool, connection_url, schema, keyword).await?,
        ObjectType::Index => list_indexes(pool, connection_url, schema, None, keyword).await?,
    };

    let total = objects.len();
    let slice = objects
        .drain(offset..total.min(offset + limit))
        .collect::<Vec<_>>();

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

pub async fn list_schemas(
    pool: &EnginePool,
    keyword: Option<&str>,
) -> Result<Vec<SchemaObject>, sqlx::Error> {
    match pool.engine() {
        EngineKind::Postgres => {
            let pool = pool.postgres().expect("postgres");
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
            let pool = pool.mysql().expect("mysql");
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
            let pool = pool.sqlite().expect("sqlite");
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
    match pool.engine() {
        EngineKind::Postgres => {
            let pool = pool.postgres().expect("postgres");
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
            let pool = pool.mysql().expect("mysql");
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
            let pool = pool.sqlite().expect("sqlite");
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
            let pool = pool.postgres().expect("postgres");
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
            let pool = pool.mysql().expect("mysql");
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
            let pool = pool.sqlite().expect("sqlite");
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
            let pool = pool.mysql().expect("mysql");
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
            let pool = pool.postgres().expect("postgres");
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
            let pool = pool.mysql().expect("mysql");
            let mut columns = mysql_columns_from_show(pool, &schema, &table_name).await?;
            if columns.is_empty() {
                columns = mysql_columns_from_information_schema(pool, &schema, &table_name).await?;
            }
            columns
        }
        EngineKind::Sqlite => {
            let pool = pool.sqlite().expect("sqlite");
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
            let pool = pool.postgres().expect("postgres");
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
                        Some(SchemaObject {
                            object_type: ObjectType::Index,
                            schema: Some(schema.into()),
                            name: name.clone(),
                            table: Some(tablename),
                            data_type: Some(def),
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
            let pool = pool.mysql().expect("mysql");
            let scope = resolve_mysql_schema(pool, schema, connection_url).await?;
            let rows = match (&scope, table) {
                (MysqlSchemaScope::One(schema_name), Some(table)) => {
                    sqlx::query(
                        "SELECT index_name, column_name, non_unique \
                         FROM information_schema.statistics \
                         WHERE table_schema = ? AND table_name = ? \
                         ORDER BY index_name, seq_in_index",
                    )
                    .bind(schema_name)
                    .bind(table)
                    .fetch_all(pool)
                    .await?
                }
                (MysqlSchemaScope::One(schema_name), None) => {
                    sqlx::query(
                        "SELECT index_name, column_name, non_unique \
                         FROM information_schema.statistics \
                         WHERE table_schema = ? ORDER BY index_name, seq_in_index",
                    )
                    .bind(schema_name)
                    .fetch_all(pool)
                    .await?
                }
                (MysqlSchemaScope::All, Some(table)) => {
                    sqlx::query(
                        "SELECT table_schema, index_name, column_name, non_unique \
                         FROM information_schema.statistics \
                         WHERE table_name = ? \
                         ORDER BY table_schema, index_name, seq_in_index",
                    )
                    .bind(table)
                    .fetch_all(pool)
                    .await?
                }
                (MysqlSchemaScope::All, None) => {
                    sqlx::query(
                        "SELECT table_schema, index_name, column_name, non_unique \
                         FROM information_schema.statistics \
                         ORDER BY table_schema, index_name, seq_in_index",
                    )
                    .fetch_all(pool)
                    .await?
                }
            };

            // Group by index name
            use std::collections::BTreeMap;
            let mut map: BTreeMap<String, (Vec<String>, bool, Option<String>)> = BTreeMap::new();
            for row in &rows {
                if matches!(scope, MysqlSchemaScope::All) && table.is_none() {
                    let sch: String = row.try_get(0).ok().unwrap_or_default();
                    let name: String = row.try_get(1).ok().unwrap_or_default();
                    let col: String = row.try_get(2).ok().unwrap_or_default();
                    let non_unique: i32 = row.try_get(3).ok().unwrap_or(1);
                    let key = format!("{sch}.{name}");
                    let entry = map
                        .entry(key)
                        .or_insert((vec![], non_unique == 0, Some(sch)));
                    entry.0.push(col);
                } else {
                    let name: String = row.try_get(0).ok().unwrap_or_default();
                    let col: String = row.try_get(1).ok().unwrap_or_default();
                    let non_unique: i32 = row.try_get(2).ok().unwrap_or(1);
                    let entry = map.entry(name.clone()).or_insert((
                        vec![],
                        non_unique == 0,
                        schema.map(str::to_string),
                    ));
                    entry.0.push(col);
                }
            }

            let objects = map
                .into_iter()
                .map(|(name, (columns, unique, sch))| SchemaObject {
                    object_type: ObjectType::Index,
                    schema: sch,
                    name: name.clone(),
                    table: table.map(str::to_string),
                    data_type: None,
                    nullable: None,
                    columns: None,
                    indexes: Some(vec![IndexInfo {
                        name,
                        columns,
                        unique,
                    }]),
                })
                .collect();
            Ok(filter_objects(objects, keyword))
        }
        EngineKind::Sqlite => {
            let pool = pool.sqlite().expect("sqlite");
            let objects = sqlite_list_index_objects(pool, schema, table).await?;
            Ok(filter_objects(objects, keyword))
        }
    }
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
        let sql = format!(
            "SELECT name FROM \"{schema_name}\".sqlite_master \
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name"
        );
        sqlx::query(&sql).fetch_all(pool).await
    }
}

fn sqlite_quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
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
    let tables: Vec<String> = if let Some(table) = table {
        vec![table.to_string()]
    } else {
        sqlite_list_table_rows(pool, &schema_name)
            .await?
            .into_iter()
            .filter_map(|r| r.try_get(0).ok())
            .collect()
    };

    let mut objects = Vec::new();
    for table_name in tables {
        let index_sql = format!("PRAGMA index_list({})", sqlite_quote_literal(&table_name));
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
                schema: Some(schema_name.clone()),
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

/// One-shot schema survey: tables (+ approx row counts) and foreign keys.
pub async fn schema_overview(
    pool: &EnginePool,
    connection_url: Option<&str>,
    schema: Option<&str>,
) -> Result<SchemaOverview, sqlx::Error> {
    let schema_name = match pool.engine() {
        EngineKind::Postgres => schema.unwrap_or("public").to_string(),
        EngineKind::Mysql => {
            let mysql = pool.mysql().expect("mysql");
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
            let pg = pool.postgres().expect("postgres");
            let rows = sqlx::query(
                "SELECT c.relname, GREATEST(c.reltuples::bigint, 0) \
                 FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relkind = 'r' \
                 ORDER BY c.relname",
            )
            .bind(&schema_name)
            .fetch_all(pg)
            .await?;
            rows.iter()
                .filter_map(|r| {
                    let name: String = r.try_get(0).ok()?;
                    let approx: i64 = r.try_get(1).unwrap_or(0);
                    Some(TableOverview {
                        name,
                        approx_rows: Some(approx),
                    })
                })
                .collect::<Vec<_>>()
        }
        EngineKind::Mysql => {
            let mysql = pool.mysql().expect("mysql");
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
            // SQLite PRAGMA requires a table; aggregate across overview tables.
            let mut all = Vec::new();
            for t in &tables {
                let fks =
                    list_foreign_keys(pool, connection_url, Some(&schema_name), Some(&t.name))
                        .await?;
                all.extend(fks);
            }
            all
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
            let pool = pool.postgres().expect("postgres");
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
            Ok(group_foreign_key_rows_pg(&rows))
        }
        EngineKind::Mysql => {
            let pool = pool.mysql().expect("mysql");
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
            Ok(group_foreign_key_rows_mysql(&rows))
        }
        EngineKind::Sqlite => {
            let pool = pool.sqlite().expect("sqlite");
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

fn group_foreign_key_rows_mysql(rows: &[MySqlRow]) -> Vec<ForeignKeyInfo> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, ForeignKeyInfo> = BTreeMap::new();
    for row in rows {
        let name: String = row.try_get(0).ok().unwrap_or_default();
        let table: String = row.try_get(1).ok().unwrap_or_default();
        let col: String = row.try_get(2).ok().unwrap_or_default();
        let ref_table: String = row.try_get(3).ok().unwrap_or_default();
        let ref_col: String = row.try_get(4).ok().unwrap_or_default();
        let on_delete: String = row.try_get(5).ok().unwrap_or_default();
        let on_update: String = row.try_get(6).ok().unwrap_or_default();
        map.entry(name.clone())
            .and_modify(|fk| {
                fk.columns.push(col.clone());
                fk.ref_columns.push(ref_col.clone());
            })
            .or_insert(ForeignKeyInfo {
                name,
                table,
                columns: vec![col],
                ref_table,
                ref_columns: vec![ref_col],
                on_delete: Some(on_delete),
                on_update: Some(on_update),
            });
    }
    map.into_values().collect()
}

fn group_foreign_key_rows_pg(rows: &[PgRow]) -> Vec<ForeignKeyInfo> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, ForeignKeyInfo> = BTreeMap::new();
    for row in rows {
        let name: String = row.try_get(0).ok().unwrap_or_default();
        let table: String = row.try_get(1).ok().unwrap_or_default();
        let col: String = row.try_get(2).ok().unwrap_or_default();
        let ref_table: String = row.try_get(3).ok().unwrap_or_default();
        let ref_col: String = row.try_get(4).ok().unwrap_or_default();
        let on_delete: String = row.try_get(5).ok().unwrap_or_default();
        let on_update: String = row.try_get(6).ok().unwrap_or_default();
        map.entry(name.clone())
            .and_modify(|fk| {
                fk.columns.push(col.clone());
                fk.ref_columns.push(ref_col.clone());
            })
            .or_insert(ForeignKeyInfo {
                name,
                table,
                columns: vec![col],
                ref_table,
                ref_columns: vec![ref_col],
                on_delete: Some(on_delete),
                on_update: Some(on_update),
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
}
