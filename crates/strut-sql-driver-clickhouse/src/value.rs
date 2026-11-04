//! ClickHouse type parsing and value mapping.
//!
//! Results arrive as `JSONCompactEachRowWithNamesAndTypes`, whose second line
//! carries the exact ClickHouse type of every column. That type string is the
//! source of truth for decoding: JSON renders 64-bit integers as strings,
//! decimals as numbers, dates as strings and complex types as nested JSON, so
//! a type-blind decoder would guess wrong.
//!
//! Parameters go the other way: scalars are sent as typed `{pN:Type}` HTTP
//! parameters (no string interpolation), while `NULL` and arrays are inlined
//! as escaped literals because ClickHouse has no parameter type that matches
//! an arbitrary array or a value of unknown type.

use bigdecimal::BigDecimal;
use chrono::{NaiveDate, NaiveDateTime};
use serde_json::Value as Json;
use std::str::FromStr;
use strut_sql_driver_api::SqlValue;
use uuid::Uuid;

/// A parsed ClickHouse type: the wrappers we must see through, plus the base
/// type name and its raw arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChType {
    Nullable(Box<ChType>),
    LowCardinality(Box<ChType>),
    Array(Box<ChType>),
    Tuple(Vec<ChType>),
    Map(Box<ChType>, Box<ChType>),
    Base { name: String, args: Vec<String> },
}

impl ChType {
    pub(crate) fn parse(text: &str) -> Self {
        let text = text.trim();
        let (name, args) = split_name_and_args(text);
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "nullable" => args
                .first()
                .map(|inner| ChType::Nullable(Box::new(ChType::parse(inner))))
                .unwrap_or(ChType::Base { name: lower, args }),
            "lowcardinality" => args
                .first()
                .map(|inner| ChType::LowCardinality(Box::new(ChType::parse(inner))))
                .unwrap_or(ChType::Base { name: lower, args }),
            "array" => args
                .first()
                .map(|inner| ChType::Array(Box::new(ChType::parse(inner))))
                .unwrap_or(ChType::Base { name: lower, args }),
            "map" if args.len() == 2 => ChType::Map(
                Box::new(ChType::parse(&args[0])),
                Box::new(ChType::parse(&args[1])),
            ),
            "tuple" => ChType::Tuple(
                args.iter()
                    .map(|a| ChType::parse(&strip_tuple_name(a)))
                    .collect(),
            ),
            _ => ChType::Base { name: lower, args },
        }
    }

    /// Whether the column can contain `NULL` (ClickHouse has no implicit
    /// nullability; only `Nullable(...)` and the `Nothing` type can).
    pub(crate) fn is_nullable(&self) -> bool {
        match self {
            ChType::Nullable(_) => true,
            ChType::LowCardinality(inner) => inner.is_nullable(),
            ChType::Base { name, .. } => name == "nothing",
            _ => false,
        }
    }
}

/// Split `Name` or `Name(arg, arg)` into the name and its top-level arguments.
fn split_name_and_args(text: &str) -> (String, Vec<String>) {
    let Some(open) = text.find('(') else {
        return (text.to_string(), Vec::new());
    };
    if !text.ends_with(')') {
        return (text.to_string(), Vec::new());
    }
    let name = text[..open].trim().to_string();
    let inner = &text[open + 1..text.len() - 1];
    (name, split_top_level(inner))
}

/// Drop the `name ` prefix of a named tuple element (`a Int64` → `Int64`).
fn strip_tuple_name(arg: &str) -> String {
    let arg = arg.trim();
    let Some(space) = arg.find(char::is_whitespace) else {
        return arg.to_string();
    };
    let head = &arg[..space];
    let rest = arg[space..].trim();
    if !rest.is_empty() && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        rest.to_string()
    } else {
        arg.to_string()
    }
}

/// Split on commas that are not nested inside parentheses or quoted strings.
fn split_top_level(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_single => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\'' if !in_double => {
                in_single = !in_single;
                current.push(c);
            }
            '"' if !in_single => {
                in_double = !in_double;
                current.push(c);
            }
            '(' if !in_single && !in_double => {
                depth += 1;
                current.push(c);
            }
            ')' if !in_single && !in_double => {
                depth -= 1;
                current.push(c);
            }
            ',' if !in_single && !in_double && depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// Decode one JSON cell using the column's ClickHouse type.
pub(crate) fn decode_value(ty: &ChType, value: &Json) -> SqlValue {
    if value.is_null() {
        return SqlValue::Null;
    }
    match ty {
        ChType::Nullable(inner) | ChType::LowCardinality(inner) => decode_value(inner, value),
        ChType::Array(inner) => match value {
            Json::Array(items) => {
                SqlValue::Array(items.iter().map(|item| decode_value(inner, item)).collect())
            }
            other => SqlValue::Json(other.clone()),
        },
        ChType::Map(_, _) => SqlValue::Json(value.clone()),
        ChType::Tuple(types) => match value {
            Json::Array(items) if items.len() == types.len() => SqlValue::Array(
                types
                    .iter()
                    .zip(items)
                    .map(|(ty, item)| decode_value(ty, item))
                    .collect(),
            ),
            // Named tuples render as JSON objects; keep them intact.
            other => SqlValue::Json(other.clone()),
        },
        ChType::Base { name, .. } => decode_base(name, value),
    }
}

fn decode_base(name: &str, value: &Json) -> SqlValue {
    match name {
        "nothing" => SqlValue::Null,
        "bool" | "boolean" => match value {
            Json::Bool(b) => SqlValue::Bool(*b),
            Json::Number(n) => SqlValue::Bool(n.as_i64().unwrap_or(0) != 0),
            Json::String(s) => SqlValue::Bool(s == "1" || s.eq_ignore_ascii_case("true")),
            _ => SqlValue::Null,
        },
        "int8" | "int16" | "int32" | "int64" | "uint8" | "uint16" | "uint32" | "uint64" => {
            decode_integer(value)
        }
        // 128/256-bit integers exceed i64 and the neutral value model has no
        // u64/i128 variant: keep the exact decimal text rather than truncating.
        "int128" | "int256" | "uint128" | "uint256" => SqlValue::String(text_of(value)),
        "float32" | "float64" => decode_float(value),
        "decimal" | "decimal32" | "decimal64" | "decimal128" | "decimal256" => {
            decode_decimal(value)
        }
        "uuid" => match text_of(value).parse::<Uuid>() {
            Ok(uuid) => SqlValue::Uuid(uuid),
            Err(_) => SqlValue::String(text_of(value)),
        },
        "date" | "date32" => match NaiveDate::parse_from_str(&text_of(value), "%Y-%m-%d") {
            Ok(date) => SqlValue::Date(date),
            Err(_) => SqlValue::String(text_of(value)),
        },
        "datetime" | "datetime64" => match parse_datetime(&text_of(value)) {
            Some(dt) => SqlValue::DateTime(dt),
            None => SqlValue::String(text_of(value)),
        },
        "json" | "object" => SqlValue::Json(value.clone()),
        "string" | "fixedstring" | "enum8" | "enum16" | "enum" | "ipv4" | "ipv6" => {
            SqlValue::String(text_of(value))
        }
        // Unknown/complex base type: preserve the payload losslessly instead
        // of inventing a scalar.
        _ => SqlValue::Json(value.clone()),
    }
}

fn text_of(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn decode_integer(value: &Json) -> SqlValue {
    match value {
        Json::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::I64(i),
            None => SqlValue::String(n.to_string()),
        },
        Json::String(s) => match s.parse::<i64>() {
            Ok(i) => SqlValue::I64(i),
            // UInt64 above i64::MAX: no lossless integer variant exists.
            Err(_) => SqlValue::String(s.clone()),
        },
        _ => SqlValue::Null,
    }
}

fn decode_float(value: &Json) -> SqlValue {
    match value {
        Json::Number(n) => match n.as_f64() {
            Some(f) => SqlValue::F64(f),
            None => SqlValue::String(n.to_string()),
        },
        Json::String(s) => match s.parse::<f64>() {
            Ok(f) => SqlValue::F64(f),
            Err(_) => SqlValue::String(s.clone()),
        },
        _ => SqlValue::Null,
    }
}

fn decode_decimal(value: &Json) -> SqlValue {
    let text = match value {
        Json::Number(n) => n.to_string(),
        Json::String(s) => s.clone(),
        _ => return SqlValue::Null,
    };
    match BigDecimal::from_str(&text) {
        Ok(d) => SqlValue::BigDecimal(d),
        Err(_) => SqlValue::String(text),
    }
}

fn parse_datetime(text: &str) -> Option<NaiveDateTime> {
    const FORMATS: &[&str] = &[
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
    ];
    FORMATS
        .iter()
        .find_map(|format| NaiveDateTime::parse_from_str(text, format).ok())
}

/// How one `?` placeholder is bound.
pub(crate) enum Binding {
    /// Literal text spliced into the SQL (already escaped).
    Inline(String),
    /// Typed ClickHouse HTTP parameter.
    Param {
        ch_type: &'static str,
        value: String,
    },
}

pub(crate) fn bind_parameter(value: &SqlValue) -> Binding {
    match value {
        SqlValue::Null => Binding::Inline("NULL".to_string()),
        SqlValue::Bool(b) => Binding::Param {
            ch_type: "Bool",
            value: if *b { "1" } else { "0" }.to_string(),
        },
        SqlValue::I64(n) => Binding::Param {
            ch_type: "Int64",
            value: n.to_string(),
        },
        SqlValue::F64(f) => Binding::Param {
            ch_type: "Float64",
            value: format_float(*f),
        },
        SqlValue::String(s) => Binding::Param {
            ch_type: "String",
            value: s.clone(),
        },
        SqlValue::Uuid(u) => Binding::Param {
            ch_type: "UUID",
            value: u.to_string(),
        },
        // No precision-free Decimal parameter type exists; ClickHouse casts a
        // String parameter to the column's Decimal type on use.
        SqlValue::BigDecimal(d) => Binding::Param {
            ch_type: "String",
            value: d.to_string(),
        },
        SqlValue::Date(d) => Binding::Param {
            ch_type: "Date",
            value: d.format("%Y-%m-%d").to_string(),
        },
        // DateTime64(9) keeps sub-second precision; ClickHouse casts it when
        // the compared column is a coarser DateTime.
        SqlValue::DateTime(dt) => Binding::Param {
            ch_type: "DateTime64(9)",
            value: dt.format("%Y-%m-%d %H:%M:%S%.9f").to_string(),
        },
        // ClickHouse strings are byte strings; non-UTF-8 bytes cannot travel
        // as an HTTP query parameter and are sent lossily.
        SqlValue::Binary(b) => Binding::Param {
            ch_type: "String",
            value: String::from_utf8_lossy(b).into_owned(),
        },
        // ClickHouse has no JSON parameter type; send the JSON text.
        SqlValue::Json(j) => Binding::Param {
            ch_type: "String",
            value: j.to_string(),
        },
        // Arrays carry element types the HTTP parameter syntax cannot express
        // generically, so they are inlined as escaped literals.
        SqlValue::Array(items) => Binding::Inline(array_literal(items)),
    }
}

fn format_float(f: f64) -> String {
    if f.is_nan() {
        "nan".to_string()
    } else if f.is_infinite() {
        if f.is_sign_negative() {
            "-inf".to_string()
        } else {
            "inf".to_string()
        }
    } else {
        f.to_string()
    }
}

fn array_literal(items: &[SqlValue]) -> String {
    let body = items.iter().map(literal).collect::<Vec<_>>().join(", ");
    format!("[{body}]")
}

fn literal(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => "NULL".to_string(),
        SqlValue::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        SqlValue::I64(n) => n.to_string(),
        SqlValue::F64(f) => format_float(*f),
        SqlValue::String(s) => quoted(s),
        SqlValue::Uuid(u) => quoted(&u.to_string()),
        SqlValue::BigDecimal(d) => d.to_string(),
        SqlValue::Date(d) => quoted(&d.format("%Y-%m-%d").to_string()),
        SqlValue::DateTime(dt) => quoted(&dt.format("%Y-%m-%d %H:%M:%S%.9f").to_string()),
        SqlValue::Binary(b) => quoted(&String::from_utf8_lossy(b)),
        SqlValue::Json(j) => quoted(&j.to_string()),
        SqlValue::Array(items) => array_literal(items),
    }
}

/// Escape a ClickHouse string literal. ClickHouse treats backslash as an
/// escape character inside single quotes, so both `\` and `'` are escaped.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            _ => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_wrapped_types() {
        assert_eq!(
            ChType::parse("Nullable(String)"),
            ChType::Nullable(Box::new(ChType::Base {
                name: "string".into(),
                args: vec![]
            }))
        );
        assert_eq!(
            ChType::parse("Array(Nullable(Int64))"),
            ChType::Array(Box::new(ChType::Nullable(Box::new(ChType::Base {
                name: "int64".into(),
                args: vec![]
            }))))
        );
        assert!(ChType::parse("Nullable(Decimal(18, 2))").is_nullable());
        assert!(!ChType::parse("Decimal(18, 2)").is_nullable());
    }

    #[test]
    fn decodes_64bit_integers_from_json_strings() {
        let ty = ChType::parse("UInt64");
        assert_eq!(decode_value(&ty, &json!("42")), SqlValue::I64(42));
        assert_eq!(
            decode_value(&ty, &json!("18446744073709551615")),
            SqlValue::String("18446744073709551615".into())
        );
    }

    #[test]
    fn decodes_temporal_and_decimal_types() {
        assert_eq!(
            decode_value(&ChType::parse("Date"), &json!("2024-01-02")),
            SqlValue::Date(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap())
        );
        assert_eq!(
            decode_value(
                &ChType::parse("DateTime64(3)"),
                &json!("2024-01-02 03:04:05.123")
            ),
            SqlValue::DateTime(
                NaiveDate::from_ymd_opt(2024, 1, 2)
                    .unwrap()
                    .and_hms_milli_opt(3, 4, 5, 123)
                    .unwrap()
            )
        );
        assert_eq!(
            decode_value(&ChType::parse("Decimal(18, 2)"), &json!(1.25)),
            SqlValue::BigDecimal(BigDecimal::from_str("1.25").unwrap())
        );
    }

    #[test]
    fn decodes_arrays_and_maps() {
        let ty = ChType::parse("Array(Nullable(Int64))");
        assert_eq!(
            decode_value(&ty, &json!([1, null])),
            SqlValue::Array(vec![SqlValue::I64(1), SqlValue::Null])
        );
        let map = ChType::parse("Map(String, UInt8)");
        assert_eq!(
            decode_value(&map, &json!({"a": 1})),
            SqlValue::Json(json!({"a": 1}))
        );
    }

    #[test]
    fn decodes_named_tuple_as_json() {
        let ty = ChType::parse("Tuple(a Int64, b String)");
        assert_eq!(
            ty,
            ChType::Tuple(vec![
                ChType::Base {
                    name: "int64".into(),
                    args: vec![]
                },
                ChType::Base {
                    name: "string".into(),
                    args: vec![]
                },
            ])
        );
        assert_eq!(
            decode_value(&ty, &json!({"a": 1, "b": "x"})),
            SqlValue::Json(json!({"a": 1, "b": "x"}))
        );
        assert_eq!(
            decode_value(&ty, &json!([1, "x"])),
            SqlValue::Array(vec![SqlValue::I64(1), SqlValue::String("x".into())])
        );
    }

    #[test]
    fn binds_scalars_as_typed_parameters() {
        match bind_parameter(&SqlValue::I64(7)) {
            Binding::Param { ch_type, value } => {
                assert_eq!(ch_type, "Int64");
                assert_eq!(value, "7");
            }
            Binding::Inline(_) => panic!("expected a parameter binding"),
        }
        match bind_parameter(&SqlValue::DateTime(
            NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(3, 4, 5)
                .unwrap(),
        )) {
            Binding::Param { ch_type, value } => {
                assert_eq!(ch_type, "DateTime64(9)");
                assert_eq!(value, "2024-01-02 03:04:05.000000000");
            }
            Binding::Inline(_) => panic!("expected a parameter binding"),
        }
    }

    #[test]
    fn inlines_null_and_arrays_as_escaped_literals() {
        match bind_parameter(&SqlValue::Null) {
            Binding::Inline(text) => assert_eq!(text, "NULL"),
            Binding::Param { .. } => panic!("NULL must be inlined"),
        }
        match bind_parameter(&SqlValue::Array(vec![
            SqlValue::I64(1),
            SqlValue::String("it's \\ here".into()),
        ])) {
            Binding::Inline(text) => assert_eq!(text, "[1, 'it\\'s \\\\ here']"),
            Binding::Param { .. } => panic!("arrays must be inlined"),
        }
    }

    #[test]
    fn quotes_literals_against_injection() {
        assert_eq!(quoted("a'b\\c"), "'a\\'b\\\\c'");
    }
}
