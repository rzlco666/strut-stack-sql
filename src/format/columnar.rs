use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    pub rows: Vec<HashMap<String, Value>>,
    pub meta: ColumnarMeta,
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
            let rows: Vec<HashMap<String, Value>> = columnar
                .rows
                .iter()
                .map(|row| {
                    let mut map = HashMap::new();
                    for (i, col) in columnar.cols.iter().enumerate() {
                        map.insert(col.clone(), row.get(i).cloned().unwrap_or(Value::Null));
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

pub fn truncate_to_bytes(mut result: ColumnarResult, max_bytes: usize) -> ColumnarResult {
    let json = serde_json::to_string(&result).unwrap_or_default();
    if json.len() <= max_bytes {
        return result;
    }

    while result.rows.len() > 1 {
        result.rows.pop();
        result.meta.truncated = true;
        result.meta.n = result.rows.len();
        let json = serde_json::to_string(&result).unwrap_or_default();
        if json.len() <= max_bytes {
            break;
        }
    }
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
