//! HTTP transport for ClickHouse.
//!
//! One `reqwest::Client` per driver gives connection pooling; every statement
//! is a `POST` whose body is the SQL text. Row-returning queries ask for
//! `JSONCompactEachRowWithNamesAndTypes` through the `default_format`
//! setting, which streams the column names, the exact ClickHouse types, and
//! then one JSON array per row — the only format that keeps type fidelity
//! while staying streamable.
//!
//! Settings used per request:
//!   * `max_execution_time` — the neutral `statement_timeout`, enforced
//!     server-side (ClickHouse answers HTTP 408 / code 159 when it fires);
//!   * `readonly=1` when the source is read-only (settings and writes are
//!     rejected by the server, not merely by the client);
//!   * `database` — the connection string's database;
//!   * `param_pN` — bound values for rewritten `{pN:Type}` placeholders.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::{Response, StatusCode, Url};
use serde_json::Value as Json;
use strut_sql_driver_api::placeholders::{
    count_question_mark_placeholders, rewrite_question_mark_placeholders,
};
use strut_sql_driver_api::{
    ColumnMeta, ConnectOptions, DriverError, QueryMeta, QueryResult, SqlValue,
};

use crate::config::Endpoint;
use crate::value::{bind_parameter, decode_value, Binding, ChType};

pub(crate) const ROW_FORMAT: &str = "JSONCompactEachRowWithNamesAndTypes";

pub(crate) struct Transport {
    client: reqwest::Client,
    base: Url,
    user: Option<String>,
    password: Option<String>,
    database: String,
    settings: Vec<(String, String)>,
    statement_timeout: Duration,
    /// Bound on connect + response headers only; streaming the body is left
    /// to the caller so a slow transfer is never misreported as a timeout.
    request_timeout: Option<Duration>,
    read_only: bool,
}

impl Transport {
    pub fn new(endpoint: Endpoint, options: &ConnectOptions) -> Result<Self, DriverError> {
        let request_timeout = endpoint.request_timeout(options);
        let client = reqwest::Client::builder()
            .connect_timeout(options.connect_timeout)
            .pool_max_idle_per_host(options.pool_size.max(1) as usize)
            .build()
            .map_err(|e| DriverError::Connection(format!("cannot build HTTP client: {e}")))?;
        Ok(Self {
            client,
            base: endpoint.base,
            user: endpoint.user,
            password: endpoint.password,
            database: endpoint.database,
            request_timeout,
            settings: endpoint.settings,
            statement_timeout: options.statement_timeout,
            read_only: options.read_only,
        })
    }

    pub fn database(&self) -> &str {
        &self.database
    }

    /// Run one statement. `fetch_rows` selects the streaming row format; when
    /// false the server's summary header supplies the affected-row count.
    pub async fn execute(
        &self,
        sql: &str,
        params: &[SqlValue],
        fetch_rows: bool,
        row_cap: Option<usize>,
    ) -> Result<QueryResult, DriverError> {
        let (bound_sql, bound_params) = bind_sql(sql, params)?;
        let response = self.send(&bound_sql, &bound_params, fetch_rows).await?;
        if !fetch_rows {
            let affected = response
                .headers()
                .get("x-clickhouse-summary")
                .and_then(|v| v.to_str().ok())
                .map(summary_rows_affected)
                .unwrap_or(0);
            // Drain the (usually empty) body so the connection is reusable.
            let _ = response.bytes().await;
            return Ok(QueryResult {
                cols: Vec::new(),
                rows: Vec::new(),
                meta: QueryMeta {
                    row_count: 0,
                    truncated: false,
                    rows_affected: Some(affected),
                },
            });
        }
        read_rows(response, row_cap).await
    }

    /// Convenience wrapper for catalog queries: rows, no cap.
    pub async fn fetch(&self, sql: &str, params: &[SqlValue]) -> Result<QueryResult, DriverError> {
        self.execute(sql, params, true, None).await
    }

    async fn send(
        &self,
        sql: &str,
        params: &[(String, String)],
        fetch_rows: bool,
    ) -> Result<Response, DriverError> {
        let mut url = self.base.clone();
        {
            let mut query = url.query_pairs_mut();
            for (key, value) in &self.settings {
                query.append_pair(key, value);
            }
            query.append_pair("database", &self.database);
            if self.read_only && !self.settings.iter().any(|(k, _)| k == "readonly") {
                query.append_pair("readonly", "1");
            }
            if !self.statement_timeout.is_zero() {
                query.append_pair(
                    "max_execution_time",
                    &format!("{:.3}", self.statement_timeout.as_secs_f64()),
                );
            }
            if fetch_rows {
                query.append_pair("default_format", ROW_FORMAT);
            }
            for (key, value) in params {
                query.append_pair(key, value);
            }
        }

        let mut request = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(sql.to_string());
        if let Some(user) = &self.user {
            request = request.header("X-ClickHouse-User", header_value(user)?);
        }
        if let Some(password) = &self.password {
            request = request.header("X-ClickHouse-Key", header_value(password)?);
        }

        let response = match self.request_timeout {
            Some(limit) => tokio::time::timeout(limit, request.send())
                .await
                .map_err(|_| DriverError::Timeout)?
                .map_err(map_request_error)?,
            None => request.send().await.map_err(map_request_error)?,
        };
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        Err(map_response_error(status, &headers, &body))
    }
}

fn header_value(value: &str) -> Result<HeaderValue, DriverError> {
    HeaderValue::from_str(value).map_err(|_| {
        DriverError::InvalidConfig(
            "ClickHouse credentials contain characters that are invalid in an HTTP header"
                .to_string(),
        )
    })
}

/// Rewrite `?` placeholders to typed `{pN:Type}` parameters and collect the
/// values to send as `param_pN` query parameters.
fn bind_sql(
    sql: &str,
    params: &[SqlValue],
) -> Result<(String, Vec<(String, String)>), DriverError> {
    let expected = count_question_mark_placeholders(sql);
    if expected != params.len() {
        return Err(DriverError::Execution(format!(
            "parameter count mismatch: {expected} placeholder(s) in SQL, {} parameter(s) supplied",
            params.len()
        )));
    }
    let mut bound: Vec<(String, String)> = Vec::new();
    let rewritten =
        rewrite_question_mark_placeholders(sql, |index| match bind_parameter(&params[index]) {
            Binding::Inline(text) => text,
            Binding::Param { ch_type, value } => {
                let name = format!("p{index}");
                bound.push((format!("param_{name}"), value));
                format!("{{{name}:{ch_type}}}")
            }
        });
    Ok((rewritten, bound))
}

/// Read the streaming row format: names line, types line, then data lines.
async fn read_rows(response: Response, row_cap: Option<usize>) -> Result<QueryResult, DriverError> {
    let mut stream = response.bytes_stream();
    let mut buffer = LineBuffer::default();
    let mut names: Option<Vec<String>> = None;
    let mut types: Option<Vec<(String, ChType)>> = None;
    let mut rows: Vec<Vec<SqlValue>> = Vec::new();
    let mut truncated = false;

    'read: while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            if e.is_timeout() {
                DriverError::Timeout
            } else {
                DriverError::Connection(e.to_string())
            }
        })?;
        buffer.push(&chunk);
        while let Some(line) = buffer.next_line() {
            if consume_line(line, &mut names, &mut types, &mut rows, row_cap)? {
                truncated = true;
                break 'read;
            }
        }
        buffer.compact();
    }
    if !truncated {
        // The final line may not end with a newline.
        buffer.push(b"\n");
        while let Some(line) = buffer.next_line() {
            if consume_line(line, &mut names, &mut types, &mut rows, row_cap)? {
                truncated = true;
                break;
            }
        }
    }
    finish_result(names, types, rows, truncated)
}

/// Consume one response line. Returns `true` when the row cap was reached and
/// a probe row proves the result continues.
fn consume_line(
    line: &[u8],
    names: &mut Option<Vec<String>>,
    types: &mut Option<Vec<(String, ChType)>>,
    rows: &mut Vec<Vec<SqlValue>>,
    row_cap: Option<usize>,
) -> Result<bool, DriverError> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(false);
    }
    if names.is_none() {
        *names = Some(parse_string_array(line, "column names")?);
        return Ok(false);
    }
    if types.is_none() {
        let raw = parse_string_array(line, "column types")?;
        let expected = names.as_ref().map(Vec::len).unwrap_or(0);
        if expected != raw.len() {
            return Err(DriverError::Execution(format!(
                "ClickHouse returned {expected} column name(s) but {} type(s)",
                raw.len()
            )));
        }
        *types = Some(
            raw.into_iter()
                .map(|text| {
                    let parsed = ChType::parse(&text);
                    (text, parsed)
                })
                .collect(),
        );
        return Ok(false);
    }
    if let Some(cap) = row_cap {
        if rows.len() >= cap {
            // The probe row exists: the result really is longer.
            return Ok(true);
        }
    }
    let known = types.as_ref().expect("checked above");
    rows.push(decode_row(line, known)?);
    Ok(false)
}

fn finish_result(
    names: Option<Vec<String>>,
    types: Option<Vec<(String, ChType)>>,
    rows: Vec<Vec<SqlValue>>,
    truncated: bool,
) -> Result<QueryResult, DriverError> {
    let names = names.unwrap_or_default();
    let types = types.unwrap_or_default();
    if names.len() != types.len() {
        return Err(DriverError::Execution(
            "ClickHouse response ended before the column type line".to_string(),
        ));
    }
    let cols = names
        .iter()
        .zip(&types)
        .map(|(name, (raw, parsed))| ColumnMeta {
            name: name.clone(),
            // Report the type exactly as ClickHouse spelled it.
            type_name: raw.clone(),
            nullable: parsed.is_nullable(),
        })
        .collect();
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

fn decode_row(line: &[u8], types: &[(String, ChType)]) -> Result<Vec<SqlValue>, DriverError> {
    let cells: Vec<Json> = serde_json::from_slice(line)
        .map_err(|e| DriverError::Execution(format!("cannot decode ClickHouse row: {e}")))?;
    if cells.len() != types.len() {
        return Err(DriverError::Execution(format!(
            "ClickHouse row has {} value(s) but {} column type(s)",
            cells.len(),
            types.len()
        )));
    }
    Ok(types
        .iter()
        .zip(&cells)
        .map(|((_, ty), cell)| decode_value(ty, cell))
        .collect())
}

fn parse_string_array(line: &[u8], what: &str) -> Result<Vec<String>, DriverError> {
    serde_json::from_slice(line)
        .map_err(|e| DriverError::Execution(format!("cannot decode ClickHouse {what}: {e}")))
}

/// `X-ClickHouse-Summary` carries affected-row counts as JSON strings.
fn summary_rows_affected(summary: &str) -> u64 {
    let Ok(parsed) = serde_json::from_str::<Json>(summary) else {
        return 0;
    };
    let field = |name: &str| {
        parsed
            .get(name)
            .and_then(Json::as_str)
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let written = field("written_rows");
    if written > 0 {
        written
    } else {
        field("result_rows")
    }
}

fn map_request_error(error: reqwest::Error) -> DriverError {
    if error.is_timeout() {
        DriverError::Timeout
    } else if error.is_connect() {
        DriverError::Connection(error.to_string())
    } else {
        DriverError::Execution(error.to_string())
    }
}

fn map_response_error(status: StatusCode, headers: &HeaderMap, body: &str) -> DriverError {
    let code = headers
        .get("x-clickhouse-exception-code")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i32>().ok());
    let message = error_message(body);
    match code {
        // TIMEOUT_EXCEEDED / TOO_SLOW
        Some(159) | Some(160) => DriverError::Timeout,
        // AUTHENTICATION_FAILED / REQUIRED_PASSWORD
        Some(516) | Some(194) => {
            DriverError::Connection(format!("authentication failed: {message}"))
        }
        // SOCKET_TIMEOUT / NETWORK_ERROR / ALL_CONNECTION_TRIES_FAILED
        Some(209) | Some(210) | Some(279) => DriverError::Connection(message),
        _ if status == StatusCode::REQUEST_TIMEOUT => DriverError::Timeout,
        _ if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN => {
            DriverError::Connection(format!("authentication failed: {message}"))
        }
        _ => DriverError::Execution(message),
    }
}

/// Error bodies are plain text, except that a `default_format` request makes
/// ClickHouse emit its header lines as empty JSON arrays before the message:
///
/// ```text
/// []
/// []
/// ["Code: 60. DB::Exception: Unknown table …"]
/// ```
fn error_message(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.starts_with('[') {
        if let Ok(Json::Array(items)) = serde_json::from_str::<Json>(trimmed) {
            let joined: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !joined.is_empty() {
                return joined.join("\n");
            }
        }
    }
    let mut messages: Vec<String> = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if !line.starts_with('[') {
            continue;
        }
        if let Ok(Json::Array(items)) = serde_json::from_str::<Json>(line) {
            messages.extend(items.iter().filter_map(|v| v.as_str().map(str::to_string)));
        }
    }
    if messages.is_empty() {
        trimmed.to_string()
    } else {
        messages.join("\n")
    }
}

/// Incremental newline splitter over the response body.
#[derive(Default)]
struct LineBuffer {
    buffer: Vec<u8>,
    start: usize,
}

impl LineBuffer {
    fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
    }

    fn next_line(&mut self) -> Option<&[u8]> {
        let offset = self.buffer[self.start..].iter().position(|b| *b == b'\n')?;
        let end = self.start + offset;
        let start = self.start;
        self.start = end + 1;
        Some(&self.buffer[start..end])
    }

    /// Drop consumed bytes once they dominate the buffer. Must not be called
    /// while a line borrowed from `next_line` is still alive.
    fn compact(&mut self) {
        if self.start == self.buffer.len() {
            self.buffer.clear();
            self.start = 0;
        } else if self.start > 64 * 1024 {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_placeholders_to_typed_parameters() {
        let params = vec![SqlValue::I64(5), SqlValue::String("x".into())];
        let (sql, bound) = bind_sql("SELECT ? AS a, ? AS b", &params).expect("bind");
        assert_eq!(sql, "SELECT {p0:Int64} AS a, {p1:String} AS b");
        assert_eq!(
            bound,
            vec![
                ("param_p0".to_string(), "5".to_string()),
                ("param_p1".to_string(), "x".to_string())
            ]
        );
    }

    #[test]
    fn inlines_null_without_a_parameter() {
        let (sql, bound) = bind_sql("SELECT ? AS a", &[SqlValue::Null]).expect("bind");
        assert_eq!(sql, "SELECT NULL AS a");
        assert!(bound.is_empty());
    }

    #[test]
    fn rejects_parameter_count_mismatch() {
        let err = bind_sql("SELECT ? AS a", &[]).unwrap_err();
        assert!(
            err.to_string().contains("parameter count mismatch"),
            "{err}"
        );
    }

    #[test]
    fn parses_summary_written_rows() {
        let summary = r#"{"read_rows":"0","written_rows":"3","result_rows":"0"}"#;
        assert_eq!(summary_rows_affected(summary), 3);
        let summary = r#"{"read_rows":"0","written_rows":"0","result_rows":"2"}"#;
        assert_eq!(summary_rows_affected(summary), 2);
        assert_eq!(summary_rows_affected("not json"), 0);
    }

    #[test]
    fn maps_timeout_exception_code() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-clickhouse-exception-code",
            HeaderValue::from_static("159"),
        );
        let error = map_response_error(StatusCode::REQUEST_TIMEOUT, &headers, "Code: 159. Timeout");
        assert!(matches!(error, DriverError::Timeout));
    }

    #[test]
    fn maps_query_error_to_execution() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-clickhouse-exception-code",
            HeaderValue::from_static("60"),
        );
        let error = map_response_error(
            StatusCode::NOT_FOUND,
            &headers,
            "Code: 60. DB::Exception: Unknown table",
        );
        assert!(matches!(error, DriverError::Execution(ref m) if m.contains("Unknown table")));
    }

    #[test]
    fn unwraps_json_wrapped_error_body() {
        assert_eq!(
            error_message(r#"["Code: 60. DB::Exception: Unknown table"]"#),
            "Code: 60. DB::Exception: Unknown table"
        );
        // `default_format` makes the server emit empty header lines first.
        assert_eq!(
            error_message("[]\n[]\n[\"Code: 60. DB::Exception: Unknown table\"]\n"),
            "Code: 60. DB::Exception: Unknown table"
        );
    }

    #[test]
    fn line_buffer_handles_split_chunks() {
        let mut buffer = LineBuffer::default();
        buffer.push(b"[\"a\"]\n[\"Str");
        assert_eq!(buffer.next_line(), Some(&b"[\"a\"]"[..]));
        assert_eq!(buffer.next_line(), None);
        buffer.push(b"ing\"]\n[1]\n");
        assert_eq!(buffer.next_line(), Some(&b"[\"String\"]"[..]));
        assert_eq!(buffer.next_line(), Some(&b"[1]"[..]));
        assert_eq!(buffer.next_line(), None);
    }

    #[test]
    fn decodes_streamed_rows_with_types() {
        let names = parse_string_array(br#"["a","b"]"#, "names").unwrap();
        let types: Vec<(String, ChType)> =
            parse_string_array(br#"["Int64","Nullable(String)"]"#, "types")
                .unwrap()
                .into_iter()
                .map(|text| {
                    let parsed = ChType::parse(&text);
                    (text, parsed)
                })
                .collect();
        assert_eq!(names.len(), types.len());
        let row = decode_row(br#"["7",null]"#, &types).unwrap();
        assert_eq!(row, vec![SqlValue::I64(7), SqlValue::Null]);
    }

    #[test]
    fn consumes_names_types_and_caps_rows() {
        let mut names = None;
        let mut types = None;
        let mut rows = Vec::new();
        assert!(!consume_line(br#"["a"]"#, &mut names, &mut types, &mut rows, Some(1)).unwrap());
        assert!(
            !consume_line(br#"["Int64"]"#, &mut names, &mut types, &mut rows, Some(1)).unwrap()
        );
        assert!(!consume_line(br#"["1"]"#, &mut names, &mut types, &mut rows, Some(1)).unwrap());
        // The probe row proves truncation and is not decoded.
        assert!(consume_line(br#"["2"]"#, &mut names, &mut types, &mut rows, Some(1)).unwrap());
        assert_eq!(rows, vec![vec![SqlValue::I64(1)]]);
        let result = finish_result(names, types, rows, true).unwrap();
        assert!(result.meta.truncated);
        assert_eq!(result.cols[0].type_name, "Int64");
        assert!(!result.cols[0].nullable);
    }
}
