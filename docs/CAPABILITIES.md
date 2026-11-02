# Engine capability matrix

This page is the explicit support contract for SQL statements (PRD-F3-09).
Rows come from `strut_sql_guard::CAPABILITY_MATRIX`; the guard rejects
unsupported cells and `crates/strut-sql-guard/src/capabilities.rs` contains
the classification test for every supported sample.

Tiers: **read** = any grant, **writes** = `--allow-writes` plus a Write grant,
**ddl** = `--allow-ddl` plus a Ddl grant, **admin** = Admin grant plus
`--allow-grants`.

## Supported

| Statement | Class | Tier | PostgreSQL | MySQL 8.4 | SQLite | Notes |
|---|---|---|---|---|---|---|
| `SELECT` | Read | read | yes | yes | yes | Guard injects/clamps LIMIT. |
| `INSERT` | Dml | writes | yes | yes | yes | |
| `UPDATE` | Dml | writes | yes | yes | yes | |
| `DELETE` | Dml | writes | yes | yes | yes | |
| `MERGE` | Dml | writes | yes | **no** | **no** | PostgreSQL 15+. |
| `CREATE TABLE` | Ddl | ddl | yes | yes | yes | `create_table` also checks the parsed class. |
| `ALTER TABLE` | Ddl | ddl | yes | yes | yes | SQLite supports a limited subset. |
| `DROP TABLE` | Ddl | ddl | yes | yes | yes | destructive tools require `confirm:true`. |
| `TRUNCATE TABLE` | Ddl | ddl | yes | yes | **no** | SQLite has no TRUNCATE. |
| `CREATE INDEX` | Ddl | ddl | yes | yes | yes | |
| `CREATE VIEW` | Ddl | ddl | yes | yes | yes | |
| `CREATE SCHEMA` | Ddl | ddl | yes | yes | **no** | |
| `CREATE DATABASE` | Ddl | ddl | yes | yes | **no** | |
| `CREATE FUNCTION` | Ddl | ddl | yes | **no** | **no** | sqlparser 0.62 parses PostgreSQL CREATE FUNCTION only; MySQL is rejected. |
| `CREATE SEQUENCE` | Ddl | ddl | yes | **no** | **no** | MySQL uses AUTO_INCREMENT; SQLite uses rowid. |
| `COMMENT ON` | Ddl | ddl | yes | **no** | **no** | MySQL/SQLite comments live in DDL. |
| `VACUUM` | Admin | admin | yes | **no** | yes | Maintenance: explicit Admin grant. |
| `ANALYZE` | Admin | admin | yes | yes | yes | Maintenance: explicit Admin grant. |
| `PRAGMA` (read-only allowlist) | Read | read | **no** | **no** | yes | `table_info`, `index_list`, `foreign_key_list`, `database_list`, `page_count`, … |
| `SHOW …` | Read | read | **no** | yes | **no** | |
| `EXPLAIN` | Read | read | yes | yes | yes | Plain EXPLAIN does not execute. |
| `EXPLAIN ANALYZE` | Read | writes | yes | yes | **no** | Executes the statement; the advisor additionally needs `--allow-explain-analyze`. |
| `GRANT` / `REVOKE` | Admin | admin | yes | yes | **no** | Requires `--allow-grants`. |

## Not supported

| Statement | PostgreSQL | MySQL 8.4 | SQLite | Reason |
|---|---|---|---|---|
| `ALTER SEQUENCE` | no | no | no | Not representable by sqlparser 0.62; rejected at parse time. |
| `PRAGMA` (write) | no | no | no | State-changing pragmas are never exposed. |
| Transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`) | no | no | no | The server owns connections; callers never own transactions. |
| `COPY`, `ATTACH DATABASE`, `LOAD DATA` | no | no | no | File/bulk transfer bypasses result caps. |
| `SET`, `USE` | no | no | no | Session state is managed by the server. |
| `CREATE TYPE`, `CREATE ROLE`, `CREATE EXTENSION` | no | no | no | Administrative DDL outside the supported surface. |
| `CALL`, `PREPARE`, `EXECUTE` | no | no | no | Would bypass AST classification. |
| `SELECT … INTO OUTFILE/DUMPFILE` | n/a | no | n/a | Server-side file writes; permanently denied. |

The guard also rejects dangerous functions per engine in every tier
(`pg_read_file`, `dblink*`, `LOAD_FILE`, `nextval`/`setval`, `pg_sleep`,
`SLEEP`, `BENCHMARK`, …). See [SECURITY.md](SECURITY.md).
