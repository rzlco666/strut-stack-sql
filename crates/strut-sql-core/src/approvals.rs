//! Approval queue for destructive DDL (R2-05).
//!
//! A destructive statement is never executed merely because a caller asked:
//! when approvals are required, the server records a pending request and
//! returns `approval_required` with an id and the expiry. The caller (or an
//! operator through the HTTP queue) accepts it, then retries with
//! `approval_id`. Entries expire after [`APPROVAL_TTL_SECS`] (15 minutes) and
//! are single-use.
//!
//! `confirm: true` remains a legacy confirmation flag. It is a caller's
//! assertion, not an approval: it is never recorded in the queue, never
//! produces an `approval_id`, and never satisfies an approval requirement.
//!
//! Pending approvals are held in memory; a process restart drops them, which
//! fails closed (the retry is rejected as expired).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const APPROVAL_TTL_SECS: u64 = 900;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone)]
pub struct ApprovalEntry {
    pub id: String,
    pub principal: String,
    pub source: String,
    pub tool: String,
    /// Redacted, human-readable one-liner (never SQL parameter values).
    pub summary: String,
    pub state: ApprovalState,
    pub created_unix: u64,
    pub expires_unix: u64,
}

impl ApprovalEntry {
    pub fn expired(&self, now_unix: u64) -> bool {
        now_unix >= self.expires_unix
    }
}

#[derive(Debug, Default)]
pub struct ApprovalQueue {
    entries: Mutex<HashMap<String, (ApprovalEntry, Instant)>>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn new_id() -> String {
    // Approval ids are not secrets, but they must be unguessable enough that
    // another principal cannot accept someone else's request. A process-local
    // counter plus a random-looking suffix derived from time/pid is enough for
    // an in-memory 15-minute queue; the principal check is the real gate.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("apr_{:x}{:x}", t, n)
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a pending approval request.
    pub fn request(
        &self,
        principal: &str,
        source: &str,
        tool: &str,
        summary: &str,
    ) -> ApprovalEntry {
        let created = now_unix();
        let entry = ApprovalEntry {
            id: new_id(),
            principal: principal.to_string(),
            source: source.to_string(),
            tool: tool.to_string(),
            summary: summary.to_string(),
            state: ApprovalState::Pending,
            created_unix: created,
            expires_unix: created + APPROVAL_TTL_SECS,
        };
        let mut entries = self.entries.lock().expect("approval queue poisoned");
        entries.insert(entry.id.clone(), (entry.clone(), Instant::now()));
        entry
    }

    /// Pending (non-expired) entries for one principal.
    pub fn pending_for(&self, principal: &str) -> Vec<ApprovalEntry> {
        let now = now_unix();
        self.entries
            .lock()
            .expect("approval queue poisoned")
            .values()
            .map(|(entry, _)| entry)
            .filter(|entry| entry.principal == principal && entry.state == ApprovalState::Pending)
            .filter(|entry| !entry.expired(now))
            .cloned()
            .collect()
    }

    /// Accept or reject a pending entry. Only the requesting principal may
    /// decide. Returns `None` when the id is unknown, expired, already decided
    /// or owned by someone else (the caller must not learn which).
    pub fn decide(&self, id: &str, principal: &str, accept: bool) -> Option<ApprovalEntry> {
        let now = now_unix();
        let mut entries = self.entries.lock().expect("approval queue poisoned");
        let (entry, _) = entries.get_mut(id)?;
        if entry.principal != principal
            || entry.state != ApprovalState::Pending
            || entry.expired(now)
        {
            return None;
        }
        entry.state = if accept {
            ApprovalState::Accepted
        } else {
            ApprovalState::Rejected
        };
        Some(entry.clone())
    }

    /// Consume an accepted approval for exactly this principal/source/tool.
    /// Single-use: the entry moves to `Rejected` afterwards so it cannot be
    /// replayed. Returns false for pending, rejected, expired or mismatched
    /// entries.
    pub fn consume(
        &self,
        id: &str,
        principal: &str,
        source: &str,
        tool: &str,
    ) -> Result<(), ApprovalError> {
        let now = now_unix();
        let mut entries = self.entries.lock().expect("approval queue poisoned");
        let Some((entry, _)) = entries.get_mut(id) else {
            return Err(ApprovalError::Unknown);
        };
        if entry.expired(now) {
            entry.state = ApprovalState::Rejected;
            return Err(ApprovalError::Expired);
        }
        if entry.principal != principal || entry.source != source || entry.tool != tool {
            // Do not disclose whose approval it is or what it was for.
            return Err(ApprovalError::Unknown);
        }
        match entry.state {
            ApprovalState::Accepted => {
                entry.state = ApprovalState::Rejected;
                Ok(())
            }
            ApprovalState::Rejected => Err(ApprovalError::Rejected),
            ApprovalState::Pending => Err(ApprovalError::Pending),
        }
    }

    /// Drop expired entries (called opportunistically on each request).
    pub fn gc(&self) {
        let now = now_unix();
        self.entries
            .lock()
            .expect("approval queue poisoned")
            .retain(|_, (entry, inserted)| {
                !entry.expired(now)
                    && inserted.elapsed() < Duration::from_secs(APPROVAL_TTL_SECS * 2)
            });
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApprovalError {
    #[error("unknown approval id")]
    Unknown,
    #[error("approval expired")]
    Expired,
    #[error("approval was rejected")]
    Rejected,
    #[error("approval is still pending")]
    Pending,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_then_consume_is_single_use() {
        let queue = ApprovalQueue::new();
        let entry = queue.request("alice", "app", "drop_table", "drop_table on users");
        assert_eq!(queue.pending_for("alice").len(), 1);
        assert_eq!(queue.pending_for("bob").len(), 0);
        assert!(queue
            .consume(&entry.id, "alice", "app", "drop_table")
            .is_err());
        assert!(queue.decide(&entry.id, "alice", true).is_some());
        assert!(queue
            .consume(&entry.id, "alice", "app", "drop_table")
            .is_ok());
        assert_eq!(
            queue.consume(&entry.id, "alice", "app", "drop_table"),
            Err(ApprovalError::Rejected),
            "approvals must not be replayable"
        );
    }

    #[test]
    fn other_principals_cannot_decide_or_consume() {
        let queue = ApprovalQueue::new();
        let entry = queue.request("alice", "app", "drop_table", "x");
        assert!(queue.decide(&entry.id, "bob", true).is_none());
        assert!(queue.decide(&entry.id, "alice", true).is_some());
        assert_eq!(
            queue.consume(&entry.id, "bob", "app", "drop_table"),
            Err(ApprovalError::Unknown)
        );
        // Source/tool mismatch is rejected too.
        assert_eq!(
            queue.consume(&entry.id, "alice", "other", "drop_table"),
            Err(ApprovalError::Unknown)
        );
    }

    #[test]
    fn rejection_and_expiry_fail_closed() {
        let queue = ApprovalQueue::new();
        let entry = queue.request("alice", "app", "truncate_table", "x");
        assert!(queue.decide(&entry.id, "alice", false).is_some());
        assert_eq!(
            queue.consume(&entry.id, "alice", "app", "truncate_table"),
            Err(ApprovalError::Rejected)
        );

        let entry = queue.request("alice", "app", "drop_column", "x");
        // Simulate expiry by rewriting the stored entry.
        {
            let mut entries = queue.entries.lock().unwrap();
            if let Some((stored, _)) = entries.get_mut(&entry.id) {
                stored.expires_unix = 1;
            }
        }
        assert_eq!(
            queue.consume(&entry.id, "alice", "app", "drop_column"),
            Err(ApprovalError::Expired)
        );
    }

    #[test]
    fn ids_are_unique() {
        let queue = ApprovalQueue::new();
        let a = queue.request("a", "s", "t", "x");
        let b = queue.request("a", "s", "t", "x");
        assert_ne!(a.id, b.id);
    }
}
