# Engine capability matrix

This page is the explicit support contract for SQL statements (PRD-F3-09).
Rows come from `strut_sql_guard::CAPABILITY_MATRIX`; the guard rejects
unsupported cells and `crates/strut-sql-guard/src/capabilities.rs` contains
the classification test for every supported sample.

Tiers: **read** = any grant, **writes** = `--allow-writes` plus a Write grant,
**ddl** = `--allow-ddl` plus a Ddl grant, **admin** = Admin grant plus
`--allow-grants`.

## Supported

| Statement | Class | Tier | PostgreSQL | MySQL 8.4 | SQLite | DuckDB | Notes |
|---|---|---|---|---|---|---|---|
| `SELECT` | Read | read | yes | yes | yes | yes | Guard injects/clamps LIMIT. |
| `INSERT` | Dml | writes | yes | yes | yes | yes | |
| `UPDATE` | Dml | writes | yes | yes | yes | yes | |
| `DELETE` | Dml | writes | yes | yes | yes | yes | |
| `MERGE` | Dml | writes | yes | **no** | **no** | yes | PostgreSQL 15+ and DuckDB. |
| `CREATE TABLE` | Ddl | ddl | yes | yes | yes | yes | `create_table` also checks the parsed class. |
| `ALTER TABLE` | Ddl | ddl | yes | yes | yes | yes | SQLite supports a limited subset; DuckDB supports column type changes via `alter_column`. |
| `DROP TABLE` | Ddl | ddl | yes | yes | yes | yes | destructive tools require `confirm:true`. |
| `TRUNCATE TABLE` | Ddl | ddl | yes | yes | **no** | yes | SQLite has no TRUNCATE; DuckDB implements it as DELETE (no CASCADE). |
| `CREATE INDEX` | Ddl | ddl | yes | yes | yes | yes | |
| `CREATE VIEW` | Ddl | ddl | yes | yes | yes | yes | |
| `CREATE SCHEMA` | Ddl | ddl | yes | yes | **no** | yes | |
| `CREATE DATABASE` | Ddl | ddl | yes | yes | **no** | **no** | SQLite/DuckDB have no CREATE DATABASE; attaching databases is not exposed. |
| `CREATE FUNCTION` | Ddl | ddl | yes | **no** | **no** | **no** | sqlparser 0.62 parses PostgreSQL CREATE FUNCTION only; MySQL/DuckDB are rejected. |
| `CREATE SEQUENCE` | Ddl | ddl | yes | **no** | **no** | yes | MySQL uses AUTO_INCREMENT; SQLite uses rowid. |
| `COMMENT ON` | Ddl | ddl | yes | **no** | **no** | **no** | MySQL/SQLite comments live in DDL; sqlparser 0.62's DuckDB dialect does not parse it. |
| `VACUUM` | Admin | admin | yes | **no** | yes | yes | Maintenance: explicit Admin grant. |
| `ANALYZE` | Admin | admin | yes | yes | yes | yes | Maintenance: explicit Admin grant. |
| `PRAGMA` (read-only allowlist) | Read | read | **no** | **no** | yes | **no** | `table_info`, `index_list`, `foreign_key_list`, `database_list`, `page_count`, … (SQLite only) |
| `SHOW …` | Read | read | **no** | yes | **no** | yes | MySQL introspection; DuckDB exposes SHOW TABLES only. |
| `EXPLAIN` | Read | read | yes | yes | yes | yes | Plain EXPLAIN does not execute. |
| `EXPLAIN ANALYZE` | Read | writes | yes | yes | **no** | yes | Executes the statement; the advisor additionally needs `--allow-explain-analyze`. |
| `GRANT` / `REVOKE` | Admin | admin | yes | yes | **no** | **no** | Requires `--allow-grants`; SQLite/DuckDB have no SQL GRANT. |

## Data type matrix

How a column value arrives in a result cell (JSON representation). A type not
listed here is returned as text via the driver's fallback; it is never silently
nulled.

| Type | PostgreSQL | MySQL 8.4 | SQLite | DuckDB | Notes |
|---|---|---|---|---|---|
| integer / serial | number | number | number | number | i64 range preserved. |
| bigint unsigned | number (decimal string beyond 2^53) | number | number | number (decimal string beyond 2^63-1) | No float round-trip. |
| real / double | number | number | number | number | Non-finite values are an `unsupported` error on DuckDB, never null. |
| numeric / decimal | number when exact in f64, else exact decimal **string** | same | text (no decimal type) | exact decimal **string** | Precision is never silently lost. |
| boolean | `true`/`false` | `true`/`false` (BOOL/TINYINT(1)) | number (0/1) | `true`/`false` | |
| uuid | string | string (as stored) | text | string | |
| timestamp / timestamptz | naive datetime / RFC 3339 string | datetime string | text | naive datetime / **unsupported** for TZ-aware | UTC offsets preserved for timestamptz; DuckDB TIMESTAMPTZ is rejected rather than coerced. |
| date / time | date string / time string | date/time string | text | date string / canonical time text | `SqlValue` has no time variant. |
| json / jsonb | JSON value | JSON value | text | text | Valid JSON stays structured on PG/MySQL; DuckDB JSON parameters need an explicit cast. |
| bytea / blob / varbinary | tagged hex object | tagged hex object | tagged hex object | tagged hex object | Binary is never dropped to null. |
| arrays (`int[]`, `text[]`, …) | JSON array | — | — | JSON array (`LIST`/fixed `ARRAY`) | Element-wise decode for common types. |
| inet / citext | string | — | — | — | |
| **vector** (pgvector) | **array of numbers** | — | — | — | Requires the `vector` extension; reads as `[1, 2.5, 3]`. There is **no** dedicated vector-search tool: query it with normal SQL (`ORDER BY embedding <-> $1`). |

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

| Statement | PostgreSQL | MySQL 8.4 | SQLite | DuckDB | Reason |
|---|---|---|---|---|---|
| `ALTER SEQUENCE` | no | no | no | no | Not representable by sqlparser 0.62; rejected at parse time. |
| `PRAGMA` (write) | no | no | no | no | State-changing pragmas are never exposed. |
| Transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`) | no | no | no | no | The server owns connections; callers never own transactions. |
| `COPY`, `ATTACH DATABASE`, `LOAD DATA` | no | no | no | no | File/bulk transfer bypasses result caps. |
| `SET`, `USE` | no | no | no | no | Session state is managed by the server. |
| `CREATE TYPE`, `CREATE ROLE`, `CREATE EXTENSION` | no | no | no | no | Administrative DDL outside the supported surface. |
| `CALL`, `PREPARE`, `EXECUTE` | no | no | no | no | Would bypass AST classification. |
| `SELECT … INTO OUTFILE/DUMPFILE` | n/a | no | n/a | n/a | Server-side file writes; permanently denied. |

The guard also rejects dangerous functions per engine in every tier
(`pg_read_file`, `dblink*`, `LOAD_FILE`, `nextval`/`setval`, `pg_sleep`,
`SLEEP`, `BENCHMARK`, DuckDB `read_*`/`*_scan`/`query_table`, …). See [SECURITY.md](SECURITY.md).
