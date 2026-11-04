use std::borrow::Cow;
use std::collections::BTreeSet;
use strut_sql_driver_api::{DriverError, SqlValue};

/// Translate neutral binds without touching literals, quoted identifiers or comments.
/// Native numbered binds are accepted, but never mixed with neutral binds.
pub(crate) fn rewrite<'a>(sql: &'a str, params: &[SqlValue]) -> Result<Cow<'a, str>, DriverError> {
    let param_count = params.len();
    if param_count > 2100 {
        return Err(error("SQL Server accepts at most 2100 parameters"));
    }
    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut copied = 0;
    let mut neutral_count = 0;
    let mut native = BTreeSet::new();
    let mut output = String::new();
    while index < bytes.len() {
        match bytes[index] {
            b'\'' | b'"' | b'[' => {
                let close = if bytes[index] == b'[' {
                    b']'
                } else {
                    bytes[index]
                };
                index += 1;
                loop {
                    if index == bytes.len() {
                        return Err(error("unterminated SQL literal or identifier"));
                    }
                    if bytes[index] == close {
                        index += 1;
                        if bytes.get(index) == Some(&close) {
                            index += 1;
                        } else {
                            break;
                        }
                    } else {
                        index += 1;
                    }
                }
            }
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index += 2;
                while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let mut depth = 1usize;
                index += 2;
                while depth > 0 {
                    if index >= bytes.len() {
                        return Err(error("unterminated SQL block comment"));
                    }
                    match (bytes[index], bytes.get(index + 1)) {
                        (b'/', Some(b'*')) => {
                            depth += 1;
                            index += 2;
                        }
                        (b'*', Some(b'/')) => {
                            depth -= 1;
                            index += 2;
                        }
                        _ => index += 1,
                    }
                }
            }
            b'?' => {
                neutral_count += 1;
                output.push_str(&sql[copied..index]);
                push_parameter(&mut output, neutral_count, params.get(neutral_count - 1));
                index += 1;
                copied = index;
            }
            b'@' => {
                let start = index;
                index += 1;
                while index < bytes.len() && identifier(bytes[index]) {
                    index += 1;
                }
                let token = &bytes[start + 1..index];
                if (start == 0 || !identifier(bytes[start - 1]))
                    && token.len() >= 2
                    && matches!(token[0], b'P' | b'p')
                    && token[1..].iter().all(u8::is_ascii_digit)
                {
                    let number = std::str::from_utf8(&token[1..])
                        .ok()
                        .and_then(|n| n.parse::<usize>().ok())
                        .ok_or_else(|| error("invalid SQL Server parameter number"))?;
                    if number == 0 || number > param_count {
                        return Err(error(
                            "SQL Server parameter number is outside the supplied bind range",
                        ));
                    }
                    native.insert(number);
                    output.push_str(&sql[copied..start]);
                    push_parameter(&mut output, number, params.get(number - 1));
                    copied = index;
                }
            }
            _ => index += 1,
        }
    }
    if neutral_count > 0 {
        if !native.is_empty() {
            return Err(error("cannot mix neutral and native SQL Server parameters"));
        }
        if neutral_count != param_count {
            return Err(error(
                "SQL placeholder count does not match supplied parameters",
            ));
        }
        output.push_str(&sql[copied..]);
        Ok(Cow::Owned(output))
    } else {
        if native.len() != param_count {
            return Err(error(
                "SQL placeholder count does not match supplied parameters",
            ));
        }
        if output.is_empty() {
            Ok(Cow::Borrowed(sql))
        } else {
            output.push_str(&sql[copied..]);
            Ok(Cow::Owned(output))
        }
    }
}

fn push_parameter(output: &mut String, number: usize, param: Option<&SqlValue>) {
    let decimal_cast =
        matches!(param, Some(SqlValue::BigDecimal(value)) if value.fractional_digit_count() == 38);
    if decimal_cast {
        output.push_str("CAST(");
    }
    output.push_str("@P");
    output.push_str(&number.to_string());
    if decimal_cast {
        output.push_str(" AS decimal(38,38))");
    }
}

fn identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'@' | b'$' | b'#') || byte >= 128
}

fn error(message: &str) -> DriverError {
    DriverError::Execution(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(sql: &str, count: usize) -> Result<Cow<'_, str>, DriverError> {
        super::rewrite(sql, &vec![SqlValue::Null; count])
    }

    #[test]
    fn preserves_quoted_text_comments_identifiers_and_unicode() {
        let sql = "SELECT N'it''s ? 猫', \"a\"\"?\", [b]]?], ? -- ? @P8\n/* ? /* ? */ ? */ + ?";
        assert_eq!(
            rewrite(sql, 2).unwrap(),
            "SELECT N'it''s ? 猫', \"a\"\"?\", [b]]?], @P1 -- ? @P8\n/* ? /* ? */ ? */ + @P2"
        );
    }

    #[test]
    fn validates_native_numbering_and_bind_counts() {
        assert_eq!(
            rewrite("SELECT @P2, @p1, @P2, @@SPID", 2).unwrap(),
            "SELECT @P2, @P1, @P2, @@SPID"
        );
        assert_eq!(
            rewrite("SELECT identifier@P1, @@P1", 0).unwrap(),
            "SELECT identifier@P1, @@P1"
        );
        for (sql, count) in [
            ("SELECT ?", 0),
            ("SELECT 1", 1),
            ("SELECT @P2", 2),
            ("SELECT @P0", 1),
            ("SELECT @P3", 2),
            ("SELECT ?, @P1", 1),
            ("SELECT @P1suffix", 1),
        ] {
            assert!(rewrite(sql, count).is_err(), "{sql}");
        }
    }

    #[test]
    fn rejects_unterminated_lexical_constructs() {
        for sql in [
            "SELECT '?' /*",
            "SELECT 'abc",
            "SELECT [abc",
            "SELECT \"abc",
            "/* /* */",
        ] {
            assert!(rewrite(sql, 0).is_err());
        }
    }

    #[test]
    fn high_scale_decimal_casts_only_real_placeholders() {
        let params = [SqlValue::BigDecimal(
            "0.00000000000000000000000000000000000001".parse().unwrap(),
        )];
        assert_eq!(
            super::rewrite("SELECT ?, '?' /* ? */", &params).unwrap(),
            "SELECT CAST(@P1 AS decimal(38,38)), '?' /* ? */"
        );
        assert_eq!(
            super::rewrite("SELECT @p1 + @P1", &params).unwrap(),
            "SELECT CAST(@P1 AS decimal(38,38)) + CAST(@P1 AS decimal(38,38))"
        );
    }
}
