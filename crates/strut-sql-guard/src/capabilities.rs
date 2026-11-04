//! Explicit per-engine statement capability matrix (PRD-F3-09).
//!
//! Every row states, for one statement form, whether each engine supports it,
//! which guard class it gets, and the minimum invocation tier. The matrix is
//! data: the guard enforces the unsupported cells, tests prove the supported
//! cells, and `docs/CAPABILITIES.md` is generated in the same shape.

use sqlparser::ast::Statement;
use strut_sql_driver_api::EngineKind;

use crate::classify::{classify, SqlClass};

/// One row of the public capability matrix.
#[derive(Debug, Clone, Copy)]
pub struct CapabilityRow {
    /// Human-facing statement form.
    pub statement: &'static str,
    /// Representative SQL used by the classification tests.
    pub sample: &'static str,
    /// Guard class when the statement is supported.
    pub class: SqlClass,
    /// Minimum policy tier for the statement.
    pub tier: &'static str,
    /// Engine support flags.
    pub postgres: bool,
    pub mysql: bool,
    pub sqlite: bool,
    pub duckdb: bool,
    pub clickhouse: bool,
    /// Why the statement is unsupported / special.
    pub note: &'static str,
}

impl CapabilityRow {
    pub fn supported(self, engine: EngineKind) -> bool {
        match engine {
            EngineKind::Postgres => self.postgres,
            EngineKind::MySql => self.mysql,
            EngineKind::Sqlite => self.sqlite,
            EngineKind::DuckDb => self.duckdb,
            EngineKind::ClickHouse => self.clickhouse,
        }
    }
}

pub const CAPABILITY_MATRIX: &[CapabilityRow] = &[
    CapabilityRow {
        statement: "SELECT",
        sample: "SELECT 1",
        class: SqlClass::Read,
        tier: "read",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: true,
        note: "LIMIT is injected/clamped by the guard.",
    },
    CapabilityRow {
        statement: "INSERT",
        sample: "INSERT INTO t (id) VALUES (1)",
        class: SqlClass::Dml,
        tier: "writes",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: true,
        note: "Requires --allow-writes.",
    },
    CapabilityRow {
        statement: "UPDATE",
        sample: "UPDATE t SET id = 2",
        class: SqlClass::Dml,
        tier: "writes",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: false,
        note: "Requires --allow-writes.",
    },
    CapabilityRow {
        statement: "DELETE",
        sample: "DELETE FROM t",
        class: SqlClass::Dml,
        tier: "writes",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: true,
        note: "Requires --allow-writes.",
    },
    CapabilityRow {
        statement: "MERGE",
        sample: "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET id = s.id",
        class: SqlClass::Dml,
        tier: "writes",
        postgres: true,
        mysql: false,
        sqlite: false,
        duckdb: true,
        clickhouse: false,
        note: "PostgreSQL 15+ and DuckDB; MySQL/SQLite rejected as unsupported.",
    },
    CapabilityRow {
        statement: "CREATE TABLE",
        sample: "CREATE TABLE t (id INT)",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: true,
        note: "Structured create_table also verifies the parsed class.",
    },
    CapabilityRow {
        statement: "ALTER TABLE",
        sample: "ALTER TABLE t ADD COLUMN name TEXT",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: true,
        note: "SQLite supports a limited ALTER TABLE subset.",
    },
    CapabilityRow {
        statement: "DROP TABLE",
        sample: "DROP TABLE t",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: true,
        note: "Destructive tools require confirm:true.",
    },
    CapabilityRow {
        statement: "TRUNCATE TABLE",
        sample: "TRUNCATE TABLE t",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: true,
        sqlite: false,
        duckdb: true,
        clickhouse: true,
        note: "SQLite has no TRUNCATE; use DELETE (write tier).",
    },
    CapabilityRow {
        statement: "CREATE INDEX",
        sample: "CREATE INDEX idx_t_id ON t (id)",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: false,
        note: "",
    },
    CapabilityRow {
        statement: "CREATE VIEW",
        sample: "CREATE VIEW v AS SELECT 1",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: true,
        note: "",
    },
    CapabilityRow {
        statement: "CREATE SCHEMA",
        sample: "CREATE SCHEMA s",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: true,
        sqlite: false,
        duckdb: true,
        clickhouse: false,
        note: "SQLite has no schema DDL.",
    },
    CapabilityRow {
        statement: "CREATE DATABASE",
        sample: "CREATE DATABASE d",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: true,
        sqlite: false,
        duckdb: false,
        clickhouse: true,
        note: "SQLite/DuckDB have no CREATE DATABASE; attaching databases is not exposed.",
    },
    CapabilityRow {
        statement: "CREATE FUNCTION",
        sample: "CREATE FUNCTION f() RETURNS INT AS 'SELECT 1'",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: false,
        sqlite: false,
        duckdb: false,
        clickhouse: false,
        note: "PostgreSQL only; sqlparser 0.62 cannot parse MySQL CREATE FUNCTION, so the server rejects it there.",
    },
    CapabilityRow {
        statement: "CREATE SEQUENCE",
        sample: "CREATE SEQUENCE s",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: false,
        sqlite: false,
        duckdb: true,
        clickhouse: false,
        note: "PostgreSQL and DuckDB; MySQL AUTO_INCREMENT and SQLite rowid replace it.",
    },
    CapabilityRow {
        statement: "COMMENT ON",
        sample: "COMMENT ON TABLE t IS 'demo'",
        class: SqlClass::Ddl,
        tier: "ddl",
        postgres: true,
        mysql: false,
        sqlite: false,
        duckdb: false,
        clickhouse: false,
        note: "DuckDB COMMENT ON is not parsed by sqlparser 0.62's DuckDbDialect; PostgreSQL syntax only on this surface; MySQL/SQLite carry comments in DDL.",
    },
    CapabilityRow {
        statement: "ALTER SEQUENCE",
        sample: "ALTER SEQUENCE s RESTART",
        class: SqlClass::Other,
        tier: "none",
        postgres: false,
        mysql: false,
        sqlite: false,
        duckdb: false,
        clickhouse: false,
        note: "Not representable by sqlparser 0.62; rejected at parse time.",
    },
    CapabilityRow {
        statement: "VACUUM",
        sample: "VACUUM t",
        class: SqlClass::Admin,
        tier: "admin",
        postgres: true,
        mysql: false,
        sqlite: true,
        duckdb: true,
        clickhouse: false,
        note: "Admin tier; requires an Admin grant plus --allow-grants.",
    },
    CapabilityRow {
        statement: "ANALYZE",
        sample: "ANALYZE t",
        class: SqlClass::Admin,
        tier: "admin",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: false,
        note: "Admin tier; requires an Admin grant plus --allow-grants.",
    },
    CapabilityRow {
        statement: "PRAGMA (read-only)",
        sample: "PRAGMA table_info('t')",
        class: SqlClass::Read,
        tier: "read",
        postgres: false,
        mysql: false,
        sqlite: true,
        duckdb: false,
        clickhouse: false,
        note: "SQLite allowlisted read pragmas only; DuckDB pragmas are not exposed; write pragmas are denied.",
    },
    CapabilityRow {
        statement: "PRAGMA (write)",
        sample: "PRAGMA journal_mode = WAL",
        class: SqlClass::Other,
        tier: "none",
        postgres: false,
        mysql: false,
        sqlite: false,
        duckdb: false,
        clickhouse: false,
        note: "State-changing pragmas are never exposed.",
    },
    CapabilityRow {
        statement: "SHOW",
        sample: "SHOW TABLES",
        class: SqlClass::Read,
        tier: "read",
        postgres: false,
        mysql: true,
        sqlite: false,
        duckdb: true,
        clickhouse: true,
        note: "MySQL introspection statements; DuckDB supports SHOW TABLES only here.",
    },
    CapabilityRow {
        statement: "EXPLAIN",
        sample: "EXPLAIN SELECT 1",
        class: SqlClass::Read,
        tier: "read",
        postgres: true,
        mysql: true,
        sqlite: true,
        duckdb: true,
        clickhouse: true,
        note: "Plain EXPLAIN does not execute the inner statement.",
    },
    CapabilityRow {
        statement: "EXPLAIN ANALYZE",
        sample: "EXPLAIN ANALYZE SELECT 1",
        class: SqlClass::Read,
        tier: "writes",
        postgres: true,
        mysql: true,
        sqlite: false,
        duckdb: true,
        clickhouse: false,
        note: "Executes the statement: requires write mode and --allow-explain-analyze in the advisor.",
    },
    CapabilityRow {
        statement: "GRANT",
        sample: "GRANT SELECT ON t TO u",
        class: SqlClass::Admin,
        tier: "admin",
        postgres: true,
        mysql: true,
        sqlite: false,
        duckdb: false,
        clickhouse: true,
        note: "Admin gate: requires an Admin grant and --allow-grants.",
    },
    CapabilityRow {
        statement: "REVOKE",
        sample: "REVOKE SELECT ON t FROM u",
        class: SqlClass::Admin,
        tier: "admin",
        postgres: true,
        mysql: true,
        sqlite: false,
        duckdb: false,
        clickhouse: true,
        note: "Admin gate: requires an Admin grant and --allow-grants.",
    },
    CapabilityRow {
        statement: "Transaction control",
        sample: "BEGIN",
        class: SqlClass::Txn,
        tier: "none",
        postgres: false,
        mysql: false,
        sqlite: false,
        duckdb: false,
        clickhouse: false,
        note: "Never exposed: sessions are managed by the server.",
    },
    CapabilityRow {
        statement: "SET / USE",
        sample: "SET search_path = public",
        class: SqlClass::Other,
        tier: "none",
        postgres: false,
        mysql: false,
        sqlite: false,
        duckdb: false,
        clickhouse: false,
        note: "Session state is managed by the server; not exposed.",
    },
    CapabilityRow {
        statement: "CREATE TYPE / ROLE / EXTENSION",
        sample: "CREATE TYPE mood AS ENUM ('ok')",
        class: SqlClass::Other,
        tier: "none",
        postgres: false,
        mysql: false,
        sqlite: false,
        duckdb: false,
        clickhouse: false,
        note: "Administrative DDL outside the supported surface; use a database client.",
    },
    CapabilityRow {
        statement: "COPY / ATTACH / LOAD DATA",
        sample: "COPY t FROM 'file'",
        class: SqlClass::Other,
        tier: "none",
        postgres: false,
        mysql: false,
        sqlite: false,
        duckdb: false,
        clickhouse: false,
        note: "Permanently denied.",
    },
];

/// Returns an unsupported-message when `stmt` is not in the engine's
/// supported matrix. Called after parsing and classification.
pub fn unsupported_statement(engine: EngineKind, stmt: &Statement) -> Option<String> {
    let unsupported = |name: &str| {
        Some(format!(
            "statement type not supported on {}: {name}",
            engine_label(engine)
        ))
    };
    match stmt {
        Statement::CreateSequence { .. } => {
            (!matches!(engine, EngineKind::Postgres | EngineKind::DuckDb))
                .then(|| unsupported_row("CREATE SEQUENCE"))
        }
        Statement::Comment { .. } => {
            (engine != EngineKind::Postgres).then(|| unsupported_row("COMMENT ON"))
        }
        Statement::Vacuum(_) => matches!(engine, EngineKind::MySql | EngineKind::ClickHouse)
            .then(|| unsupported_row("VACUUM")),
        Statement::Pragma { .. } => {
            if engine != EngineKind::Sqlite {
                return unsupported("PRAGMA");
            }
            // Only read-only pragmay pass; classify() labels write pragmas Other.
            match classify(stmt) {
                Ok(class) if class.sql_class() == SqlClass::Read => None,
                _ => Some(unsupported_row("PRAGMA (write)")),
            }
        }
        Statement::CreateSchema { .. } => {
            matches!(engine, EngineKind::Sqlite | EngineKind::ClickHouse)
                .then(|| unsupported_row("CREATE SCHEMA"))
        }
        Statement::CreateDatabase { .. } => {
            matches!(engine, EngineKind::Sqlite | EngineKind::DuckDb)
                .then(|| unsupported_row("CREATE DATABASE"))
        }
        Statement::CreateFunction(_) => {
            matches!(engine, EngineKind::Sqlite | EngineKind::DuckDb | EngineKind::ClickHouse)
                .then(|| unsupported_row("CREATE FUNCTION"))
        }
        Statement::Truncate { .. } => {
            (engine == EngineKind::Sqlite).then(|| unsupported_row("TRUNCATE TABLE"))
        }
        // ClickHouse has no UPDATE statement; mutations go through
        // `ALTER TABLE ... UPDATE`, which this surface does not assemble.
        Statement::Update(_) => {
            (engine == EngineKind::ClickHouse).then(|| unsupported_row("UPDATE"))
        }
        // ClickHouse has no standalone CREATE INDEX; data-skipping indexes
        // are added with `ALTER TABLE ... ADD INDEX`.
        Statement::CreateIndex(_) => {
            (engine == EngineKind::ClickHouse).then(|| unsupported_row("CREATE INDEX"))
        }
        // `ANALYZE TABLE` is not ClickHouse syntax.
        Statement::Analyze(_) => {
            (engine == EngineKind::ClickHouse).then(|| unsupported_row("ANALYZE"))
        }
        Statement::Merge { .. } => (!matches!(engine, EngineKind::Postgres | EngineKind::DuckDb))
            .then(|| unsupported_row("MERGE")),
        Statement::Explain {
            analyze: true,
            statement,
            ..
        } => {
            if matches!(engine, EngineKind::Sqlite | EngineKind::ClickHouse) {
                return Some(unsupported_row("EXPLAIN ANALYZE"));
            }
            // The inner statement's own capability still applies.
            unsupported_statement(engine, statement)
        }
        Statement::Explain { statement, .. } if engine == EngineKind::DuckDb => {
            unsupported_statement(engine, statement)
        }
        Statement::ShowTables { .. } if engine == EngineKind::DuckDb => None,
        Statement::ShowTables { .. }
        | Statement::ShowColumns { .. }
        | Statement::ShowVariable { .. }
        | Statement::ShowVariables { .. }
        | Statement::ShowSchemas { .. }
        | Statement::ShowDatabases { .. }
        | Statement::ShowCatalogs { .. }
        | Statement::ShowCreate { .. }
        | Statement::ShowViews { .. }
        | Statement::ShowFunctions { .. }
        | Statement::ShowStatus { .. }
        | Statement::ShowCollation { .. }
        | Statement::ShowProcessList { .. } => {
            (!matches!(engine, EngineKind::MySql | EngineKind::ClickHouse))
                .then(|| unsupported_row("SHOW"))
        }
        Statement::Grant { .. } | Statement::Revoke { .. } => {
            matches!(engine, EngineKind::Sqlite | EngineKind::DuckDb)
                .then(|| unsupported_row("GRANT/REVOKE"))
        }
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. } => Some(unsupported_row("transaction control")),
        _ => None,
    }
}

fn unsupported_row(name: &str) -> String {
    format!("statement type not supported: {name}")
}

pub fn engine_label(engine: EngineKind) -> &'static str {
    match engine {
        EngineKind::Postgres => "postgres",
        EngineKind::MySql => "mysql",
        EngineKind::Sqlite => "sqlite",
        EngineKind::DuckDb => "duckdb",
        EngineKind::ClickHouse => "clickhouse",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{classify_sql, validate_and_prepare, WriteMode};

    fn mode_for(tier: &str) -> WriteMode {
        match tier {
            "read" => WriteMode::ReadOnly,
            "writes" => WriteMode::AllowWrites,
            "ddl" | "admin" => WriteMode::AllowDdl,
            _ => WriteMode::ReadOnly,
        }
    }

    #[test]
    fn every_supported_matrix_row_is_classified_and_runs_at_its_tier() {
        for row in CAPABILITY_MATRIX {
            for engine in [
                EngineKind::Postgres,
                EngineKind::MySql,
                EngineKind::Sqlite,
                EngineKind::DuckDb,
                EngineKind::ClickHouse,
            ] {
                if !row.supported(engine) {
                    continue;
                }
                let class = classify_sql(row.sample, engine).unwrap_or_else(|e| {
                    panic!(
                        "supported row '{}' failed to classify on {}: {e}",
                        row.statement,
                        engine_label(engine)
                    )
                });
                assert_eq!(
                    class.sql_class(),
                    row.class,
                    "class mismatch for '{}' on {}",
                    row.statement,
                    engine_label(engine)
                );
                validate_and_prepare(row.sample, &[], engine, mode_for(row.tier), 100)
                    .unwrap_or_else(|e| {
                        panic!(
                            "supported row '{}' rejected on {} at tier {}: {e}",
                            row.statement,
                            engine_label(engine),
                            row.tier
                        )
                    });
            }
        }
    }

    #[test]
    fn every_unsupported_matrix_row_is_rejected() {
        for row in CAPABILITY_MATRIX {
            for engine in [
                EngineKind::Postgres,
                EngineKind::MySql,
                EngineKind::Sqlite,
                EngineKind::DuckDb,
                EngineKind::ClickHouse,
            ] {
                if row.supported(engine) {
                    continue;
                }
                let result =
                    validate_and_prepare(row.sample, &[], engine, WriteMode::AllowDdl, 100);
                assert!(
                    result.is_err(),
                    "unsupported row '{}' was accepted on {}",
                    row.statement,
                    engine_label(engine)
                );
            }
        }
    }
}
