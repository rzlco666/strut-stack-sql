/// Redact secrets from database URLs in error messages and logs.
///
/// Parsing rules (SQL-SEC-09):
/// - The authority is split at the LAST `@`, so passwords containing `@`
///   stay inside the userinfo and are fully redacted.
/// - The password is everything after the FIRST `:` inside the userinfo,
///   so passwords containing `:` are fully redacted too.
/// - Query parameter values whose names look secret-bearing
///   (`password`, `passwd`, `pwd`, `sslpassword`, `secret`, `token`, …)
///   are redacted as well.
pub fn redact_url(url: &str) -> String {
    let (head, query) = match url.split_once('?') {
        Some((h, q)) => (h, Some(q)),
        None => (url, None),
    };

    let mut out = redact_authority(head);
    if let Some(q) = query {
        out.push('?');
        out.push_str(&redact_query(q));
    }
    out
}

fn redact_authority(head: &str) -> String {
    let Some(scheme_end) = head.find("://") else {
        return head.to_string();
    };
    let scheme = &head[..scheme_end + 3];
    let authority = &head[scheme_end + 3..];
    // Authority ends at the first path separator.
    let (authority, path) = match authority.find('/') {
        Some(i) => (&authority[..i], &authority[i..]),
        None => (authority, ""),
    };
    // Userinfo ends at the LAST '@' (passwords may contain '@').
    let Some(at) = authority.rfind('@') else {
        return head.to_string();
    };
    let userinfo = &authority[..at];
    let host_part = &authority[at..]; // includes '@'
                                      // Password starts at the FIRST ':' inside userinfo.
    let Some(colon) = userinfo.find(':') else {
        return head.to_string();
    };
    let user = &userinfo[..colon];
    format!("{scheme}{user}:***{host_part}{path}")
}

fn is_secret_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["password", "passwd", "pwd", "secret", "token"]
        .iter()
        .any(|needle| n.contains(needle))
}

fn redact_query(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _value)) if is_secret_param(name) => format!("{name}=***"),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_mysql_password() {
        let url = "mysql://root:secret@127.0.0.1:3306/mydb";
        assert_eq!(redact_url(url), "mysql://root:***@127.0.0.1:3306/mydb");
    }

    #[test]
    fn redacts_password_containing_at_sign() {
        // Old implementation cut at the FIRST '@' and leaked fragments.
        let url = "mysql://root:p@ss@127.0.0.1:3306/mydb";
        assert_eq!(redact_url(url), "mysql://root:***@127.0.0.1:3306/mydb");
    }

    #[test]
    fn redacts_password_containing_colon() {
        let url = "postgresql://user:p:ss:word@db.internal:5432/app";
        assert_eq!(
            redact_url(url),
            "postgresql://user:***@db.internal:5432/app"
        );
    }

    #[test]
    fn redacts_sensitive_query_params() {
        let url = "postgresql://u:p@h:5432/db?sslmode=require&password=hunter2&token=abc123&sslpassword=zzz";
        assert_eq!(
            redact_url(url),
            "postgresql://u:***@h:5432/db?sslmode=require&password=***&token=***&sslpassword=***"
        );
    }

    #[test]
    fn url_without_password_unchanged() {
        let url = "mysql://root@127.0.0.1:3306/mydb";
        assert_eq!(redact_url(url), url);
        let url2 = "postgresql://db.internal:5432/app";
        assert_eq!(redact_url(url2), url2);
    }

    #[test]
    fn sqlite_path_unchanged() {
        assert_eq!(redact_url("sqlite://./data.db"), "sqlite://./data.db");
        assert_eq!(redact_url("sqlite::memory:"), "sqlite::memory:");
    }
}
