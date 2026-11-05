//! SQL Server over TDS with bounded, cancellation-safe connection reuse.
//!
//! URLs use `sqlserver://user:password@host[:1433]/database` (`mssql` is an alias).
//! TLS and certificate validation are enabled by default. Disposable development
//! servers may explicitly opt into `trust_server_certificate=true`; `encrypt=false`
//! explicitly disables TLS. Credentials and database names are percent-decoded.
//!
//! Statement deadlines are client-side: cancellation, timeouts, protocol errors,
//! and row-cap early exits drop the connection rather than reusing pending TDS
//! state. This is not a server-side execution deadline or a rollback guarantee.
//! Each new socket establishes `QUOTED_IDENTIFIER ON` and `NOCOUNT OFF` in one
//! trusted warmup batch, so DONE affected-row counts for bound DML do not depend
//! on the login's server user options. A popped idle connection is probed with a
//! drained `SELECT 1`; a dead or killed session is discarded and replaced once,
//! without retrying the caller's statement. Read-only ApplicationIntent is
//! routing only; the shared guard and least-privilege database credentials remain
//! the write-prevention boundary.
//!
//! Query metadata preserves empty results; Tiberius does not expose nullability
//! flags, precision or scale, so result nullability is conservative (`true`).
//! Catalog nullability is exact. Time and offset datetime values decode as strings
//! (the neutral API has no matching types); JSON text stays text. `datetime2`
//! parameters have SQL Server's 100ns precision, money uses Tiberius' f64 decoder,
//! and arrays require explicit JSON conversion. Only the first row result set is
//! returned; subsequent result sets are drained without retaining their rows.
//!
//! Commands without bind parameters are sent as TDS batches: SQL Server only
//! compiles module DDL (`CREATE PROCEDURE`, `VIEW`, `FUNCTION`, `TRIGGER`) as the
//! first statement of a top-level batch, while an RPC `sp_executesql` call is a
//! stored-procedure invocation that rejects it. Batches do not expose DONE row
//! counts through Tiberius, so a batch command's affected-row count is reset with
//! `SET NOCOUNT OFF` before the command and read from `@@ROWCOUNT` afterwards on
//! the same leased session; the count covers the command's last statement.
//! Parameterized commands keep real bind parameters and their per-statement DONE
//! counts.
//!
//! Estimated plans are compiled from a batch with the bind types declared and no
//! bound value interpolated: SQL Server returns no estimated plan for an RPC
//! `sp_executesql` call. The optimizer therefore compiles against unknown
//! parameter values, never the bound ones. Actual plans execute the bound
//! statement through `sp_executesql`, and only the XML plan result set is
//! retained; data rows are discarded. Statements with no actual Showplan output
//! (for example a trivial scalar SELECT) return an error rather than a
//! substituted estimated plan. Plan rowsets arrive as a single `nvarchar` column,
//! so detection validates the Showplan XML root rather than the TDS column type.

mod codec;
mod config;
mod introspect;
mod placeholder;

use async_trait::async_trait;
use futures_util::TryStreamExt;
use std::sync::{Arc, Mutex};
use strut_sql_driver_api::{
    BoundQuery, CapabilityNote, ConnectOptions, DriverError, EngineDriver, EngineKind,
    ForeignKeyInfo, IndexInfo, ObjectType, QueryMeta, QueryResult, RoutineInfo, SchemaObject,
    SqlValue, TableInfo, TableRowCount, TableStats,
};
use tiberius::{Client, Column, ColumnType, Config, Query, QueryItem, QueryStream};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type Connection = Client<Compat<TcpStream>>;

pub struct SqlServerDriver {
    config: Config,
    options: ConnectOptions,
    permits: Arc<Semaphore>,
    idle: Mutex<Vec<Connection>>,
}

// A lease returns its client only on an explicit, completely drained success.
// Dropping a request future at any await point drops both socket and permit.
struct Lease {
    client: Connection,
    _permit: OwnedSemaphorePermit,
}

impl SqlServerDriver {
    pub async fn new(connection_string: &str) -> Result<Self, DriverError> {
        Self::new_with_options(connection_string, &ConnectOptions::default()).await
    }

    pub async fn new_with_options(
        connection_string: &str,
        options: &ConnectOptions,
    ) -> Result<Self, DriverError> {
        let config = config::parse_config(connection_string, options)?;
        let driver = Self {
            config,
            options: options.clone(),
            permits: Arc::new(Semaphore::new(options.pool_size as usize)),
            idle: Mutex::new(Vec::new()),
        };
        let lease = driver.acquire().await?;
        driver.release(lease);
        Ok(driver)
    }

    async fn connect(&self) -> Result<Connection, DriverError> {
        let tcp = TcpStream::connect(self.config.get_addr())
            .await
            .map_err(|_| DriverError::Connection("SQL Server TCP connection failed".into()))?;
        tcp.set_nodelay(true).map_err(|_| {
            DriverError::Connection("SQL Server socket configuration failed".into())
        })?;
        let mut client = Client::connect(self.config.clone(), tcp.compat_write())
            .await
            .map_err(|error| DriverError::Connection(safe_error(&error)))?;
        {
            // Guard-rendered identifiers use standard double quotes. Establish
            // their semantics explicitly rather than trusting database defaults.
            // NOCOUNT OFF keeps DONE affected-row counts for bound DML even when
            // the login's server user options default NOCOUNT to ON.
            let mut stream = client
                .simple_query("SET QUOTED_IDENTIFIER ON; SET NOCOUNT OFF")
                .await
                .map_err(|error| DriverError::Connection(safe_error(&error)))?;
            while stream
                .try_next()
                .await
                .map_err(|error| DriverError::Connection(safe_error(&error)))?
                .is_some()
            {}
        }
        Ok(client)
    }

    async fn acquire(&self) -> Result<Lease, DriverError> {
        timeout(self.options.connect_timeout, async {
            let permit = self
                .permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| DriverError::Closed)?;
            let client = self.idle.lock().map_err(|_| DriverError::Closed)?.pop();
            let client = match client {
                // A pooled session can be killed or die between checkouts. Probe
                // the popped client; a dead one is discarded and replaced once.
                // The caller's statement is never retried on another connection.
                Some(mut client) => {
                    if probe_connection(&mut client).await.is_ok() {
                        client
                    } else {
                        drop(client);
                        self.connect().await?
                    }
                }
                None => self.connect().await?,
            };
            Ok(Lease {
                client,
                _permit: permit,
            })
        })
        .await
        .map_err(|_| DriverError::Timeout)?
    }

    fn release(&self, lease: Lease) {
        if let Ok(mut idle) = self.idle.lock() {
            idle.push(lease.client);
        }
        // The permit is dropped only after the idle client has been returned.
    }

    async fn run(&self, query: &BoundQuery) -> Result<QueryResult, DriverError> {
        let sql = placeholder::rewrite(&query.sql, &query.params)?;
        let mut lease = self.acquire().await?;
        let (result, reusable) = timeout(self.options.statement_timeout, async {
            if query.fetch_rows {
                return Self::fetch_rows(sql.as_ref(), query, &mut lease.client).await;
            }
            if query.params.is_empty() {
                if sql.trim().is_empty() {
                    return Ok((command_result(0), true));
                }
                // SQL Server compiles module DDL (CREATE PROCEDURE, VIEW,
                // FUNCTION, TRIGGER) only as the first statement of a top-level
                // batch. An RPC sp_executesql call is a procedure invocation, so
                // it rejects those statements. Parameterless commands therefore
                // travel as TDS batches; a batch does not expose its DONE row
                // counts through Tiberius' stream API, so the affected-row count
                // is read with @@ROWCOUNT on the same leased session.
                reset_row_count(&mut lease.client).await?;
                drain(&mut lease.client, sql.as_ref()).await?;
                let affected = affected_rows(&mut lease.client).await?;
                return Ok((command_result(affected), true));
            }
            let mut prepared = Query::new(sql.as_ref());
            codec::bind(&mut prepared, &query.params)?;
            let affected = prepared
                .execute(&mut lease.client)
                .await
                .map_err(execution_error)?
                .total();
            Ok((command_result(affected), true))
        })
        .await
        .map_err(|_| DriverError::Timeout)??;
        if reusable {
            self.release(lease);
        }
        Ok(result)
    }

    async fn fetch_rows(
        sql: &str,
        query: &BoundQuery,
        client: &mut Connection,
    ) -> Result<(QueryResult, bool), DriverError> {
        let mut prepared = Query::new(sql);
        codec::bind(&mut prepared, &query.params)?;
        let mut stream = prepared.query(client).await.map_err(execution_error)?;
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let mut first_result = None;
        let mut truncated = false;
        while let Some(item) = stream.try_next().await.map_err(execution_error)? {
            match item {
                QueryItem::Metadata(metadata) if first_result.is_none() => {
                    first_result = Some(metadata.result_index());
                    columns = codec::columns(metadata.columns());
                }
                QueryItem::Row(row) if first_result == Some(row.result_index()) => {
                    // Probe one row past the cap, without decoding or retaining it.
                    if query.row_cap.is_some_and(|cap| rows.len() >= cap) {
                        truncated = true;
                        break;
                    }
                    rows.push(codec::row(row)?);
                }
                _ => {}
            }
        }
        let row_count = rows.len();
        Ok((
            QueryResult {
                cols: columns,
                rows,
                meta: QueryMeta {
                    row_count,
                    truncated,
                    rows_affected: None,
                },
            },
            !truncated,
        ))
    }

    pub(crate) async fn catalog_query(
        &self,
        sql: &str,
        params: Vec<SqlValue>,
    ) -> Result<QueryResult, DriverError> {
        self.run(&BoundQuery {
            sql: sql.to_owned(),
            params,
            row_cap: None,
            fetch_rows: true,
        })
        .await
        .map_err(|error| match error {
            DriverError::Execution(message) | DriverError::Unsupported(message) => {
                DriverError::Introspection(message)
            }
            other => other,
        })
    }

    async fn explain_query(
        &self,
        query: &BoundQuery,
        analyze: bool,
    ) -> Result<QueryResult, DriverError> {
        let sql = placeholder::rewrite(&query.sql, &query.params)?;
        let mut lease = self.acquire().await?;
        let result = timeout(self.options.statement_timeout, async {
            set_plan_mode(&mut lease.client, analyze, true).await?;
            let plan = if analyze {
                // Actual plans are emitted for the executed sp_executesql
                // statement, so the real bind values stay on the wire and the
                // plan rowset follows the statement's data rows.
                let mut prepared = Query::new(sql.as_ref());
                codec::bind(&mut prepared, &query.params)?;
                let mut stream = prepared
                    .query(&mut lease.client)
                    .await
                    .map_err(execution_error)?;
                collect_plan(&mut stream, true).await?
            } else {
                // SQL Server returns no estimated plan for an RPC
                // sp_executesql call because that is stored-procedure
                // execution; plans are only produced for top-level batches.
                // No bound value is needed to compile an estimated plan, so the
                // batch declares the parameter types and never interpolates a
                // value into the SQL text.
                let batch = estimated_plan_batch(sql.as_ref(), &query.params)?;
                let mut stream = lease
                    .client
                    .simple_query(batch)
                    .await
                    .map_err(execution_error)?;
                collect_plan(&mut stream, false).await?
            };
            set_plan_mode(&mut lease.client, analyze, false).await?;
            let xml = plan.ok_or_else(|| {
                DriverError::Execution(if analyze {
                    // Never present an estimated plan as the executed plan; some
                    // statements (for example a trivial scalar SELECT) have no
                    // actual Showplan output at all.
                    "SQL Server returned no actual execution plan for this statement".into()
                } else {
                    "SQL Server did not return an XML execution plan".into()
                })
            })?;
            Ok::<_, DriverError>(QueryResult {
                cols: vec![strut_sql_driver_api::ColumnMeta {
                    name: "query_plan".into(),
                    type_name: "xml".into(),
                    nullable: false,
                }],
                rows: vec![vec![SqlValue::String(xml)]],
                meta: QueryMeta {
                    row_count: 1,
                    truncated: false,
                    rows_affected: None,
                },
            })
        })
        .await
        .map_err(|_| DriverError::Timeout)??;
        self.release(lease);
        Ok(result)
    }
}

fn estimated_plan_batch(sql: &str, params: &[SqlValue]) -> Result<String, DriverError> {
    let declarations = codec::declarations(params)?;
    if declarations.is_empty() {
        Ok(sql.to_owned())
    } else {
        Ok(format!("{declarations} {sql}"))
    }
}

/// Retains the last XML showplan and nothing else.
///
/// Plan rowsets arrive as a single string column; SQL Server transmits them as
/// `nvarchar(max)` (column name "Microsoft SQL Server 2005 XML Showplan"), not
/// as the `xml` TDS type, so the column type alone cannot identify them. Row
/// values are only decoded for plan-shaped rowsets, and the XML root namespace
/// is what decides whether a value is a plan.
async fn collect_plan(
    stream: &mut QueryStream<'_>,
    named_plan_column_only: bool,
) -> Result<Option<String>, DriverError> {
    let mut plan = None;
    let mut plan_result = None;
    while let Some(item) = stream.try_next().await.map_err(execution_error)? {
        match item {
            QueryItem::Metadata(metadata) => {
                let columns = metadata.columns();
                let is_plan_rowset =
                    columns.len() == 1 && (plan_column(&columns[0]) || !named_plan_column_only);
                plan_result = is_plan_rowset.then_some(metadata.result_index());
            }
            QueryItem::Row(row) if plan_result == Some(row.result_index()) => {
                if let Some(SqlValue::String(xml)) = codec::row(row)?.into_iter().next() {
                    if is_showplan(&xml) {
                        // Actual plans follow the statement's data rows; the
                        // estimated batch emits its single plan document here.
                        plan = Some(xml);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(plan)
}

fn plan_column(column: &Column) -> bool {
    // SQL Server names the result column "Microsoft SQL Server 2005 XML
    // Showplan" and sends it as nvarchar(max); some versions may report xml.
    column.column_type() == ColumnType::Xml
        || column.name().to_ascii_lowercase().contains("showplan")
}

fn execution_error(error: tiberius::error::Error) -> DriverError {
    DriverError::Execution(safe_error(&error))
}

/// Sends a TDS batch and fully drains its response.
async fn drain(client: &mut Connection, sql: &str) -> Result<(), DriverError> {
    let mut stream = client.simple_query(sql).await.map_err(execution_error)?;
    while stream.try_next().await.map_err(execution_error)?.is_some() {}
    Ok(())
}

/// Cheap health probe for a popped idle connection.
///
/// The probe is fully drained so a healthy client is left clean. Its error is
/// only used to decide whether the connection can be reused; callers never see
/// it and their own statement is not retried against another connection.
async fn probe_connection(client: &mut Connection) -> Result<(), DriverError> {
    drain(client, "SELECT 1").await
}

/// Clears the session's `@@ROWCOUNT` before a separately batched command.
///
/// DDL does not necessarily update `@@ROWCOUNT`, so without this reset a
/// command could report the count of an unrelated earlier DML statement. `SET
/// NOCOUNT OFF` is a trusted, driver-generated batch; the user's SQL and body
/// are never altered. The session's NOCOUNT state is the driver default.
async fn reset_row_count(client: &mut Connection) -> Result<(), DriverError> {
    drain(client, "SET NOCOUNT OFF").await
}

/// Reads the affected-row count of the statement executed immediately before.
///
/// Tiberius' batch stream drops DONE row counts, but SQL Server keeps the last
/// statement's `@@ROWCOUNT` in the session across batches.
async fn affected_rows(client: &mut Connection) -> Result<u64, DriverError> {
    let mut stream = client
        .simple_query("SELECT @@ROWCOUNT")
        .await
        .map_err(execution_error)?;
    while let Some(item) = stream.try_next().await.map_err(execution_error)? {
        if let QueryItem::Row(row) = item {
            return match codec::row(row)?.into_iter().next() {
                Some(SqlValue::I64(affected)) if affected >= 0 => Ok(affected as u64),
                _ => Err(DriverError::Execution(
                    "SQL Server did not report an affected-row count".into(),
                )),
            };
        }
    }
    Err(DriverError::Execution(
        "SQL Server did not report an affected-row count".into(),
    ))
}

async fn set_plan_mode(
    client: &mut Connection,
    analyze: bool,
    enabled: bool,
) -> Result<(), DriverError> {
    let sql = match (analyze, enabled) {
        (false, true) => "SET SHOWPLAN_XML ON",
        (false, false) => "SET SHOWPLAN_XML OFF",
        (true, true) => "SET STATISTICS XML ON",
        (true, false) => "SET STATISTICS XML OFF",
    };
    // A direct batch is required: SET options inside sp_executesql may be scoped
    // to its invocation rather than the leased session.
    let mut stream = client
        .simple_query(sql)
        .await
        .map_err(|error| DriverError::Execution(safe_error(&error)))?;
    while stream
        .try_next()
        .await
        .map_err(|error| DriverError::Execution(safe_error(&error)))?
        .is_some()
    {}
    Ok(())
}

fn is_showplan(xml: &str) -> bool {
    roxmltree::Document::parse(xml).is_ok_and(|document| {
        let root = document.root_element();
        root.tag_name().name() == "ShowPlanXML"
            && root.tag_name().namespace()
                == Some("http://schemas.microsoft.com/sqlserver/2004/07/showplan")
    })
}

fn command_result(affected: u64) -> QueryResult {
    QueryResult {
        cols: Vec::new(),
        rows: Vec::new(),
        meta: QueryMeta {
            row_count: 0,
            truncated: false,
            rows_affected: Some(affected),
        },
    }
}

// Never stringify upstream errors: server messages and conversion diagnostics
// can embed SQL literals, bound values, account names, or connection information.
fn safe_error(error: &tiberius::error::Error) -> String {
    match error {
        tiberius::error::Error::Server(_) => format!(
            "SQL Server rejected the request (error {})",
            error.code().unwrap_or_default()
        ),
        tiberius::error::Error::Io { .. } => "SQL Server transport failed".into(),
        tiberius::error::Error::Tls(_) => {
            "SQL Server TLS negotiation or certificate validation failed".into()
        }
        tiberius::error::Error::Conversion(_) => "SQL Server value conversion failed".into(),
        tiberius::error::Error::Encoding(_)
        | tiberius::error::Error::Utf8
        | tiberius::error::Error::Utf16 => "SQL Server text decoding failed".into(),
        _ => "SQL Server protocol request failed".into(),
    }
}

#[allow(clippy::double_must_use)]
#[async_trait]
impl EngineDriver for SqlServerDriver {
    fn kind(&self) -> EngineKind {
        EngineKind::SqlServer
    }

    fn capability_notes(&self) -> Vec<CapabilityNote> {
        let mut notes = vec![
            CapabilityNote {
                code: "sqlserver_client_statement_timeout".into(),
                message: "SQL Server statement timeouts are client-side; timed-out connections are discarded, but server-side cancellation/rollback completion is not guaranteed".into(),
            },
            CapabilityNote {
                code: "sqlserver_conservative_result_nullability".into(),
                message: "TDS query result nullability is not exposed by Tiberius; result columns are conservatively nullable (catalog nullability remains exact)".into(),
            },
        ];
        if self.options.read_only {
            notes.push(CapabilityNote {
                code: "sqlserver_read_only_intent_only".into(),
                message: "SQL Server ApplicationIntent=ReadOnly is routing intent, not session write enforcement; use the shared SQL guard and restricted database credentials".into(),
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
        self.run(query).await
    }
    async fn explain(&self, query: &BoundQuery, analyze: bool) -> Result<QueryResult, DriverError> {
        self.explain_query(query, analyze).await
    }
    async fn list_schemas(&self) -> Result<Vec<SchemaObject>, DriverError> {
        introspect::list_schemas(self).await
    }
    async fn list_tables(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        introspect::list_tables(self, schema).await
    }
    async fn list_columns(&self, schema: Option<&str>) -> Result<Vec<SchemaObject>, DriverError> {
        introspect::list_columns(self, schema).await
    }
    async fn describe_table(
        &self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<TableInfo, DriverError> {
        introspect::describe_table(self, schema, table).await
    }
    async fn list_indexes(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<IndexInfo>, DriverError> {
        introspect::list_indexes(self, schema, table).await
    }
    async fn list_foreign_keys(
        &self,
        schema: Option<&str>,
        table: Option<&str>,
    ) -> Result<Vec<ForeignKeyInfo>, DriverError> {
        introspect::list_foreign_keys(self, schema, table).await
    }
    async fn resolve_schema(&self, schema: Option<&str>) -> Result<String, DriverError> {
        introspect::resolve_schema(self, schema).await
    }
    async fn approximate_row_counts(
        &self,
        schema: Option<&str>,
    ) -> Result<Vec<TableRowCount>, DriverError> {
        introspect::approximate_row_counts(self, schema).await
    }
    async fn list_routines(&self) -> Result<Vec<RoutineInfo>, DriverError> {
        introspect::list_routines(self).await
    }
    async fn table_stats(&self, schema: Option<&str>) -> Result<Vec<TableStats>, DriverError> {
        introspect::table_stats(self, schema).await
    }
    async fn search_objects(
        &self,
        keyword: Option<&str>,
        object_type: Option<ObjectType>,
    ) -> Result<Vec<SchemaObject>, DriverError> {
        introspect::search_objects(self, keyword, object_type).await
    }
    async fn ping(&self) -> Result<(), DriverError> {
        self.run(&BoundQuery {
            sql: "SELECT 1".into(),
            params: vec![],
            row_cap: Some(1),
            fetch_rows: true,
        })
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{estimated_plan_batch, is_showplan};
    use strut_sql_driver_api::SqlValue;

    #[test]
    fn estimated_plan_batch_never_embeds_bound_values() {
        let params = [
            SqlValue::String("sensitive-marker-猫".into()),
            SqlValue::I64(42_424_242),
        ];
        let batch = estimated_plan_batch("SELECT @P1, @P2", &params).unwrap();
        assert!(!batch.contains("sensitive-marker"));
        assert!(!batch.contains("42424242"));
        assert_eq!(estimated_plan_batch("SELECT 1", &[]).unwrap(), "SELECT 1");
    }

    #[test]
    fn identifies_plan_roots_by_xml_namespace_not_substrings() {
        assert!(is_showplan(
            r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" />"#
        ));
        assert!(is_showplan(
            r#"<p:ShowPlanXML xmlns:p="http://schemas.microsoft.com/sqlserver/2004/07/showplan" />"#
        ));
        assert!(!is_showplan(
            r#"<data><ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" /></data>"#
        ));
        assert!(!is_showplan(
            r#"<ShowPlanXML xmlns="https://example.invalid/data" />"#
        ));
        assert!(!is_showplan("not XML"));
    }
}
