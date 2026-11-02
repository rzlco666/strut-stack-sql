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

pub fn format_columnar(format: ResultFormat, mut columnar: ColumnarResult) -> serde_json::Value {
    // PRD-F2-01: duplicate column names never overwrite silently; make the
    // collision visible in meta as well as in the Rows-mode key suffix.
    let duplicate_warnings = duplicate_column_warnings(&columnar.cols);
    if !duplicate_warnings.is_empty() {
        columnar
            .meta
            .warnings
            .get_or_insert_with(Vec::new)
            .extend(duplicate_warnings);
    }
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

fn duplicate_column_warnings(cols: &[String]) -> Vec<String> {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for col in cols {
        *counts.entry(col.as_str()).or_insert(0) += 1;
    }
    let mut warnings: Vec<String> = counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(name, n)| {
            format!("duplicate column name '{name}' returned {n} times; values kept in position (rows mode suffixes '#2', '#3', …)")
        })
        .collect();
    warnings.sort();
    warnings
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
    /// Non-fatal capability degradations for this session (PRD-F2-11).
    /// Absent when every requested capability is effective.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warnings: Option<Vec<String>>,
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
                warnings: None,
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

fn serialized_len(result: &ColumnarResult) -> usize {
    serde_json::to_string(result)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
}

/// Shrink string cells until the result fits, with a visible marker on every
/// truncated cell. Guarantees that a single row cannot exceed `max_bytes`
/// on its own (assuming the column-name skeleton fits).
fn shrink_cells_to_fit(result: &mut ColumnarResult, max_bytes: usize) {
    let ncols = result
        .rows
        .iter()
        .map(|r| r.len())
        .max()
        .unwrap_or(0)
        .max(1);
    let mut cap = max_bytes / ncols;
    if cap <= TRUNCATED_MARKER.len() {
        cap = TRUNCATED_MARKER.len() + 1;
    }
    for _ in 0..32 {
        for row in &mut result.rows {
            for cell in row.iter_mut() {
                if let Value::String(s) = cell {
                    if s.len() > cap {
                        *s = truncate_str_cell(s, cap);
                        result.meta.truncated = true;
                    }
                }
            }
        }
        result.meta.n = result.rows.len();
        if serialized_len(result) <= max_bytes {
            return;
        }
        let next = cap / 2;
        if next <= TRUNCATED_MARKER.len() {
            // Everything truncatable is already minimal; drop the row(s)
            // rather than return an over-budget payload.
            result.rows.clear();
            result.meta.n = 0;
            result.meta.truncated = true;
            return;
        }
        cap = next;
    }
}

/// Shrink `result` so its serialized JSON fits `max_bytes`.
///
/// PRD-F2-02: row counts are found with a binary search over per-row sizes
/// (each row serialized exactly once → O(n log n), no repeated whole-result
/// re-serialization). A single remaining row that still overflows gets
/// per-cell truncation with a `…[truncated]` marker, so one cell cannot
/// break the byte ceiling by itself.
pub fn truncate_to_bytes(mut result: ColumnarResult, max_bytes: usize) -> ColumnarResult {
    // Skeleton = result with zero rows (cols + meta).
    let skeleton_len = {
        let saved_n = result.meta.n;
        let rows = std::mem::take(&mut result.rows);
        result.meta.n = 0;
        let len = serialized_len(&result);
        result.rows = rows;
        result.meta.n = saved_n;
        len
    };

    if serialized_len(&result) <= max_bytes {
        return result;
    }

    let row_lens: Vec<usize> = result
        .rows
        .iter()
        .map(|r| serde_json::to_string(r).map(|s| s.len()).unwrap_or(0))
        .collect();
    let mut prefix = Vec::with_capacity(row_lens.len() + 1);
    prefix.push(0usize);
    for len in &row_lens {
        prefix.push(prefix.last().copied().unwrap_or(0) + len);
    }
    // Serialized size with the first `k` rows: skeleton + row payloads +
    // commas between them (`k - 1`); the skeleton already holds `[]`.
    let size_for = |k: usize| -> usize {
        skeleton_len
            .saturating_add(prefix[k])
            .saturating_add(k.saturating_sub(1))
    };

    let n = result.rows.len();
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if size_for(mid) <= max_bytes {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }

    if lo == 0 {
        // Not even one full row fits: keep the first row and truncate cells.
        result.rows.truncate(1);
        result.meta.truncated = true;
        result.meta.n = result.rows.len();
        shrink_cells_to_fit(&mut result, max_bytes);
        result.meta.n = result.rows.len();
        return result;
    }

    if lo < n {
        result.rows.truncate(lo);
        result.meta.truncated = true;
    }
    result.meta.n = result.rows.len();
    // Exact verification; meta.n digit-count drift is at most a few bytes.
    while result.rows.len() > 1 && serialized_len(&result) > max_bytes {
        result.rows.pop();
        result.meta.truncated = true;
        result.meta.n = result.rows.len();
    }
    if serialized_len(&result) > max_bytes {
        shrink_cells_to_fit(&mut result, max_bytes);
        result.meta.n = result.rows.len();
    }
    result
}

pub fn to_json_text(value: &impl Serialize) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }).to_string())
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
                warnings: None,
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
            warnings: None,
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
        let warnings = out["meta"]["warnings"].as_array().expect("warnings");
        assert!(
            warnings.iter().any(|w| w
                .as_str()
                .unwrap_or_default()
                .contains("duplicate column name 'v'")),
            "{warnings:?}"
        );
    }

    #[test]
    fn columnar_format_warns_about_duplicate_columns() {
        let result = ColumnarResult {
            cols: vec!["id".into(), "id".into(), "name".into()],
            rows: vec![vec![Value::from(1), Value::from(2), Value::from("a")]],
            meta: meta(1),
        };
        let out = format_columnar(ResultFormat::Columnar, result);
        assert_eq!(out["cols"].as_array().unwrap().len(), 3);
        let warnings = out["meta"]["warnings"].as_array().expect("warnings");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].as_str().unwrap().contains("'id'"));
    }

    #[test]
    fn unique_columns_do_not_add_warnings() {
        let result = ColumnarResult {
            cols: vec!["a".into(), "b".into()],
            rows: vec![vec![Value::from(1), Value::from(2)]],
            meta: meta(1),
        };
        let out = format_columnar(ResultFormat::Columnar, result);
        assert!(out["meta"].get("warnings").is_none());
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
    fn truncate_wide_single_row_shrinks_every_cell() {
        let giant = "y".repeat(5_000);
        let result = ColumnarResult {
            cols: (0..8).map(|i| format!("c{i}")).collect(),
            rows: vec![vec![Value::String(giant); 8]],
            meta: meta(1),
        };
        let out = truncate_to_bytes(result, 2_000);
        let serialized = serde_json::to_string(&out).unwrap();
        assert!(
            serialized.len() <= 2_000,
            "one wide row must fit the ceiling, got {} bytes",
            serialized.len()
        );
        assert!(out.meta.truncated);
        assert_eq!(out.meta.n, 1);
        assert_eq!(out.rows[0].len(), 8, "no cell may be dropped");
        for cell in &out.rows[0] {
            assert!(
                cell.as_str().unwrap().contains("[truncated]"),
                "each oversized cell keeps a marker"
            );
        }
    }

    #[test]
    fn truncate_row_count_is_the_largest_prefix_that_fits() {
        // 40 identical rows. Verify the binary-search result is exactly the
        // largest prefix that serializes under the ceiling.
        let result = ColumnarResult {
            cols: vec!["c".into()],
            rows: (0..40)
                .map(|i| vec![Value::String(format!("value-{i:03}-{}", "x".repeat(20)))])
                .collect(),
            meta: meta(40),
        };
        let max = 600;
        let out = truncate_to_bytes(result, max);
        assert!(serde_json::to_string(&out).unwrap().len() <= max);
        assert!(out.rows.len() < 40);
        // Adding the next row back must exceed the ceiling.
        let mut probe = out.clone();
        let omitted = vec![Value::String(format!(
            "value-{:03}-{}",
            out.rows.len(),
            "x".repeat(20)
        ))];
        probe.rows.push(omitted);
        probe.meta.n = probe.rows.len();
        assert!(
            serde_json::to_string(&probe).unwrap().len() > max,
            "truncation must not stop short of the maximum fitting prefix"
        );
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
                warnings: None,
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

#[cfg(test)]
mod escaping_tests {
    use super::*;
    use serde::ser::{Error as _, Serializer};
    use serde::Serialize;

    struct Failing;

    impl Serialize for Failing {
        fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(S::Error::custom("quote\"backslash\\newline\n"))
        }
    }

    #[test]
    fn json_fallback_escapes_error_text() {
        let text = to_json_text(&Failing);
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["error"], "quote\"backslash\\newline\n");
    }
}
