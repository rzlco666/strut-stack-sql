use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use ipnetwork::IpNetwork;
use serde_json::Value;
use sqlx::{mysql::MySqlRow, postgres::PgRow, sqlite::SqliteRow, Column, Row, TypeInfo, ValueRef};
use uuid::Uuid;

pub fn bigdecimal_to_json(v: &BigDecimal) -> Value {
    if v.fractional_digit_count() == 0 {
        if let Ok(i) = v.to_string().parse::<i64>() {
            if (-(1i64 << 53)..=(1i64 << 53)).contains(&i) {
                return Value::from(i);
            }
        }
    }
    if let Ok(f) = v.to_string().parse::<f64>() {
        if f.is_finite() {
            return Value::from(f);
        }
    }
    Value::String(v.to_string())
}

fn bytes_to_hex(v: &[u8]) -> String {
    v.iter().map(|b| format!("{b:02x}")).collect()
}

fn mysql_type_is_bool(type_name: &str) -> bool {
    type_name.eq_ignore_ascii_case("BOOL") || type_name.eq_ignore_ascii_case("BOOLEAN")
}

/// MySQL character / string-ish types that should decode as UTF-8 JSON strings.
pub fn mysql_type_is_text(type_name: &str) -> bool {
    let t = type_name.to_ascii_uppercase();
    matches!(
        t.as_str(),
        "VARCHAR"
            | "CHAR"
            | "TEXT"
            | "TINYTEXT"
            | "MEDIUMTEXT"
            | "LONGTEXT"
            | "ENUM"
            | "SET"
            | "JSON"
            | "VAR_STRING"
            | "STRING"
    ) || t.contains("CHAR")
        || t.contains("TEXT")
}

/// MySQL binary types that should stay hex-encoded for agents.
pub fn mysql_type_is_binary(type_name: &str) -> bool {
    let t = type_name.to_ascii_uppercase();
    matches!(
        t.as_str(),
        "BLOB"
            | "TINYBLOB"
            | "MEDIUMBLOB"
            | "LONGBLOB"
            | "BINARY"
            | "VARBINARY"
            | "BIT"
            | "GEOMETRY"
            | "POINT"
            | "LINESTRING"
            | "POLYGON"
    ) || t.contains("BLOB")
        || t.contains("BINARY")
}

fn utf8_from_bytes(v: Vec<u8>) -> String {
    String::from_utf8(v).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Decode a MySQL cell as human-readable text (schema tools + execute path).
pub fn mysql_decode_text_by_index(row: &MySqlRow, index: usize) -> Option<String> {
    if let Ok(v) = row.try_get::<String, _>(index) {
        return Some(v);
    }
    if let Ok(v) = row.try_get::<&str, _>(index) {
        return Some(v.to_string());
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
        return Some(utf8_from_bytes(v));
    }
    None
}

/// Decode a named MySQL column as human-readable text.
pub fn mysql_decode_text_by_name(row: &MySqlRow, name: &str) -> Option<String> {
    if let Ok(v) = row.try_get::<String, _>(name) {
        return Some(v);
    }
    if let Ok(v) = row.try_get::<&str, _>(name) {
        return Some(v.to_string());
    }
    if let Ok(v) = row.try_get::<Vec<u8>, _>(name) {
        return Some(utf8_from_bytes(v));
    }
    None
}

pub fn decode_mysql_cell(row: &MySqlRow, index: usize) -> Result<Value, sqlx::Error> {
    let raw = row.try_get_raw(index)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }

    if let Ok(v) = row.try_get::<i64, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<u64, _>(index) {
        if v <= (1u64 << 53) {
            return Ok(Value::from(v));
        }
        return Ok(Value::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<i32, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<u32, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<f64, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<f32, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<BigDecimal, _>(index) {
        return Ok(bigdecimal_to_json(&v));
    }
    if let Ok(v) = row.try_get::<NaiveDate, _>(index) {
        return Ok(Value::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<NaiveTime, _>(index) {
        return Ok(Value::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<NaiveDateTime, _>(index) {
        return Ok(Value::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<DateTime<Utc>, _>(index) {
        return Ok(Value::String(v.to_rfc3339()));
    }

    let type_name = row.column(index).type_info().name();

    if mysql_type_is_text(type_name) {
        if let Some(s) = mysql_decode_text_by_index(row, index) {
            return Ok(Value::String(s));
        }
    }

    if mysql_type_is_binary(type_name) {
        if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
            return Ok(Value::String(bytes_to_hex(&v)));
        }
    }

    if mysql_type_is_bool(type_name) {
        if let Ok(v) = row.try_get::<bool, _>(index) {
            return Ok(Value::Bool(v));
        }
    }

    if let Ok(v) = row.try_get::<String, _>(index) {
        return Ok(Value::String(v));
    }
    // Fallback: prefer UTF-8 when bytes are valid text; otherwise hex (true binary).
    if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
        return Ok(match String::from_utf8(v) {
            Ok(s) => Value::String(s),
            Err(e) => Value::String(bytes_to_hex(e.as_bytes())),
        });
    }
    Ok(Value::Null)
}

/// PostgreSQL types that should always surface as JSON strings for agents.
pub fn pg_type_is_textish(type_name: &str) -> bool {
    let t = type_name.to_ascii_lowercase();
    matches!(
        t.as_str(),
        "varchar"
            | "bpchar"
            | "text"
            | "name"
            | "citext"
            | "xml"
            | "uuid"
            | "inet"
            | "cidr"
            | "macaddr"
            | "macaddr8"
            | "date"
            | "time"
            | "timetz"
            | "timestamp"
            | "timestamptz"
            | "interval"
            | "json"
            | "jsonb"
            | "ltree"
            | "lquery"
            | "ltxtquery"
    ) || t.contains("char")
        || t.contains("text")
}

pub fn decode_pg_cell(row: &PgRow, index: usize) -> Result<Value, sqlx::Error> {
    let raw = row.try_get_raw(index)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }

    if let Ok(v) = row.try_get::<i64, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<i32, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<f64, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<BigDecimal, _>(index) {
        return Ok(bigdecimal_to_json(&v));
    }
    if let Ok(v) = row.try_get::<bool, _>(index) {
        return Ok(Value::Bool(v));
    }
    if let Ok(v) = row.try_get::<Uuid, _>(index) {
        return Ok(Value::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<DateTime<Utc>, _>(index) {
        return Ok(Value::String(v.to_rfc3339()));
    }
    if let Ok(v) = row.try_get::<NaiveDateTime, _>(index) {
        return Ok(Value::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<NaiveDate, _>(index) {
        return Ok(Value::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<NaiveTime, _>(index) {
        return Ok(Value::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<IpNetwork, _>(index) {
        return Ok(Value::String(v.to_string()));
    }
    if let Ok(v) = row.try_get::<Value, _>(index) {
        return Ok(v);
    }
    if let Ok(v) = row.try_get::<String, _>(index) {
        return Ok(Value::String(v));
    }

    let type_name = row.column(index).type_info().name();
    if pg_type_is_textish(type_name) {
        if let Ok(v) = row.try_get::<&str, _>(index) {
            return Ok(Value::String(v.to_string()));
        }
        if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
            return Ok(Value::String(utf8_from_bytes(v)));
        }
    }

    // Last resort: never silently Null a non-NULL cell when bytes are available.
    if let Ok(v) = row.try_get::<Vec<u8>, _>(index) {
        return Ok(match String::from_utf8(v) {
            Ok(s) => Value::String(s),
            Err(e) => Value::String(bytes_to_hex(e.as_bytes())),
        });
    }
    Ok(Value::Null)
}

pub fn decode_sqlite_cell(row: &SqliteRow, index: usize) -> Result<Value, sqlx::Error> {
    let raw = row.try_get_raw(index)?;
    if raw.is_null() {
        return Ok(Value::Null);
    }

    if let Ok(v) = row.try_get::<i64, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<f64, _>(index) {
        return Ok(Value::from(v));
    }
    if let Ok(v) = row.try_get::<bool, _>(index) {
        return Ok(Value::Bool(v));
    }
    if let Ok(v) = row.try_get::<String, _>(index) {
        return Ok(Value::String(v));
    }
    Ok(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigdecimal_integer_fits_as_number() {
        let v: BigDecimal = "184".parse().unwrap();
        assert_eq!(bigdecimal_to_json(&v), Value::from(184));
    }

    #[test]
    fn bigdecimal_decimal_as_float() {
        let v: BigDecimal = "107744.5".parse().unwrap();
        let json = bigdecimal_to_json(&v);
        assert!(json.is_number());
    }

    #[test]
    fn mysql_type_is_bool_recognizes_boolean_types() {
        assert!(mysql_type_is_bool("BOOL"));
        assert!(mysql_type_is_bool("BOOLEAN"));
        assert!(!mysql_type_is_bool("BIGINT"));
        assert!(!mysql_type_is_bool("TINYINT"));
    }

    #[test]
    fn mysql_type_is_text_recognizes_string_types() {
        for t in [
            "VARCHAR",
            "CHAR",
            "TEXT",
            "TINYTEXT",
            "MEDIUMTEXT",
            "LONGTEXT",
            "ENUM",
            "SET",
            "JSON",
            "VAR_STRING",
            "varchar",
        ] {
            assert!(mysql_type_is_text(t), "{t} should be text");
            assert!(!mysql_type_is_binary(t), "{t} should not be binary");
        }
    }

    #[test]
    fn mysql_type_is_binary_recognizes_blob_types() {
        for t in ["BLOB", "TINYBLOB", "VARBINARY", "BINARY", "BIT", "LONGBLOB"] {
            assert!(mysql_type_is_binary(t), "{t} should be binary");
            assert!(!mysql_type_is_text(t), "{t} should not be text");
        }
    }

    #[test]
    fn bytes_to_hex_encodes_ascii() {
        assert_eq!(bytes_to_hex(b"edoc"), "65646f63");
    }

    #[test]
    fn utf8_from_bytes_valid_and_lossy() {
        assert_eq!(utf8_from_bytes(b"edoc_pid_dev".to_vec()), "edoc_pid_dev");
        let bad = vec![0xff, 0xfe, b'a'];
        let s = utf8_from_bytes(bad);
        assert!(s.contains('a'));
    }

    #[test]
    fn pg_type_is_textish_covers_agent_pain_types() {
        for t in [
            "uuid",
            "timestamptz",
            "timestamp",
            "inet",
            "cidr",
            "citext",
            "text",
            "varchar",
            "macaddr",
        ] {
            assert!(pg_type_is_textish(t), "{t} should be textish");
        }
        assert!(!pg_type_is_textish("int4"));
        assert!(!pg_type_is_textish("bytea"));
    }
}
