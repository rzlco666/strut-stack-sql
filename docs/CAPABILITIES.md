# Engine capability matrix

This page is the explicit support contract for SQL statements (PRD-F3-09).
Rows come from `strut_sql_guard::CAPABILITY_MATRIX`; the guard rejects
unsupported cells and `crates/strut-sql-guard/src/capabilities.rs` contains
the classification test for every supported sample.

Tiers: **read** = any grant, **writes** = `--allow-writes` plus a Write grant,
**ddl** = `--allow-ddl` plus a Ddl grant, **admin** = Admin grant plus
`--allow-grants`.

## Supported

| Statement | Class | Tier | PostgreSQL | MySQL 8.4 | SQLite | DuckDB | ClickHouse | Notes |
|---|---|---|---|---|---|---|---|---|
| `SELECT` | Read | read | yes | yes | yes | yes | yes | Guard injects/clamps LIMIT. |
| `INSERT` | Dml | writes | yes | yes | yes | yes | yes | ClickHouse inserts append to the table's parts. |
| `UPDATE` | Dml | writes | yes | yes | yes | yes | **no** | ClickHouse has no `UPDATE` statement; use `ALTER TABLE … UPDATE` through a database client. |
| `DELETE` | Dml | writes | yes | yes | yes | yes | yes | ClickHouse lightweight `DELETE` is asynchronous by default. |
| `MERGE` | Dml | writes | yes | **no** | **no** | yes | **no** | PostgreSQL 15+ and DuckDB. |
| `CREATE TABLE` | Ddl | ddl | yes | yes | yes | yes | yes | ClickHouse requires an `ENGINE` (and usually `ORDER BY`); pass the full DDL. `create_table` also checks the parsed class. |
| `ALTER TABLE` | Ddl | ddl | yes | yes | yes | yes | yes | SQLite supports a limited subset; DuckDB supports column type changes via `alter_column`. |
| `DROP TABLE` | Ddl | ddl | yes | yes | yes | yes | yes | destructive tools require `confirm:true`. |
| `TRUNCATE TABLE` | Ddl | ddl | yes | yes | **no** | yes | yes | SQLite has no TRUNCATE; DuckDB implements it as DELETE (no CASCADE). |
| `CREATE INDEX` | Ddl | ddl | yes | yes | yes | yes | **no** | ClickHouse adds skipping indexes with `ALTER TABLE … ADD INDEX`. |
| `CREATE VIEW` | Ddl | ddl | yes | yes | yes | yes | yes | |
| `CREATE SCHEMA` | Ddl | ddl | yes | yes | **no** | yes | **no** | ClickHouse has databases, not schemas. |
| `CREATE DATABASE` | Ddl | ddl | yes | yes | **no** | **no** | yes | SQLite/DuckDB have no CREATE DATABASE; attaching databases is not exposed. |
| `CREATE FUNCTION` | Ddl | ddl | yes | **no** | **no** | **no** | **no** | sqlparser 0.62 parses PostgreSQL CREATE FUNCTION only; MySQL/DuckDB are rejected; ClickHouse uses `CREATE FUNCTION name AS (x) -> expr`, which this surface does not assemble. |
| `CREATE SEQUENCE` | Ddl | ddl | yes | **no** | **no** | yes | **no** | MySQL uses AUTO_INCREMENT; SQLite uses rowid; ClickHouse has none. |
| `COMMENT ON` | Ddl | ddl | yes | **no** | **no** | **no** | **no** | MySQL/SQLite comments live in DDL; sqlparser 0.62's DuckDB dialect does not parse it; ClickHouse uses `ALTER TABLE … COMMENT`. |
| `VACUUM` | Admin | admin | yes | **no** | yes | yes | **no** | Maintenance: explicit Admin grant. |
| `ANALYZE` | Admin | admin | yes | yes | yes | yes | **no** | Maintenance: explicit Admin grant. `ANALYZE TABLE` is not ClickHouse syntax. |
| `PRAGMA` (read-only allowlist) | Read | read | **no** | **no** | yes | **no** | **no** | `table_info`, `index_list`, `foreign_key_list`, `database_list`, `page_count`, … (SQLite only) |
| `SHOW …` | Read | read | **no** | yes | **no** | yes | yes | MySQL introspection; DuckDB exposes SHOW TABLES only. |
| `EXPLAIN` | Read | read | yes | yes | yes | yes | yes | Plain EXPLAIN does not execute. ClickHouse answers `EXPLAIN PLAN`. |
| `EXPLAIN ANALYZE` | Read | writes | yes | yes | **no** | yes | **no** | Executes the statement; the advisor additionally needs `--allow-explain-analyze`. |
| `GRANT` / `REVOKE` | Admin | admin | yes | yes | **no** | **no** | yes | Requires `--allow-grants`; SQLite/DuckDB have no SQL GRANT. |

## Data type matrix

How a column value arrives in a result cell (JSON representation). A type not
listed here is returned as text via the driver's fallback; it is never silently
nulled.

| Type | PostgreSQL | MySQL 8.4 | SQLite | DuckDB | ClickHouse | Notes |
|---|---|---|---|---|---|---|
| integer / serial | number | number | number | number | number | i64 range preserved. ClickHouse `UInt64` above `i64::MAX` arrives as an exact decimal **string**. |
| bigint unsigned | number (decimal string beyond 2^53) | number | number | number (decimal string beyond 2^63-1) | number (exact string beyond `i64::MAX`) | No float round-trip. |
| int128 / int256 | — | — | — | — | exact decimal **string** | The neutral value model has no 128/256-bit variant. |
| real / double | number | number | number | number | number | Non-finite values are an `unsupported` error on DuckDB, never null. |
| numeric / decimal | number when exact in f64, else exact decimal **string** | same | text (no decimal type) | exact decimal **string** | exact decimal **string** | Precision is never silently lost. |
| boolean | `true`/`false` | `true`/`false` (BOOL/TINYINT(1)) | number (0/1) | `true`/`false` | `true`/`false` | |
| uuid | string | string (as stored) | text | string | string | |
| timestamp / timestamptz | naive datetime / RFC 3339 string | datetime string | text | naive datetime / **unsupported** for TZ-aware | naive datetime | UTC offsets preserved for timestamptz; DuckDB TIMESTAMPTZ is rejected rather than coerced. ClickHouse stores no timezone in `DateTime`; `DateTime64` keeps sub-second precision. |
| date / time | date string / time string | date/time string | text | date string / canonical time text | date string | `SqlValue` has no time variant. ClickHouse has no `TIME` type. |
| json / jsonb | JSON value | JSON value | text | text | JSON value (`JSON`/`Object`), string otherwise | Valid JSON stays structured on PG/MySQL; DuckDB JSON parameters need an explicit cast. |
| bytea / blob / varbinary | tagged hex object | tagged hex object | tagged hex object | tagged hex object | hex string | Binary is never dropped to null. ClickHouse `String`/`FixedString` are byte strings and decode as text. |
| arrays (`int[]`, `text[]`, …) | JSON array | — | — | JSON array (`LIST`/fixed `ARRAY`) | JSON array | Element-wise decode for common types. |
| `Map`, named `Tuple` | — | — | — | — | JSON object | Lossless fallback for shapes the neutral model cannot express. |
| inet / citext | string | — | — | — | string (`IPv4`/`IPv6`) | |
| **vector** (pgvector) | **array of numbers** | — | — | — | — | Requires the `vector` extension; reads as `[1, 2.5, 3]`. There is **no** dedicated vector-search tool: query it with normal SQL (`ORDER BY embedding <-> $1`). |

### ClickHouse parameter binding

ClickHouse has no positional `?` placeholder and no generic array parameter
type, so the driver rewrites `?` per value:

| Value | Wire form |
|---|---|
| `NULL` | literal `NULL` (a value of unknown type has no safe parameter type) |
| boolean / integer / float / string / UUID / date / datetime | typed `{pN:Type}` HTTP parameter (`param_pN`) |
| decimal | `{pN:String}`; ClickHouse casts to the column's `Decimal` |
| JSON, binary | `{pN:String}` (binary is sent as UTF-8, lossy for non-UTF-8 bytes) |
| array | escaped array literal (`[1, 'a']`), recursively |

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

| Statement | PostgreSQL | MySQL 8.4 | SQLite | DuckDB | ClickHouse | Reason |
|---|---|---|---|---|---|---|
| `ALTER SEQUENCE` | no | no | no | no | no | Not representable by sqlparser 0.62; rejected at parse time. |
| `PRAGMA` (write) | no | no | no | no | no | State-changing pragmas are never exposed. |
| Transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`) | no | no | no | no | no | The server owns connections; callers never own transactions. |
| `COPY`, `ATTACH DATABASE`, `LOAD DATA` | no | no | no | no | no | File/bulk transfer bypasses result caps. |
| `SET`, `USE` | no | no | no | no | no | Session state is managed by the server. |
| `CREATE TYPE`, `CREATE ROLE`, `CREATE EXTENSION` | no | no | no | no | no | Administrative DDL outside the supported surface. |
| `CALL`, `PREPARE`, `EXECUTE` | no | no | no | no | no | Would bypass AST classification. |
| `SELECT … INTO OUTFILE/DUMPFILE` | n/a | no | n/a | n/a | n/a | Server-side file writes; permanently denied. |

The guard also rejects dangerous functions per engine in every tier
(`pg_read_file`, `dblink*`, `LOAD_FILE`, `nextval`/`setval`, `pg_sleep`,
`SLEEP`, `BENCHMARK`, DuckDB `read_*`/`*_scan`/`query_table`, and the ClickHouse table functions `file`, `url`, `s3`, `remote`, `mysql`, `postgresql`, `jdbc`, `odbc`, `hdfs`, `executable`, …). See [SECURITY.md](SECURITY.md).
