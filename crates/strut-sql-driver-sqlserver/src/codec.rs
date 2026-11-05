use bigdecimal::{BigDecimal, ToPrimitive, Zero};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use std::fmt::Write;
use strut_sql_driver_api::{ColumnMeta, DriverError, SqlValue};
use tiberius::{numeric::Numeric, Column, ColumnData, ColumnType, FromSqlOwned, Query, Row};

pub(crate) fn bind<'a>(query: &mut Query<'a>, params: &'a [SqlValue]) -> Result<(), DriverError> {
    for param in params {
        match param {
            SqlValue::Null => query.bind(None::<&str>),
            SqlValue::Bool(value) => query.bind(*value),
            SqlValue::I64(value) => query.bind(*value),
            SqlValue::F64(value) => {
                if !value.is_finite() {
                    return Err(execution(
                        "SQL Server floating-point parameters must be finite",
                    ));
                }
                query.bind(*value);
            }
            SqlValue::String(value) => query.bind(value.as_str()),
            SqlValue::Uuid(value) => query.bind(*value),
            SqlValue::BigDecimal(value) => {
                let number = decimal(value)?;
                if value.fractional_digit_count() == 38 {
                    // Tiberius emits precision 39 for Numeric(scale=38) with no
                    // integer part. The lexical rewriter adds an exact decimal
                    // cast so the wire string never becomes an implicit float.
                    query.bind(value.to_plain_string());
                } else {
                    query.bind(number);
                }
            }
            SqlValue::Date(value) => {
                check_year(value.year())?;
                query.bind(*value);
            }
            SqlValue::DateTime(value) => {
                check_year(value.year())?;
                if value.nanosecond() >= 1_000_000_000 {
                    return Err(execution(
                        "SQL Server does not support leap-second parameters",
                    ));
                }
                query.bind(*value);
            }
            SqlValue::Binary(value) => query.bind(value.as_slice()),
            SqlValue::Json(value) => query.bind(value.to_string()),
            SqlValue::Array(_) => {
                return Err(DriverError::Unsupported(
                    "SQL Server has no native array parameter type; use JSON explicitly".into(),
                ))
            }
        }
    }
    Ok(())
}

fn check_year(year: i32) -> Result<(), DriverError> {
    if !(1..=9999).contains(&year) {
        return Err(execution(
            "SQL Server date parameters require a year from 1 through 9999",
        ));
    }
    Ok(())
}

/// T-SQL declarations mirroring the wire types used by [`bind`].
///
/// Estimated plans are only produced for top-level batches, which cannot carry
/// TDS bind values. Declaring variables preserves the parameterized shape of
/// the statement for the optimizer without interpolating any bound value into
/// the SQL text.
pub(crate) fn declarations(params: &[SqlValue]) -> Result<String, DriverError> {
    if params.is_empty() {
        return Ok(String::new());
    }
    let mut sql = String::from("DECLARE ");
    for (index, param) in params.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        write!(sql, "@P{} {}", index + 1, declare_type(param)?)
            .map_err(|_| execution("SQL Server plan parameter declaration failed"))?;
    }
    sql.push(';');
    Ok(sql)
}

fn declare_type(value: &SqlValue) -> Result<String, DriverError> {
    Ok(match value {
        SqlValue::Null => "nvarchar(4000)".into(),
        SqlValue::Bool(_) => "bit".into(),
        SqlValue::I64(_) => "bigint".into(),
        SqlValue::F64(value) => {
            if !value.is_finite() {
                return Err(execution(
                    "SQL Server floating-point parameters must be finite",
                ));
            }
            "float(53)".into()
        }
        SqlValue::String(value) => text_type(value.len()),
        SqlValue::Uuid(_) => "uniqueidentifier".into(),
        SqlValue::BigDecimal(value) => {
            if value.fractional_digit_count() == 38 {
                // `bind` sends the scale-38 workaround as text, so the plan must
                // compile against the same parameter type.
                "nvarchar(4000)".into()
            } else {
                let number = decimal(value)?;
                format!("numeric({},{})", number.precision(), number.scale())
            }
        }
        SqlValue::Date(_) => "date".into(),
        SqlValue::DateTime(value) => {
            if value.nanosecond() >= 1_000_000_000 {
                return Err(execution(
                    "SQL Server does not support leap-second parameters",
                ));
            }
            "datetime2".into()
        }
        SqlValue::Binary(value) => {
            if value.len() <= 8000 {
                "varbinary(8000)".into()
            } else {
                "varbinary(max)".into()
            }
        }
        SqlValue::Json(value) => text_type(value.to_string().len()),
        SqlValue::Array(_) => {
            return Err(DriverError::Unsupported(
                "SQL Server has no native array parameter type; use JSON explicitly".into(),
            ))
        }
    })
}

fn text_type(bytes: usize) -> String {
    if bytes <= 4000 {
        "nvarchar(4000)".into()
    } else {
        "nvarchar(max)".into()
    }
}

/// Avoid Tiberius' infallible BigDecimal binding path, which panics on overflow.
fn decimal(value: &BigDecimal) -> Result<Numeric, DriverError> {
    if value.is_zero() {
        return Ok(Numeric::new_with_scale(0, 0));
    }
    let scale = value.fractional_digit_count();
    let digits = value.digits();
    if scale > 38 || digits > 38 || (scale < 0 && i128::from(digits) - i128::from(scale) > 38) {
        return Err(execution(
            "decimal parameter exceeds SQL Server precision or scale (38)",
        ));
    }
    let normalized;
    let value = if scale < 0 {
        normalized = value.with_scale(0);
        &normalized
    } else {
        value
    };
    let (mantissa, scale) = value.as_bigint_and_exponent();
    let mantissa = mantissa
        .to_i128()
        .ok_or_else(|| execution("decimal parameter exceeds SQL Server precision (38)"))?;
    Ok(Numeric::new_with_scale(mantissa, scale as u8))
}

pub(crate) fn columns(columns: &[Column]) -> Vec<ColumnMeta> {
    columns
        .iter()
        .map(|column| ColumnMeta {
            name: column.name().to_owned(),
            type_name: type_name(column.column_type()).to_owned(),
            // Tiberius does not expose the TDS column nullability flags. Never infer
            // NOT NULL from an observed non-null row (including zero-row results).
            nullable: true,
        })
        .collect()
}

fn type_name(kind: ColumnType) -> &'static str {
    match kind {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 | ColumnType::Intn => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money => "money",
        ColumnType::Money4 => "smallmoney",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigBinary => "binary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
}

pub(crate) fn row(row: Row) -> Result<Vec<SqlValue>, DriverError> {
    row.into_iter().map(value).collect()
}

fn value(value: ColumnData<'static>) -> Result<SqlValue, DriverError> {
    Ok(match value {
        ColumnData::U8(value) => value
            .map(|v| SqlValue::I64(i64::from(v)))
            .unwrap_or(SqlValue::Null),
        ColumnData::I16(value) => value
            .map(|v| SqlValue::I64(i64::from(v)))
            .unwrap_or(SqlValue::Null),
        ColumnData::I32(value) => value
            .map(|v| SqlValue::I64(i64::from(v)))
            .unwrap_or(SqlValue::Null),
        ColumnData::I64(value) => value.map(SqlValue::I64).unwrap_or(SqlValue::Null),
        ColumnData::F32(value) => value
            .map(|v| SqlValue::F64(f64::from(v)))
            .unwrap_or(SqlValue::Null),
        ColumnData::F64(value) => value.map(SqlValue::F64).unwrap_or(SqlValue::Null),
        ColumnData::Bit(value) => value.map(SqlValue::Bool).unwrap_or(SqlValue::Null),
        ColumnData::String(value) => value
            .map(|v| SqlValue::String(v.into_owned()))
            .unwrap_or(SqlValue::Null),
        ColumnData::Guid(value) => value.map(SqlValue::Uuid).unwrap_or(SqlValue::Null),
        ColumnData::Binary(value) => value
            .map(|v| SqlValue::Binary(v.into_owned()))
            .unwrap_or(SqlValue::Null),
        ColumnData::Numeric(value) => value
            .map(|v| SqlValue::BigDecimal(BigDecimal::new(v.value().into(), i64::from(v.scale()))))
            .unwrap_or(SqlValue::Null),
        ColumnData::Xml(value) => value
            .map(|v| SqlValue::String(v.to_string()))
            .unwrap_or(SqlValue::Null),
        value @ (ColumnData::DateTime(_)
        | ColumnData::SmallDateTime(_)
        | ColumnData::DateTime2(_)) => decode::<NaiveDateTime>(value)?
            .map(SqlValue::DateTime)
            .unwrap_or(SqlValue::Null),
        value @ ColumnData::Date(_) => decode::<NaiveDate>(value)?
            .map(SqlValue::Date)
            .unwrap_or(SqlValue::Null),
        value @ ColumnData::Time(_) => decode::<NaiveTime>(value)?
            .map(|v| SqlValue::String(v.to_string()))
            .unwrap_or(SqlValue::Null),
        value @ ColumnData::DateTimeOffset(_) => decode::<DateTime<FixedOffset>>(value)?
            .map(|v| SqlValue::String(v.to_rfc3339()))
            .unwrap_or(SqlValue::Null),
    })
}

fn decode<T: FromSqlOwned>(value: ColumnData<'static>) -> Result<Option<T>, DriverError> {
    T::from_sql_owned(value).map_err(|_| execution("SQL Server result value conversion failed"))
}

fn execution(message: &str) -> DriverError {
    DriverError::Execution(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn decimal_bounds_are_fallible_and_negative_scales_are_exact() {
        for input in [
            "1e-39",
            "1e1000000000",
            "123456789012345678901234567890123456789",
            "-170141183460469231731687303715884105728",
        ] {
            assert!(
                decimal(&BigDecimal::from_str(input).unwrap()).is_err(),
                "{input}"
            );
        }
        let number = decimal(&BigDecimal::from_str("12e3").unwrap()).unwrap();
        assert_eq!(number.value(), 12000);
        assert_eq!(number.scale(), 0);
        assert!(
            decimal(&BigDecimal::from_str("99999999999999999999999999999999999999").unwrap())
                .is_ok()
        );
    }

    #[test]
    fn codec_preserves_integer_float_binary_decimal_and_null_values() {
        assert_eq!(value(ColumnData::I32(Some(42))).unwrap(), SqlValue::I64(42));
        assert_eq!(
            value(ColumnData::F32(Some(2.75))).unwrap(),
            SqlValue::F64(2.75)
        );
        assert_eq!(
            value(ColumnData::Binary(Some(vec![0, 255].into()))).unwrap(),
            SqlValue::Binary(vec![0, 255])
        );
        assert_eq!(
            value(ColumnData::Numeric(Some(Numeric::new_with_scale(1234, 2)))).unwrap(),
            SqlValue::BigDecimal(BigDecimal::from_str("12.34").unwrap())
        );
        assert_eq!(value(ColumnData::I16(None)).unwrap(), SqlValue::Null);
    }

    #[test]
    fn rejects_values_that_cannot_be_bound_without_loss_or_panics() {
        for value in [
            SqlValue::F64(f64::NAN),
            SqlValue::Array(vec![]),
            SqlValue::Date(NaiveDate::from_ymd_opt(0, 1, 1).unwrap()),
        ] {
            let params = [value];
            let mut query = Query::new("SELECT @P1");
            assert!(bind(&mut query, &params).is_err());
        }
    }
}
