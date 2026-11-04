//! Cursor pagination plumbing (R2-01): signing/verification, order-key
//! uniqueness checks, and the mapping from cursor failures to structured tool
//! errors. The page rewrite itself lives in the guard so every SQL path keeps
//! going through one AST pipeline.

use std::sync::{Arc, LazyLock};

use serde_json::Value;
use strut_sql_driver_api::{EngineDriver, EngineKind, IndexInfo};
use strut_sql_guard::KeysetShape;

use crate::cursor::{now_unix, CursorCodec, CursorError, CursorPayload};

/// Process-wide cursor codec (key from `STRUT_STACK_SQL_CURSOR_KEY` or random).
static CODEC: LazyLock<CursorCodec> = LazyLock::new(CursorCodec::new);

pub fn codec() -> &'static CursorCodec {
    &CODEC
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeysetError {
    #[error("CURSOR_EXPIRED: cursor expired; restart the query without a cursor")]
    Expired,
    #[error("CURSOR_INVALID: {0}")]
    Invalid(String),
    #[error("CURSOR_UNSUPPORTED_QUERY: {0}")]
    Unsupported(String),
    #[error("CURSOR_ORDER_KEY_NOT_UNIQUE: {0}")]
    NotUnique(String),
    #[error("CURSOR_ORDER_KEY_NOT_SELECTED: {0}")]
    NotSelected(String),
    #[error("CURSOR_NULL_KEY: {0}")]
    NullKey(String),
}

impl KeysetError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Expired => "CURSOR_EXPIRED",
            Self::Invalid(_) => "CURSOR_INVALID",
            Self::Unsupported(_) => "CURSOR_UNSUPPORTED_QUERY",
            Self::NotUnique(_) => "CURSOR_ORDER_KEY_NOT_UNIQUE",
            Self::NotSelected(_) => "CURSOR_ORDER_KEY_NOT_SELECTED",
            Self::NullKey(_) => "CURSOR_NULL_KEY",
        }
    }

    /// Cursor errors are caller errors: retrying the same request re-fails.
    pub fn retryable(&self) -> bool {
        false
    }
}

impl From<CursorError> for KeysetError {
    fn from(err: CursorError) -> Self {
        match err {
            CursorError::Expired => Self::Expired,
            CursorError::Invalid(msg) => Self::Invalid(msg),
        }
    }
}

/// Decoded cursor plus the caller-visible SQL/parameter fingerprints it must
/// match, so a cursor cannot be replayed against a different query or source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCursor {
    pub after: Value,
    pub descending: bool,
    pub page_size: usize,
}

pub fn verify_cursor(
    token: &str,
    source: &str,
    sql: &str,
    params: &[Value],
) -> Result<VerifiedCursor, KeysetError> {
    let payload = codec().decode(token, now_unix())?;
    if payload.source != source {
        return Err(KeysetError::Invalid(format!(
            "cursor belongs to source '{}'",
            payload.source
        )));
    }
    if payload.sql_hash != CursorPayload::sql_hash(sql) {
        return Err(KeysetError::Invalid(
            "cursor was issued for different SQL; resend the original query".into(),
        ));
    }
    if payload.params_hash != CursorPayload::params_hash(params) {
        return Err(KeysetError::Invalid(
            "cursor was issued for different parameter values; resend the original query".into(),
        ));
    }
    Ok(VerifiedCursor {
        after: payload.after,
        descending: payload.descending,
        page_size: payload.page_size,
    })
}

pub fn issue_cursor(
    source: &str,
    sql: &str,
    params: &[Value],
    after: Value,
    descending: bool,
    page_size: usize,
) -> String {
    codec().issue(
        source,
        sql,
        params,
        after,
        descending,
        page_size,
        now_unix(),
    )
}

fn split_qualified(shape: &KeysetShape) -> (Option<String>, String) {
    match shape.table_qualified.rsplit_once('.') {
        Some((schema, table)) if !schema.is_empty() => {
            (Some(schema.to_string()), table.to_string())
        }
        _ => (None, shape.table.clone()),
    }
}

/// A keyset page can only be free of duplicates/lost rows when the ordering
/// column is unique. Verified against the engine's own catalog: a primary key
/// or a single-column unique index. Composite unique indexes do not qualify.
pub async fn ensure_unique_order_column(
    driver: &Arc<dyn EngineDriver>,
    shape: &KeysetShape,
) -> Result<(), KeysetError> {
    let (schema, table) = split_qualified(shape);
    let column = shape.order_column.to_ascii_lowercase();
    // ClickHouse primary keys are sorting keys, not uniqueness constraints:
    // a keyset page over them can repeat or skip rows, so only an explicit
    // unique index qualifies there.
    let primary_implies_unique = driver.kind() != EngineKind::ClickHouse;
    let indexes = driver
        .list_indexes(schema.as_deref(), Some(&table))
        .await
        .map_err(|e| {
            KeysetError::Unsupported(format!("could not read indexes for table '{table}': {e}"))
        })?;
    let unique_single = indexes
        .iter()
        .any(|index| unique_single_column(index, &column, primary_implies_unique));
    if unique_single {
        return Ok(());
    }
    // Fallback: primary keys are not always reported as indexes by every
    // engine/driver combination; the column catalog knows them too.
    if primary_implies_unique {
        if let Ok(info) = driver.describe_table(schema.as_deref(), &table).await {
            if info
                .columns
                .iter()
                .any(|c| c.is_primary_key && c.name.eq_ignore_ascii_case(&column))
            {
                return Ok(());
            }
        }
    }
    Err(KeysetError::NotUnique(format!(
        "ORDER BY column '{column}' on table '{table}' is not a primary key or single-column unique index; \
         add one, or use page_offset pagination"
    )))
}

/// Whether `index` proves a single-column unique ordering. ClickHouse sorting
/// keys pass `primary_implies_unique = false` because they are not unique.
fn unique_single_column(index: &IndexInfo, column: &str, primary_implies_unique: bool) -> bool {
    (index.unique || (index.primary && primary_implies_unique))
        && index.columns.len() == 1
        && index.columns[0].eq_ignore_ascii_case(column)
}

/// Index of the ordering column in the projected result.
pub fn order_key_index(cols: &[String], column: &str) -> Result<usize, KeysetError> {
    cols.iter()
        .position(|c| c.eq_ignore_ascii_case(column))
        .ok_or_else(|| {
            KeysetError::NotSelected(format!(
                "the ORDER BY column '{column}' must also be selected so the next page can start from it"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn index(name: &str, columns: &[&str], unique: bool, primary: bool) -> IndexInfo {
        IndexInfo {
            schema: None,
            table: "t".into(),
            name: name.into(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            unique,
            primary,
        }
    }

    #[test]
    fn clickhouse_primary_key_does_not_imply_unique_ordering() {
        let primary = index("PRIMARY", &["id"], false, true);
        assert!(!unique_single_column(&primary, "id", false));
        assert!(unique_single_column(&primary, "id", true));
        let unique = index("idx_id", &["id"], true, false);
        assert!(unique_single_column(&unique, "id", false));
        let composite = index("uniq_ab", &["a", "b"], true, false);
        assert!(!unique_single_column(&composite, "a", false));
    }

    #[test]
    fn cursor_is_bound_to_sql_params_and_source() {
        let token = issue_cursor(
            "app",
            "SELECT * FROM t ORDER BY id",
            &[json!(1)],
            json!(5),
            false,
            10,
        );
        assert!(verify_cursor(&token, "app", "SELECT * FROM t ORDER BY id", &[json!(1)]).is_ok());
        assert!(matches!(
            verify_cursor(&token, "other", "SELECT * FROM t ORDER BY id", &[json!(1)]),
            Err(KeysetError::Invalid(_))
        ));
        assert!(matches!(
            verify_cursor(
                &token,
                "app",
                "SELECT * FROM t ORDER BY id DESC",
                &[json!(1)]
            ),
            Err(KeysetError::Invalid(_))
        ));
        assert!(matches!(
            verify_cursor(&token, "app", "SELECT * FROM t ORDER BY id", &[json!(2)]),
            Err(KeysetError::Invalid(_))
        ));
    }

    #[test]
    fn expired_cursor_maps_to_structured_error() {
        // Build a payload that is already expired through the codec directly.
        let payload = CursorPayload {
            v: 1,
            exp: 1,
            source: "app".into(),
            sql_hash: CursorPayload::sql_hash("SELECT 1"),
            params_hash: CursorPayload::params_hash(&[]),
            after: json!(1),
            descending: false,
            page_size: 10,
        };
        let token = codec().encode(&payload);
        assert_eq!(
            verify_cursor(&token, "app", "SELECT 1", &[]),
            Err(KeysetError::Expired)
        );
        assert_eq!(KeysetError::Expired.code(), "CURSOR_EXPIRED");
    }

    #[test]
    fn order_key_must_be_projected() {
        let cols = vec!["id".to_string(), "name".to_string()];
        assert_eq!(order_key_index(&cols, "ID").expect("case-insensitive"), 0);
        assert!(matches!(
            order_key_index(&cols, "created_at"),
            Err(KeysetError::NotSelected(_))
        ));
    }

    #[test]
    fn every_keyset_error_has_a_stable_code() {
        for (err, code) in [
            (KeysetError::Expired, "CURSOR_EXPIRED"),
            (KeysetError::Invalid("x".into()), "CURSOR_INVALID"),
            (
                KeysetError::Unsupported("x".into()),
                "CURSOR_UNSUPPORTED_QUERY",
            ),
            (
                KeysetError::NotUnique("x".into()),
                "CURSOR_ORDER_KEY_NOT_UNIQUE",
            ),
            (
                KeysetError::NotSelected("x".into()),
                "CURSOR_ORDER_KEY_NOT_SELECTED",
            ),
            (KeysetError::NullKey("x".into()), "CURSOR_NULL_KEY"),
        ] {
            assert_eq!(err.code(), code);
            assert!(!err.retryable());
        }
    }
}
