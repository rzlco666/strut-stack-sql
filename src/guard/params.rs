use serde_json::Value;

use crate::db::EngineKind;
use crate::guard::GuardError;

// SQL-BUG-13 fix: there is exactly ONE scanner in this module now. Counting,
// validation and the PostgreSQL `?` → `$N` rewrite all derive from the same
// scan, so they can never drift apart again.
//
// The scanner is aware of:
//   * '...' strings — '' escaping AND backslash escaping (MySQL semantics;
//     also covers PostgreSQL E'...' escape strings),
//   * E'...', B'...', X'...' prefixed string literals (PostgreSQL),
//   * "..." quoted identifiers and `...` backtick identifiers,
//   * -- line comments, # line comments (MySQL), /* */ block comments
//     including PostgreSQL-style NESTED block comments,
//   * $tag$ ... $tag$ dollar-quoted strings (PostgreSQL),
//   * $1, $2, ... numbered placeholders (only meaningful outside literals).
//
// Delimiters are all ASCII, so scanning byte-wise is safe for UTF-8 input:
// multi-byte sequences never contain ASCII delimiter bytes.

#[derive(Debug, Default)]
struct ScanOutcome {
    qmark_positions: Vec<usize>,
    /// Largest `$N` index seen in code position (0 = none).
    pg_max_index: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanState {
    Normal,
    /// Inside '...' — backslash escapes the next byte (MySQL / E'...').
    SingleQuote,
    DoubleQuote,
    Backtick,
    LineComment,
    /// Nesting depth of /* */ (PostgreSQL allows nested block comments).
    BlockComment(u32),
    DollarQuote,
}

fn scan_sql(sql: &str) -> ScanOutcome {
    let bytes = sql.as_bytes();
    let mut out = ScanOutcome::default();
    let mut i = 0usize;
    let mut state = ScanState::Normal;
    let mut dollar_tag: Vec<u8> = Vec::new();

    while i < bytes.len() {
        let b = bytes[i];
        match state {
            ScanState::Normal => match b {
                b'\'' => state = ScanState::SingleQuote,
                b'"' => state = ScanState::DoubleQuote,
                b'`' => state = ScanState::Backtick,
                b'?' => out.qmark_positions.push(i),
                b'-' if bytes.get(i + 1) == Some(&b'-') => {
                    state = ScanState::LineComment;
                    i += 1;
                }
                b'#' => state = ScanState::LineComment,
                b'/' if bytes.get(i + 1) == Some(&b'*') => {
                    state = ScanState::BlockComment(1);
                    i += 1;
                }
                b'$' => {
                    if let Some(n) = parse_pg_number(bytes, i) {
                        out.pg_max_index = out.pg_max_index.max(n);
                        i += 1 + digits_len(n);
                        continue;
                    }
                    if let Some((tag, end)) = parse_dollar_tag(bytes, i) {
                        dollar_tag = tag;
                        state = ScanState::DollarQuote;
                        i = end;
                        continue;
                    }
                }
                // Prefixed string literals: E'...', B'...', X'...' (and the
                // lowercase forms). The prefix letter must not be the tail of
                // an identifier (e.g. `name'x'` is not a literal start in
                // practice, but `x` glued to a name is a column reference).
                b'E' | b'e' | b'B' | b'b' | b'X' | b'x' | b'U' | b'u'
                    if bytes.get(i + 1) == Some(&b'\'') && !prev_is_ident(bytes, i) =>
                {
                    state = ScanState::SingleQuote;
                    i += 1;
                }
                _ => {}
            },
            ScanState::SingleQuote => match b {
                // Backslash escapes the following byte: \' \\ \n ... This is
                // MySQL behaviour for all '...' strings and PostgreSQL
                // behaviour for E'...' strings.
                b'\\' => {
                    i += 1; // also consume the escaped byte (loop adds 1 more)
                }
                b'\'' => {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 1; // '' escaped quote — stay inside the string
                    } else {
                        state = ScanState::Normal;
                    }
                }
                _ => {}
            },
            ScanState::DoubleQuote => {
                if b == b'"' {
                    if bytes.get(i + 1) == Some(&b'"') {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::Backtick => {
                if b == b'`' {
                    if bytes.get(i + 1) == Some(&b'`') {
                        i += 1;
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::LineComment => {
                if b == b'\n' {
                    state = ScanState::Normal;
                }
            }
            ScanState::BlockComment(depth) => {
                if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    state = ScanState::BlockComment(depth + 1);
                    i += 1;
                } else if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    i += 1;
                    state = if depth <= 1 {
                        ScanState::Normal
                    } else {
                        ScanState::BlockComment(depth - 1)
                    };
                }
            }
            ScanState::DollarQuote => {
                if b == b'$' && dollar_close_matches(bytes, i, &dollar_tag) {
                    i += dollar_tag.len() + 1; // consume "tag$" after the '$'
                    dollar_tag = Vec::new();
                    state = ScanState::Normal;
                }
            }
        }
        i += 1;
    }
    out
}

fn prev_is_ident(bytes: &[u8], i: usize) -> bool {
    i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_' || bytes[i - 1] == b'$')
}

fn digits_len(mut n: usize) -> usize {
    let mut len = 1;
    while n >= 10 {
        n /= 10;
        len += 1;
    }
    len
}

/// True when the bytes at `start` are exactly `$<tag>$`.
fn dollar_close_matches(bytes: &[u8], start: usize, tag: &[u8]) -> bool {
    let after = start + 1;
    bytes.len() > after + tag.len()
        && &bytes[after..after + tag.len()] == tag
        && bytes[after + tag.len()] == b'$'
}

/// Returns `(tag, end_index_exclusive)` for an opening `$tag$` or `$$`.
/// A tag may not start with a digit (those are `$N` placeholders).
fn parse_dollar_tag(bytes: &[u8], start: usize) -> Option<(Vec<u8>, usize)> {
    if bytes.get(start) != Some(&b'$') {
        return None;
    }
    let mut j = start + 1;
    while j < bytes.len() && bytes[j] != b'$' {
        let ch = bytes[j];
        if !ch.is_ascii_alphanumeric() && ch != b'_' {
            return None;
        }
        if j == start + 1 && ch.is_ascii_digit() {
            return None;
        }
        j += 1;
    }
    if j >= bytes.len() || j == start + 1 && bytes.get(j) != Some(&b'$') {
        return None;
    }
    if bytes[j] != b'$' {
        return None;
    }
    Some((bytes[start + 1..j].to_vec(), j + 1))
}

/// Parses `$<digits>` at `start`; returns the placeholder index.
fn parse_pg_number(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'$') {
        return None;
    }
    let mut j = start + 1;
    if j >= bytes.len() || !bytes[j].is_ascii_digit() {
        return None;
    }
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    std::str::from_utf8(&bytes[start + 1..j]).ok()?.parse().ok()
}

/// Count `?` placeholders outside string literals and comments.
pub fn count_question_mark_placeholders(sql: &str) -> usize {
    scan_sql(sql).qmark_positions.len()
}

/// Largest PostgreSQL `$N` placeholder index outside literals (0 = none).
pub fn count_pg_numbered_placeholders(sql: &str) -> usize {
    scan_sql(sql).pg_max_index
}

pub fn placeholder_count(sql: &str, engine: EngineKind) -> usize {
    let scan = scan_sql(sql);
    if !scan.qmark_positions.is_empty() {
        return scan.qmark_positions.len();
    }
    if engine == EngineKind::Postgres {
        return scan.pg_max_index;
    }
    0
}

pub fn validate_param_count(
    sql: &str,
    params: &[Value],
    engine: EngineKind,
) -> Result<(), GuardError> {
    let scan = scan_sql(sql);
    let qm = scan.qmark_positions.len();
    let pg_num = scan.pg_max_index;

    if qm > 0 && pg_num > 0 {
        return Err(GuardError::Denied(
            "mixing ? and $N placeholders is not allowed".into(),
        ));
    }

    let expected = if qm > 0 {
        qm
    } else if engine == EngineKind::Postgres {
        pg_num
    } else if pg_num > 0 {
        return Err(GuardError::Denied(
            "PostgreSQL $N placeholders are only supported on PostgreSQL sources".into(),
        ));
    } else {
        0
    };

    if expected == 0 && !params.is_empty() {
        return Err(GuardError::Denied(
            "unexpected params for SQL without placeholders".into(),
        ));
    }
    if params.len() != expected {
        return Err(GuardError::Denied(format!(
            "placeholder count mismatch: expected {expected}, got {}",
            params.len()
        )));
    }
    Ok(())
}

/// Replace `?` with `$1`, `$2`, … outside string literals (PostgreSQL
/// execution path). Uses the same single scanner as counting, so a `?`
/// inside a literal/comment is never rewritten and a real placeholder is
/// never missed.
pub fn rewrite_placeholders_for_postgres(sql: &str) -> String {
    let scan = scan_sql(sql);
    if scan.qmark_positions.is_empty() {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len() + scan.qmark_positions.len() * 2);
    let mut last = 0usize;
    for (idx, &pos) in scan.qmark_positions.iter().enumerate() {
        out.push_str(&sql[last..pos]);
        out.push('$');
        out.push_str(&(idx + 1).to_string());
        last = pos + 1;
    }
    out.push_str(&sql[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_question_marks_outside_strings() {
        assert_eq!(
            count_question_mark_placeholders("SELECT * FROM t WHERE id = ?"),
            1
        );
        assert_eq!(
            count_question_mark_placeholders("SELECT '?' AS q, ? AS p"),
            1
        );
        assert_eq!(count_question_mark_placeholders("SELECT 1"), 0);
    }

    #[test]
    fn rewrites_question_marks_for_postgres() {
        assert_eq!(
            rewrite_placeholders_for_postgres("SELECT * FROM t WHERE id = ? AND x = ?"),
            "SELECT * FROM t WHERE id = $1 AND x = $2"
        );
        assert_eq!(
            rewrite_placeholders_for_postgres("SELECT '?' AS q WHERE id = ?"),
            "SELECT '?' AS q WHERE id = $1"
        );
    }

    #[test]
    fn validates_param_count() {
        validate_param_count("SELECT 1", &[], EngineKind::Mysql).unwrap();
        validate_param_count("SELECT ? AS v", &[Value::from(1)], EngineKind::Mysql).unwrap();
        validate_param_count("SELECT ? AS v", &[Value::from(1)], EngineKind::Sqlite).unwrap();
        let err =
            validate_param_count("SELECT 1", &[Value::from(1)], EngineKind::Mysql).unwrap_err();
        assert!(err.to_string().contains("unexpected params"));
        let err = validate_param_count("SELECT ?", &[], EngineKind::Mysql).unwrap_err();
        assert!(err.to_string().contains("mismatch"));
    }

    #[test]
    fn rejects_mixed_placeholder_styles() {
        let err = validate_param_count(
            "SELECT * FROM t WHERE id = $1 AND x = ?",
            &[Value::from(1), Value::from(2)],
            EngineKind::Postgres,
        )
        .unwrap_err();
        assert!(err.to_string().contains("mixing"));
    }

    #[test]
    fn pg_native_numbered_placeholders() {
        assert_eq!(
            count_pg_numbered_placeholders("SELECT * FROM t WHERE id = $1 AND x = $2"),
            2
        );
        validate_param_count(
            "SELECT * FROM t WHERE id = $1",
            &[Value::from(42)],
            EngineKind::Postgres,
        )
        .unwrap();
        let err =
            validate_param_count("SELECT $1", &[Value::from(1)], EngineKind::Sqlite).unwrap_err();
        assert!(err.to_string().contains("PostgreSQL"));
    }

    // --- SQL-BUG-13 regression tests: one scanner, all lexical corners ---

    #[test]
    fn backslash_escaped_quote_keeps_string_open() {
        // MySQL: 'a\' ? b' is ONE string; only the trailing ? is a placeholder.
        let sql = r"SELECT 'a\' ? b' AS s, ? AS p";
        assert_eq!(count_question_mark_placeholders(sql), 1);
        assert_eq!(
            rewrite_placeholders_for_postgres(sql),
            r"SELECT 'a\' ? b' AS s, $1 AS p"
        );
    }

    #[test]
    fn pg_escape_string_e_prefix() {
        // PostgreSQL E'...' — \' is an escaped quote, ? inside stays literal.
        let sql = r"SELECT E'it\'s a ?' AS s, ? AS p";
        assert_eq!(count_question_mark_placeholders(sql), 1);
        assert_eq!(
            rewrite_placeholders_for_postgres(sql),
            r"SELECT E'it\'s a ?' AS s, $1 AS p"
        );
    }

    #[test]
    fn doubled_quote_escape_still_works() {
        let sql = "SELECT 'it''s ?' AS s, ? AS p";
        assert_eq!(count_question_mark_placeholders(sql), 1);
    }

    #[test]
    fn bit_and_hex_literals_are_strings() {
        assert_eq!(count_question_mark_placeholders("SELECT B'0101', ?"), 1);
        assert_eq!(count_question_mark_placeholders("SELECT X'1F', ?"), 1);
        // A ? glued inside a quoted hex literal is not a placeholder.
        assert_eq!(count_question_mark_placeholders("SELECT X'?', ?"), 1);
    }

    #[test]
    fn mysql_hash_comment_hides_placeholders() {
        let sql = "SELECT ? # a ? in a comment\n, ? AS p";
        assert_eq!(count_question_mark_placeholders(sql), 2);
        assert_eq!(
            rewrite_placeholders_for_postgres(sql),
            "SELECT $1 # a ? in a comment\n, $2 AS p"
        );
    }

    #[test]
    fn nested_block_comments_hide_placeholders() {
        // PostgreSQL nested block comments: the first */ only closes the
        // inner comment, so both ? stay hidden until the outer */.
        let sql = "SELECT /* outer /* inner ? */ still ? */ ? AS p";
        assert_eq!(count_question_mark_placeholders(sql), 1);
        assert_eq!(
            rewrite_placeholders_for_postgres(sql),
            "SELECT /* outer /* inner ? */ still ? */ $1 AS p"
        );
    }

    #[test]
    fn dollar_quoted_strings_hide_placeholders() {
        assert_eq!(count_question_mark_placeholders("SELECT $$a?b$$, ?"), 1);
        assert_eq!(
            count_question_mark_placeholders("SELECT $tag$x?$tag$, ?"),
            1
        );
        assert_eq!(
            rewrite_placeholders_for_postgres("SELECT $tag$x?$tag$, ?"),
            "SELECT $tag$x?$tag$, $1"
        );
    }

    #[test]
    fn line_and_block_comments_hide_placeholders() {
        assert_eq!(count_question_mark_placeholders("SELECT ? -- ?\n, ?"), 2);
        assert_eq!(count_question_mark_placeholders("SELECT /* ? */ ?"), 1);
    }

    #[test]
    fn pg_numbered_inside_literals_ignored() {
        assert_eq!(
            count_pg_numbered_placeholders("SELECT '$9' AS s, $2 AS p"),
            2
        );
        assert_eq!(count_pg_numbered_placeholders("SELECT $$ $9 $$, $1"), 1);
    }

    #[test]
    fn count_and_rewrite_always_agree() {
        // The core BUG-13 property: for tricky SQL, the number of $N tokens
        // the rewrite produces equals the counted ? placeholders.
        let cases = [
            r"SELECT 'a\' ?' , ? , E'x\' ?', ?",
            "SELECT /* a /* ? */ ? */ ?, # ?\n ?",
            "SELECT $t$?$t$, ?, '$$', ?",
            "SELECT \"?\", `?`, ?",
        ];
        for sql in cases {
            let counted = count_question_mark_placeholders(sql);
            let rewritten = rewrite_placeholders_for_postgres(sql);
            assert_eq!(
                count_pg_numbered_placeholders(&rewritten),
                counted,
                "sql={sql} rewritten={rewritten}"
            );
        }
    }
}
