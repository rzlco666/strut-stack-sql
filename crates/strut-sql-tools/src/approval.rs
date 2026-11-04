//! Approval gate for destructive DDL (R2-05).
//!
//! `confirm: true` is a legacy confirmation flag and never counts as an
//! approval. When the process requires approvals, a destructive action must
//! carry an `approval_id` created by the queue and accepted by the same
//! principal. The id is consumed on use, expires after 15 minutes, and the
//! pending request is returned to the caller as a structured
//! `approval_required` error (with the HTTP queue URL path in docs).

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use serde_json::json;
use strut_sql_core::approvals::{ApprovalError, APPROVAL_TTL_SECS};
use strut_sql_core::config::AppConfig;
use strut_sql_core::redact::redact_url;

/// Statement labels that need approval when the process requires it.
pub fn destructive_label(label: &str) -> bool {
    let upper = label.to_ascii_uppercase();
    upper.starts_with("DROP")
        || upper.starts_with("TRUNCATE")
        || (upper.starts_with("ALTER") && upper.contains("DROP COLUMN"))
}

fn approval_error(code: &str, message: &str) -> McpError {
    let message = redact_url(message);
    McpError::new(
        rmcp::model::ErrorCode(-32015),
        message.clone(),
        Some(json!({
            "code": code,
            "message": message,
            "retryable": false,
        })),
    )
}

/// Enforce the approval requirement for one destructive action.
///
/// * approvals disabled → no-op (legacy deployments keep working);
/// * valid accepted `approval_id` → consumed, execution proceeds;
/// * otherwise a pending request is recorded and `approval_required` is
///   returned with the id and expiry. Nothing is executed or locked.
pub fn require(
    config: &Arc<AppConfig>,
    tool: &str,
    source: &str,
    summary: &str,
    approval_id: Option<&str>,
) -> Result<(), McpError> {
    require_forced(config, tool, source, summary, approval_id, false)
}

/// Like [`require`], but `force` demands an approval even when the process
/// flag is off (migration apply always needs one, per the V2 contract).
pub fn require_forced(
    config: &Arc<AppConfig>,
    tool: &str,
    source: &str,
    summary: &str,
    approval_id: Option<&str>,
    force: bool,
) -> Result<(), McpError> {
    if !config.require_approval && !force {
        return Ok(());
    }
    let queue = &config.approvals;
    queue.gc();
    if let Some(id) = approval_id {
        return match queue.consume(id, &config.principal_id, source, tool) {
            Ok(()) => Ok(()),
            Err(ApprovalError::Expired) => Err(approval_error(
                "approval_expired",
                "approval expired (15-minute limit); request a new one",
            )),
            Err(ApprovalError::Rejected) => Err(approval_error(
                "approval_rejected",
                "approval was rejected or already used; request a new one",
            )),
            Err(ApprovalError::Pending) => Err(approval_error(
                "approval_pending",
                "approval has not been accepted yet",
            )),
            Err(ApprovalError::Unknown) => Err(approval_error(
                "approval_unknown",
                "unknown approval id for this principal/source/tool",
            )),
        };
    }
    let entry = queue.request(&config.principal_id, source, tool, summary);
    let message = format!(
        "approval required for {tool}: accept approval {} (expires in {} minutes), then retry with approval_id",
        entry.id,
        APPROVAL_TTL_SECS / 60
    );
    Err(McpError::new(
        rmcp::model::ErrorCode(-32015),
        message.clone(),
        Some(json!({
            "code": "approval_required",
            "message": message,
            "retryable": false,
            "approval_id": entry.id,
            "expires_unix": entry.expires_unix,
            "ttl_seconds": APPROVAL_TTL_SECS,
            "accept_path": format!("/approvals/{}/accept", entry.id),
        })),
    ))
}
