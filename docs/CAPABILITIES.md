# Engine capability matrix

This page is the explicit support contract for SQL statements (PRD-F3-09).
Rows come from `strut_sql_guard::CAPABILITY_MATRIX`; the guard rejects
unsupported cells and `crates/strut-sql-guard/src/capabilities.rs` contains
the classification test for every supported sample.

Tiers: **read** = any grant, **writes** = `--allow-writes` plus a Write grant,
**ddl** = `--allow-ddl` plus a Ddl grant, **admin** = Admin grant plus
`--allow-grants`.

## Supported

| Statement | Class | Tier | MySQL 8.4 | PostgreSQL | SQLite | DuckDB | SQL Server | ClickHouse | Notes |
|---|---|---|---|---|---|---|---|---|---|
| `SELECT` | Read | read | yes | yes | yes | yes | yes | yes | Guard injects/clamps LIMIT; Guard bounds rows with LIMIT/FETCH or SQL Server TOP/OFFSET FETCH. |
| `INSERT` | Dml | writes | yes | yes | yes | yes | yes | yes | ClickHouse inserts append to the table's parts. |
| `UPDATE` | Dml | writes | yes | yes | yes | yes | yes | **no** | ClickHouse has no `UPDATE` statement; use `ALTER TABLE … UPDATE` through a database client. |
| `DELETE` | Dml | writes | yes | yes | yes | yes | yes | yes | ClickHouse lightweight `DELETE` is asynchronous by default. |
| `MERGE` | Dml | writes | **no** | yes | **no** | yes | yes | **no** | PostgreSQL 15+ and DuckDB and SQL Server. |
| `CREATE TABLE` | Ddl | ddl | yes | yes | yes | yes | yes | yes | ClickHouse requires an `ENGINE` (and usually `ORDER BY`); pass the full DDL. `create_table` also checks the parsed class. |
| `ALTER TABLE` | Ddl | ddl | yes | yes | yes | yes | yes | yes | SQLite supports a limited subset; DuckDB supports column type changes via `alter_column`; SQL Server structured alter requires explicit nullability. |
| `DROP TABLE` | Ddl | ddl | yes | yes | yes | yes | yes | yes | destructive tools require `confirm:true`; Destructive tools require `confirm:true`. |
| `TRUNCATE TABLE` | Ddl | ddl | yes | yes | **no** | yes | yes | yes | SQLite has no TRUNCATE; DuckDB implements it as DELETE (no CASCADE). |
| `CREATE INDEX` | Ddl | ddl | yes | yes | yes | yes | yes | **no** | ClickHouse adds skipping indexes with `ALTER TABLE … ADD INDEX`. |
| `CREATE VIEW` | Ddl | ddl | yes | yes | yes | yes | yes | yes |  |
| `CREATE SCHEMA` | Ddl | ddl | yes | yes | **no** | yes | yes | **no** | ClickHouse has databases, not schemas. |
| `CREATE DATABASE` | Ddl | ddl | yes | yes | **no** | **no** | yes | yes | SQLite/DuckDB have no CREATE DATABASE; attaching databases is not exposed. |
| `CREATE FUNCTION` | Ddl | ddl | **no** | yes | **no** | **no** | **no** | **no** | sqlparser 0.62 parses PostgreSQL CREATE FUNCTION only; MySQL/DuckDB are rejected; ClickHouse uses `CREATE FUNCTION name AS (x) -> expr`, which this surface does not assemble; Routine bodies are not safely representable for other engines by sqlparser 0.62. |
| `CREATE SEQUENCE` | Ddl | ddl | **no** | yes | **no** | yes | yes | **no** | MySQL uses AUTO_INCREMENT; SQLite uses rowid; ClickHouse has none. |
| `COMMENT ON` | Ddl | ddl | **no** | yes | **no** | **no** | **no** | **no** | MySQL/SQLite comments live in DDL; sqlparser 0.62's DuckDB dialect does not parse it; ClickHouse uses `ALTER TABLE … COMMENT`; PostgreSQL syntax only. |
| `VACUUM` | Admin | admin | **no** | yes | yes | yes | **no** | **no** | Maintenance: explicit Admin grant. |
| `ANALYZE` | Admin | admin | yes | yes | yes | yes | **no** | **no** | Maintenance: explicit Admin grant. `ANALYZE TABLE` is not ClickHouse syntax. |
| `PRAGMA` (read-only allowlist) | Read | read | **no** | **no** | yes | **no** | **no** | **no** | `table_info`, `index_list`, `foreign_key_list`, `database_list`, `page_count`, … (SQLite only) |
| `SHOW …` | Read | read | yes | **no** | **no** | yes | **no** | yes | MySQL introspection; DuckDB exposes SHOW TABLES only. |
| `EXPLAIN` | Read | read | yes | yes | yes | yes | **no** | yes | Plain EXPLAIN does not execute. ClickHouse answers `EXPLAIN PLAN`; SQL Server uses the advisor instead. |
| `EXPLAIN ANALYZE` | Read | writes | yes | yes | **no** | yes | **no** | **no** | Executes the statement; the advisor additionally needs `--allow-explain-analyze`. |
| `GRANT` / `REVOKE` | Admin | admin | yes | yes | **no** | **no** | yes | yes | Requires `--allow-grants`; SQLite/DuckDB have no SQL GRANT. |

These are guard-supported statement families, not a promise that every
engine-specific syntax variant is accepted by sqlparser or the database.
SQL Server `analyze_query_performance` supports SELECT-like queries through
`SHOWPLAN_XML` (estimated) and `STATISTICS XML` (`analyze:true`, actual);
neither requires exposing user `SET` statements. Actual plans execute the
query and require a Write grant/process write mode plus
`--allow-explain-analyze`. The database login also needs `SHOWPLAN` permission
and the permissions required by the query. Other engines' advisor paths are
unchanged.

## Data type matrix

How a supported column value arrives in a result cell (JSON representation).
Unsupported SQL Server/TDS types can produce a typed execution error; there
is no universal text fallback or silent-null fallback.

| Type | MySQL 8.4 | PostgreSQL | SQLite | DuckDB | SQL Server | ClickHouse | Notes |
|---|---|---|---|---|---|---|---|
| integer / serial / identity | number | number | number | number | number | number | i64 range preserved; ClickHouse `UInt64` above `i64::MAX` arrives as an exact decimal **string**; SQL Server tinyint through bigint decode without float conversion. |
| bigint unsigned | number | number (decimal string beyond 2^53) | number | number (decimal string beyond 2^63-1) | — | number (exact string beyond `i64::MAX`) | No float round-trip; SQL Server has no unsigned bigint. |
| int128 / int256 | — | — | — | — | — | exact decimal **string** | The neutral value model has no 128/256-bit variant. |
| real / double | number | number | number | number | number | number | Non-finite values are an `unsupported` error on DuckDB, never null; SQL Server real/float use floating-point decoding. |
| numeric / decimal | same | number when exact in f64, else exact decimal **string** | text (no decimal type) | exact decimal **string** | number when exact in f64, else exact decimal **string** | exact decimal **string** | Precision is never silently lost; SQL Server decimal/numeric use exact neutral BigDecimal; binds are limited to precision/scale 38. |
| boolean / bit | `true`/`false` (BOOL/TINYINT(1)) | `true`/`false` | number (0/1) | `true`/`false` | `true`/`false` | `true`/`false` |  |
| uuid / uniqueidentifier | string (as stored) | string | text | string | string | string |  |
| timestamp / timestamptz / datetime2 | datetime string | naive datetime / RFC 3339 string | text | naive datetime / **unsupported** for TZ-aware | datetime string; datetimeoffset is RFC 3339 string | naive datetime | UTC offsets preserved for timestamptz; DuckDB TIMESTAMPTZ is rejected rather than coerced; ClickHouse stores no timezone in `DateTime`; `DateTime64` keeps sub-second precision; SQL Server `timestamp` means rowversion binary, not a date/time. |
| date / time | date/time string | date string / time string | text | date string / canonical time text | date string / time string | date string | `SqlValue` has no time variant; ClickHouse has no `TIME` type; SQL Server datetime binds use 100ns precision. |
| json / jsonb | JSON value | JSON value | text | text | text | JSON value (`JSON`/`Object`), string otherwise | Valid JSON stays structured on PG/MySQL; DuckDB JSON parameters need an explicit cast; SQL Server JSON binds are serialized text; JSON-looking text is not auto-parsed. |
| bytea / blob / varbinary / rowversion | tagged hex object | tagged hex object | tagged hex object | tagged hex object | tagged hex object | hex string | Binary is never dropped to null. ClickHouse `String`/`FixedString` are byte strings and decode as text. |
| arrays (`int[]`, `text[]`, …) | — | JSON array | — | JSON array (`LIST`/fixed `ARRAY`) | unsupported | JSON array | Element-wise decode for common types; SQL Server array binds are rejected; use explicit JSON text instead. |
| `Map`, named `Tuple` | — | — | — | — | — | JSON object | Lossless fallback for shapes the neutral model cannot express. |
| inet / citext | — | string | — | — | — | string (`IPv4`/`IPv6`) |  |
| **vector** (pgvector) | — | **array of numbers** | — | — | — | — | Requires the `vector` extension; reads as `[1, 2.5, 3]`. There is **no** dedicated vector-search tool: query it with normal SQL (`ORDER BY embedding <-> $1`). |
| money / smallmoney | — | — | — | — | number | — | Tiberius decodes via f64; `CAST(value AS decimal(19,4))` for exact monetary values. |
| xml | — | — | — | — | string | — | SQL Server XML decodes as text. |

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

SQL Server result metadata conservatively reports `nullable:true`; exact
nullability is available through catalog introspection. Time and
datetimeoffset decode as strings because the neutral API has no corresponding
value variants. Only the first row result set is returned; later sets are
drained. See [SQL Server limitations](CONFIGURATION.md#sql-server).

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

| Statement | MySQL 8.4 | PostgreSQL | SQLite | DuckDB | SQL Server | ClickHouse | Reason |
|---|---|---|---|---|---|---|---|
| `ALTER SEQUENCE` | no | no | no | no | no | no | Not representable by sqlparser 0.62; rejected at parse time. |
| `PRAGMA` (write) | no | no | no | no | no | no | State-changing pragmas are never exposed. |
| Transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`) | no | no | no | no | no | no | The server owns connections; callers never own transactions. |
| `COPY`, `ATTACH DATABASE`, `LOAD DATA` | no | no | no | no | no | no | File/bulk transfer bypasses result caps. |
| `SET`, `USE` | no | no | no | no | no | no | Session state is managed by the server. |
| `CREATE TYPE`, `CREATE ROLE`, `CREATE EXTENSION` | no | no | no | no | no | no | Administrative DDL outside the supported surface. |
| `CALL`, `PREPARE`, `EXECUTE` / SQL Server `EXEC` | no | no | no | no | no | no | Would bypass AST classification. |
| `SELECT … INTO OUTFILE/DUMPFILE` | no | n/a | n/a | n/a | n/a | n/a | Server-side file writes; permanently denied. |

The guard also rejects dangerous functions per engine in every tier
(`pg_read_file`, `dblink*`, `LOAD_FILE`, `nextval`/`setval`, `pg_sleep`,
`SLEEP`, `BENCHMARK`, DuckDB `read_*`/`*_scan`/`query_table`, and the ClickHouse table functions `file`, `url`, `s3`, `remote`, `mysql`, `postgresql`, `jdbc`, `odbc`, `hdfs`, `executable`, …, SQL Server `OPENROWSET`/`OPENQUERY`/`OPENDATASOURCE`, `xp_*`, `sp_oa*`, file/audit/trace readers, …). SQL Server checks traverse the full AST, including TOP, ORDER BY, window expressions, OUTPUT and MERGE. See [SECURITY.md](SECURITY.md).
