use std::collections::BTreeMap;

use strut_sql_driver_api::{
    ColumnInfo, DriverError, ForeignKeyInfo, IndexInfo, ObjectType, RoutineInfo, RowEstimateKind,
    SchemaObject, SqlValue, TableInfo, TableRowCount, TableStats,
};

use crate::SqlServerDriver;

// All caller-supplied names are values, never SQL identifiers. A NULL schema
// parameter denotes the explicit wildcard scope; omitted/empty scopes resolve
// through SCHEMA_NAME(), which observes the connected database user's default.
async fn scope(driver: &SqlServerDriver, schema: Option<&str>) -> Result<SqlValue, DriverError> {
    if schema == Some("*") {
        Ok(SqlValue::Null)
    } else {
        resolve_schema(driver, schema).await.map(SqlValue::String)
    }
}

fn optional_name(name: Option<&str>) -> SqlValue {
    name.map_or(SqlValue::Null, |name| SqlValue::String(name.to_owned()))
}

fn value(row: &[SqlValue], index: usize) -> Result<&SqlValue, DriverError> {
    row.get(index).ok_or_else(|| {
        DriverError::Introspection(format!("SQL Server catalog row is missing field {index}"))
    })
}

fn decode_error(index: usize, expected: &str) -> DriverError {
    DriverError::Introspection(format!(
        "SQL Server catalog field {index} is not {expected}"
    ))
}

fn text(row: &[SqlValue], index: usize) -> Result<&str, DriverError> {
    match value(row, index)? {
        SqlValue::String(value) => Ok(value),
        _ => Err(decode_error(index, "text")),
    }
}

fn optional_text(row: &[SqlValue], index: usize) -> Result<Option<String>, DriverError> {
    match value(row, index)? {
        SqlValue::Null => Ok(None),
        SqlValue::String(value) => Ok(Some(value.clone())),
        _ => Err(decode_error(index, "nullable text")),
    }
}

fn boolean(row: &[SqlValue], index: usize) -> Result<bool, DriverError> {
    match value(row, index)? {
        SqlValue::Bool(value) => Ok(*value),
        _ => Err(decode_error(index, "a bit")),
    }
}

fn optional_count(row: &[SqlValue], index: usize) -> Result<Option<i64>, DriverError> {
    match value(row, index)? {
        SqlValue::Null => Ok(None),
        SqlValue::I64(value) if *value >= 0 => Ok(Some(*value)),
        _ => Err(decode_error(index, "a nullable nonnegative bigint")),
    }
}

pub(crate) async fn resolve_schema(
    driver: &SqlServerDriver,
    schema: Option<&str>,
) -> Result<String, DriverError> {
    match schema {
        Some("*") => Err(DriverError::InvalidConfig(
            "SQL Server operation requires one schema, not '*'; omit schema for the user's default"
                .into(),
        )),
        Some(name) if !name.is_empty() => Ok(name.to_owned()),
        _ => {
            let result = driver
                .catalog_query("SELECT CONVERT(nvarchar(128), SCHEMA_NAME())", Vec::new())
                .await?;
            let row = result.rows.first().ok_or_else(|| {
                DriverError::Introspection("SQL Server did not return a default schema".into())
            })?;
            let name = text(row, 0)?;
            if name.is_empty() {
                return Err(DriverError::Introspection(
                    "SQL Server returned an empty default schema".into(),
                ));
            }
            Ok(name.to_owned())
        }
    }
}

pub(crate) async fn list_schemas(
    driver: &SqlServerDriver,
) -> Result<Vec<SchemaObject>, DriverError> {
    let result = driver
        .catalog_query(
            "SELECT s.name FROM sys.schemas s \
             WHERE s.name NOT IN (N'sys', N'INFORMATION_SCHEMA') ORDER BY s.name",
            Vec::new(),
        )
        .await?;
    result
        .rows
        .iter()
        .map(|row| {
            Ok(SchemaObject {
                object_type: ObjectType::Schema,
                schema: None,
                name: text(row, 0)?.to_owned(),
                table: None,
                data_type: None,
                nullable: None,
                comment: None,
            })
        })
        .collect()
}

pub(crate) async fn list_tables(
    driver: &SqlServerDriver,
    schema: Option<&str>,
) -> Result<Vec<SchemaObject>, DriverError> {
    let result = driver
        .catalog_query(
            "SELECT s.name, t.name, CONVERT(nvarchar(max), ep.value) \
             FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id \
             LEFT JOIN sys.extended_properties ep ON ep.class = 1 \
               AND ep.major_id = t.object_id AND ep.minor_id = 0 AND ep.name = N'MS_Description' \
             WHERE t.is_ms_shipped = 0 AND (@P1 IS NULL OR s.name = @P1) \
             ORDER BY s.name, t.name",
            vec![scope(driver, schema).await?],
        )
        .await?;
    result
        .rows
        .iter()
        .map(|row| {
            Ok(SchemaObject {
                object_type: ObjectType::Table,
                schema: Some(text(row, 0)?.to_owned()),
                name: text(row, 1)?.to_owned(),
                table: None,
                data_type: None,
                nullable: None,
                comment: optional_text(row, 2)?,
            })
        })
        .collect()
}

// sys.types preserves alias/CLR types; built-in length/precision/scale are
// rendered explicitly rather than losing nvarchar(max) or decimal(p,s).
const COLUMNS_SQL: &str = "\
SELECT s.name, o.name, c.name, \
 CASE \
  WHEN ty.is_user_defined = 1 THEN QUOTENAME(SCHEMA_NAME(ty.schema_id)) + N'.' + QUOTENAME(ty.name) \
  WHEN ty.name IN (N'varchar', N'char', N'varbinary', N'binary') \
   THEN ty.name + N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CONVERT(nvarchar(10), c.max_length) END + N')' \
  WHEN ty.name IN (N'nvarchar', N'nchar') \
   THEN ty.name + N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CONVERT(nvarchar(10), c.max_length / 2) END + N')' \
  WHEN ty.name IN (N'decimal', N'numeric') \
   THEN ty.name + N'(' + CONVERT(nvarchar(10), c.precision) + N',' + CONVERT(nvarchar(10), c.scale) + N')' \
  WHEN ty.name IN (N'datetime2', N'datetimeoffset', N'time') \
   THEN ty.name + N'(' + CONVERT(nvarchar(10), c.scale) + N')' \
  ELSE ty.name END, \
 c.is_nullable, COALESCE(dc.definition, OBJECT_DEFINITION(c.default_object_id)), \
 CONVERT(bit, CASE WHEN EXISTS ( \
  SELECT 1 FROM sys.indexes pk \
  JOIN sys.index_columns ic ON ic.object_id = pk.object_id AND ic.index_id = pk.index_id \
  WHERE pk.object_id = o.object_id AND pk.is_primary_key = 1 \
   AND ic.column_id = c.column_id AND ic.key_ordinal > 0 \
 ) THEN 1 ELSE 0 END), CONVERT(nvarchar(max), ep.value) \
FROM sys.objects o \
JOIN sys.schemas s ON s.schema_id = o.schema_id \
JOIN sys.columns c ON c.object_id = o.object_id \
JOIN sys.types ty ON ty.user_type_id = c.user_type_id \
LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
LEFT JOIN sys.extended_properties ep ON ep.class = 1 \
 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = N'MS_Description' \
WHERE o.type IN (N'U', N'V') AND o.is_ms_shipped = 0 \
 AND (@P1 IS NULL OR s.name = @P1) AND (@P2 IS NULL OR o.name = @P2) \
ORDER BY s.name, o.name, c.column_id";

pub(crate) async fn list_columns(
    driver: &SqlServerDriver,
    schema: Option<&str>,
) -> Result<Vec<SchemaObject>, DriverError> {
    let result = driver
        .catalog_query(
            COLUMNS_SQL,
            vec![scope(driver, schema).await?, SqlValue::Null],
        )
        .await?;
    result
        .rows
        .iter()
        .map(|row| {
            Ok(SchemaObject {
                object_type: ObjectType::Column,
                schema: Some(text(row, 0)?.to_owned()),
                name: text(row, 2)?.to_owned(),
                table: Some(text(row, 1)?.to_owned()),
                data_type: Some(text(row, 3)?.to_owned()),
                nullable: Some(boolean(row, 4)?),
                comment: optional_text(row, 7)?,
            })
        })
        .collect()
}

pub(crate) async fn describe_table(
    driver: &SqlServerDriver,
    schema: Option<&str>,
    table: &str,
) -> Result<TableInfo, DriverError> {
    let schema = resolve_schema(driver, schema).await?;
    let result = driver
        .catalog_query(
            COLUMNS_SQL,
            vec![
                SqlValue::String(schema.clone()),
                SqlValue::String(table.to_owned()),
            ],
        )
        .await?;
    let first = result.rows.first().ok_or_else(|| {
        DriverError::Introspection(format!(
            "SQL Server table or view {schema}.{table} does not exist or is not visible"
        ))
    })?;
    let columns = result
        .rows
        .iter()
        .map(|row| {
            Ok(ColumnInfo {
                name: text(row, 2)?.to_owned(),
                data_type: text(row, 3)?.to_owned(),
                nullable: boolean(row, 4)?,
                default_value: optional_text(row, 5)?,
                is_primary_key: boolean(row, 6)?,
                key: None,
                extra: None,
                comment: optional_text(row, 7)?,
            })
        })
        .collect::<Result<Vec<_>, DriverError>>()?;
    Ok(TableInfo {
        schema: Some(text(first, 0)?.to_owned()),
        name: text(first, 1)?.to_owned(),
        columns,
    })
}

pub(crate) async fn list_indexes(
    driver: &SqlServerDriver,
    schema: Option<&str>,
    table: Option<&str>,
) -> Result<Vec<IndexInfo>, DriverError> {
    let result = driver
        .catalog_query(
            "SELECT s.name, o.name, i.name, c.name, i.is_unique, i.is_primary_key \
             FROM sys.indexes i \
             JOIN sys.objects o ON o.object_id = i.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             LEFT JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
               AND ic.is_included_column = 0 AND (i.type NOT IN (1, 2) OR ic.key_ordinal > 0) \
             LEFT JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
             WHERE o.type IN (N'U', N'V') AND o.is_ms_shipped = 0 \
               AND i.index_id > 0 AND i.is_hypothetical = 0 \
               AND (@P1 IS NULL OR s.name = @P1) AND (@P2 IS NULL OR o.name = @P2) \
             ORDER BY s.name, o.name, i.name, ic.key_ordinal, ic.index_column_id",
            vec![scope(driver, schema).await?, optional_name(table)],
        )
        .await?;
    let mut indexes: BTreeMap<(&str, &str, &str), IndexInfo> = BTreeMap::new();
    for row in &result.rows {
        let schema = text(row, 0)?;
        let table = text(row, 1)?;
        let name = text(row, 2)?;
        let column = optional_text(row, 3)?;
        let unique = boolean(row, 4)?;
        let primary = boolean(row, 5)?;
        let index = indexes
            .entry((schema, table, name))
            .or_insert_with(|| IndexInfo {
                schema: Some(schema.to_owned()),
                table: table.to_owned(),
                name: name.to_owned(),
                columns: Vec::new(),
                unique,
                primary,
            });
        if let Some(column) = column {
            index.columns.push(column);
        }
    }
    Ok(indexes.into_values().collect())
}

pub(crate) async fn list_foreign_keys(
    driver: &SqlServerDriver,
    schema: Option<&str>,
    table: Option<&str>,
) -> Result<Vec<ForeignKeyInfo>, DriverError> {
    let result = driver
        .catalog_query(
            "SELECT s.name, t.name, fk.name, c.name, rs.name, rt.name, rc.name, \
               REPLACE(fk.delete_referential_action_desc, N'_', N' '), \
               REPLACE(fk.update_referential_action_desc, N'_', N' ') \
             FROM sys.foreign_keys fk \
             JOIN sys.tables t ON t.object_id = fk.parent_object_id \
             JOIN sys.schemas s ON s.schema_id = t.schema_id \
             JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
             JOIN sys.columns c ON c.object_id = fkc.parent_object_id AND c.column_id = fkc.parent_column_id \
             JOIN sys.tables rt ON rt.object_id = fkc.referenced_object_id \
             JOIN sys.schemas rs ON rs.schema_id = rt.schema_id \
             JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
             WHERE t.is_ms_shipped = 0 AND (@P1 IS NULL OR s.name = @P1) \
               AND (@P2 IS NULL OR t.name = @P2) \
             ORDER BY s.name, t.name, fk.name, fkc.constraint_column_id",
            vec![scope(driver, schema).await?, optional_name(table)],
        )
        .await?;
    let mut keys: BTreeMap<(&str, &str, &str), ForeignKeyInfo> = BTreeMap::new();
    for row in &result.rows {
        let schema = text(row, 0)?;
        let table = text(row, 1)?;
        let name = text(row, 2)?;
        let column = text(row, 3)?.to_owned();
        let ref_schema = text(row, 4)?;
        let ref_table = text(row, 5)?;
        let ref_column = text(row, 6)?.to_owned();
        let on_delete = text(row, 7)?;
        let on_update = text(row, 8)?;
        let key = keys
            .entry((schema, table, name))
            .or_insert_with(|| ForeignKeyInfo {
                schema: Some(schema.to_owned()),
                table: table.to_owned(),
                name: name.to_owned(),
                columns: Vec::new(),
                ref_schema: Some(ref_schema.to_owned()),
                ref_table: ref_table.to_owned(),
                ref_columns: Vec::new(),
                on_delete: Some(on_delete.to_owned()),
                on_update: Some(on_update.to_owned()),
            });
        key.columns.push(column);
        key.ref_columns.push(ref_column);
    }
    Ok(keys.into_values().collect())
}

const ROW_COUNTS_SQL: &str = "\
SELECT t.name, CASE WHEN t.is_memory_optimized = 1 THEN NULL ELSE p.approx_rows END \
FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id \
OUTER APPLY (SELECT SUM(p.rows) AS approx_rows FROM sys.partitions p \
 WHERE p.object_id = t.object_id AND p.index_id IN (0, 1)) p \
WHERE t.is_ms_shipped = 0 AND s.name = @P1 ORDER BY t.name";

pub(crate) async fn approximate_row_counts(
    driver: &SqlServerDriver,
    schema: Option<&str>,
) -> Result<Vec<TableRowCount>, DriverError> {
    // The API has no schema field on TableRowCount; reject wildcard scope
    // rather than conflating equally named tables from different schemas.
    let schema = resolve_schema(driver, schema).await?;
    let result = driver
        .catalog_query(ROW_COUNTS_SQL, vec![SqlValue::String(schema)])
        .await?;
    result
        .rows
        .iter()
        .map(|row| {
            Ok(TableRowCount {
                name: text(row, 0)?.to_owned(),
                approx_rows: optional_count(row, 1)?,
            })
        })
        .collect()
}

pub(crate) async fn table_stats(
    driver: &SqlServerDriver,
    schema: Option<&str>,
) -> Result<Vec<TableStats>, DriverError> {
    let schema = resolve_schema(driver, schema).await?;
    let result = driver
        .catalog_query(
            "SELECT t.name, \
               CASE WHEN t.is_memory_optimized = 1 THEN NULL ELSE counts.approx_rows END, \
               CASE WHEN t.is_memory_optimized = 1 OR special.has_special = 1 \
                 OR t.filestream_data_space_id IS NOT NULL THEN NULL ELSE sizes.table_bytes END, \
               CASE WHEN t.is_memory_optimized = 1 OR special.has_special = 1 THEN NULL ELSE sizes.index_bytes END \
             FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id \
             OUTER APPLY (SELECT SUM(p.rows) AS approx_rows FROM sys.partitions p \
               WHERE p.object_id = t.object_id AND p.index_id IN (0, 1)) counts \
             OUTER APPLY (SELECT \
               SUM(CASE WHEN p.index_id IN (0, 1) THEN au.used_pages ELSE CONVERT(bigint, 0) END) * CONVERT(bigint, 8192) AS table_bytes, \
               SUM(CASE WHEN p.index_id > 1 THEN au.used_pages ELSE CONVERT(bigint, 0) END) * CONVERT(bigint, 8192) AS index_bytes \
               FROM sys.partitions p JOIN sys.allocation_units au \
                 ON (au.type IN (1, 3) AND au.container_id = p.hobt_id) \
                   OR (au.type = 2 AND au.container_id = p.partition_id) \
               WHERE p.object_id = t.object_id) sizes \
             OUTER APPLY (SELECT CONVERT(bit, CASE WHEN EXISTS ( \
               SELECT 1 FROM sys.indexes i WHERE i.object_id = t.object_id \
                 AND i.type NOT IN (0, 1, 2) AND i.is_hypothetical = 0 \
               ) THEN 1 ELSE 0 END) AS has_special) special \
             WHERE t.is_ms_shipped = 0 AND s.name = @P1 ORDER BY t.name",
            vec![SqlValue::String(schema.clone())],
        )
        .await?;
    // Allocation-unit accounting above covers ordinary rowstore allocations,
    // including LOB/overflow pages, but not FILESTREAM payloads. Do not publish
    // partial sizes for XML, spatial, columnstore or memory-optimized storage
    // with other allocation paths. NULL is unavailable, not zero.
    // sys.partitions.rows is approximate.
    let mut by_table: BTreeMap<String, Vec<IndexInfo>> = BTreeMap::new();
    for index in list_indexes(driver, Some(&schema), None).await? {
        by_table.entry(index.table.clone()).or_default().push(index);
    }
    result
        .rows
        .iter()
        .map(|row| {
            let table = text(row, 0)?.to_owned();
            let approx_rows = optional_count(row, 1)?;
            let indexes = by_table.remove(&table).unwrap_or_default();
            Ok(TableStats {
                table,
                approx_rows,
                row_estimate: if approx_rows.is_some() {
                    RowEstimateKind::CatalogStats
                } else {
                    RowEstimateKind::Unavailable
                },
                table_bytes: optional_count(row, 2)?,
                index_bytes: optional_count(row, 3)?,
                indexes,
            })
        })
        .collect()
}

pub(crate) async fn list_routines(
    driver: &SqlServerDriver,
) -> Result<Vec<RoutineInfo>, DriverError> {
    let result = driver
        .catalog_query(
            "SELECT s.name, o.name, \
               CASE WHEN o.type IN (N'P', N'PC') THEN N'PROCEDURE' ELSE N'FUNCTION' END, \
               CASE WHEN o.type IN (N'P', N'PC') THEN NULL \
                WHEN o.type IN (N'IF', N'TF', N'FT') THEN N'TABLE' \
                WHEN ty.is_user_defined = 1 THEN QUOTENAME(SCHEMA_NAME(ty.schema_id)) + N'.' + QUOTENAME(ty.name) \
                WHEN ty.name IN (N'varchar', N'char', N'varbinary', N'binary') \
                 THEN ty.name + N'(' + CASE WHEN p.max_length = -1 THEN N'max' ELSE CONVERT(nvarchar(10), p.max_length) END + N')' \
                WHEN ty.name IN (N'nvarchar', N'nchar') \
                 THEN ty.name + N'(' + CASE WHEN p.max_length = -1 THEN N'max' ELSE CONVERT(nvarchar(10), p.max_length / 2) END + N')' \
                WHEN ty.name IN (N'decimal', N'numeric') \
                 THEN ty.name + N'(' + CONVERT(nvarchar(10), p.precision) + N',' + CONVERT(nvarchar(10), p.scale) + N')' \
                WHEN ty.name IN (N'datetime2', N'datetimeoffset', N'time') \
                 THEN ty.name + N'(' + CONVERT(nvarchar(10), p.scale) + N')' \
                ELSE ty.name END \
             FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
             LEFT JOIN sys.parameters p ON p.object_id = o.object_id AND p.parameter_id = 0 \
             LEFT JOIN sys.types ty ON ty.user_type_id = p.user_type_id \
             WHERE o.is_ms_shipped = 0 AND o.type IN (N'P', N'PC', N'FN', N'IF', N'TF', N'FS', N'FT', N'AF') \
             ORDER BY s.name, o.name",
            Vec::new(),
        )
        .await?;
    result
        .rows
        .iter()
        .map(|row| {
            Ok(RoutineInfo {
                schema: Some(text(row, 0)?.to_owned()),
                name: text(row, 1)?.to_owned(),
                routine_type: text(row, 2)?.to_owned(),
                return_type: optional_text(row, 3)?,
            })
        })
        .collect()
}

pub(crate) async fn search_objects(
    driver: &SqlServerDriver,
    keyword: Option<&str>,
    object_type: Option<ObjectType>,
) -> Result<Vec<SchemaObject>, DriverError> {
    let mut objects = Vec::new();
    if object_type.is_none() || object_type == Some(ObjectType::Schema) {
        objects.extend(list_schemas(driver).await?);
    }
    if object_type.is_none() || object_type == Some(ObjectType::Table) {
        objects.extend(list_tables(driver, Some("*")).await?);
    }
    if object_type.is_none() || object_type == Some(ObjectType::Column) {
        objects.extend(list_columns(driver, Some("*")).await?);
    }
    if object_type.is_none() || object_type == Some(ObjectType::Index) {
        objects.extend(
            list_indexes(driver, Some("*"), None)
                .await?
                .into_iter()
                .map(|index| SchemaObject {
                    object_type: ObjectType::Index,
                    schema: index.schema,
                    name: index.name,
                    table: Some(index.table),
                    data_type: None,
                    nullable: None,
                    comment: None,
                }),
        );
    }
    if object_type.is_none() || object_type == Some(ObjectType::ForeignKey) {
        objects.extend(
            list_foreign_keys(driver, Some("*"), None)
                .await?
                .into_iter()
                .map(|key| SchemaObject {
                    object_type: ObjectType::ForeignKey,
                    schema: key.schema,
                    name: key.name,
                    table: Some(key.table),
                    data_type: None,
                    nullable: None,
                    comment: None,
                }),
        );
    }
    if object_type.is_none() || object_type == Some(ObjectType::Routine) {
        objects.extend(
            list_routines(driver)
                .await?
                .into_iter()
                .map(|routine| SchemaObject {
                    object_type: ObjectType::Routine,
                    schema: routine.schema,
                    name: routine.name,
                    table: None,
                    data_type: routine.return_type,
                    nullable: None,
                    comment: None,
                }),
        );
    }
    if let Some(keyword) = keyword {
        let keyword = keyword.to_lowercase();
        objects.retain(|object| {
            object.name.to_lowercase().contains(&keyword)
                || object
                    .table
                    .as_ref()
                    .is_some_and(|name| name.to_lowercase().contains(&keyword))
                || object
                    .schema
                    .as_ref()
                    .is_some_and(|name| name.to_lowercase().contains(&keyword))
                || object
                    .comment
                    .as_ref()
                    .is_some_and(|comment| comment.to_lowercase().contains(&keyword))
        });
    }
    // Type groups above have stable order; each catalog query orders its
    // objects by schema, owner/table, name or column ordinal. Keyword matching
    // is literal case-insensitive substring matching, as in the API default.
    Ok(objects)
}
