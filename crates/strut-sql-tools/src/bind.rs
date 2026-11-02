use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;
use strut_sql_driver_api::SqlValue;
use uuid::Uuid;

/// Rich-typed parameter marker (SQL-GAP-06). Agents may pass a parameter as a
/// single-key tagged object to request a native bind instead of a plain
/// string:
///
/// - `{"$uuid": "…"}`     → native UUID (PG/MySQL/SQLite)
/// - `{"$date": "2026-01-31"}` → native DATE
/// - `{"$datetime": "2026-01-31T12:00:00"}` → native TIMESTAMP/DATETIME
/// - `{"$decimal": "123.45"}`    → native DECIMAL (PG/MySQL); SQLite has no
///   decimal type in sqlx, so it binds as TEXT (documented fallback).
/// - `{"$json": …}`       → native JSON/JSONB (PG/MySQL); SQLite binds the
///   compact JSON text (documented fallback).
#[derive(Debug, Clone, PartialEq)]
pub enum RichParam {
    Uuid(Uuid),
    Date(NaiveDate),
    DateTime(NaiveDateTime),
    Decimal(BigDecimal),
    Json(Value),
}

/// Parse a JSON parameter into a [`RichParam`] when it carries a rich marker.
pub fn parse_rich_marker(p: &Value) -> Option<Result<RichParam, String>> {
    let obj = p.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    let (key, val) = obj.iter().next()?;
    if !key.starts_with('$') {
        return None;
    }
    let parsed = match key.as_str() {
        "$uuid" => val
            .as_str()
            .ok_or_else(|| "$uuid parameter must be a string".to_string())
            .and_then(|s| {
                Uuid::parse_str(s)
                    .map(RichParam::Uuid)
                    .map_err(|e| format!("invalid $uuid parameter: {e}"))
            }),
        "$date" => val
            .as_str()
            .ok_or_else(|| "$date parameter must be a string".to_string())
            .and_then(|s| {
                NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .map(RichParam::Date)
                    .map_err(|e| format!("invalid $date parameter: {e}"))
            }),
        "$datetime" => val
            .as_str()
            .ok_or_else(|| "$datetime parameter must be a string".to_string())
            .and_then(|s| parse_datetime_param(s).map(RichParam::DateTime)),
        "$decimal" => val
            .as_str()
            .ok_or_else(|| "$decimal parameter must be a string".to_string())
            .and_then(|s| {
                s.parse::<BigDecimal>()
                    .map(RichParam::Decimal)
                    .map_err(|e| format!("invalid $decimal parameter: {e}"))
            }),
        "$json" => Ok(RichParam::Json(val.clone())),
        other => Err(format!(
            "unknown rich parameter marker '{other}' (supported: $uuid, $date, $datetime, $decimal, $json)"
        )),
    };
    Some(parsed)
}

fn parse_datetime_param(s: &str) -> Result<NaiveDateTime, String> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc).naive_utc());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt);
        }
    }
    Err(format!(
        "invalid $datetime parameter: '{s}' (expected RFC 3339 or YYYY-MM-DDTHH:MM:SS)"
    ))
}

/// Convert JSON tool parameters into driver-api bound values.
pub fn to_sql_values(params: &[Value]) -> Result<Vec<SqlValue>, String> {
    params.iter().map(json_to_sql_value).collect()
}

fn json_to_sql_value(p: &Value) -> Result<SqlValue, String> {
    if let Some(rich) = parse_rich_marker(p) {
        return Ok(match rich? {
            RichParam::Uuid(u) => SqlValue::Uuid(u),
            RichParam::Date(d) => SqlValue::Date(d),
            RichParam::DateTime(dt) => SqlValue::DateTime(dt),
            RichParam::Decimal(d) => SqlValue::BigDecimal(d),
            RichParam::Json(v) => SqlValue::Json(v),
        });
    }
    Ok(match p {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::I64(i)
            } else if let Some(f) = n.as_f64() {
                SqlValue::F64(f)
            } else {
                return Err("invalid numeric parameter".into());
            }
        }
        Value::String(s) => SqlValue::String(s.clone()),
        Value::Array(_) | Value::Object(_) => {
            return Err(
                "array and object parameters are not supported (use a $json marker for JSON)"
                    .into(),
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rich_marker_uuid_parses() {
        let p = json!({"$uuid": "550e8400-e29b-41d4-a716-446655440000"});
        let parsed = parse_rich_marker(&p).expect("marker").expect("valid");
        assert_eq!(
            parsed,
            RichParam::Uuid(Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap())
        );
    }

    #[test]
    fn rich_marker_date_and_datetime_parse() {
        let d = parse_rich_marker(&json!({"$date": "2026-01-31"}))
            .expect("marker")
            .expect("valid");
        assert_eq!(
            d,
            RichParam::Date(NaiveDate::from_ymd_opt(2026, 1, 31).unwrap())
        );
        let dt = parse_rich_marker(&json!({"$datetime": "2026-01-31T12:30:00"}))
            .expect("marker")
            .expect("valid");
        assert!(matches!(dt, RichParam::DateTime(_)));
        let rfc = parse_rich_marker(&json!({"$datetime": "2026-01-31T12:30:00+07:00"}))
            .expect("marker")
            .expect("valid");
        // +07:00 converted to naive UTC = 05:30.
        assert_eq!(
            rfc,
            RichParam::DateTime(
                NaiveDate::from_ymd_opt(2026, 1, 31)
                    .unwrap()
                    .and_hms_opt(5, 30, 0)
                    .unwrap()
            )
        );
    }

    #[test]
    fn rich_marker_decimal_and_json_parse() {
        let d = parse_rich_marker(&json!({"$decimal": "123.4500"}))
            .expect("marker")
            .expect("valid");
        assert!(matches!(d, RichParam::Decimal(_)));
        let j = parse_rich_marker(&json!({"$json": {"a": [1, 2]}}))
            .expect("marker")
            .expect("valid");
        assert!(matches!(j, RichParam::Json(_)));
    }

    #[test]
    fn rich_marker_rejects_malformed_and_unknown() {
        assert!(parse_rich_marker(&json!({"$uuid": "not-a-uuid"}))
            .expect("marker")
            .is_err());
        assert!(parse_rich_marker(&json!({"$date": "31/01/2026"}))
            .expect("marker")
            .is_err());
        assert!(parse_rich_marker(&json!({"$wat": "x"}))
            .expect("marker")
            .is_err());
    }

    #[test]
    fn plain_values_are_not_markers() {
        assert!(parse_rich_marker(&json!(42)).is_none());
        assert!(parse_rich_marker(&json!("$uuid")).is_none());
        assert!(parse_rich_marker(&json!({"name": "x"})).is_none());
        // Multi-key objects are never treated as markers.
        assert!(parse_rich_marker(
            &json!({"$uuid": "550e8400-e29b-41d4-a716-446655440000", "x": 1})
        )
        .is_none());
    }

    #[test]
    fn unknown_marker_rejected() {
        let err = parse_rich_marker(&json!({"$nope": 1}))
            .expect("marker")
            .unwrap_err();
        assert!(err.contains("unknown rich parameter marker"), "{err}");
    }

    #[test]
    fn plain_objects_and_arrays_rejected() {
        assert!(to_sql_values(&[json!({"a": 1})]).is_err());
        assert!(to_sql_values(&[json!([1, 2])]).is_err());
    }
}
