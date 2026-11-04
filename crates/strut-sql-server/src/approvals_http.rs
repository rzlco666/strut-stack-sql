//! HTTP approval queue (R2-05).
//!
//! `/mcp` callers (or an operator on the same machine) can inspect and decide
//! pending destructive-DDL approvals. Authentication is the `/mcp` middleware;
//! decisions are bound to the requesting principal — another principal cannot
//! accept or reject someone else's request, and the API does not disclose
//! whether an unknown id exists.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use strut_sql_core::approvals::{ApprovalEntry, ApprovalState};
use strut_sql_core::redact::redact_url;

use crate::{CallerPrincipal, HttpState};

fn entry_json(entry: &ApprovalEntry) -> serde_json::Value {
    json!({
        "id": entry.id,
        "tool": entry.tool,
        "source": entry.source,
        "summary": redact_url(&entry.summary),
        "state": match entry.state {
            ApprovalState::Pending => "pending",
            ApprovalState::Accepted => "accepted",
            ApprovalState::Rejected => "rejected",
        },
        "created_unix": entry.created_unix,
        "expires_unix": entry.expires_unix,
    })
}

pub async fn list_approvals(
    State(state): State<HttpState>,
    Extension(principal): Extension<CallerPrincipal>,
) -> Response {
    let queue = &state.config.approvals;
    queue.gc();
    let pending = queue.pending_for(&principal.0.id);
    Json(json!({
        "principal": principal.0.id,
        "pending": pending.iter().map(entry_json).collect::<Vec<_>>(),
    }))
    .into_response()
}

async fn decide(state: HttpState, principal: CallerPrincipal, id: &str, accept: bool) -> Response {
    match state.config.approvals.decide(id, &principal.0.id, accept) {
        Some(entry) => Json(json!({
            "ok": true,
            "approval": entry_json(&entry),
        }))
        .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "code": "approval_unknown",
                "message": "unknown, expired or already decided approval for this principal",
            })),
        )
            .into_response(),
    }
}

pub async fn accept_approval(
    State(state): State<HttpState>,
    Extension(principal): Extension<CallerPrincipal>,
    Path(id): Path<String>,
) -> Response {
    decide(state, principal, &id, true).await
}

pub async fn reject_approval(
    State(state): State<HttpState>,
    Extension(principal): Extension<CallerPrincipal>,
    Path(id): Path<String>,
) -> Response {
    decide(state, principal, &id, false).await
}
