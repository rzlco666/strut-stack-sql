//! ClickHouse catalog introspection.
//!
//! ClickHouse's "schema" is its database: `system.databases`, `system.tables`
//! and `system.columns` are the catalog. Two honest limitations are baked in:
//!
//! * there are no foreign keys, so `list_foreign_keys` is always empty;
//! * there are no standalone indexes: the primary key (the table's sorting
//!   key) and data-skipping indexes are what the engine has, and both are
//!   reported through `IndexInfo`.

use strut_sql_driver_api::{
    DriverError, ForeignKeyInfo, IndexInfo, ObjectType, RoutineInfo, RowEstimateKind, SchemaObject,
    SqlValue, TableInfo, TableRowCount, TableStats,
};

use crate::transport::Transport;

pub(crate) async fn list_schemas(transport: &Transport) -> Result<Vec<SchemaObject>, DriverError> {
    let result = transport
        .fetch("SELECT name FROM system.databases ORDER BY name", &[])
        .await?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            Some(SchemaObject {
                object_type: ObjectType::Schema,
                schema: None,
                name: text(row, 0)?,
                table: None,
                data_type: None,
                nullable: None,
                comment: None,
            })
        })
        .collect())
}

pub(crate) async fn list_schemas_page(
    transport: &Transport,
    keyword: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<SchemaObject>, bool), DriverError> {
    let mut sql = String::from("SELECT name FROM system.databases");
    let mut params = Vec::new();
    if let Some(keyword) = keyword {
        sql.push_str(" WHERE positionCaseInsensitive(name, ?) > 0");
        params.push(SqlValue::String(keyword.to_string()));
    }
    sql.push_str(" ORDER BY name LIMIT ? OFFSET ?");
    params.push(SqlValue::I64(limit.saturating_add(1) as i64));
    params.push(SqlValue::I64(offset as i64));
    let rows = transport.fetch(&sql, &params).await?.rows;
    let mut objects: Vec<SchemaObject> = rows
        .iter()
        .filter_map(|row| {
            Some(SchemaObject {
                object_type: ObjectType::Schema,
                schema: None,
                name: text(row, 0)?,
                table: None,
                data_type: None,
                nullable: None,
                comment: None,
            })
        })
        .collect();
    let has_more = objects.len() > limit;
    objects.truncate(limit);
    Ok((objects, has_more))
}

pub(crate) async fn list_tables(
    transport: &Transport,
    schema: &str,
) -> Result<Vec<SchemaObject>, DriverError> {
    let result = transport
        .fetch(
            "SELECT name FROM system.tables \
             WHERE database = ? AND is_temporary = 0 ORDER BY name",
            &[SqlValue::String(schema.to_string())],
        )
        .await?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            Some(SchemaObject {
                object_type: ObjectType::Table,
                schema: Some(schema.to_string()),
                name: text(row, 0)?,
                table: None,
                data_type: None,
                nullable: None,
                comment: None,
            })
        })
        .collect())
}

pub(crate) async fn list_tables_page(
    transport: &Transport,
    schema: &str,
    keyword: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<SchemaObject>, bool), DriverError> {
    let mut sql =
        String::from("SELECT name FROM system.tables WHERE database = ? AND is_temporary = 0");
    let mut params = vec![SqlValue::String(schema.to_string())];
    if let Some(keyword) = keyword {
        sql.push_str(" AND positionCaseInsensitive(name, ?) > 0");
        params.push(SqlValue::String(keyword.to_string()));
    }
    sql.push_str(" ORDER BY name LIMIT ? OFFSET ?");
    params.push(SqlValue::I64(limit.saturating_add(1) as i64));
    params.push(SqlValue::I64(offset as i64));
    let rows = transport.fetch(&sql, &params).await?.rows;
    let mut objects: Vec<SchemaObject> = rows
        .iter()
        .filter_map(|row| {
            Some(SchemaObject {
                object_type: ObjectType::Table,
                schema: Some(schema.to_string()),
                name: text(row, 0)?,
                table: None,
                data_type: None,
                nullable: None,
                comment: None,
            })
        })
        .collect();
    let has_more = objects.len() > limit;
    objects.truncate(limit);
    Ok((objects, has_more))
}

pub(crate) async fn list_columns(
    transport: &Transport,
    schema: &str,
) -> Result<Vec<SchemaObject>, DriverError> {
    let result = transport
        .fetch(
            "SELECT table, name, type FROM system.columns \
             WHERE database = ? ORDER BY table, position",
            &[SqlValue::String(schema.to_string())],
        )
        .await?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            let table = text(row, 0)?;
            let name = text(row, 1)?;
            let data_type = text(row, 2)?;
            let nullable = crate::value::ChType::parse(&data_type).is_nullable();
            Some(SchemaObject {
                object_type: ObjectType::Column,
                schema: Some(schema.to_string()),
                name,
                table: Some(table),
                data_type: Some(data_type),
                nullable: Some(nullable),
                comment: None,
            })
        })
        .collect())
}

pub(crate) async fn list_columns_page(
    transport: &Transport,
    schema: &str,
    keyword: Option<&str>,
    offset: usize,
    limit: usize,
) -> Result<(Vec<SchemaObject>, bool), DriverError> {
    let mut sql = String::from("SELECT table, name, type FROM system.columns WHERE database = ?");
    let mut params = vec![SqlValue::String(schema.to_string())];
    if let Some(keyword) = keyword {
        sql.push_str(
            " AND (positionCaseInsensitive(name, ?) > 0 \
               OR positionCaseInsensitive(table, ?) > 0)",
        );
        params.push(SqlValue::String(keyword.to_string()));
        params.push(SqlValue::String(keyword.to_string()));
    }
    sql.push_str(" ORDER BY table, position LIMIT ? OFFSET ?");
    params.push(SqlValue::I64(limit.saturating_add(1) as i64));
    params.push(SqlValue::I64(offset as i64));
    let rows = transport.fetch(&sql, &params).await?.rows;
    let mut objects: Vec<SchemaObject> = rows
        .iter()
        .filter_map(|row| {
            let table = text(row, 0)?;
            let name = text(row, 1)?;
            let data_type = text(row, 2)?;
            let nullable = crate::value::ChType::parse(&data_type).is_nullable();
            Some(SchemaObject {
                object_type: ObjectType::Column,
                schema: Some(schema.to_string()),
                name,
                table: Some(table),
                data_type: Some(data_type),
                nullable: Some(nullable),
                comment: None,
            })
        })
        .collect();
    let has_more = objects.len() > limit;
    objects.truncate(limit);
    Ok((objects, has_more))
}

pub(crate) async fn describe_table(
    transport: &Transport,
    schema: &str,
    table: &str,
) -> Result<TableInfo, DriverError> {
    let result = transport
        .fetch(
            "SELECT name, type, default_expression, comment, is_in_primary_key \
             FROM system.columns WHERE database = ? AND table = ? ORDER BY position",
            &[
                SqlValue::String(schema.to_string()),
                SqlValue::String(table.to_string()),
            ],
        )
        .await?;
    let columns = result
        .rows
        .iter()
        .filter_map(|row| {
            let name = text(row, 0)?;
            let data_type = text(row, 1)?;
            let default_expression = text(row, 2).filter(|d| !d.is_empty());
            let comment = text(row, 3).filter(|c| !c.is_empty());
            let is_primary_key = int(row, 4).unwrap_or(0) != 0;
            let nullable = crate::value::ChType::parse(&data_type).is_nullable();
            Some(strut_sql_driver_api::ColumnInfo {
                name,
                data_type,
                nullable,
                default_value: default_expression,
                is_primary_key,
                key: None,
                extra: None,
                comment,
            })
        })
        .collect();
    Ok(TableInfo {
        schema: Some(schema.to_string()),
        name: table.to_string(),
        columns,
    })
}

pub(crate) async fn list_indexes(
    transport: &Transport,
    schema: &str,
    table: Option<&str>,
) -> Result<Vec<IndexInfo>, DriverError> {
    let mut indexes = Vec::new();

    let mut primary_sql =
        String::from("SELECT name, primary_key FROM system.tables WHERE database = ?");
    let mut primary_params = vec![SqlValue::String(schema.to_string())];
    if let Some(table) = table {
        primary_sql.push_str(" AND name = ?");
        primary_params.push(SqlValue::String(table.to_string()));
    }
    primary_sql.push_str(" AND is_temporary = 0 ORDER BY name");
    for row in transport.fetch(&primary_sql, &primary_params).await?.rows {
        let Some(table_name) = text(&row, 0) else {
            continue;
        };
        let expression = text(&row, 1).unwrap_or_default();
        if expression.trim().is_empty() {
            continue;
        }
        indexes.push(IndexInfo {
            schema: Some(schema.to_string()),
            table: table_name,
            name: "PRIMARY".to_string(),
            columns: split_key_columns(&expression),
            // ClickHouse primary keys are not uniqueness constraints.
            unique: false,
            primary: true,
        });
    }

    let mut skip_sql = String::from(
        "SELECT table, name, expr FROM system.data_skipping_indices WHERE database = ?",
    );
    let mut skip_params = vec![SqlValue::String(schema.to_string())];
    if let Some(table) = table {
        skip_sql.push_str(" AND table = ?");
        skip_params.push(SqlValue::String(table.to_string()));
    }
    skip_sql.push_str(" ORDER BY table, name");
    for row in transport.fetch(&skip_sql, &skip_params).await?.rows {
        let (Some(table_name), Some(name)) = (text(&row, 0), text(&row, 1)) else {
            continue;
        };
        let expression = text(&row, 2).unwrap_or_default();
        indexes.push(IndexInfo {
            schema: Some(schema.to_string()),
            table: table_name,
            name,
            columns: split_key_columns(&expression),
            unique: false,
            primary: false,
        });
    }

    Ok(indexes)
}

pub(crate) async fn list_foreign_keys() -> Vec<ForeignKeyInfo> {
    // ClickHouse has no foreign-key constraints.
    Vec::new()
}

pub(crate) async fn list_routines(transport: &Transport) -> Result<Vec<RoutineInfo>, DriverError> {
    let result = transport
        .fetch(
            "SELECT name FROM system.functions WHERE origin = 'SQLUserDefined' ORDER BY name",
            &[],
        )
        .await?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            Some(RoutineInfo {
                schema: None,
                name: text(row, 0)?,
                routine_type: "function".to_string(),
                return_type: None,
            })
        })
        .collect())
}

pub(crate) async fn table_row_counts(
    transport: &Transport,
    schema: &str,
) -> Result<Vec<TableRowCount>, DriverError> {
    let result = transport
        .fetch(
            "SELECT name, total_rows FROM system.tables \
             WHERE database = ? AND is_temporary = 0 ORDER BY name",
            &[SqlValue::String(schema.to_string())],
        )
        .await?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            Some(TableRowCount {
                name: text(row, 0)?,
                approx_rows: int(row, 1),
            })
        })
        .collect())
}

pub(crate) async fn table_stats(
    transport: &Transport,
    schema: &str,
) -> Result<Vec<TableStats>, DriverError> {
    let result = transport
        .fetch(
            "SELECT name, total_rows, total_bytes FROM system.tables \
             WHERE database = ? AND is_temporary = 0 ORDER BY name",
            &[SqlValue::String(schema.to_string())],
        )
        .await?;
    let indexes = list_indexes(transport, schema, None).await?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            let table = text(row, 0)?;
            let approx_rows = int(row, 1);
            let table_bytes = int(row, 2);
            Some(TableStats {
                table: table.clone(),
                approx_rows,
                row_estimate: if approx_rows.is_some() {
                    RowEstimateKind::CatalogStats
                } else {
                    RowEstimateKind::Unavailable
                },
                table_bytes,
                // ClickHouse does not split index bytes out of total_bytes.
                index_bytes: None,
                indexes: indexes
                    .iter()
                    .filter(|i| i.table == table)
                    .cloned()
                    .collect(),
            })
        })
        .collect())
}

fn text(row: &[SqlValue], index: usize) -> Option<String> {
    match row.get(index)? {
        SqlValue::String(s) => Some(s.clone()),
        SqlValue::Null => None,
        other => Some(format!("{other:?}")),
    }
}

fn int(row: &[SqlValue], index: usize) -> Option<i64> {
    match row.get(index)? {
        SqlValue::I64(n) => Some(*n),
        SqlValue::F64(f) => Some(*f as i64),
        SqlValue::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Split a ClickHouse key expression (`a`, `a, b`, `(a, b)`) into columns.
fn split_key_columns(expression: &str) -> Vec<String> {
    let trimmed = expression.trim();
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(trimmed);
    let mut columns = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in inner.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth -= 1;
                current.push(c);
            }
            ',' if depth == 0 => {
                columns.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        columns.push(current.trim().to_string());
    }
    columns.retain(|c| !c.is_empty());
    columns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_primary_key_expressions() {
        assert_eq!(split_key_columns("id"), vec!["id".to_string()]);
        assert_eq!(
            split_key_columns("(user_id, toDate(ts))"),
            vec!["user_id".to_string(), "toDate(ts)".to_string()]
        );
        assert_eq!(
            split_key_columns("a, b"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn extracts_cells() {
        let row = vec![
            SqlValue::String("t".into()),
            SqlValue::I64(4),
            SqlValue::Null,
        ];
        assert_eq!(text(&row, 0).as_deref(), Some("t"));
        assert_eq!(int(&row, 1), Some(4));
        assert_eq!(int(&row, 2), None);
        assert_eq!(text(&row, 9), None);
    }
}
