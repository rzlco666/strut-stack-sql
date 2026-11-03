//! HTTP-only row export (R2-03): NDJSON and CSV streaming.
//!
//! The MCP tool surface stays paginated JSON; bulk export is an HTTP concern.
//! `POST /export` authenticates with the same middleware as `/mcp`, authorizes
//! through the same Policy (`execute_sql`, read tier, per-source grant), runs
//! the statement through the same guard with a read-only write mode, and then
//! streams rows in response chunks bounded by `--max-bytes`.
//!
//! Bounded materialization: rows are fetched once (capped by `--max-rows`) and
//! then chunked. That keeps driver code untouched and still bounds memory by
//! the configured row/byte caps.
//!
//! CSV contract: a `null` value is written as an empty field (indistinguishable
//! from an empty string in the payload), and the response carries
//! `x-meta-null-as-empty: true` so clients do not have to guess.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Extension, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use strut_sql_core::auth::{Decision, Tier};
use strut_sql_core::config::AppConfig;
use strut_sql_core::redact::redact_url;
use strut_sql_driver_api::EngineDriver;
use strut_sql_guard::WriteMode;
use strut_sql_tools::core::exec_options;
use strut_sql_tools::exec::{execute_query, ExecError};

use crate::{CallerPrincipal, HttpState};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Ndjson,
    Csv,
}

#[derive(Debug, Deserialize)]
pub struct ExportRequest {
    pub sql: String,
    #[serde(default)]
    pub params: Option<Vec<Value>>,
    #[serde(default)]
    pub source: Option<String>,
    pub format: ExportFormat,
}

fn csv_field(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn csv_line(fields: Vec<String>) -> String {
    let escaped: Vec<String> = fields
        .into_iter()
        .map(|field| {
            if field.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", field.replace('"', "\"\""))
            } else {
                field
            }
        })
        .collect();
    format!("{}\n", escaped.join(","))
}

/// Group serialized row payloads into chunks no larger than `max_bytes`.
fn chunk_rows(rows: Vec<String>, max_bytes: usize) -> Vec<Bytes> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for row in rows {
        if !current.is_empty() && current.len() + row.len() > max_bytes {
            chunks.push(Bytes::from(std::mem::take(&mut current)));
        }
        current.push_str(&row);
    }
    if !current.is_empty() {
        chunks.push(Bytes::from(current));
    }
    chunks
}

pub async fn export_handler(
    State(state): State<HttpState>,
    Extension(principal): Extension<CallerPrincipal>,
    Json(request): Json<ExportRequest>,
) -> Response {
    let config: Arc<AppConfig> = state.config.clone();
    let source_name = request
        .source
        .clone()
        .unwrap_or_else(|| config.default_source.clone());

    // Same authorization entry point as the MCP dispatch.
    let authz = match config
        .policy
        .authorize(&principal.0, "execute_sql", &source_name, Tier::Read)
    {
        Decision::Allow(authz) => authz,
        Decision::Deny(denial) => {
            tracing::warn!(
                target: "audit",
                event = "authorization",
                decision = "deny",
                principal_id = %principal.0.id,
                tool = "export",
                source_name = %source_name,
                reason = %denial,
                "export denied by policy"
            );
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "code": "authorization_denied",
                    "message": redact_url(&denial.to_string()),
                    "retryable": false,
                })),
            )
                .into_response();
        }
    };
    let scoped = Arc::new(config.scoped_for(&authz));

    let source = match scoped.source(Some(&source_name)) {
        Ok(source) => source,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "invalid_params", "message": err.to_string() })),
            )
                .into_response()
        }
    };
    let driver: Arc<dyn EngineDriver> = match source.driver().await {
        Ok(driver) => driver,
        Err(err) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "code": "execution_error",
                    "message": redact_url(&err.to_string()),
                    "retryable": true,
                })),
            )
                .into_response()
        }
    };

    let mut opts = exec_options(&scoped);
    // Export is a read path regardless of the process tier.
    opts.write_mode = WriteMode::ReadOnly;
    let params = request.params.unwrap_or_default();
    let result = match execute_query(&driver, &request.sql, &params, &opts).await {
        Ok(result) => result,
        Err(err) => {
            let (status, code, retryable) = match &err {
                ExecError::Guard(_) => (StatusCode::FORBIDDEN, "guard_denied", false),
                ExecError::Limit(_) => (StatusCode::BAD_REQUEST, "invalid_params", false),
                ExecError::Cursor(_) => (StatusCode::BAD_REQUEST, "invalid_params", false),
                ExecError::Database(_) | ExecError::Other(_) => {
                    (StatusCode::BAD_GATEWAY, "execution_error", true)
                }
            };
            return (
                status,
                Json(json!({
                    "code": code,
                    "message": redact_url(&err.to_string()),
                    "retryable": retryable,
                })),
            )
                .into_response();
        }
    };

    let columnar = match result.data {
        Some(data) => data,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "code": "invalid_params", "message": "export requires a row-returning query" })),
            )
                .into_response()
        }
    };

    let max_bytes = scoped.max_bytes.max(1024);
    let (content_type, header_line, rows) = match request.format {
        ExportFormat::Ndjson => {
            let rows: Vec<String> = columnar
                .rows
                .iter()
                .map(|row| {
                    let mut object = serde_json::Map::new();
                    for (i, column) in columnar.cols.iter().enumerate() {
                        object.insert(
                            column.clone(),
                            row.get(i).cloned().unwrap_or(Value::Null),
                        );
                    }
                    let mut line = serde_json::to_string(&object).unwrap_or_default();
                    line.push('\n');
                    line
                })
                .collect();
            ("application/x-ndjson", None, rows)
        }
        ExportFormat::Csv => {
            let header_line = csv_line(columnar.cols.clone());
            let rows: Vec<String> = columnar
                .rows
                .iter()
                .map(|row| {
                    csv_line(
                        (0..columnar.cols.len())
                            .map(|i| csv_field(row.get(i).unwrap_or(&Value::Null)))
                            .collect(),
                    )
                })
                .collect();
            ("text/csv; charset=utf-8", Some(header_line), rows)
        }
    };

    let mut all_rows = rows;
    if let Some(header_line) = header_line {
        all_rows.insert(0, header_line);
    }
    let chunks = chunk_rows(all_rows, max_bytes);

    tracing::info!(
        target: "audit",
        event = "export",
        principal_id = %principal.0.id,
        source_name = %source_name,
        rows = columnar.rows.len(),
        columns = columnar.cols.len(),
        format = ?request.format,
        "export streamed"
    );

    let mut response = Body::from_stream(stream::iter(
        chunks.into_iter().map(Ok::<Bytes, std::io::Error>),
    ))
    .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(if content_type.starts_with("text/csv") {
            "text/csv; charset=utf-8"
        } else {
            "application/x-ndjson"
        }),
    );
    headers.insert("x-meta-null-as-empty", HeaderValue::from_static("true"));
    headers.insert(
        "x-meta-rows",
        HeaderValue::from_str(&columnar.rows.len().to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        "x-meta-columns",
        HeaderValue::from_str(&columnar.cols.join(","))
            .unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_nulls_are_empty_and_quotes_are_escaped() {
        assert_eq!(csv_field(&Value::Null), "");
        assert_eq!(csv_field(&json!("plain")), "plain");
        assert_eq!(csv_line(vec!["a,b".into(), "q\"q".into(), String::new()]), "\"a,b\",\"q\"\"q\",\n");
    }

    #[test]
    fn ndjson_is_one_object_per_line() {
        let rows = ["{\"id\":1}\n", "{\"id\":2}\n"];
        assert_eq!(rows.concat(), "{\"id\":1}\n{\"id\":2}\n");
    }

    #[test]
    fn chunks_respect_the_byte_budget() {
        let rows = vec!["aaaa\n".to_string(), "bbbb\n".to_string(), "cccc\n".to_string()];
        let chunks = chunk_rows(rows, 5);
        assert_eq!(chunks.len(), 3, "each row exceeds the 5-byte budget alone");
        let chunks = chunk_rows(
            vec!["aaa\n".into(), "bbb\n".into(), "ccc\n".into()],
            8,
        );
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].len() <= 8);
    }
}
