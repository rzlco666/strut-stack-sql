use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use serde::Serialize;
use serde_json::Value;
use sqlx::{mysql::MySqlRow, postgres::PgRow, sqlite::SqliteRow, Column, Row};

use crate::config::WriteMode;
use crate::db::bind::{bind_mysql_params, bind_pg_params, bind_sqlite_params};
use crate::db::value::{decode_mysql_cell, decode_pg_cell, decode_sqlite_cell};
use crate::db::{EngineKind, EnginePool};
use crate::format::{truncate_to_bytes, ColumnarMeta, ColumnarResult};
use crate::guard::{
    validate_and_prepare_with_options, GuardError, PrepareOptions, SqlClass, StmtClass,
};

/// Whether a guard-classified statement yields rows and must go through the
/// fetch path (SQL-BUG-05). This replaces the old string-prefix heuristic
/// (`is_select_like` on the raw SQL): the decision now comes exclusively from
/// the guard's own classification — `is_select_like()` for queries, plus the
/// other row-returning Read statements (SHOW / EXPLAIN / DESCRIBE), which the
/// guard labels separately. A writing CTE classified as DML by the guard now
/// correctly takes the execute path.
fn class_fetches_rows(class: &StmtClass) -> bool {
    class.is_select_like() || class.sql_class() == SqlClass::Read
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Guard(#[from] GuardError),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<ColumnarResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchResult {
    pub results: Vec<QueryResult>,
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub write_mode: WriteMode,
    pub max_rows: u32,
    pub max_bytes: usize,
    pub timeout: Duration,
    pub limit_injected: bool,
    pub page_offset: usize,
    pub page_size: Option<usize>,
    /// Case-insensitive column names to redact in result cells (opt-in).
    pub deny_columns: Vec<String>,
}

pub async fn execute_query(
    pool: &EnginePool,
    sql: &str,
    params: &[Value],
    opts: &ExecOptions,
) -> Result<QueryResult, ExecError> {
    let engine = pool.engine();
    let prepared = validate_and_prepare_with_options(
        sql,
        params,
        engine,
        opts.write_mode,
        opts.max_rows,
        PrepareOptions {
            page_offset: opts.page_offset,
            page_size: opts.page_size,
        },
    )?;
    let started = Instant::now();
    let audit = std::env::var("MCP_SQL_LOG")
        .map(|v| v.contains("audit"))
        .unwrap_or(false);
    if audit {
        tracing::info!(sql = %sql, param_count = params.len(), "audit: execute");
    }

    if prepared.class.requires_writes_for_explain() && !opts.write_mode.allows_dml() {
        tracing::warn!(
            duration_ms = started.elapsed().as_millis() as u64,
            engine = ?engine,
            error = "explain_analyze_requires_writes",
            "query failed"
        );
        return Ok(QueryResult {
            ok: false,
            data: None,
            error: Some("EXPLAIN ANALYZE executes the query and requires --allow-writes".into()),
        });
    }

    // SQL-BUG-05 / SQL-SEC (limit_unverified): execution shape derives from
    // the guard's classification, never from the SQL text. When the guard
    // could not verify the statement's LIMIT (parameterized / FETCH FIRST),
    // cap the rows actually taken from the DB at max_rows (+1 probe row) and
    // mark the result truncated.
    let fetch_rows = class_fetches_rows(&prepared.class);
    let row_cap = if prepared.limit_unverified && !prepared.server_pagination {
        Some(opts.max_rows as usize)
    } else {
        None
    };

    let result = match tokio::time::timeout(
        opts.timeout,
        run_sql(
            pool,
            &prepared.sql,
            &prepared.params,
            engine,
            fetch_rows,
            row_cap,
        ),
    )
    .await
    {
        Ok(Ok(mut columnar)) => {
            if prepared.limit_injected {
                columnar.meta.limit_injected = Some(true);
            }
            if prepared.limit_clamped {
                columnar.meta.limit_clamped = Some(true);
            }
            if prepared.server_pagination {
                let page_size = prepared.page_size as usize;
                let fetched = columnar.rows.len();
                let has_more = fetched > page_size;
                if has_more {
                    columnar.rows.truncate(page_size);
                }
                columnar.meta.n = columnar.rows.len();
                columnar.meta.page_offset = Some(opts.page_offset);
                columnar.meta.page_size = Some(page_size);
                columnar.meta.total_fetched = Some(fetched);
                columnar.meta.has_more = Some(has_more);
            } else {
                let page_size = opts.page_size.unwrap_or(opts.max_rows as usize);
                if opts.page_offset > 0 || opts.page_size.is_some() {
                    columnar.apply_pagination(opts.page_offset, page_size);
                }
            }
            let columnar = truncate_to_bytes(columnar, opts.max_bytes);
            let columnar = redact_denied_columns(columnar, &opts.deny_columns);
            tracing::info!(
                duration_ms = started.elapsed().as_millis() as u64,
                rows = columnar.meta.n,
                engine = ?engine,
                limit_injected = prepared.limit_injected,
                limit_clamped = prepared.limit_clamped,
                truncated = columnar.meta.truncated,
                param_count = prepared.params.len(),
                "query executed"
            );
            Ok(QueryResult {
                ok: true,
                data: Some(columnar),
                error: None,
            })
        }
        Ok(Err(e)) => {
            tracing::warn!(
                duration_ms = started.elapsed().as_millis() as u64,
                engine = ?engine,
                error = %e,
                "query failed"
            );
            Err(e)
        }
        Err(_) => {
            tracing::warn!(
                duration_ms = started.elapsed().as_millis() as u64,
                engine = ?engine,
                error = "query_timeout",
                "query failed (client timeout; server statement_timeout should also abort PG/MySQL)"
            );
            Err(ExecError::Other(
                "query timeout (client limit; server-side statement_timeout also applied on PG/MySQL)"
                    .into(),
            ))
        }
    };

    result
}

pub async fn execute_batch(
    pool: &EnginePool,
    queries: Vec<(String, Vec<Value>)>,
    opts: &ExecOptions,
    concurrency: usize,
    fail_fast: bool,
) -> BatchResult {
    let results = stream::iter(queries)
        .map(|(sql, params)| {
            let pool = pool.clone();
            let opts = opts.clone();
            async move {
                match execute_query(&pool, &sql, &params, &opts).await {
                    Ok(r) => r,
                    Err(e) => QueryResult {
                        ok: false,
                        data: None,
                        error: Some(e.to_string()),
                    },
                }
            }
        })
        .buffered(concurrency)
        .collect::<Vec<_>>()
        .await;

    if fail_fast {
        if let Some(failed) = results.iter().find(|r| !r.ok) {
            return BatchResult {
                results: vec![failed.clone()],
            };
        }
    }

    BatchResult { results }
}

/// Stream rows from a bound query, stopping after `cap` kept rows plus one
/// probe row. Returns the kept rows and whether a probe row existed (i.e. the
/// result was truncated at `cap`).
macro_rules! fetch_rows_capped {
    ($query:expr, $pool:expr, $cap:expr) => {{
        let mut stream = $query.fetch($pool);
        let mut rows = Vec::new();
        let mut overflow = false;
        while let Some(item) = stream.next().await {
            let row = item?;
            if rows.len() >= $cap {
                overflow = true;
                break;
            }
            rows.push(row);
        }
        (rows, overflow)
    }};
}

async fn run_sql(
    pool: &EnginePool,
    sql: &str,
    params: &[Value],
    engine: EngineKind,
    fetch_rows: bool,
    row_cap: Option<usize>,
) -> Result<ColumnarResult, ExecError> {
    match engine {
        EngineKind::Postgres => {
            let pool = pool
                .postgres()
                .map_err(|e| ExecError::Other(e.to_string()))?;
            if fetch_rows {
                let query =
                    bind_pg_params(sql, params).map_err(|e| ExecError::Other(e.to_string()))?;
                if let Some(cap) = row_cap {
                    let (rows, overflow) = fetch_rows_capped!(query, pool, cap);
                    let mut columnar = pg_rows_to_columnar(&rows)?;
                    if overflow {
                        columnar.meta.truncated = true;
                    }
                    Ok(columnar)
                } else {
                    let rows = query.fetch_all(pool).await?;
                    pg_rows_to_columnar(&rows)
                }
            } else {
                let result = bind_pg_params(sql, params)
                    .map_err(|e| ExecError::Other(e.to_string()))?
                    .execute(pool)
                    .await?;
                Ok(ColumnarResult::empty_command(result.rows_affected()))
            }
        }
        EngineKind::Mysql => {
            let pool = pool.mysql().map_err(|e| ExecError::Other(e.to_string()))?;
            if fetch_rows {
                let query =
                    bind_mysql_params(sql, params).map_err(|e| ExecError::Other(e.to_string()))?;
                if let Some(cap) = row_cap {
                    let (rows, overflow) = fetch_rows_capped!(query, pool, cap);
                    let mut columnar = mysql_rows_to_columnar(&rows)?;
                    if overflow {
                        columnar.meta.truncated = true;
                    }
                    Ok(columnar)
                } else {
                    let rows = query.fetch_all(pool).await?;
                    mysql_rows_to_columnar(&rows)
                }
            } else {
                let result = bind_mysql_params(sql, params)
                    .map_err(|e| ExecError::Other(e.to_string()))?
                    .execute(pool)
                    .await?;
                Ok(ColumnarResult::empty_command(result.rows_affected()))
            }
        }
        EngineKind::Sqlite => {
            let pool = pool.sqlite().map_err(|e| ExecError::Other(e.to_string()))?;
            if fetch_rows {
                let query =
                    bind_sqlite_params(sql, params).map_err(|e| ExecError::Other(e.to_string()))?;
                if let Some(cap) = row_cap {
                    let (rows, overflow) = fetch_rows_capped!(query, pool, cap);
                    let mut columnar = sqlite_rows_to_columnar(&rows)?;
                    if overflow {
                        columnar.meta.truncated = true;
                    }
                    Ok(columnar)
                } else {
                    let rows = query.fetch_all(pool).await?;
                    sqlite_rows_to_columnar(&rows)
                }
            } else {
                let result = bind_sqlite_params(sql, params)
                    .map_err(|e| ExecError::Other(e.to_string()))?
                    .execute(pool)
                    .await?;
                Ok(ColumnarResult::empty_command(result.rows_affected()))
            }
        }
    }
}

fn pg_rows_to_columnar(rows: &[PgRow]) -> Result<ColumnarResult, ExecError> {
    if rows.is_empty() {
        return Ok(empty_columnar());
    }

    let cols: Vec<String> = rows[0]
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();

    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = Vec::with_capacity(cols.len());
        for (i, _) in row.columns().iter().enumerate() {
            values.push(decode_pg_cell(row, i).map_err(ExecError::Database)?);
        }
        out_rows.push(values);
    }

    let n = out_rows.len();
    Ok(ColumnarResult {
        cols,
        rows: out_rows,
        meta: ColumnarMeta {
            n,
            truncated: false,
            rows_affected: None,
            limit_injected: None,
            limit_clamped: None,
            page_offset: None,
            page_size: None,
            has_more: None,
            total_fetched: None,
            redacted_columns: None,
        },
    })
}

fn mysql_rows_to_columnar(rows: &[MySqlRow]) -> Result<ColumnarResult, ExecError> {
    if rows.is_empty() {
        return Ok(empty_columnar());
    }

    let cols: Vec<String> = rows[0]
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();

    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = Vec::with_capacity(cols.len());
        for (i, _) in row.columns().iter().enumerate() {
            values.push(decode_mysql_cell(row, i).map_err(ExecError::Database)?);
        }
        out_rows.push(values);
    }

    let n = out_rows.len();
    Ok(ColumnarResult {
        cols,
        rows: out_rows,
        meta: ColumnarMeta {
            n,
            truncated: false,
            rows_affected: None,
            limit_injected: None,
            limit_clamped: None,
            page_offset: None,
            page_size: None,
            has_more: None,
            total_fetched: None,
            redacted_columns: None,
        },
    })
}

fn sqlite_rows_to_columnar(rows: &[SqliteRow]) -> Result<ColumnarResult, ExecError> {
    if rows.is_empty() {
        return Ok(empty_columnar());
    }

    let cols: Vec<String> = rows[0]
        .columns()
        .iter()
        .map(|c| c.name().to_string())
        .collect();

    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = Vec::with_capacity(cols.len());
        for (i, _) in row.columns().iter().enumerate() {
            values.push(decode_sqlite_cell(row, i).map_err(ExecError::Database)?);
        }
        out_rows.push(values);
    }

    let n = out_rows.len();
    Ok(ColumnarResult {
        cols,
        rows: out_rows,
        meta: ColumnarMeta {
            n,
            truncated: false,
            rows_affected: None,
            limit_injected: None,
            limit_clamped: None,
            page_offset: None,
            page_size: None,
            has_more: None,
            total_fetched: None,
            redacted_columns: None,
        },
    })
}

fn empty_columnar() -> ColumnarResult {
    ColumnarResult {
        cols: vec![],
        rows: vec![],
        meta: ColumnarMeta {
            n: 0,
            truncated: false,
            rows_affected: None,
            limit_injected: None,
            limit_clamped: None,
            page_offset: None,
            page_size: None,
            has_more: None,
            total_fetched: None,
            redacted_columns: None,
        },
    }
}

/// Replace denied column cells with `"[redacted]"` and record names in meta.
///
/// SQL-SEC-16: this redaction is **name-based and best-effort only**. Matching
/// uses the column names as returned by the database — i.e. the alias when
/// the query supplies one (`SELECT password AS p` evades a `password` entry).
/// sqlx 0.8's portable `Column` API exposes only the returned name, not a
/// pre-alias origin name, so origin-based matching is not available here.
/// Treat `deny_columns` as a seat-belt for accidental exposure, never as an
/// access-control boundary; real protection is a read-only DB user and views
/// that exclude sensitive columns.
pub fn redact_denied_columns(
    mut result: ColumnarResult,
    deny_columns: &[String],
) -> ColumnarResult {
    if deny_columns.is_empty() || result.cols.is_empty() {
        return result;
    }
    tracing::warn!(
        deny_columns = deny_columns.len(),
        "deny_columns redaction active: name-based best-effort (aliases can evade it)"
    );
    let denied: Vec<(usize, String)> = result
        .cols
        .iter()
        .enumerate()
        .filter_map(|(i, col)| {
            deny_columns
                .iter()
                .find(|d| d.eq_ignore_ascii_case(col))
                .map(|_| (i, col.clone()))
        })
        .collect();
    if denied.is_empty() {
        return result;
    }
    let redacted_value = Value::String("[redacted]".into());
    for row in &mut result.rows {
        for &(idx, _) in &denied {
            if let Some(cell) = row.get_mut(idx) {
                *cell = redacted_value.clone();
            }
        }
    }
    result.meta.redacted_columns = Some(denied.into_iter().map(|(_, name)| name).collect());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::validate_and_prepare;

    fn class_of(sql: &str, engine: EngineKind, mode: WriteMode) -> StmtClass {
        validate_and_prepare(sql, &[], engine, mode, 100)
            .expect("guard accepts")
            .class
    }

    #[test]
    fn fetch_decision_comes_from_guard_class() {
        // SELECT fetches rows.
        assert!(class_fetches_rows(&class_of(
            "SELECT 1",
            EngineKind::Postgres,
            WriteMode::ReadOnly
        )));
        // SHOW is Read-classed by the guard (label != SELECT) and returns rows.
        assert!(class_fetches_rows(&class_of(
            "SHOW PROCESSLIST",
            EngineKind::Mysql,
            WriteMode::ReadOnly
        )));
        // A plain SELECT-shaped prefix must NOT decide: DML executes.
        assert!(!class_fetches_rows(&class_of(
            "INSERT INTO t VALUES (1)",
            EngineKind::Postgres,
            WriteMode::AllowWrites
        )));
        assert!(!class_fetches_rows(&class_of(
            "DROP TABLE t",
            EngineKind::Postgres,
            WriteMode::AllowDdl
        )));
    }
}
