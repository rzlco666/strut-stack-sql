use std::future::Future;
use std::hash::{Hash, Hasher};
use std::time::Instant;

use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;

// ---------------------------------------------------------------------------
// Audit (SQL-GAP-03): one structured `target: "audit"` event per tool call.
// Never logs raw SQL or parameters — only a non-cryptographic fingerprint
// (`sql_fp`, std DefaultHasher) for correlation, the tool, the source name,
// the elapsed time, and the outcome. Enable/inspect with
// `MCP_SQL_LOG=audit=info` (audit events are info-level, so the default
// filter shows them too).
// ---------------------------------------------------------------------------

pub fn sql_fingerprint(sql: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sql.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub async fn audit_call<Fut>(
    tool: &'static str,
    source: Option<String>,
    sql_fp: Option<String>,
    fut: Fut,
) -> Result<CallToolResult, McpError>
where
    Fut: Future<Output = Result<CallToolResult, McpError>>,
{
    let started = Instant::now();
    let result = fut.await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let source = source.as_deref().unwrap_or("-");
    let sql_fp = sql_fp.as_deref().unwrap_or("-");
    match &result {
        Ok(_) => tracing::info!(
            target: "audit",
            tool,
            source_name = source,
            sql_fp,
            elapsed_ms,
            ok = true,
            "tool_call"
        ),
        Err(e) => tracing::info!(
            target: "audit",
            tool,
            source_name = source,
            sql_fp,
            elapsed_ms,
            ok = false,
            error = %e,
            "tool_call"
        ),
    }
    result
}
