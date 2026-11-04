//! A single owned DuckDB connection, serialized off the async executor.
//!
//! `row_cap` bounds neutral result decoding/retention, not DuckDB's native
//! result: duckdb-rs `Statement::execute` materializes the query before rows
//! are visited. The engine interrupt deadline still covers materialization.
//! Pool size is deliberately ignored: an in-memory database and session state
//! must survive between calls. No connection cloning or replacement occurs.
//! Neutral TIME results use canonical time text; timezone-aware temporal,
//! interval, struct/map/union, and nested UHUGEINT results are rejected rather
//! than silently coerced. UUID/JSON parameters require explicit SQL casts from
//! supported scalar parameters. Read-only in-memory sessions require the
//! caller's SQL guard, reported through `capability_notes`.

use std::str::FromStr;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDate, NaiveTime};
use duckdb::core::{LogicalTypeHandle, LogicalTypeId};
use duckdb::types::{Decimal, TimeUnit, Value};
use duckdb::{AccessMode, Config, Connection, InterruptHandle};
use strut_sql_driver_api::{
    BoundQuery, CapabilityNote, ColumnInfo, ColumnMeta, ConnectOptions, DriverError, EngineDriver,
    EngineKind, ForeignKeyInfo, IndexInfo, ObjectType, QueryMeta, QueryResult, RoutineInfo,
    SchemaObject, SqlValue, TableInfo, TableRowCount,
};

pub struct DuckDbDriver {
    connection: Arc<tokio::sync::Mutex<Connection>>,
    acquire_timeout: Duration,
    statement_timeout: Duration,
    memory_read_only: bool,
}

impl DuckDbDriver {
    pub async fn new(connection_string: &str) -> Result<Self, DriverError> {
        Self::new_with_options(connection_string, &ConnectOptions::default()).await
    }

    pub async fn new_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Self, DriverError> {
        let path = connection_path(connection_string)?.to_owned();
        // DuckDB cannot open :memory: with AccessMode::ReadOnly. Keep this
        // useful default URL available, but never claim native read-only:
        // the capability note requires the caller's normal SQL guard.
        let memory_read_only = options.read_only && path == ":memory:";
        let read_only = options.read_only && !memory_read_only;
        let open = tokio::task::spawn_blocking(move || {
            let mut config = Config::default()
                .enable_external_access(false)
                .and_then(|c| c.enable_autoload_extension(false))
                .map_err(connection_error)?;
            if read_only {
                config = config
                    .access_mode(AccessMode::ReadOnly)
                    .map_err(connection_error)?;
            }
            Connection::open_with_flags(path, config).map_err(connection_error)
        });
        let connection = tokio::time::timeout(options.connect_timeout, open)
            .await
            .map_err(|_| DriverError::Timeout)?
            .map_err(|e| DriverError::Connection(e.to_string()))??;
        Ok(Self {
            connection: Arc::new(tokio::sync::Mutex::new(connection)),
            acquire_timeout: options.connect_timeout,
            statement_timeout: options.statement_timeout,
            memory_read_only,
        })
    }

    async fn catalog(
        &self,
        sql: &str,
        params: Vec<SqlValue>,
    ) -> Result<Vec<Vec<SqlValue>>, DriverError> {
        self.execute(&BoundQuery {
            sql: sql.to_owned(),
            params,
            row_cap: None,
            fetch_rows: true,
        })
        .await
        .map(|r| r.rows)
        .map_err(|e| match e {
            DriverError::Execution(message) => DriverError::Introspection(message),
            other => other,
        })
    }
}

fn connection_path(url: &str) -> Result<&str, DriverError> {
    let path = url
        .strip_prefix("duckdb://")
        .or_else(|| url.strip_prefix("duckdb:"))
        .ok_or_else(|| DriverError::InvalidConfig("expected a duckdb: connection URL".into()))?;
    if path.is_empty() || path.contains(['?', '#', '\0']) {
        return Err(DriverError::InvalidConfig(
            "DuckDB URL needs a path or :memory:; URL query options and fragments are not supported".into(),
        ));
    }
    Ok(path)
}

fn connection_error(error: duckdb::Error) -> DriverError {
    DriverError::Connection(error.to_string())
}

fn execution_error(error: duckdb::Error) -> DriverError {
    DriverError::Execution(error.to_string())
}

#[derive(Default)]
struct QueryState {
    finished: bool,
    cancelled: bool,
    timed_out: bool,
}

type QueryControl = Arc<(Mutex<QueryState>, Condvar)>;

// Drop of the async execute future wakes the independent watchdog. Unlike an
// async timer, this survives runtime shutdown while spawn_blocking is draining.
struct CancelOnDrop(QueryControl);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        let (lock, wake) = &*self.0;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if !state.finished {
            state.cancelled = true;
            wake.notify_all();
        }
    }
}

struct FinishQuery(QueryControl);

impl Drop for FinishQuery {
    fn drop(&mut self) {
        let (lock, wake) = &*self.0;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        state.finished = true;
        wake.notify_all();
    }
}

fn watchdog(control: QueryControl, interrupt: Arc<InterruptHandle>, timeout: Duration) {
    let deadline = Instant::now().checked_add(timeout);
    let (lock, wake) = &*control;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    while !state.finished {
        let remaining = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|d| d.is_zero()) {
            state.timed_out = true;
        }
        let delay = if state.cancelled || state.timed_out {
            // Repeat until completion: cancellation may race the transition
            // from prepare/bind to native execution, which resets interrupts.
            // This mutex also gates completion, preventing stale interrupts
            // from leaking into the next serialized statement.
            interrupt.interrupt();
            Duration::from_millis(5)
        } else {
            remaining.unwrap_or(Duration::from_secs(3600))
        };
        let (next, _) = wake
            .wait_timeout(state, delay)
            .unwrap_or_else(|e| e.into_inner());
        state = next;
    }
}

fn execute_blocking(
    connection: &Connection,
    query: &BoundQuery,
) -> Result<QueryResult, DriverError> {
    let params = query
        .params
        .iter()
        .map(bind_value)
        .collect::<Result<Vec<_>, _>>()?;
    let mut statement = connection.prepare(&query.sql).map_err(execution_error)?;
    // DuckDB resolves result types only after binding and execution: before that
    // it reports a single `Invalid` column for parameterized statements. Types
    // are therefore validated on the executed result, before any row is decoded.
    let affected = statement
        .execute(duckdb::params_from_iter(params.iter()))
        .map_err(execution_error)?;
    if !query.fetch_rows {
        return Ok(QueryResult {
            cols: Vec::new(),
            rows: Vec::new(),
            meta: QueryMeta {
                row_count: 0,
                truncated: false,
                rows_affected: Some(affected as u64),
            },
        });
    }
    let schema = statement.schema();
    let types = (0..statement.column_count())
        .map(|i| statement.column_logical_type(i))
        .collect::<Vec<_>>();
    let cols = schema
        .fields()
        .iter()
        .zip(&types)
        .map(|(field, logical)| {
            Ok(ColumnMeta {
                name: field.name().clone(),
                type_name: type_name(logical, field.data_type())?,
                nullable: field.is_nullable(),
            })
        })
        .collect::<Result<Vec<_>, DriverError>>()?;
    let mut rows = Vec::new();
    let mut truncated = false;
    let mut cursor = statement.raw_query();
    while let Some(row) = cursor.next().map_err(execution_error)? {
        if query.row_cap.is_some_and(|cap| rows.len() >= cap) {
            truncated = true;
            break;
        }
        let mut values = Vec::with_capacity(types.len());
        for (i, logical) in types.iter().enumerate() {
            // duckdb-rs panics on Arrow `Null` columns (`SELECT NULL`), and
            // such a column can only ever hold NULL.
            if logical.id() == LogicalTypeId::SqlNull {
                values.push(SqlValue::Null);
            } else {
                values.push(decode_value(
                    row.get::<_, Value>(i).map_err(execution_error)?,
                    logical,
                )?);
            }
        }
        rows.push(values);
    }
    Ok(QueryResult {
        cols,
        meta: QueryMeta {
            row_count: rows.len(),
            truncated,
            rows_affected: None,
        },
        rows,
    })
}

fn unsupported(message: impl Into<String>) -> DriverError {
    DriverError::Unsupported(message.into())
}

fn bind_value(value: &SqlValue) -> Result<Value, DriverError> {
    Ok(match value {
        SqlValue::Null => Value::Null,
        SqlValue::Bool(v) => Value::Boolean(*v),
        SqlValue::I64(v) => Value::BigInt(*v),
        SqlValue::F64(v) if v.is_finite() => Value::Double(*v),
        SqlValue::F64(_) => return Err(unsupported("non-finite floating-point parameters have no JSON-safe neutral representation")),
        SqlValue::String(v) => Value::Text(v.clone()),
        SqlValue::Binary(v) => Value::Blob(v.clone()),
        SqlValue::Date(v) => {
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch");
            let days = i32::try_from(v.signed_duration_since(epoch).num_days())
                .map_err(|_| unsupported("date exceeds DuckDB DATE range"))?;
            Value::Date32(days)
        }
        SqlValue::DateTime(v) => {
            if v.and_utc().timestamp_subsec_nanos() % 1_000 != 0 {
                return Err(unsupported("DuckDB timestamp parameter binding supports microseconds, not sub-microsecond precision"));
            }
            Value::Timestamp(TimeUnit::Microsecond, v.and_utc().timestamp_micros())
        }
        SqlValue::BigDecimal(v) => {
            let (coefficient, scale) = v.normalized().as_bigint_and_exponent();
            let mut digits = coefficient.to_string();
            let scale = if scale < 0 {
                let zeroes = usize::try_from(scale.unsigned_abs()).map_err(|_| unsupported("decimal exceeds DuckDB precision 38"))?;
                if digits.trim_start_matches('-').len().saturating_add(zeroes) > 38 {
                    return Err(unsupported("decimal exceeds DuckDB precision 38"));
                }
                digits.extend(std::iter::repeat_n('0', zeroes));
                0
            } else { scale };
            let scale = u8::try_from(scale).map_err(|_| unsupported("decimal exceeds DuckDB scale 38"))?;
            let width = digits.trim_start_matches('-').len().max(usize::from(scale)).max(1);
            let width = u8::try_from(width).map_err(|_| unsupported("decimal exceeds DuckDB precision 38"))?;
            let coefficient = i128::from_str(&digits).map_err(|_| unsupported("decimal exceeds DuckDB precision 38"))?;
            Value::Decimal(Decimal::new(width, scale, coefficient).map_err(|e| unsupported(e.to_string()))?)
        }
        SqlValue::Array(values) => {
            let values = values.iter().map(bind_value).collect::<Result<Vec<_>, _>>()?;
            let element = values.iter().find(|v| !matches!(v, Value::Null))
                .ok_or_else(|| unsupported("empty/all-NULL array parameters lack a safe native element type; use an explicit SQL list expression"))?;
            for value in values.iter().filter(|v| !matches!(v, Value::Null)) {
                let same_type = std::mem::discriminant(element) == std::mem::discriminant(value);
                let exact_decimal = match (element, value) {
                    (Value::Decimal(a), Value::Decimal(b)) => a.width() == b.width() && a.scale() == b.scale(),
                    _ => true,
                };
                if !same_type || !exact_decimal || matches!(value, Value::List(_) | Value::Array(_)) {
                    return Err(unsupported("heterogeneous, differently-scaled decimal, and nested array parameters require explicit SQL typing to avoid implicit lossy casts"));
                }
            }
            Value::List(values)
        }
        SqlValue::Uuid(_) => return Err(unsupported("typed UUID parameter binding is not available in duckdb-rs; use a string parameter with an explicit SQL UUID cast")),
        SqlValue::Json(_) => return Err(unsupported("typed JSON parameters require an extension; external extensions are disabled")),
    })
}

fn validate_type(logical: &LogicalTypeHandle, nested: bool) -> Result<(), DriverError> {
    use LogicalTypeId as T;
    match logical.id() {
        T::Boolean
        | T::Tinyint
        | T::Smallint
        | T::Integer
        | T::Bigint
        | T::UTinyint
        | T::USmallint
        | T::UInteger
        | T::UBigint
        | T::Float
        | T::Double
        | T::Timestamp
        | T::TimestampS
        | T::TimestampMs
        | T::TimestampNs
        | T::Date
        | T::Time
        | T::TimeNs
        | T::Hugeint
        | T::Varchar
        | T::Blob
        | T::Decimal
        | T::Enum
        | T::Uuid => Ok(()),
        // An untyped NULL column is valid as a whole column, but duckdb-rs
        // cannot decode Arrow `Null` elements inside a list.
        T::SqlNull if !nested => Ok(()),
        T::UHugeint if !nested => Ok(()),
        T::List | T::Array => validate_type(&logical.child(0), true),
        other => Err(unsupported(format!(
            "DuckDB {other:?} results cannot be represented losslessly by this driver"
        ))),
    }
}

fn type_name(
    logical: &LogicalTypeHandle,
    arrow: &duckdb::arrow::datatypes::DataType,
) -> Result<String, DriverError> {
    use LogicalTypeId as T;
    validate_type(logical, false)?;
    Ok(match logical.id() {
        T::Boolean => "BOOLEAN".into(),
        T::Tinyint => "TINYINT".into(),
        T::Smallint => "SMALLINT".into(),
        T::Integer => "INTEGER".into(),
        T::Bigint => "BIGINT".into(),
        T::UTinyint => "UTINYINT".into(),
        T::USmallint => "USMALLINT".into(),
        T::UInteger => "UINTEGER".into(),
        T::UBigint => "UBIGINT".into(),
        T::Hugeint => "HUGEINT".into(),
        T::UHugeint => "UHUGEINT".into(),
        T::Float => "FLOAT".into(),
        T::Double => "DOUBLE".into(),
        T::Varchar => "VARCHAR".into(),
        T::Blob => "BLOB".into(),
        T::Uuid => "UUID".into(),
        T::Date => "DATE".into(),
        T::Time => "TIME".into(),
        T::TimeNs => "TIME_NS".into(),
        T::Timestamp => "TIMESTAMP".into(),
        T::TimestampS => "TIMESTAMP_S".into(),
        T::TimestampMs => "TIMESTAMP_MS".into(),
        T::TimestampNs => "TIMESTAMP_NS".into(),
        T::Decimal => format!(
            "DECIMAL({},{})",
            logical.decimal_width(),
            logical.decimal_scale()
        ),
        T::List => match arrow {
            duckdb::arrow::datatypes::DataType::List(child) => {
                format!("{}[]", type_name(&logical.child(0), child.data_type())?)
            }
            _ => {
                return Err(unsupported(
                    "unexpected DuckDB list physical representation",
                ))
            }
        },
        T::Array => match arrow {
            duckdb::arrow::datatypes::DataType::FixedSizeList(child, size) => format!(
                "{}[{size}]",
                type_name(&logical.child(0), child.data_type())?
            ),
            _ => {
                return Err(unsupported(
                    "unexpected DuckDB array physical representation",
                ))
            }
        },
        T::Enum => "ENUM".into(),
        T::SqlNull => "NULL".into(),
        _ => return Err(unsupported("unsupported DuckDB result type")),
    })
}

fn decode_value(value: Value, logical: &LogicalTypeHandle) -> Result<SqlValue, DriverError> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Boolean(v) => SqlValue::Bool(v),
        Value::TinyInt(v) => SqlValue::I64(v.into()),
        Value::SmallInt(v) => SqlValue::I64(v.into()),
        Value::Int(v) => SqlValue::I64(v.into()),
        Value::BigInt(v) => SqlValue::I64(v),
        Value::UTinyInt(v) => SqlValue::I64(v.into()),
        Value::USmallInt(v) => SqlValue::I64(v.into()),
        Value::UInt(v) => SqlValue::I64(v.into()),
        Value::HugeInt(v) => SqlValue::BigDecimal(BigDecimal::from(v)),
        Value::UHugeInt(v) => SqlValue::BigDecimal(
            BigDecimal::from_str(&v.to_string()).map_err(|e| unsupported(e.to_string()))?,
        ),
        Value::UBigInt(v) => match i64::try_from(v) {
            Ok(v) => SqlValue::I64(v),
            Err(_) => SqlValue::BigDecimal(BigDecimal::from(v)),
        },
        Value::Float(v) if v.is_finite() => SqlValue::F64(f64::from(v)),
        Value::Double(v) if v.is_finite() => SqlValue::F64(v),
        Value::Float(_) | Value::Double(_) => {
            return Err(unsupported(
                "non-finite floating-point result has no JSON-safe neutral representation",
            ))
        }
        Value::Decimal(v) => {
            SqlValue::BigDecimal(BigDecimal::new(v.value().into(), i64::from(v.scale())))
        }
        Value::Text(v) if logical.id() == LogicalTypeId::Uuid => {
            SqlValue::Uuid(uuid::Uuid::parse_str(&v).map_err(|e| unsupported(e.to_string()))?)
        }
        Value::Text(v) | Value::Enum(v) => SqlValue::String(v),
        Value::Blob(v) if logical.id() == LogicalTypeId::Uuid => {
            SqlValue::Uuid(uuid::Uuid::from_slice(&v).map_err(|e| unsupported(e.to_string()))?)
        }
        Value::Blob(v) => SqlValue::Binary(v),
        Value::Date32(v) => {
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch");
            SqlValue::Date(
                epoch
                    .checked_add_signed(chrono::Duration::days(i64::from(v)))
                    .ok_or_else(|| {
                        unsupported("DuckDB DATE is outside the neutral chrono date range")
                    })?,
            )
        }
        Value::Timestamp(unit, v) => {
            let divisor = units_per_second(unit);
            let seconds = v.div_euclid(divisor);
            let nanos = (v.rem_euclid(divisor) * (1_000_000_000 / divisor)) as u32;
            SqlValue::DateTime(
                DateTime::from_timestamp(seconds, nanos)
                    .ok_or_else(|| {
                        unsupported(
                            "DuckDB TIMESTAMP is outside the neutral chrono timestamp range",
                        )
                    })?
                    .naive_utc(),
            )
        }
        Value::Time64(unit, v) => {
            let divisor = units_per_second(unit);
            if !(0..86_400 * divisor).contains(&v) {
                return Err(unsupported(
                    "DuckDB TIME value is outside the neutral time range",
                ));
            }
            let time = NaiveTime::from_num_seconds_from_midnight_opt(
                (v / divisor) as u32,
                ((v % divisor) * (1_000_000_000 / divisor)) as u32,
            )
            .ok_or_else(|| unsupported("invalid DuckDB TIME value"))?;
            // SqlValue has no Time variant: canonical time text preserves all digits.
            SqlValue::String(time.to_string())
        }
        Value::List(values) | Value::Array(values) => SqlValue::Array(
            values
                .into_iter()
                .map(|v| decode_value(v, &logical.child(0)))
                .collect::<Result<_, _>>()?,
        ),
        _ => {
            return Err(unsupported(
                "DuckDB result value has no lossless neutral representation",
            ))
        }
    })
}

fn units_per_second(unit: TimeUnit) -> i64 {
    match unit {
        TimeUnit::Second => 1,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Nanosecond => 1_000_000_000,
    }
}

#[allow(clippy::double_must_use)]
#[async_trait]
impl EngineDriver for DuckDbDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::DuckDb
    }

    fn capability_notes(&self) -> Vec<CapabilityNote> {
        let mut notes = vec![
            CapabilityNote {
                code: "duckdb_serial_connection".into(),
                message: "DuckDB uses one persistent serialized connection; pool_size is not used.".into(),
            },
            CapabilityNote {
                code: "duckdb_materialized_results".into(),
                message: "row_cap limits decoded output, not native query materialization; engine interruption enforces statement_timeout.".into(),
            },
            CapabilityNote {
                code: "duckdb_connect_timeout".into(),
                message: "connect_timeout bounds caller connection/acquisition wait; native database opening cannot be interrupted and may finish in the blocking task after timeout.".into(),
            },
        ];
        if self.memory_read_only {
            notes.push(CapabilityNote {
                code: "duckdb_memory_read_only".into(),
                message: "DuckDB in-memory databases have no native read-only access mode; read_only requires the caller's SQL guard. File databases enforce native AccessMode::ReadOnly.".into(),
            });
        }
        notes
    }

    async fn from_connection_string_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Box<Self>, DriverError> {
        Ok(Box::new(
            Self::new_with_options(connection_string, options).await?,
        ))
    }

    async fn execute(&self, query: &BoundQuery) -> Result<QueryResult, DriverError> {
        let connection =
            tokio::time::timeout(self.acquire_timeout, self.connection.clone().lock_owned())
                .await
                .map_err(|_| DriverError::Timeout)?;
        let query = query.clone();
        let timeout = self.statement_timeout;
        let control = Arc::new((Mutex::new(QueryState::default()), Condvar::new()));
        let cancellation = CancelOnDrop(control.clone());
        let result = tokio::task::spawn_blocking(move || {
            let finish = FinishQuery(control.clone());
            let watch_control = control.clone();
            let interrupt = connection.interrupt_handle();
            let watcher = std::thread::Builder::new()
                .name("duckdb-deadline".into())
                .spawn(move || watchdog(watch_control, interrupt, timeout))
                .map_err(|e| {
                    DriverError::Execution(format!("cannot start DuckDB interrupt watchdog: {e}"))
                })?;
            let cancelled = control
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .cancelled;
            let result = if cancelled {
                Err(DriverError::Closed)
            } else {
                execute_blocking(&connection, &query)
            };
            // Mark finished while still holding exclusive connection ownership.
            // FinishQuery also covers unwinding; watchdog never interrupts a
            // connection after this point, even if the next query has started.
            drop(finish);
            let (timed_out, cancelled) = {
                let state = control.0.lock().unwrap_or_else(|e| e.into_inner());
                (state.timed_out, state.cancelled)
            };
            watcher
                .join()
                .map_err(|_| DriverError::Execution("DuckDB interrupt watchdog panicked".into()))?;
            if timed_out {
                Err(DriverError::Timeout)
            } else if cancelled {
                Err(DriverError::Closed)
            } else {
                result
            }
        })
        .await
        .map_err(|e| DriverError::Execution(e.to_string()))?;
        drop(cancellation);
        result
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaObject>, DriverError> {
        // DuckDB flags the default `main` schema as internal, so filtering on
        // `internal` would hide the user's primary schema. System catalogs are
        // the only schemas that must never surface.
        self.catalog(
            "SELECT schema_name, comment FROM duckdb_schemas() \
            WHERE database_name = current_database() \
            AND schema_name NOT IN ('information_schema', 'pg_catalog') \
            ORDER BY schema_name",
            vec![],
        )
        .await?
        .iter()
        .map(|row| {
            Ok(SchemaObject {
                object_type: ObjectType::Schema,
                schema: None,
                name: text(row, 0)?,
                table: None,
                data_type: None,
                nullable: None,
                comment: optional_text(row, 1)?,
            })
        })
        .collect()
    }

    async fn resolve_schema(&self, schema: Option<&str>) -> Result<String, DriverError> {
        if let Some(schema) = schema.filter(|s| !s.is_empty()) {
            return Ok(schema.into());
        }
        let rows = self.catalog("SELECT current_schema()", vec![]).await?;
        text(
            rows.first()
                .ok_or_else(|| catalog_shape("missing current_schema"))?,
            0,
        )
    }

    async fn list_tables(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let schema = schema.unwrap_or("main");
        self.catalog(
            "SELECT table_name, comment FROM duckdb_tables() \
            WHERE database_name = current_database() AND schema_name = ? AND NOT internal \
            ORDER BY table_name",
            vec![SqlValue::String(schema.into())],
        )
        .await?
        .iter()
        .map(|row| {
            Ok(SchemaObject {
                object_type: ObjectType::Table,
                schema: Some(schema.into()),
                name: text(row, 0)?,
                table: None,
                data_type: None,
                nullable: None,
                comment: optional_text(row, 1)?,
            })
        })
        .collect()
    }

    async fn list_columns(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        let schema = schema.unwrap_or("main");
        self.catalog(
            "SELECT table_name, column_name, data_type, is_nullable, comment \
            FROM duckdb_columns() WHERE database_name = current_database() \
            AND schema_name = ? AND NOT internal ORDER BY table_name, column_index",
            vec![SqlValue::String(schema.into())],
        )
        .await?
        .iter()
        .map(|row| {
            Ok(SchemaObject {
                object_type: ObjectType::Column,
                schema: Some(schema.into()),
                table: Some(text(row, 0)?),
                name: text(row, 1)?,
                data_type: Some(text(row, 2)?),
                nullable: Some(boolean(row, 3)?),
                comment: optional_text(row, 4)?,
            })
        })
        .collect()
    }

    async fn describe_table(
        &self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<TableInfo, DriverError> {
        let schema = schema.unwrap_or("main");
        let rows = self
            .catalog(
                "SELECT c.column_name, c.data_type, c.is_nullable, \
            CASE WHEN c.is_generated THEN NULL ELSE c.column_default END, \
            EXISTS (SELECT 1 FROM duckdb_constraints() k \
                WHERE k.database_name = c.database_name AND k.schema_name = c.schema_name \
                AND k.table_name = c.table_name AND k.constraint_type = 'PRIMARY KEY' \
                AND list_contains(k.constraint_column_names, c.column_name)), c.comment \
            FROM duckdb_columns() c WHERE c.database_name = current_database() \
            AND c.schema_name = ? AND c.table_name = ? AND NOT c.internal ORDER BY c.column_index",
                vec![
                    SqlValue::String(schema.into()),
                    SqlValue::String(table.into()),
                ],
            )
            .await?;
        let columns = rows
            .iter()
            .map(|row| {
                Ok(ColumnInfo {
                    name: text(row, 0)?,
                    data_type: text(row, 1)?,
                    nullable: boolean(row, 2)?,
                    default_value: optional_text(row, 3)?,
                    is_primary_key: boolean(row, 4)?,
                    key: None,
                    extra: None,
                    comment: optional_text(row, 5)?,
                })
            })
            .collect::<Result<_, DriverError>>()?;
        Ok(TableInfo {
            schema: Some(schema.into()),
            name: table.into(),
            columns,
        })
    }

    async fn list_indexes(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<IndexInfo>, DriverError> {
        let schema = schema.unwrap_or("main");
        let scope = [
            SqlValue::String(schema.into()),
            table.map_or(SqlValue::Null, |t| SqlValue::String(t.into())),
        ];
        let rows = self
            .catalog(
                "SELECT table_name, constraint_name, constraint_column_names, \
            constraint_type = 'PRIMARY KEY' FROM duckdb_constraints() \
            WHERE database_name = current_database() AND schema_name = ? \
            AND (? IS NULL OR table_name = ?) AND constraint_type IN ('PRIMARY KEY', 'UNIQUE') \
            ORDER BY table_name, constraint_index",
                vec![scope[0].clone(), scope[1].clone(), scope[1].clone()],
            )
            .await?;
        let mut indexes = rows
            .iter()
            .map(|row| {
                Ok(IndexInfo {
                    schema: Some(schema.into()),
                    table: text(row, 0)?,
                    name: text(row, 1)?,
                    columns: text_array(row, 2)?,
                    unique: true,
                    primary: boolean(row, 3)?,
                })
            })
            .collect::<Result<Vec<_>, DriverError>>()?;
        let rows = self.catalog("SELECT table_name, index_name, CAST(expressions AS VARCHAR[]), is_unique, is_primary \
            FROM duckdb_indexes() WHERE database_name = current_database() AND schema_name = ? \
            AND (? IS NULL OR table_name = ?) ORDER BY table_name, index_name",
            vec![scope[0].clone(), scope[1].clone(), scope[1].clone()]).await?;
        for row in &rows {
            let expressions = text_array(row, 2)?;
            let columns = expressions
                .iter()
                .map(|e| index_column(e))
                .collect::<Result<_, _>>()?;
            indexes.push(IndexInfo {
                schema: Some(schema.into()),
                table: text(row, 0)?,
                name: text(row, 1)?,
                columns,
                unique: boolean(row, 3)?,
                primary: boolean(row, 4)?,
            });
        }
        Ok(indexes)
    }

    async fn list_foreign_keys(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        let schema = schema.unwrap_or("main");
        let table = table.map_or(SqlValue::Null, |t| SqlValue::String(t.into()));
        self.catalog(
            "SELECT table_name, constraint_name, constraint_column_names, \
            referenced_table, referenced_column_names FROM duckdb_constraints() \
            WHERE database_name = current_database() AND schema_name = ? \
            AND (? IS NULL OR table_name = ?) AND constraint_type = 'FOREIGN KEY' \
            ORDER BY table_name, constraint_index",
            vec![SqlValue::String(schema.into()), table.clone(), table],
        )
        .await?
        .iter()
        .map(|row| {
            Ok(ForeignKeyInfo {
                schema: Some(schema.into()),
                table: text(row, 0)?,
                name: text(row, 1)?,
                columns: text_array(row, 2)?,
                ref_schema: Some(schema.into()),
                ref_table: text(row, 3)?,
                ref_columns: text_array(row, 4)?,
                // Bundled DuckDB rejects cross-schema FKs and supports only NO ACTION.
                on_delete: Some("NO ACTION".into()),
                on_update: Some("NO ACTION".into()),
            })
        })
        .collect()
    }

    async fn approximate_row_counts(
        &self,
        schema: Option<&str>,
    ) -> Result<Vec<TableRowCount>, DriverError> {
        self.catalog("SELECT table_name, estimated_size FROM duckdb_tables() \
            WHERE database_name = current_database() AND schema_name = ? AND NOT internal ORDER BY table_name",
            vec![SqlValue::String(schema.unwrap_or("main").into())]).await?.iter()
            .map(|row| Ok(TableRowCount { name: text(row, 0)?, approx_rows: optional_integer(row, 1)? })).collect()
    }

    async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError> {
        self.catalog("SELECT schema_name, function_name, function_type, return_type FROM duckdb_functions() \
            WHERE database_name = current_database() AND NOT internal \
            ORDER BY schema_name, function_name, function_type, parameter_types", vec![]).await?.iter()
            .map(|row| Ok(RoutineInfo {
                schema: Some(text(row, 0)?), name: text(row, 1)?, routine_type: text(row, 2)?,
                return_type: optional_text(row, 3)?,
            })).collect()
    }

    async fn search_objects(
        &self,
        keyword: Option<&str>,
        object_type: Option<ObjectType>,
    ) -> Result<Vec<SchemaObject>, DriverError> {
        let mut objects = Vec::new();
        if object_type.is_none() || object_type == Some(ObjectType::Schema) {
            objects.extend(self.list_schemas().await?);
        }
        if object_type.is_none() || object_type == Some(ObjectType::Table) {
            objects.extend(self.list_tables(None).await?);
        }
        if object_type.is_none() || object_type == Some(ObjectType::Column) {
            objects.extend(self.list_columns(None).await?);
        }
        if object_type.is_none() || object_type == Some(ObjectType::Index) {
            objects.extend(self.list_indexes(None, None).await?.into_iter().map(|i| {
                SchemaObject {
                    object_type: ObjectType::Index,
                    schema: i.schema,
                    name: i.name,
                    table: Some(i.table),
                    data_type: None,
                    nullable: None,
                    comment: None,
                }
            }));
        }
        if object_type.is_none() || object_type == Some(ObjectType::ForeignKey) {
            objects.extend(
                self.list_foreign_keys(None, None)
                    .await?
                    .into_iter()
                    .map(|i| SchemaObject {
                        object_type: ObjectType::ForeignKey,
                        schema: i.schema,
                        name: i.name,
                        table: Some(i.table),
                        data_type: None,
                        nullable: None,
                        comment: None,
                    }),
            );
        }
        if object_type.is_none() || object_type == Some(ObjectType::Routine) {
            objects.extend(
                self.list_routines()
                    .await?
                    .into_iter()
                    .map(|r| SchemaObject {
                        object_type: ObjectType::Routine,
                        schema: r.schema,
                        name: r.name,
                        table: None,
                        data_type: r.return_type,
                        nullable: None,
                        comment: None,
                    }),
            );
        }
        if let Some(keyword) = keyword {
            objects.retain(|o| {
                strut_sql_driver_api::schema_object_matches(o, keyword)
                    || o.comment
                        .as_ref()
                        .is_some_and(|c| c.to_lowercase().contains(&keyword.to_lowercase()))
            });
        }
        Ok(objects)
    }

    async fn ping(&self) -> Result<(), DriverError> {
        self.catalog("SELECT 1", vec![]).await.map(|_| ())
    }
}

fn catalog_shape(message: &str) -> DriverError {
    DriverError::Introspection(format!("unexpected DuckDB catalog value: {message}"))
}

fn text(row: &[SqlValue], index: usize) -> Result<String, DriverError> {
    match row.get(index) {
        Some(SqlValue::String(v)) => Ok(v.clone()),
        _ => Err(catalog_shape("expected text")),
    }
}

fn optional_text(row: &[SqlValue], index: usize) -> Result<Option<String>, DriverError> {
    match row.get(index) {
        Some(SqlValue::Null) => Ok(None),
        _ => text(row, index).map(Some),
    }
}

fn boolean(row: &[SqlValue], index: usize) -> Result<bool, DriverError> {
    match row.get(index) {
        Some(SqlValue::Bool(v)) => Ok(*v),
        _ => Err(catalog_shape("expected boolean")),
    }
}

fn optional_integer(row: &[SqlValue], index: usize) -> Result<Option<i64>, DriverError> {
    match row.get(index) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::I64(v)) => Ok(Some(*v)),
        _ => Err(catalog_shape("row estimate exceeds neutral i64 range")),
    }
}

fn text_array(row: &[SqlValue], index: usize) -> Result<Vec<String>, DriverError> {
    match row.get(index) {
        Some(SqlValue::Array(values)) => values
            .iter()
            .map(|v| match v {
                SqlValue::String(v) => Ok(v.clone()),
                _ => Err(catalog_shape("expected text array element")),
            })
            .collect(),
        _ => Err(catalog_shape("expected text array")),
    }
}

// Only exact simple SQL identifiers fit IndexInfo::columns. Expressions are
// deliberately rejected rather than reporting expression text as fake names.
fn index_column(expression: &str) -> Result<String, DriverError> {
    if let Some(rest) = expression.strip_prefix('"') {
        let mut chars = rest.chars().peekable();
        let mut identifier = String::new();
        while let Some(ch) = chars.next() {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    identifier.push('"');
                } else if chars.next().is_none() {
                    return Ok(identifier);
                } else {
                    break;
                }
            } else {
                identifier.push(ch);
            }
        }
    } else if !expression.is_empty()
        && expression
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_alphabetic() || (i > 0 && c.is_ascii_digit()))
    {
        return Ok(expression.into());
    }
    Err(unsupported(
        "DuckDB expression index keys cannot be represented by IndexInfo::columns",
    ))
}
