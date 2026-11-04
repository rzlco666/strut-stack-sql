# DuckDB

DuckDB is an embedded engine: the database is a file or an in-memory instance
inside the server process — there is no server to connect to, no port, and no
credentials. `strut-sql-driver-duckdb` exposes it through the same driver
contract and tool surface as PostgreSQL, MySQL, and SQLite.

## Connection URLs

| URL | Meaning |
|---|---|
| `duckdb::memory:` | Ephemeral in-memory database, one per server process |
| `duckdb://./data/app.duckdb` | File database, relative to `--workspace` (or the working directory) |
| `duckdb:/absolute/path/app.duckdb` | File database, absolute path |
| `duckdb:///absolute/path/app.duckdb` | Same, with the double-slash form |

URL query parameters (`?access_mode=…`) and fragments are rejected at startup:
configuration is expressed by CLI flags, never by a DSN query string. The engine
is also selectable explicitly with `engine = "duckdb"` in a multi-source TOML.

```bash
DATABASE_URL=duckdb::memory: strut-stack-sql --full-tools
DUCKDB_URL=duckdb://./analytics.duckdb strut-stack-sql --allow-ddl
```

```toml
[[sources]]
name = "analytics"
engine = "duckdb"
url = "duckdb://./analytics.duckdb"
```

Environment resolution order is `DATABASE_URL`, then the other explicit URLs in
this order: `POSTGRES_URL`, `MYSQL_URL`, `SQLITE_URL`, `DUCKDB_URL` — then the
engine-specific parts (`POSTGRES_*`, `MYSQL_*`).

## Embedded semantics

- **One serialized connection.** The driver owns a single DuckDB connection and
  runs every statement on it off the async executor. `pool_size` is ignored: an
  in-memory database and its session state must survive between calls, and
  DuckDB already uses every core inside one query. Concurrent tool calls queue
  on that connection and are bounded by `connect_timeout` (connection
  acquisition) and `statement_timeout`.
- **State persists for the life of the process.** Tables created through
  `:memory:` stay readable across calls and die with the process. File databases
  persist across restarts and can be reopened concurrently by other clients.
- **Timeouts interrupt natively.** `statement_timeout` is enforced with
  DuckDB's `InterruptHandle` from an independent watchdog thread, so a runaway
  query stops inside the engine instead of leaking a blocking task. Interrupts
  are gated by connection ownership, so a cancelled statement never poisons the
  next one.
- **Results are materialized by DuckDB.** `row_cap` bounds decoded output and
  retention, not the engine's own result set; a query that produces a huge
  intermediate result still pays for it before the first row is decoded.

## Hardening

The connection is opened with external access disabled and extension
autoload/autoinstall off, so `read_csv('…')`, `INSTALL`, `LOAD`, and network
access fail inside DuckDB even if a statement slips through. On top of that the
guard refuses the primitives whose reachability depends on DuckDB features:

- file/network readers and writers (`read_*`, `write_*`, `*_scan`,
  `http_get`, `http_post`, `glob`, `st_get*`);
- dynamic SQL/table indirection (`query`, `query_table`);
- extension, secret, and connection-state primitives (`install_extension`,
  `load_extension`, `attach*`, `detach*`, `set_secret`, …);
- file/path replacement scans (`SELECT * FROM 'file.csv'`);
- `ATTACH`/`DETACH`, `COPY`, `INSTALL`, `LOAD`, transaction control and session
  `SET`, which the capability matrix never exposes.

Table policy and row filters are enforced over DuckDB's wider AST: table
references inside struct expressions, PIVOT/ASOF subtrees, and table functions
are walked with the `sqlparser` visitor. Statements whose table access cannot be
proven (dynamic table functions, `catalog.schema.table` references under a row
filter, `TABLE` shorthand) are denied rather than guessed.

**Filesystem allowlist.** `--duckdb-allowlist DIR[,DIR…]` (repeatable) constrains
every file-backed DuckDB source to those directories. A source outside them is
rejected at startup before any tool call; `duckdb::memory:` is always allowed.
Relative paths resolve against the working directory and symlinks are
canonicalized before the check. Default: no restriction.

**Read-only.** A read-mode grant opens a file database with DuckDB's native
`AccessMode::ReadOnly`. In-memory databases have no native read-only mode, so
opening `:memory:` read-only relies on the SQL guard; the driver reports that
degradation as the capability note `duckdb_memory_read_only`. A database file
that does not exist yet cannot be created read-only, so the first run against a
new path needs `--allow-writes` (the same constraint as SQLite's `?mode=ro`).

## Types

Values arrive in the neutral `SqlValue` model; nothing is silently nulled.

| DuckDB type | JSON representation | Notes |
|---|---|---|
| `BOOLEAN` | `true`/`false` | |
| `TINYINT` … `BIGINT`, unsigned integers | number | `UBIGINT` becomes an exact decimal string above `i64::MAX` |
| `HUGEINT` / `UHUGEINT` | decimal string | Exact, never rounded through f64 |
| `FLOAT` / `DOUBLE` | number | Non-finite values are an `unsupported` error, not `null` |
| `DECIMAL(p,s)` | decimal string | Exact coefficient/scale preserved |
| `VARCHAR`, `ENUM` | string | |
| `BLOB` | `{"$blob_hex": "…"}` | Binary is never dropped |
| `UUID` | UUID string | |
| `DATE` | date string | Outside the `chrono` range is an `unsupported` error |
| `TIMESTAMP`, `TIMESTAMP_S/MS/NS` | naive datetime string | Nanoseconds preserved up to `TIMESTAMP_NS` |
| `TIME`, `TIME_NS` | canonical time text | `SqlValue` has no time variant |
| `LIST` / fixed `ARRAY` | JSON array | Element-wise decode; decimal scale preserved |

Rejected as `unsupported` instead of coerced: `TIMESTAMPTZ`, `TIME WITH TIME
ZONE`, `INTERVAL`, `STRUCT`, `MAP`, `UNION`, and `UHUGEINT` nested inside a
list/array. Parameter binding accepts every scalar in the table above plus
`LIST` values with a uniform element type; typed `UUID` and `JSON` parameters
require an explicit SQL cast (`?::UUID`, `?::JSON`) because duckdb-rs has no
bindable variant for them.

## Introspection

Catalogs come from DuckDB's own table functions scoped to
`current_database()`: `duckdb_schemas()`, `duckdb_tables()`, `duckdb_columns()`,
`duckdb_constraints()`, `duckdb_indexes()`, and `duckdb_functions()`. Composite
primary keys keep their declared column order, foreign keys keep their column
pairing, and `CREATE MACRO` routines are listed. Expression index keys cannot be
represented by `IndexInfo::columns`, so a catalog that contains one returns an
`unsupported` introspection error instead of fake column names.

## EXPLAIN

`analyze_query_performance` uses `EXPLAIN (FORMAT JSON)` (row
`physical_plan`) and, when requested and allowed,
`EXPLAIN (ANALYZE, FORMAT JSON)` (row `analyzed_plan`, whose `operator` tree
adds `intermediate_rows` and `timing` per node). The planner output is
normalized into the same flat node model as the other engines: operator name,
depth, relation, estimated cardinality, and — for `ANALYZE` — actual rows and
per-operator time. DuckDB's plan arrays are walked node by node, including the
`RESULT_COLLECTOR`/`EXECUTE` wrappers `ANALYZE` adds.
DuckDB has no cost model, so `total_cost` stays absent and the advisor reasons
from row estimates, scans, and joins.

## Tests

```bash
cargo test -p strut-sql-driver-duckdb      # driver + conformance kit
cargo test --test duckdb_integration       # config, policy, tools, EXPLAIN
```

Both are embedded: no Docker, no external server, and every test uses its own
in-memory database or temporary file.

## Known limitations

- A single connection serializes concurrent tool calls; DuckDB's own
  intra-query parallelism is unaffected.
- `row_cap` truncates decoded rows only; native materialization is not capped.
- Native connection opening cannot be interrupted, so `connect_timeout` bounds
  the caller's wait, not the opening itself.
- `UUID`/`JSON` parameters need explicit casts (see above).
- `COMMENT ON` is not on the exposed surface: sqlparser 0.62's DuckDB dialect
  does not parse it, and DuckDB comments are read (not written) through the
  catalog tools.
