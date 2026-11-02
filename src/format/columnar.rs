use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const AUTO_ROW_THRESHOLD: usize = 10;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ResultFormat {
    #[default]
    Columnar,
    Auto,
    Rows,
}

pub fn resolve_result_format(override_fmt: Option<ResultFormat>) -> ResultFormat {
    if let Some(fmt) = override_fmt {
        return fmt;
    }
    match std::env::var("MCP_SQL_FORMAT")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "rows" => ResultFormat::Rows,
        "auto" => ResultFormat::Auto,
        _ => ResultFormat::Columnar,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RowObjectResult {
    pub rows: Vec<Map<String, Value>>,
    pub meta: ColumnarMeta,
}

/// Column names for Rows mode: `serde_json::Map` keeps a deterministic key
/// order (insertion order with the `preserve_order` feature, alphabetical
/// otherwise). Duplicate column names get a `#2`/`#3` suffix so a later
/// duplicate never silently overwrites an earlier column's data (SQL-BUG-11).
pub fn deduped_row_keys(cols: &[String]) -> Vec<String> {
    let mut seen: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    cols.iter()
        .map(|c| {
            let n = seen.entry(c.as_str()).or_insert(0);
            *n += 1;
            if *n == 1 {
                c.clone()
            } else {
                format!("{c}#{}", *n)
            }
        })
        .collect()
}

pub fn format_columnar(format: ResultFormat, columnar: ColumnarResult) -> serde_json::Value {
    // Default path is always columnar (token-efficient). Rows/Auto only when requested.
    let effective = match format {
        ResultFormat::Rows => ResultFormat::Rows,
        ResultFormat::Auto if columnar.rows.len() <= AUTO_ROW_THRESHOLD => ResultFormat::Rows,
        _ => ResultFormat::Columnar,
    };
    match effective {
        ResultFormat::Columnar => serde_json::to_value(&columnar).unwrap_or(Value::Null),
        ResultFormat::Rows | ResultFormat::Auto => {
            let keys = deduped_row_keys(&columnar.cols);
            let rows: Vec<Map<String, Value>> = columnar
                .rows
                .iter()
                .map(|row| {
                    let mut map = Map::new();
                    for (i, key) in keys.iter().enumerate() {
                        map.insert(key.clone(), row.get(i).cloned().unwrap_or(Value::Null));
                    }
                    map
                })
                .collect();
            serde_json::to_value(RowObjectResult {
                rows,
                meta: columnar.meta,
            })
            .unwrap_or(Value::Null)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnarResult {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub meta: ColumnarMeta,
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnarMeta {
    pub n: usize,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_affected: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_injected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_clamped: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_fetched: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted_columns: Option<Vec<String>>,
}

impl ColumnarResult {
    pub fn empty_command(rows_affected: u64) -> Self {
        Self {
            cols: vec![],
            rows: vec![],
            meta: ColumnarMeta {
                n: 0,
                truncated: false,
                rows_affected: Some(rows_affected),
                limit_injected: None,
                limit_clamped: None,
                page_offset: None,
                page_size: None,
                has_more: None,
                total_fetched: None,
                redacted_columns: None,
            },
        }
    }

    pub fn apply_pagination(&mut self, page_offset: usize, page_size: usize) {
        let total_fetched = self.rows.len();
        let start = page_offset.min(total_fetched);
        let end = (start + page_size).min(total_fetched);
        self.rows = self.rows[start..end].to_vec();
        self.meta.n = self.rows.len();
        self.meta.page_offset = Some(page_offset);
        self.meta.page_size = Some(page_size);
        self.meta.total_fetched = Some(total_fetched);
        self.meta.has_more = Some(end < total_fetched);
    }
}

const TRUNCATED_MARKER: &str = "…[truncated]";

/// Truncate a string to at most `max` bytes on a char boundary, appending a
/// visible marker. Returns the input unchanged when it already fits.
fn truncate_str_cell(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(TRUNCATED_MARKER.len());
    let mut end = budget.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATED_MARKER}", &s[..end])
}

/// Shrink `result` so its serialized JSON fits `max_bytes`.
///
/// SQL-BUG-11: the old implementation re-serialized the whole result after
/// every dropped row — O(rows × size). This version serializes each row
/// exactly once, tracks the running total, and drops whole rows from the end
/// in O(1) per row. If a single remaining row (or a single giant cell) still
/// overflows, string cells are truncated with a `…[truncated]` marker.
pub fn truncate_to_bytes(mut result: ColumnarResult, max_bytes: usize) -> ColumnarResult {
    // Fixed overhead: serialize with rows emptied (cols + meta skeleton).
    let skeleton_len = {
        let saved_n = result.meta.n;
        let rows = std::mem::take(&mut result.rows);
        result.meta.n = 0;
        let len = serde_json::to_string(&result).map(|s| s.len()).unwrap_or(0);
        result.rows = rows;
        result.meta.n = saved_n;
        len
    };

    // Per-row serialized lengths, each computed once. +1 accounts for the
    // comma separator between rows.
    let row_lens: Vec<usize> = result
        .rows
        .iter()
        .map(|r| serde_json::to_string(r).map(|s| s.len() + 1).unwrap_or(0))
        .collect();
    let mut total = skeleton_len + row_lens.iter().sum::<usize>();

    if total <= max_bytes {
        return result;
    }

    // Drop rows from the end, subtracting their known sizes — no re-serialize.
    while result.rows.len() > 1 && total > max_bytes {
        let dropped = row_lens[result.rows.len() - 1];
        result.rows.pop();
        total -= dropped;
        result.meta.truncated = true;
    }
    result.meta.n = result.rows.len();

    // One verification pass; digit-count drift in `meta.n` can nudge the size.
    if serde_json::to_string(&result).map(|s| s.len()).unwrap_or(0) > max_bytes
        && result.rows.len() > 1
    {
        result.rows.pop();
        result.meta.truncated = true;
        result.meta.n = result.rows.len();
    }

    // Single-row (or zero-row) overflow: shrink the largest string cells.
    // Each pass halves the biggest cell, so serialized size falls geometrically.
    for _ in 0..24 {
        let cur = serde_json::to_string(&result).map(|s| s.len()).unwrap_or(0);
        if cur <= max_bytes {
            break;
        }
        // Find the longest string cell across the remaining rows.
        let mut longest: Option<(usize, usize, usize)> = None; // (row, col, len)
        for (ri, row) in result.rows.iter().enumerate() {
            for (ci, cell) in row.iter().enumerate() {
                if let Value::String(s) = cell {
                    if longest.map(|(_, _, l)| s.len() > l).unwrap_or(true) {
                        longest = Some((ri, ci, s.len()));
                    }
                }
            }
        }
        let Some((ri, ci, len)) = longest else {
            break; // nothing truncatable left
        };
        if len <= TRUNCATED_MARKER.len() + 8 {
            break;
        }
        let target = (len / 2).max(TRUNCATED_MARKER.len() + 8);
        if let Value::String(s) = &mut result.rows[ri][ci] {
            *s = truncate_str_cell(s, target);
        }
        result.meta.truncated = true;
    }
    result.meta.n = result.rows.len();
    result
}

pub fn to_json_text(value: &impl Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| format!(r#"{{"error":"{e}"}}"#))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn serializes_limit_clamped_meta() {
        let result = ColumnarResult {
            cols: vec!["c".into()],
            rows: vec![vec![Value::from(1)]],
            meta: ColumnarMeta {
                n: 1,
                truncated: false,
                rows_affected: None,
                limit_injected: None,
                limit_clamped: Some(true),
                page_offset: None,
                page_size: None,
                has_more: None,
                total_fetched: None,
                redacted_columns: None,
            },
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"limit_clamped\":true"));
    }

    fn meta(n: usize) -> ColumnarMeta {
        ColumnarMeta {
            n,
            truncated: false,
            rows_affected: None,
            limit_injected: None,
            limit_clamped: None,
            page_offset: None,
            page_size: None,
            has_more: None,
            total_fetched: None,
            redacted_columns: None,
        }
    }

    #[test]
    fn deduped_row_keys_suffixes_duplicates() {
        let keys = deduped_row_keys(&["id".into(), "name".into(), "id".into(), "id".into()]);
        assert_eq!(keys, vec!["id", "name", "id#2", "id#3"]);
    }

    #[test]
    fn rows_format_keeps_duplicate_columns() {
        let result = ColumnarResult {
            cols: vec!["v".into(), "v".into()],
            rows: vec![vec![Value::from(1), Value::from(2)]],
            meta: meta(1),
        };
        let out = format_columnar(ResultFormat::Rows, result);
        let row = &out["rows"][0];
        assert_eq!(row["v"], Value::from(1));
        assert_eq!(row["v#2"], Value::from(2));
    }

    #[test]
    fn truncate_drops_rows_and_fits() {
        let result = ColumnarResult {
            cols: vec!["c".into()],
            rows: (0..50)
                .map(|i| vec![Value::String(format!("row-{i:03}-xxxxxxxxxx"))])
                .collect(),
            meta: meta(50),
        };
        let out = truncate_to_bytes(result, 300);
        assert!(serde_json::to_string(&out).unwrap().len() <= 300);
        assert!(out.meta.truncated);
        assert_eq!(out.meta.n, out.rows.len());
        assert!(out.rows.len() < 50);
    }

    #[test]
    fn truncate_single_giant_cell() {
        let giant = "x".repeat(20_000);
        let result = ColumnarResult {
            cols: vec!["c".into()],
            rows: vec![vec![Value::String(giant)]],
            meta: meta(1),
        };
        let out = truncate_to_bytes(result, 500);
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(
            serialized.len() <= 500,
            "giant cell must be truncated, got {} bytes",
            serialized.len()
        );
        assert!(out.meta.truncated);
        let cell = out.rows[0][0].as_str().unwrap();
        assert!(cell.contains("[truncated]"));
    }

    #[test]
    fn truncate_str_cell_respects_char_boundary() {
        let s = "é".repeat(100); // 2 bytes each
        let out = truncate_str_cell(&s, 51);
        assert!(out.len() <= 51 + TRUNCATED_MARKER.len());
        assert!(out.ends_with(TRUNCATED_MARKER));
    }

    #[test]
    fn apply_pagination_slices_rows() {
        let mut result = ColumnarResult {
            cols: vec!["id".into()],
            rows: (0..5).map(|i| vec![Value::from(i)]).collect(),
            meta: ColumnarMeta {
                n: 5,
                truncated: false,
                rows_affected: None,
                limit_injected: None,
                limit_clamped: None,
                page_offset: None,
                page_size: None,
                has_more: None,
                total_fetched: None,
                redacted_columns: None,
            },
        };
        result.apply_pagination(2, 2);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.meta.n, 2);
        assert_eq!(result.meta.page_offset, Some(2));
        assert_eq!(result.meta.page_size, Some(2));
        assert_eq!(result.meta.total_fetched, Some(5));
        assert_eq!(result.meta.has_more, Some(true));
    }
}
