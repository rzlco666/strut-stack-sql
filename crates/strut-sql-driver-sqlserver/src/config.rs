use percent_encoding::percent_decode_str;
use strut_sql_driver_api::{ConnectOptions, DriverError};
use tiberius::{AuthMethod, Config, EncryptionLevel};
use url::{Host, Url};

pub(crate) fn parse_config(
    connection_string: &str,
    options: &ConnectOptions,
) -> Result<Config, DriverError> {
    if options.pool_size == 0 {
        return Err(invalid("pool_size must be greater than zero"));
    }
    if options.connect_timeout.is_zero() || options.statement_timeout.is_zero() {
        return Err(invalid(
            "connection and statement timeouts must be greater than zero",
        ));
    }
    let url = Url::parse(connection_string).map_err(|_| invalid("invalid SQL Server URL"))?;
    if !matches!(url.scheme(), "sqlserver" | "mssql") {
        return Err(invalid("expected sqlserver:// or mssql:// URL"));
    }
    if url.fragment().is_some() {
        return Err(invalid("SQL Server URL fragments are not supported"));
    }
    let host = match url.host() {
        Some(Host::Domain(host)) if !host.is_empty() => host.to_owned(),
        Some(Host::Ipv4(host)) => host.to_string(),
        Some(Host::Ipv6(host)) => host.to_string(),
        _ => return Err(invalid("SQL Server URL requires a host")),
    };
    let user = decode(url.username())?;
    if user.is_empty() {
        return Err(invalid("SQL Server URL requires a username"));
    }
    let password = decode(url.password().unwrap_or_default())?;
    let path = url.path().strip_prefix('/').unwrap_or(url.path());
    if path.is_empty() || path.contains('/') {
        return Err(invalid("SQL Server URL requires one database path segment"));
    }
    let database = decode(path)?;
    if database.is_empty()
        || database.contains('\0')
        || user.contains('\0')
        || password.contains('\0')
    {
        return Err(invalid(
            "SQL Server URL contains an invalid database or credential",
        ));
    }
    let mut encrypt = true;
    let mut trust = false;
    let mut seen_encrypt = false;
    let mut seen_trust = false;
    for (key, value) in url.query_pairs() {
        let (target, seen) = match key.as_ref() {
            "encrypt" => (&mut encrypt, &mut seen_encrypt),
            "trust_server_certificate" => (&mut trust, &mut seen_trust),
            _ => return Err(invalid("unsupported SQL Server connection option")),
        };
        if *seen {
            return Err(invalid("duplicate SQL Server connection option"));
        }
        *seen = true;
        *target = match value.as_ref() {
            "true" => true,
            "false" => false,
            _ => {
                return Err(invalid(
                    "SQL Server connection options require true or false",
                ))
            }
        };
    }
    let mut config = Config::new();
    config.host(host);
    config.port(url.port().unwrap_or(1433));
    config.database(database);
    config.authentication(AuthMethod::sql_server(user, password));
    config.encryption(if encrypt {
        EncryptionLevel::Required
    } else {
        EncryptionLevel::NotSupported
    });
    if trust {
        config.trust_cert();
    }
    // ApplicationIntent is routing, NOT a write-prevention boundary.
    config.readonly(options.read_only);
    Ok(config)
}

fn decode(value: &str) -> Result<String, DriverError> {
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && (index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            return Err(invalid("invalid percent escape in SQL Server URL"));
        }
    }
    percent_decode_str(value)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| invalid("invalid UTF-8 in SQL Server URL"))
}

fn invalid(message: &str) -> DriverError {
    DriverError::InvalidConfig(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_urls_without_exposing_credentials() {
        for url in [
            "postgres://secret:password@localhost/db",
            "sqlserver://secret:password@localhost/",
            "sqlserver://secret:password@localhost/db/extra",
            "sqlserver://secret:password@localhost/db?encrypt=yes",
            "sqlserver://secret:password@localhost/db?encrypt=true&encrypt=false",
            "sqlserver://secret:password@localhost/db?password=leaked",
            "sqlserver://secret:%ff@localhost/db",
            "sqlserver://secret:%zz@localhost/db",
        ] {
            let error = parse_config(url, &ConnectOptions::default())
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret"));
            assert!(!error.contains("password"));
            assert!(!error.contains("leaked"));
        }
    }

    #[test]
    fn accepts_encoded_credentials_database_and_ipv6() {
        assert!(parse_config("mssql://user%40domain:p%3Ass@[::1]:14333/sales%2Farchive?encrypt=true&trust_server_certificate=true", &ConnectOptions::default()).is_ok());
    }
}
