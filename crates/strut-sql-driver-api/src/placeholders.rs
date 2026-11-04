//! Neutral `?` placeholder scanning shared by engine drivers.
//!
//! The driver API contract is that callers write positional `?` placeholders
//! on every engine; each driver rewrites them to whatever its wire protocol
//! needs. That rewrite must never touch a `?` inside a string literal,
//! quoted identifier or comment, so the scanner here is the single place
//! that knows those lexical rules.
//!
//! The scanner is aware of:
//!   * `'...'` strings — `''` doubling AND backslash escaping (MySQL,
//!     ClickHouse and PostgreSQL `E'...'` semantics),
//!   * `"..."` quoted identifiers and `` `...` `` backtick identifiers,
//!   * `--` and `#` line comments,
//!   * `/* */` block comments, including nested ones (PostgreSQL,
//!     ClickHouse),
//!   * `$tag$ ... $tag$` dollar-quoted strings (PostgreSQL).
//!
//! Delimiters are ASCII, so scanning byte-wise is safe for UTF-8 input:
//! multi-byte sequences never contain ASCII delimiter bytes.

/// Byte offsets of every `?` that appears in code position, in order.
pub fn question_mark_positions(sql: &str) -> Vec<usize> {
    let bytes = sql.as_bytes();
    let mut positions = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'\'' => i = skip_single_quoted(bytes, i),
            b'"' => i = skip_doubled(bytes, i, b'"'),
            b'`' => i = skip_doubled(bytes, i, b'`'),
            b'?' => {
                positions.push(i);
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => i = skip_line_comment(bytes, i),
            b'#' => i = skip_line_comment(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => i = skip_block_comment(bytes, i),
            b'$' => i = skip_dollar_quoted(bytes, i),
            _ => i += 1,
        }
    }
    positions
}

/// Number of `?` placeholders in code position.
pub fn count_question_mark_placeholders(sql: &str) -> usize {
    question_mark_positions(sql).len()
}

/// Replace every code-position `?` with the next string produced by
/// `replacement`, in order. Literals and comments are copied verbatim.
pub fn rewrite_question_mark_placeholders(
    sql: &str,
    mut replacement: impl FnMut(usize) -> String,
) -> String {
    let positions = question_mark_positions(sql);
    if positions.is_empty() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + positions.len() * 8);
    let mut last = 0usize;
    for (index, &pos) in positions.iter().enumerate() {
        out.push_str(&sql[last..pos]);
        out.push_str(&replacement(index));
        last = pos + 1;
    }
    out.push_str(&sql[last..]);
    out
}

/// Returns the index just past the closing quote of a `'...'` literal that
/// starts at `start`.
fn skip_single_quoted(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\'' => {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    return i + 1;
                }
            }
            _ => i += 1,
        }
    }
    bytes.len()
}

/// Skip a `"..."` or `` `...` `` region where the delimiter doubles to escape.
fn skip_doubled(bytes: &[u8], start: usize, delim: u8) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == delim {
            if bytes.get(i + 1) == Some(&delim) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

fn skip_line_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    let mut depth = 1u32;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// Skip a `$tag$ ... $tag$` string, or a single `$` when the tag is invalid.
fn skip_dollar_quoted(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if bytes.get(i) != Some(&b'$') {
        return start + 1;
    }
    let tag = &bytes[start..=i];
    let mut j = i + 1;
    while j + tag.len() <= bytes.len() {
        if &bytes[j..j + tag.len()] == tag {
            return j + tag.len();
        }
        j += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_placeholders_outside_literals() {
        assert_eq!(count_question_mark_placeholders("SELECT ? AS a, ? AS b"), 2);
        assert_eq!(
            count_question_mark_placeholders("SELECT '?' AS q, ? AS a"),
            1
        );
        assert_eq!(
            count_question_mark_placeholders("SELECT 1 -- ?\n, ? /* ? */"),
            1
        );
        assert_eq!(count_question_mark_placeholders("SELECT $$??$$, ?"), 1);
        assert_eq!(count_question_mark_placeholders("SELECT \"a?\" FROM t"), 0);
        assert_eq!(count_question_mark_placeholders("SELECT `a?` FROM t"), 0);
        assert_eq!(
            count_question_mark_placeholders("SELECT 'it''s ?', 'a\\'?' , ?"),
            1
        );
    }

    #[test]
    fn rewrites_only_code_positions() {
        let out = rewrite_question_mark_placeholders("SELECT '?' AS q, ? AS a", |i| {
            format!("{{p{i}:String}}")
        });
        assert_eq!(out, "SELECT '?' AS q, {p0:String} AS a");
    }

    #[test]
    fn nested_block_comments_are_skipped() {
        assert_eq!(count_question_mark_placeholders("/* a /* ? */ ? */ ?"), 1);
    }

    #[test]
    fn utf8_text_is_preserved() {
        let out = rewrite_question_mark_placeholders("SELECT 'héllo ?' AS x, ? AS y", |i| {
            format!("${}", i + 1)
        });
        assert_eq!(out, "SELECT 'héllo ?' AS x, $1 AS y");
    }
}
