use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use serde::Serialize;
use serde_json::Value;
use strut_sql_driver_api::{BoundQuery, DriverError, EngineDriver, EngineKind, SqlValue};
use strut_sql_format::{truncate_to_bytes, ColumnarMeta, ColumnarResult};
use strut_sql_guard::{
    validate_and_prepare_keyset, validate_and_prepare_with_options, GuardError, KeysetRequest,
    MaskMode, PrepareOptions, SqlClass, StmtClass, TablePolicy, WriteMode,
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
    #[error("{0}")]
    Limit(String),
    /// Cursor pagination failure with a structured code (R2-01).
    #[error(transparent)]
    Cursor(#[from] crate::keyset::KeysetError),
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
    /// Column masks from the caller's grant (R2-04).
    pub masks: Vec<(String, MaskMode)>,
    /// Table allow/deny + row filters from the caller's grant (R2-04).
    pub table_policy: Option<TablePolicy>,
    /// Hashed principal id for trace events (R2-07); never the raw id.
    pub principal: String,
    /// Source name for trace events.
    pub source: String,
    /// Include the prepared SQL text in the trace event (opt-in).
    pub trace_sql: bool,
    /// Per-principal concurrency limiter (R2-08); `None` disables it.
    pub limiter: Option<Arc<strut_sql_core::limits::PrincipalLimiter>>,
}

/// Hard governance limits for one `execute_batch` call (PRD-F2-10).
#[derive(Debug, Clone, Copy)]
pub struct BatchLimits {
    pub max_statements: usize,
    /// Total wall-clock budget for the whole batch.
    pub deadline: Duration,
    /// Total serialized result-byte ceiling across all items.
    pub max_bytes: usize,
}

pub async fn execute_query(
    driver: &Arc<dyn EngineDriver>,
    sql: &str,
    params: &[Value],
    opts: &ExecOptions,
) -> Result<QueryResult, ExecError> {
    let engine = driver.kind();
    // R2-08: one principal cannot hold more than its share of the pool.
    let _permit = acquire_principal_slot(opts)?;
    let prepared = validate_and_prepare_with_options(
        sql,
        params,
        engine,
        opts.write_mode,
        opts.max_rows,
        PrepareOptions {
            page_offset: opts.page_offset,
            page_size: opts.page_size,
            table_policy: opts.table_policy.clone(),
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
        return Err(ExecError::Guard(GuardError::Denied(
            "EXPLAIN ANALYZE executes the query and requires --allow-writes".into(),
        )));
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
            let columnar = apply_masks(columnar, &opts.masks);
            // OTel-shaped attributes on one event (R2-07). Cardinality is
            // bounded: no parameter values, the principal is hashed, and the
            // statement text appears only when the operator opted in and the
            // grant does not narrow tables (scoped_for clears trace_sql then).
            tracing::info!(
                "otel.name" = "execute_sql",
                "db.system" = ?engine,
                "db.operation" = "execute",
                "strut.source" = %opts.source,
                "strut.principal" = %opts.principal,
                "strut.rows" = columnar.meta.n,
                "strut.duration_ms" = started.elapsed().as_millis() as u64,
                "db.statement" = if opts.trace_sql { Some(prepared.sql.as_str()) } else { None },
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

/// Result of one keyset (cursor) page.
#[derive(Debug)]
pub struct KeysetPage {
    pub result: QueryResult,
    /// True when the probe row proved another page exists.
    pub has_more: bool,
    /// Value of the ordering key in the last returned row (the next cursor).
    pub next_key: Option<Value>,
    pub descending: bool,
    pub page_size: usize,
}

/// Execute one cursor page. `after = None` is the first page.
///
/// The guard rewrites the statement to `WHERE … AND key > ? ORDER BY key
/// LIMIT page_size+1`; the probe row is dropped. Because the predicate walks
/// the key instead of an offset, a row inserted or deleted between pages
/// cannot duplicate or skip another row.
pub async fn execute_keyset_page(
    driver: &Arc<dyn EngineDriver>,
    sql: &str,
    params: &[Value],
    opts: &ExecOptions,
    after: Option<Value>,
    page_size: u32,
) -> Result<KeysetPage, ExecError> {
    let engine = driver.kind();
    let _permit = acquire_principal_slot(opts)?;
    let (prepared, shape) = validate_and_prepare_keyset(
        sql,
        params,
        engine,
        opts.write_mode,
        &KeysetRequest {
            after,
            page_size,
            table_policy: opts.table_policy.clone(),
        },
    )?;
    let columnar = run_sql(driver, &prepared.sql, &prepared.params, engine, true, None).await?;
    let key_index = crate::keyset::order_key_index(&columnar.cols, &shape.order_column)?;
    let fetched = columnar.rows.len();
    let page = page_size as usize;
    let has_more = fetched > page;
    let mut columnar = columnar;
    if has_more {
        columnar.rows.truncate(page);
    }
    let next_key = if has_more {
        match columnar
            .rows
            .last()
            .and_then(|row| row.get(key_index))
            .cloned()
        {
            Some(Value::Null) | None => {
                return Err(crate::keyset::KeysetError::NullKey(
                    "cursor key is NULL or missing; order by a NOT NULL unique column".into(),
                )
                .into())
            }
            Some(value) => Some(value),
        }
    } else {
        None
    };
    columnar.meta.n = columnar.rows.len();
    columnar.meta.page_size = Some(page);
    columnar.meta.has_more = Some(has_more);
    columnar.meta.total_fetched = Some(fetched);
    columnar.meta.page_offset = None;
    let columnar = truncate_to_bytes(columnar, opts.max_bytes);
    let columnar = redact_denied_columns(columnar, &opts.deny_columns);
    let columnar = apply_masks(columnar, &opts.masks);
    Ok(KeysetPage {
        result: QueryResult {
            ok: true,
            data: Some(columnar),
            error: None,
        },
        has_more,
        next_key,
        descending: shape.descending,
        page_size: page,
    })
}

fn acquire_principal_slot(
    opts: &ExecOptions,
) -> Result<Option<tokio::sync::OwnedSemaphorePermit>, ExecError> {
    let Some(limiter) = &opts.limiter else {
        return Ok(None);
    };
    match limiter.try_acquire(&opts.principal) {
        Some(permit) => Ok(Some(permit)),
        None if limiter.per_principal() == 0 => Ok(None),
        None => Err(ExecError::Other(
            "this principal already has its maximum concurrent queries; retry shortly".into(),
        )),
    }
}

pub async fn execute_batch(
    driver: &Arc<dyn EngineDriver>,
    queries: Vec<(String, Vec<Value>)>,
    opts: &ExecOptions,
    concurrency: usize,
    fail_fast: bool,
    limits: &BatchLimits,
) -> Result<BatchResult, ExecError> {
    // PRD-F2-10: reject oversized batches before any statement runs.
    if queries.len() > limits.max_statements {
        return Err(ExecError::Limit(format!(
            "batch has {} statements; limit is {} (--batch-max-statements)",
            queries.len(),
            limits.max_statements
        )));
    }

    // Total batch deadline. Each item observes the same absolute deadline, so
    // in-flight work cannot extend the batch beyond it. Items that do not
    // finish produce a structured per-item failure (fail-soft), unless
    // fail_fast collapses the batch.
    let deadline = tokio::time::Instant::now() + limits.deadline;
    let results = stream::iter(queries)
        .map(|(sql, params)| {
            let driver = driver.clone();
            let opts = opts.clone();
            async move {
                match tokio::time::timeout_at(
                    deadline,
                    execute_query(&driver, &sql, &params, &opts),
                )
                .await
                {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => QueryResult {
                        ok: false,
                        data: None,
                        error: Some(e.to_string()),
                    },
                    Err(_) => QueryResult {
                        ok: false,
                        data: None,
                        error: Some("batch deadline exceeded before this item completed".into()),
                    },
                }
            }
        })
        .buffered(concurrency)
        .collect::<Vec<_>>()
        .await;

    if fail_fast {
        if let Some(failed) = results.iter().find(|r| !r.ok) {
            return Ok(BatchResult {
                results: vec![failed.clone()],
            });
        }
    }

    // Total byte ceiling across items. Results arrive in request order, so the
    // first N items get the budget and later items are truncated to the
    // remainder or fail with a structured per-item error.
    const ITEM_OVERHEAD: usize = 96;
    let mut used = 0usize;
    let mut bounded = results;
    for result in &mut bounded {
        let data_len = result
            .data
            .as_ref()
            .and_then(|d| serde_json::to_string(d).ok())
            .map(|s| s.len())
            .unwrap_or(0);
        let need = data_len.saturating_add(ITEM_OVERHEAD);
        let remaining = limits.max_bytes.saturating_sub(used);
        if need <= remaining {
            used += need;
            continue;
        }
        let Some(data) = result.data.take() else {
            // Error items have no payload; count only the wrapper.
            if ITEM_OVERHEAD <= remaining {
                used += ITEM_OVERHEAD;
            }
            continue;
        };
        let avail = remaining.saturating_sub(ITEM_OVERHEAD);
        if avail < 128 {
            result.ok = false;
            result.error = Some(format!(
                "batch byte ceiling ({} bytes) reached before this item",
                limits.max_bytes
            ));
            used = limits.max_bytes;
            continue;
        }
        let mut truncated = truncate_to_bytes(data, avail);
        truncated.meta.truncated = true;
        used += serde_json::to_string(&truncated)
            .map(|s| s.len())
            .unwrap_or(avail)
            .saturating_add(ITEM_OVERHEAD);
        result.data = Some(truncated);
    }

    Ok(BatchResult { results: bounded })
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
    let warnings: Option<Vec<String>> = {
        let notes = driver.capability_notes();
        if notes.is_empty() {
            None
        } else {
            let messages: Vec<String> = notes
                .into_iter()
                .map(|n| format!("{}: {}", n.code, n.message))
                .collect();
            tracing::warn!(engine = ?driver.kind(), warnings = ?messages, "driver capability degraded");
            Some(messages)
        }
    };

    if !fetch_rows {
        let mut result = ColumnarResult::empty_command(result.meta.rows_affected.unwrap_or(0));
        result.meta.warnings = warnings;
        return Ok(result);
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
            next_cursor: None,
            warnings,
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
            EngineKind::Postgres | EngineKind::Sqlite | EngineKind::DuckDb => {
                let mut map = serde_json::Map::new();
                map.insert("$blob_hex".into(), Value::String(bytes_to_hex(&b)));
                Value::Object(map)
            }
            EngineKind::MySql | EngineKind::ClickHouse => Value::String(bytes_to_hex(&b)),
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
/// Apply per-column masks from the grant (R2-04): `partial` keeps a
/// recognisable prefix, `hash` replaces the value with a SHA-256 digest, and
/// `null` replaces it with JSON null. Masked column names are reported in
/// `meta.redacted_columns`.
pub fn apply_masks(mut columnar: ColumnarResult, masks: &[(String, MaskMode)]) -> ColumnarResult {
    if masks.is_empty() {
        return columnar;
    }
    let mut masked: Vec<String> = Vec::new();
    for (index, column) in columnar.cols.iter().enumerate() {
        let Some(mode) = masks
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(column))
            .map(|(_, mode)| *mode)
        else {
            continue;
        };
        masked.push(column.clone());
        for row in &mut columnar.rows {
            if let Some(cell) = row.get_mut(index) {
                *cell = mask_value(cell, mode);
            }
        }
    }
    if !masked.is_empty() {
        masked.sort();
        columnar
            .meta
            .redacted_columns
            .get_or_insert_with(Vec::new)
            .extend(masked);
    }
    columnar
}

fn mask_value(value: &Value, mode: MaskMode) -> Value {
    match mode {
        MaskMode::Null => Value::Null,
        MaskMode::Hash => match value {
            Value::Null => Value::Null,
            other => {
                use sha2::{Digest, Sha256};
                let raw = match other {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                Value::String(format!("{:x}", Sha256::digest(raw.as_bytes())))
            }
        },
        MaskMode::Partial => match value {
            Value::Null => Value::Null,
            Value::String(s) if s.contains('@') => {
                let (user, domain) = s.split_once('@').unwrap_or((s, ""));
                let first = user
                    .chars()
                    .next()
                    .map(|c| c.to_string())
                    .unwrap_or_default();
                Value::String(format!("{first}***@{domain}"))
            }
            Value::String(s) => {
                let kept: String = s.chars().take(2).collect();
                Value::String(format!("{kept}***"))
            }
            _ => Value::String("***".to_string()),
        },
    }
}

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
    fn binary_cells_are_tagged_hex_for_postgres_sqlite_and_duckdb() {
        let v = sql_value_to_json(SqlValue::Binary(b"edoc".to_vec()), EngineKind::Sqlite);
        assert_eq!(v, serde_json::json!({"$blob_hex": "65646f63"}));
        let v = sql_value_to_json(SqlValue::Binary(b"edoc".to_vec()), EngineKind::Postgres);
        assert_eq!(v, serde_json::json!({"$blob_hex": "65646f63"}));
        let v = sql_value_to_json(SqlValue::Binary(b"edoc".to_vec()), EngineKind::DuckDb);
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

    async fn sqlite() -> Arc<dyn EngineDriver> {
        use strut_sql_driver_api::ConnectOptions;
        use strut_sql_driver_sqlite::SqliteDriver;
        let driver = SqliteDriver::from_connection_string_with_options(
            "sqlite::memory:",
            &ConnectOptions {
                pool_size: 2,
                ..ConnectOptions::default()
            },
        )
        .await
        .expect("sqlite driver");
        Arc::new(*driver)
    }

    fn exec_opts() -> ExecOptions {
        ExecOptions {
            write_mode: WriteMode::ReadOnly,
            max_rows: 100,
            max_bytes: 64 * 1024,
            timeout: Duration::from_secs(5),
            limit_injected: false,
            page_offset: 0,
            page_size: None,
            deny_columns: Vec::new(),
            masks: vec![],
            table_policy: None,
            principal: String::new(),
            source: String::new(),
            trace_sql: false,
            limiter: None,
        }
    }

    fn limits(max_statements: usize, deadline_ms: u64, max_bytes: usize) -> BatchLimits {
        BatchLimits {
            max_statements,
            deadline: Duration::from_millis(deadline_ms),
            max_bytes,
        }
    }

    #[tokio::test]
    async fn batch_rejects_more_statements_than_limit() {
        let driver = sqlite().await;
        let queries = (0..3)
            .map(|i| (format!("SELECT {i}"), Vec::new()))
            .collect();
        let err = execute_batch(
            &driver,
            queries,
            &exec_opts(),
            2,
            false,
            &limits(2, 5_000, 1024 * 1024),
        )
        .await
        .expect_err("oversized batch must be rejected");
        assert!(matches!(err, ExecError::Limit(_)), "{err}");
        assert!(err.to_string().contains("limit is 2"), "{err}");
    }

    #[tokio::test]
    async fn batch_enforces_total_byte_ceiling_across_items() {
        let driver = sqlite().await;
        let queries = vec![
            ("SELECT hex(zeroblob(800))".to_string(), Vec::new()),
            ("SELECT hex(zeroblob(800))".to_string(), Vec::new()),
            ("SELECT hex(zeroblob(800))".to_string(), Vec::new()),
        ];
        let ceiling = 1_500usize;
        let batch = execute_batch(
            &driver,
            queries,
            &exec_opts(),
            2,
            false,
            &limits(10, 5_000, ceiling),
        )
        .await
        .expect("batch");

        let actual = serde_json::to_string(&batch).unwrap().len();
        assert!(
            actual <= ceiling,
            "serialized batch {actual} must fit ceiling {ceiling}"
        );
        let mut used = 0usize;
        for result in &batch.results {
            used += 96;
            if let Some(data) = &result.data {
                used += serde_json::to_string(data).unwrap().len();
            }
        }
        assert!(
            used <= ceiling,
            "batch payload {used} must fit ceiling {ceiling}"
        );
        assert!(
            batch.results.iter().any(|r| !r.ok || r.data.is_none()),
            "items past the ceiling must fail or be omitted"
        );
    }

    #[tokio::test]
    async fn batch_deadline_fails_soft_per_item() {
        let driver = sqlite().await;
        let queries = vec![(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) \
             SELECT count(*) FROM c"
                .to_string(),
            Vec::new(),
        )];
        let batch = execute_batch(
            &driver,
            queries,
            &exec_opts(),
            1,
            false,
            &limits(10, 40, 1024 * 1024),
        )
        .await
        .expect("batch");
        assert_eq!(batch.results.len(), 1);
        assert!(!batch.results[0].ok);
        assert!(
            batch.results[0]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("batch deadline"),
            "{:?}",
            batch.results[0]
        );
    }

    #[tokio::test]
    async fn batch_is_fail_soft_without_fail_fast() {
        let driver = sqlite().await;
        let queries = vec![
            ("SELECT 1".to_string(), Vec::new()),
            ("SELECT * FROM missing_table".to_string(), Vec::new()),
            ("SELECT 3".to_string(), Vec::new()),
        ];
        let batch = execute_batch(
            &driver,
            queries,
            &exec_opts(),
            2,
            false,
            &limits(10, 5_000, 1024 * 1024),
        )
        .await
        .expect("batch");
        assert_eq!(batch.results.len(), 3);
        assert!(batch.results[0].ok);
        assert!(!batch.results[1].ok);
        assert!(batch.results[2].ok);
    }
}
