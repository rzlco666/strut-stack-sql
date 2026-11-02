use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use serde::Serialize;
use serde_json::Value;
use strut_sql_driver_api::{BoundQuery, DriverError, EngineDriver, EngineKind, SqlValue};
use strut_sql_format::{truncate_to_bytes, ColumnarMeta, ColumnarResult};
use strut_sql_guard::{
    validate_and_prepare_with_options, GuardError, PrepareOptions, SqlClass, StmtClass, WriteMode,
};

use crate::bind::to_sql_values;

/// Whether a guard-classified statement yields rows and must go through the
/// fetch path (SQL-BUG-05). This replaces the old string-prefix heuristic
/// (`is_select_like` on the raw SQL): the decision now comes exclusively from
/// the guard's own classification — `is_select_like()` for queries, plus the
/// other row-returning Read statements (SHOW / EXPLAIN / DESCRIBE), which the
/// guard labels separately. A writing CTE classified as DML by the guard now
/// correctly takes the execute path.
pub(crate) fn class_fetches_rows(class: &StmtClass) -> bool {
    class.is_select_like() || class.sql_class() == SqlClass::Read
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Guard(#[from] GuardError),
    #[error("database error: {0}")]
    Database(String),
    #[error("{0}")]
    Other(String),
}

impl From<DriverError> for ExecError {
    fn from(e: DriverError) -> Self {
        ExecError::Database(e.to_string())
    }
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
    driver: &Arc<dyn EngineDriver>,
    sql: &str,
    params: &[Value],
    opts: &ExecOptions,
) -> Result<QueryResult, ExecError> {
    let engine = driver.kind();
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
            driver,
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
    driver: &Arc<dyn EngineDriver>,
    queries: Vec<(String, Vec<Value>)>,
    opts: &ExecOptions,
    concurrency: usize,
    fail_fast: bool,
) -> BatchResult {
    let results = stream::iter(queries)
        .map(|(sql, params)| {
            let driver = driver.clone();
            let opts = opts.clone();
            async move {
                match execute_query(&driver, &sql, &params, &opts).await {
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

async fn run_sql(
    driver: &Arc<dyn EngineDriver>,
    sql: &str,
    params: &[Value],
    engine: EngineKind,
    fetch_rows: bool,
    row_cap: Option<usize>,
) -> Result<ColumnarResult, ExecError> {
    let bound = BoundQuery {
        sql: sql.to_string(),
        params: to_sql_values(params).map_err(ExecError::Other)?,
        row_cap,
        fetch_rows,
    };
    let result = driver.execute(&bound).await?;

    if !fetch_rows {
        return Ok(ColumnarResult::empty_command(
            result.meta.rows_affected.unwrap_or(0),
        ));
    }

    let cols = result.cols.into_iter().map(|c| c.name).collect();
    let rows = result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|v| sql_value_to_json(v, engine))
                .collect()
        })
        .collect::<Vec<Vec<Value>>>();
    let n = rows.len();
    Ok(ColumnarResult {
        cols,
        rows,
        meta: ColumnarMeta {
            n,
            truncated: result.meta.truncated,
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

fn bigdecimal_to_json(v: &bigdecimal::BigDecimal) -> Value {
    // SQL-BUG-06(c): never lose precision silently. Emit a JSON number only
    // when the value survives a decimal → f64 → decimal round-trip exactly;
    // otherwise emit the exact decimal as a JSON string.
    if v.fractional_digit_count() == 0 {
        if let Ok(i) = v.to_string().parse::<i64>() {
            if (-(1i64 << 53)..=(1i64 << 53)).contains(&i) {
                return Value::from(i);
            }
        }
    }
    let s = v.to_string();
    if let Ok(f) = s.parse::<f64>() {
        if f.is_finite() {
            if let Ok(back) = f.to_string().parse::<bigdecimal::BigDecimal>() {
                if back == *v {
                    return Value::from(f);
                }
            }
        }
    }
    Value::String(s)
}

fn bytes_to_hex(v: &[u8]) -> String {
    v.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn sql_value_to_json(value: SqlValue, engine: EngineKind) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Bool(b) => Value::Bool(b),
        SqlValue::I64(n) => Value::from(n),
        SqlValue::F64(f) => Value::from(f),
        SqlValue::String(s) => Value::String(s),
        SqlValue::Uuid(u) => Value::String(u.to_string()),
        SqlValue::BigDecimal(d) => bigdecimal_to_json(&d),
        SqlValue::Date(d) => Value::String(d.to_string()),
        SqlValue::DateTime(dt) => Value::String(dt.to_string()),
        SqlValue::Binary(b) => match engine {
            EngineKind::Postgres | EngineKind::Sqlite => {
                let mut map = serde_json::Map::new();
                map.insert("$blob_hex".into(), Value::String(bytes_to_hex(&b)));
                Value::Object(map)
            }
            EngineKind::MySql => Value::String(bytes_to_hex(&b)),
        },
        SqlValue::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| sql_value_to_json(v, engine))
                .collect(),
        ),
        SqlValue::Json(j) => j,
    }
}

/// Replace denied column cells with `"[redacted]"` and record names in meta.
///
/// SQL-SEC-16: this redaction is **name-based and best-effort only**. Matching
/// uses the column names as returned by the database — i.e. the alias when
/// the query supplies one (`SELECT password AS p` evades a `password` entry).
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
    use strut_sql_guard::validate_and_prepare;

    fn class_of(sql: &str, engine: EngineKind, mode: WriteMode) -> StmtClass {
        validate_and_prepare(sql, &[], engine, mode, 100)
            .expect("guard accepts")
            .class
    }

    #[test]
    fn bigdecimal_integer_fits_as_number() {
        let v: bigdecimal::BigDecimal = "184".parse().unwrap();
        assert_eq!(bigdecimal_to_json(&v), Value::from(184));
    }

    #[test]
    fn bigdecimal_decimal_as_float() {
        let v: bigdecimal::BigDecimal = "107744.5".parse().unwrap();
        assert!(bigdecimal_to_json(&v).is_number());
    }

    #[test]
    fn bigdecimal_high_precision_falls_back_to_string() {
        // f64 cannot represent this exactly — precision must not be lost.
        let v: bigdecimal::BigDecimal = "12345678901234567890.123456789".parse().unwrap();
        assert_eq!(
            bigdecimal_to_json(&v),
            Value::String("12345678901234567890.123456789".into())
        );
    }

    #[test]
    fn bigdecimal_exact_binary_friendly_stays_number() {
        let v: bigdecimal::BigDecimal = "0.1".parse().unwrap();
        assert!(bigdecimal_to_json(&v).is_number());
        let v: bigdecimal::BigDecimal = "9007199254740993".parse().unwrap(); // 2^53 + 1
        assert!(
            bigdecimal_to_json(&v).is_string(),
            "unsafe integer → string"
        );
    }

    #[test]
    fn binary_cells_are_tagged_hex_for_postgres_and_sqlite() {
        let v = sql_value_to_json(SqlValue::Binary(b"edoc".to_vec()), EngineKind::Sqlite);
        assert_eq!(v, serde_json::json!({"$blob_hex": "65646f63"}));
        let v = sql_value_to_json(SqlValue::Binary(b"edoc".to_vec()), EngineKind::Postgres);
        assert_eq!(v, serde_json::json!({"$blob_hex": "65646f63"}));
        let v = sql_value_to_json(SqlValue::Binary(b"edoc".to_vec()), EngineKind::MySql);
        assert_eq!(v, Value::String("65646f63".into()));
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
            EngineKind::MySql,
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
