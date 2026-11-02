use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum BindError {
    #[error("{0}")]
    Invalid(String),
}

/// Rich-typed parameter marker (SQL-GAP-06). Agents may pass a parameter as a
/// single-key tagged object to request a native bind instead of a plain
/// string:
///
/// - `{"$uuid": "…"}`     → native UUID (PG/MySQL/SQLite)
/// - `{"$date": "2026-01-31"}` → native DATE
/// - `{"$datetime": "2026-01-31T12:00:00"}` → native TIMESTAMP/DATETIME
/// - `{"$decimal": "123.45"}`    → native DECIMAL (PG/MySQL); SQLite has no
///   decimal type in sqlx, so it binds as TEXT (documented fallback — SQLite
///   NUMERIC affinity converts it in numeric contexts).
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
/// Returns `None` for ordinary (untagged) values, `Some(Err)` for malformed
/// markers or unknown `$`-tags so they fail loudly instead of being treated
/// as generic objects.
pub fn parse_rich_marker(p: &Value) -> Option<Result<RichParam, BindError>> {
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
            .ok_or_else(|| BindError::Invalid("$uuid parameter must be a string".into()))
            .and_then(|s| {
                Uuid::parse_str(s)
                    .map(RichParam::Uuid)
                    .map_err(|e| BindError::Invalid(format!("invalid $uuid parameter: {e}")))
            }),
        "$date" => val
            .as_str()
            .ok_or_else(|| BindError::Invalid("$date parameter must be a string".into()))
            .and_then(|s| {
                NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .map(RichParam::Date)
                    .map_err(|e| BindError::Invalid(format!("invalid $date parameter: {e}")))
            }),
        "$datetime" => val
            .as_str()
            .ok_or_else(|| BindError::Invalid("$datetime parameter must be a string".into()))
            .and_then(|s| parse_datetime_param(s).map(RichParam::DateTime)),
        "$decimal" => val
            .as_str()
            .ok_or_else(|| BindError::Invalid("$decimal parameter must be a string".into()))
            .and_then(|s| {
                s.parse::<BigDecimal>()
                    .map(RichParam::Decimal)
                    .map_err(|e| BindError::Invalid(format!("invalid $decimal parameter: {e}")))
            }),
        "$json" => Ok(RichParam::Json(val.clone())),
        other => Err(BindError::Invalid(format!(
            "unknown rich parameter marker '{other}' (supported: $uuid, $date, $datetime, $decimal, $json)"
        ))),
    };
    Some(parsed)
}

/// Accepts `YYYY-MM-DDTHH:MM:SS[.fff]` (or with a space) and RFC 3339 with an
/// offset (converted to UTC, stored naive-UTC).
fn parse_datetime_param(s: &str) -> Result<NaiveDateTime, BindError> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc).naive_utc());
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(dt);
        }
    }
    Err(BindError::Invalid(format!(
        "invalid $datetime parameter: '{s}' (expected RFC 3339 or YYYY-MM-DDTHH:MM:SS)"
    )))
}

pub fn bind_pg_params<'q>(
    sql: &'q str,
    params: &[Value],
) -> Result<sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>, BindError> {
    let mut q = sqlx::query(sql);
    for p in params {
        q = bind_pg_value(q, p)?;
    }
    Ok(q)
}

pub fn bind_mysql_params<'q>(
    sql: &'q str,
    params: &[Value],
) -> Result<sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>, BindError> {
    let mut q = sqlx::query(sql);
    for p in params {
        q = bind_mysql_value(q, p)?;
    }
    Ok(q)
}

pub fn bind_sqlite_params<'q>(
    sql: &'q str,
    params: &[Value],
) -> Result<sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>, BindError> {
    let mut q = sqlx::query(sql);
    for p in params {
        q = bind_sqlite_value(q, p)?;
    }
    Ok(q)
}

fn bind_pg_value<'q>(
    q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    p: &Value,
) -> Result<sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>, BindError> {
    if let Some(rich) = parse_rich_marker(p) {
        let rich = rich?;
        let q = match rich {
            RichParam::Uuid(u) => q.bind(u),
            RichParam::Date(d) => q.bind(d),
            RichParam::DateTime(dt) => q.bind(dt),
            RichParam::Decimal(d) => q.bind(d),
            RichParam::Json(v) => q.bind(v),
        };
        return Ok(q);
    }
    let q = match p {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => bind_number(q, n)?,
        Value::String(s) => q.bind(s.clone()),
        Value::Array(_) | Value::Object(_) => {
            return Err(BindError::Invalid(
                "array and object parameters are not supported (use a $json marker for JSON)"
                    .into(),
            ));
        }
    };
    Ok(q)
}

fn bind_mysql_value<'q>(
    q: sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>,
    p: &Value,
) -> Result<sqlx::query::Query<'q, sqlx::MySql, sqlx::mysql::MySqlArguments>, BindError> {
    if let Some(rich) = parse_rich_marker(p) {
        let rich = rich?;
        let q = match rich {
            RichParam::Uuid(u) => q.bind(u),
            RichParam::Date(d) => q.bind(d),
            RichParam::DateTime(dt) => q.bind(dt),
            RichParam::Decimal(d) => q.bind(d),
            RichParam::Json(v) => q.bind(v),
        };
        return Ok(q);
    }
    let q = match p {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => bind_number(q, n)?,
        Value::String(s) => q.bind(s.clone()),
        Value::Array(_) | Value::Object(_) => {
            return Err(BindError::Invalid(
                "array and object parameters are not supported (use a $json marker for JSON)"
                    .into(),
            ));
        }
    };
    Ok(q)
}

fn bind_sqlite_value<'q>(
    q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    p: &Value,
) -> Result<sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>, BindError> {
    if let Some(rich) = parse_rich_marker(p) {
        let rich = rich?;
        let q = match rich {
            RichParam::Uuid(u) => q.bind(u),
            RichParam::Date(d) => q.bind(d),
            RichParam::DateTime(dt) => q.bind(dt),
            // sqlx SQLite has no DECIMAL type: bind the exact decimal text
            // (documented fallback; NUMERIC affinity applies in comparisons).
            RichParam::Decimal(d) => q.bind(d.to_string()),
            // sqlx SQLite cannot bind serde_json::Value: bind compact JSON text.
            RichParam::Json(v) => q.bind(serde_json::to_string(&v).unwrap_or_default()),
        };
        return Ok(q);
    }
    let q = match p {
        Value::Null => q.bind(None::<String>),
        Value::Bool(b) => q.bind(*b),
        Value::Number(n) => bind_number(q, n)?,
        Value::String(s) => q.bind(s.clone()),
        Value::Array(_) | Value::Object(_) => {
            return Err(BindError::Invalid(
                "array and object parameters are not supported (use a $json marker for JSON)"
                    .into(),
            ));
        }
    };
    Ok(q)
}

fn bind_number<'q, DB>(
    q: sqlx::query::Query<'q, DB, <DB as sqlx::Database>::Arguments<'q>>,
    n: &serde_json::Number,
) -> Result<sqlx::query::Query<'q, DB, <DB as sqlx::Database>::Arguments<'q>>, BindError>
where
    DB: sqlx::Database,
    for<'a> i64: sqlx::Encode<'a, DB> + sqlx::Type<DB>,
    for<'a> f64: sqlx::Encode<'a, DB> + sqlx::Type<DB>,
{
    let q = if let Some(i) = n.as_i64() {
        q.bind(i)
    } else if let Some(f) = n.as_f64() {
        q.bind(f)
    } else if let Some(u) = n.as_u64() {
        q.bind(
            i64::try_from(u)
                .map_err(|_| BindError::Invalid(format!("parameter value out of range: {u}")))?,
        )
    } else {
        return Err(BindError::Invalid("invalid numeric parameter".into()));
    };
    Ok(q)
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
        assert_eq!(j, RichParam::Json(json!({"a": [1, 2]})));
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
    fn plain_object_param_still_rejected_by_binders() {
        let p = json!({"plain": "object"});
        assert!(bind_pg_params("SELECT $1", std::slice::from_ref(&p)).is_err());
    }
}
