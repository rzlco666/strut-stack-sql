//! In-memory per-principal session query history (PRD-F3-08).
//!
//! Records only: normalized SQL hash, statement label, duration, row count,
//! and a hash of the bound parameter set (never parameter values). History is
//! process-local and lost on restart. Off unless a principal's grant sets
//! `query_history = true` (or the local operator opts in via
//! `STRUT_STACK_SQL_QUERY_HISTORY=1`).

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::{LazyLock, Mutex};

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HistorySuggestion {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone)]
struct HistoryEntry {
    hash: u64,
    label: String,
    duration_ms: u64,
    rows: usize,
    param_hash: u64,
}

const MAX_ENTRIES_PER_PRINCIPAL: usize = 1000;
const N_PLUS_ONE_EXECUTIONS: usize = 10;
const SLOW_QUERY_MS: u64 = 500;
const REPEATED_SLOW_EXECUTIONS: usize = 3;

static HISTORY: LazyLock<Mutex<BTreeMap<String, Vec<HistoryEntry>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Normalize SQL for hashing without parameter values: literals become `?`,
/// case and whitespace are normalized. Deterministic for identical shapes.
pub fn normalized_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut pending_space = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                out.push('?');
                // Skip the quoted literal, honoring doubled quotes.
                while let Some(next) = chars.next() {
                    if next == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            }
            '0'..='9' => {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                out.push('?');
                while chars
                    .peek()
                    .is_some_and(|n| n.is_ascii_digit() || *n == '.')
                {
                    chars.next();
                }
            }
            c if c.is_whitespace() => pending_space = true,
            c => {
                if pending_space && !out.is_empty() {
                    out.push(' ');
                }
                pending_space = false;
                out.push(c.to_ascii_lowercase());
            }
        }
    }
    out
}

pub fn sql_hash(sql: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    normalized_sql(sql).hash(&mut hasher);
    hasher.finish()
}

fn params_hash(params: &[serde_json::Value]) -> u64 {
    let encoded = serde_json::to_string(params).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    encoded.hash(&mut hasher);
    hasher.finish()
}

/// Record one execution and return session-level suggestions relevant to the
/// caller. Never stores SQL text, parameter values, or result data.
pub fn record(
    principal: &str,
    sql: &str,
    label: &str,
    duration_ms: u64,
    rows: usize,
    params: &[serde_json::Value],
) -> Vec<HistorySuggestion> {
    let hash = sql_hash(sql);
    let param_hash = params_hash(params);
    let Ok(mut map) = HISTORY.lock() else {
        return Vec::new();
    };
    let entries = map.entry(principal.to_string()).or_default();
    if entries.len() >= MAX_ENTRIES_PER_PRINCIPAL {
        entries.drain(..entries.len() / 2);
    }
    entries.push(HistoryEntry {
        hash,
        label: label.to_string(),
        duration_ms,
        rows,
        param_hash,
    });

    let mut grouped: BTreeMap<u64, Vec<&HistoryEntry>> = BTreeMap::new();
    for entry in entries.iter() {
        grouped.entry(entry.hash).or_default().push(entry);
    }
    let mut suggestions = Vec::new();
    for (hash, group) in grouped {
        let count = group.len();
        let distinct_params: BTreeSet<u64> = group.iter().map(|e| e.param_hash).collect();
        let max_duration = group.iter().map(|e| e.duration_ms).max().unwrap_or(0);
        let label = group.first().map(|e| e.label.as_str()).unwrap_or("?");
        let total_rows: usize = group.iter().map(|e| e.rows).sum();
        let distinct_param_count = distinct_params.len();
        if count >= N_PLUS_ONE_EXECUTIONS && distinct_param_count >= 2 {
            suggestions.push(HistorySuggestion {
                code: "n_plus_one".into(),
                message: format!(
                    "{label} shape {hash:016x} ran {count} times with {distinct_param_count} parameter sets ({total_rows} rows total); consider a batch/join"
                ),
            });
        }
        if count >= REPEATED_SLOW_EXECUTIONS && max_duration >= SLOW_QUERY_MS {
            suggestions.push(HistorySuggestion {
                code: "repeated_slow_query".into(),
                message: format!(
                    "query shape {hash:016x} ran {count} times and took up to {max_duration} ms; cache or optimize it"
                ),
            });
        }
    }
    suggestions.sort_by(|a, b| {
        (a.code.as_str(), a.message.as_str()).cmp(&(b.code.as_str(), b.message.as_str()))
    });
    suggestions.dedup_by(|a, b| a.code == b.code && a.message == b.message);
    suggestions
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalization_removes_literal_values() {
        assert_eq!(
            normalized_sql("SELECT * FROM t WHERE id = 42 AND name = 'bob'"),
            normalized_sql("SELECT * FROM t WHERE id = 7 AND name = 'alice'")
        );
    }

    #[test]
    fn n_plus_one_is_detected_after_threshold() {
        let principal = format!("test-{}", std::process::id());
        let mut suggestions = Vec::new();
        for i in 0..N_PLUS_ONE_EXECUTIONS {
            suggestions = record(
                &principal,
                "SELECT * FROM orders WHERE user_id = ?",
                "SELECT",
                2,
                1,
                &[json!(i)],
            );
        }
        assert!(suggestions.iter().any(|s| s.code == "n_plus_one"));
    }
}
