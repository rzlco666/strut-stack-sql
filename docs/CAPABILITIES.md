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

## Data type matrix

How a column value arrives in a result cell (JSON representation). A type not
listed here is returned as text via the driver's fallback; it is never silently
nulled.

| Type | PostgreSQL | MySQL 8.4 | SQLite | Notes |
|---|---|---|---|---|
| integer / serial | number | number | number | i64 range preserved. |
| bigint unsigned | number (decimal string beyond 2^53) | number | number | No float round-trip. |
| real / double | number | number | number | |
| numeric / decimal | number when exact in f64, else exact decimal **string** | same | text (no decimal type) | Precision is never silently lost. |
| boolean | `true`/`false` | `true`/`false` (BOOL/TINYINT(1)) | number (0/1) | |
| uuid | string | string (as stored) | text | |
| timestamp / timestamptz | naive datetime / RFC 3339 string | datetime string | text | UTC offsets preserved for timestamptz. |
| date / time | date string / time string | date/time string | text | |
| json / jsonb | JSON value | JSON value | text | Valid JSON stays structured on PG/MySQL. |
| bytea / blob / varbinary | tagged hex object | tagged hex object | tagged hex object | Binary is never dropped to null. |
| arrays (`int[]`, `text[]`, …) | JSON array | — | — | Element-wise decode for common types. |
| inet / citext | string | — | — | |
| **vector** (pgvector) | **array of numbers** | — | — | Requires the `vector` extension; reads as `[1, 2.5, 3]`. There is **no** dedicated vector-search tool: query it with normal SQL (`ORDER BY embedding <-> $1`). |

### pgvector

`vector` is supported as a **data type**, not as a search API:

- with `CREATE EXTENSION vector`, a `vector(n)` column decodes to a JSON array
  of numbers (conformance case: `vector_columns_read_as_arrays_of_numbers`,
  `#[ignore]`d unless a pgvector-enabled DSN is provided);
- similarity queries are ordinary SQL through `execute_sql`
  (`ORDER BY embedding <=> ?` with a `$vector` value written as a JSON array
  string, or a literal `'[...]'`);
- creating the extension requires DDL mode; reading the column does not.

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
