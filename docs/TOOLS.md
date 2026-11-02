# MCP Tools — strut-stack-sql

Demo data for examples below: `docker compose up -d` + [`.env.example`](../.env.example) (`demo.users`, `demo.orders` on Postgres port **5433**).

## Default tools (token-efficient)

### `search_objects`

Progressive schema discovery.

| Param | Type | Notes |
|-------|------|-------|
| `object_type` | `schema` \| `table` \| `column` \| `index` | required |
| `keyword` | string? | case-insensitive filter |
| `schema` | string? | scope |
| `source` | string? | multi-source name |
| `offset` | number? | default 0; an offset past the end returns an empty page, never an error |
| `limit` | number? | default 50, max 200 |
| `include_score` | bool? | include relevance score per object; ranking is exact > prefix > substring > comment match |

### `execute_sql`

Run SQL or concurrent batch.

| Param | Type | Notes |
|-------|------|-------|
| `sql` | string? | single statement |
| `params` | array? | bound values for `?` placeholders (single-query mode) |
| `page_offset` | number? | row offset after guard LIMIT (single-query only; default 0) |
| `page_size` | number? | rows per page (single-query only; default `--max-rows`) |
| `format` | `columnar` \| `rows` \| `auto`? | response shape (default **columnar**; `auto` = row objects if ≤10 rows, else columnar — only when explicitly requested, or via `MCP_SQL_FORMAT=auto`) |
| `queries` | string[] or object[]? | batch (not both with `sql`) |
| `source` | string? | connection name |

Use `?` for placeholders in the MCP API (PostgreSQL sources auto-rewrite to `$1`, `$2`, … before execution). Advanced PostgreSQL clients may send native `$N` placeholders instead of `?`.

**Single query with params:**

```json
{
  "sql": "SELECT * FROM users WHERE id = ?",
  "params": ["42"]
}
```

**Batch (legacy strings + parameterized objects):**

```json
{
  "queries": [
    "SELECT 1",
    { "sql": "SELECT * FROM users WHERE id = ?", "params": ["42"] }
  ]
}
```

Do not pass top-level `params` together with `queries[]` — put `params` on each batch item instead. `page_offset` / `page_size` are not supported in batch mode.

**Pagination (large result sets):**

```json
{
  "sql": "SELECT id, name FROM users ORDER BY id",
  "page_offset": 0,
  "page_size": 50
}
```

Loop until `meta.has_more` is `false`, incrementing `page_offset` by `page_size` each call. Pagination meta: `page_offset`, `page_size`, `has_more`, `total_fetched`.

**Response (single):**

```json
{
  "ok": true,
  "data": {
    "cols": ["id", "name"],
    "rows": [[1, "alice"]],
    "meta": { "n": 1, "truncated": false, "limit_injected": true }
  }
}
```

`meta.limit_injected` is set when the guard auto-appends `LIMIT` (no user LIMIT). `meta.limit_clamped` is set when a user-specified LIMIT exceeds `--max-rows` and was reduced. When `--deny-columns` matches result columns, cells are replaced with `"[redacted]"` and `meta.redacted_columns` lists those names.

Typed Postgres values (`uuid`, `timestamptz`, `inet`, `citext`, …) serialize as JSON strings by default (no agent-side `::text` cast required).

**Response (batch):**

```json
{
  "results": [
    { "ok": true, "data": { "cols": [], "rows": [], "meta": { "n": 0, "truncated": false } } },
    { "ok": false, "error": "SQL guard: DDL blocked; restart with --allow-ddl" }
  ]
}
```

Batch results preserve input order of `queries[]` (concurrent but ordered via `buffered`). Map results by array index.

### `analyze_query_performance`

Runs `EXPLAIN (FORMAT JSON)` (Postgres), `EXPLAIN FORMAT=JSON` (MySQL), or `EXPLAIN QUERY PLAN` (SQLite). Returns a distilled summary.

Pass the **raw SELECT** only — do not wrap with `EXPLAIN` (the tool wraps for you).

Supports optional `params` for `?` placeholders (same rules as `execute_sql`).

```json
{
  "sql": "SELECT * FROM users WHERE id = ?",
  "params": [42]
}
```

On MySQL, `total_cost` and `plan_rows` are best-effort (aggregated from plan nodes when top-level `cost_info` is absent). Sequential scan warnings still apply.

```json
{
  "engine": "postgresql",
  "query": "SELECT ...",
  "analyze": false,
  "total_cost": 12.5,
  "plan_rows": 1000,
  "nodes": [{ "node_type": "Seq Scan", "relation": "users", "rows": 50000, "filter": "(status = 'open'::text)" }],
  "issues": [{ "severity": "warning", "code": "full_scan", "message": "full scan over ~50000 rows", "suggestion": "Add an index ..." }],
  "index_suggestions": [{ "sql": "CREATE INDEX ...;", "table": "users", "columns": ["status"], "plan_node": "Seq Scan", "estimated_rows": 50000, "confidence": "high", "reason": "sequential scan ..." }]
}
```

Pass `analyze: true` to run `EXPLAIN ANALYZE` (PostgreSQL/MySQL only). It
executes the statement and additionally requires the server opt-in
`--allow-explain-analyze` (or `STRUT_STACK_SQL_ALLOW_EXPLAIN_ANALYZE=1`).
`index_suggestions` are advice text; the advisor never executes DDL.
`issues[].severity` is `info` | `warning` | `critical`.

## Full tools (`--full-tools`)

Introspection (always when `--full-tools`):

| Tool | Purpose |
|------|---------|
| `list_sources` | Connection targets: `name`, `engine`, `host`, `port`, `database` (no credentials) |
| `list_schemas` | Schemas / databases |
| `list_tables` | Tables in schema (paginated: `offset`, `limit`) |
| `describe_table` | Columns + indexes (`unique` + `columns[]`; PG domain types via `udt_name`) |
| `list_indexes` | Flat indexes: `{schema?,table?,name,columns,unique}` (no nested `indexes[]`, no `CREATE INDEX` blob) |
| `list_foreign_keys` | Foreign keys for schema/table |
| `schema_overview` | One-shot: tables + approx row counts (`meta.row_counts=approx`; omit if never-analyzed / unknown — do not treat missing/`0`-after-ANALYZE as exact; use `COUNT(*)` for exact) + FKs + per-table `fk_in`/`fk_out`, `hub` and `orphan` markers (`meta.hubs`, `meta.orphans`); cap 200 tables stated as `meta.truncated` |
| `get_relationship_graph` | `{schema?, source?, table?, depth?}` → `{nodes, edges, meta}` from FK metadata; `depth` default 2, max 4 (focus table only) |
| `suggest_joins` | `{tables: [2..8], schema?, source?}` → shortest FK paths with copy-ready `on` conditions and `confidence`; no-FK-path same-name candidates are marked `guess: true` |
| `get_schema_context` | `{schema?, source?, token_budget? (default 4000, max 32000), tables?, keyword?}` → deterministic compact digest (`context`) with `meta {approx_tokens, truncated, tables_included, tables_total, tables_omitted, columns_omitted, relations_omitted}`; expands around focus tables via the FK graph |

DDL tools appear **only** when `--full-tools` **and** `--allow-ddl` (hidden in read-only / writes-only to save tokens):

| Tool | Purpose |
|------|---------|
| `schema_mutate` | Unified DDL (`action`: create_table, drop_table, …) |
| `create_table` | Alias — `ddl` CREATE TABLE string |
| `drop_table` | Alias — needs `confirm: true` |
| `add_column` | Alias — `table` + `column` |
| `alter_column` | Alias — MySQL `MODIFY`, Postgres `TYPE`; SQLite unsupported |
| `drop_column` | Alias — needs `confirm: true` |
| `truncate_table` | Alias — needs `confirm: true` |

Destructive ops (`drop_*`, `truncate_table`) require `"confirm": true`. Prefer `search_objects` in default mode to save tokens.

## Supported SQL surface

The guard (`src/guard/classify.rs`) decides everything. If a statement type
is not listed as supported below, it is **rejected in every mode** — there is
no silent subset. (Before 1.1.0 this surface was undocumented; several types
fell into the reject bucket without any doc saying so.)

### Read — allowed in every mode (ReadOnly and up)

- `SELECT` (including `WITH … SELECT` — **read-only CTEs only**, see below),
  set operations (`UNION` / `INTERSECT` / `EXCEPT`)
- `EXPLAIN`, `EXPLAIN <statement>` (`EXPLAIN ANALYZE` executes the statement
  and therefore requires the write tier of the inner statement)
- `SHOW …` family: `SHOW TABLES`, `SHOW COLUMNS`, `SHOW VARIABLES`,
  `SHOW DATABASES`/`SCHEMAS`, `SHOW CREATE`, `SHOW PROCESSLIST`, …

### DML — requires `--allow-writes` (or `--allow-ddl`)

- `INSERT`, `UPDATE`, `DELETE`, `MERGE`
- A CTE whose body contains any of the above (e.g.
  `WITH d AS (DELETE … RETURNING …) SELECT …`) is classified **DML**,
  never Read — classification is recursive since 1.1.0

### DDL — requires `--allow-ddl`

- `CREATE TABLE` / `CREATE VIEW` / `CREATE INDEX` / `CREATE SCHEMA` /
  `CREATE DATABASE`; `CREATE FUNCTION` is PostgreSQL-only under the current
  parser
- `CREATE SEQUENCE` (PostgreSQL) and `COMMENT ON` (PostgreSQL)
- `DROP …`, `ALTER TABLE`, `ALTER INDEX`, `TRUNCATE` (not SQLite)
- `GRANT` / `REVOKE` are their own class (**Admin**, since 1.1.0) and need
  more than `--allow-ddl`: the additional explicit `--allow-grants` opt-in
  (see [SECURITY.md](SECURITY.md)).

### Admin — requires `--allow-grants`

- `VACUUM`, `ANALYZE` (maintenance; explicit Admin grant + `--allow-grants`)

### Read-only engine pragmas

- SQLite read pragmas from the allowlist (`PRAGMA table_info('t')`,
  `index_list`, `foreign_key_list`, `database_list`, `page_count`, …).
  State-changing pragmas are denied. See [CAPABILITIES.md](CAPABILITIES.md)
  for the exact per-engine matrix.

### Always rejected — in every mode, including `--allow-ddl`

| Statement | Why |
|-----------|-----|
| Transaction control (`BEGIN` / `START TRANSACTION` / `COMMIT` / `ROLLBACK` / `SAVEPOINT` / `RELEASE`) | The server manages connections; callers never own transactions |
| Multiple statements in one string (`SELECT 1; DROP …`) | One string = one statement, always |
| `COPY` | Bulk file/stdin transfer bypasses result caps |
| `ATTACH DATABASE` / `DETACH` | Would widen the reachable data beyond configured sources |
| `LOAD DATA` | Server-side file ingestion |
| `CALL`, `PREPARE`, `EXECUTE` (prepared-statement control) | Would bypass AST classification of the executed statement |
| `SELECT … INTO`, `INTO OUTFILE`, `INTO DUMPFILE` in read modes | Creates tables/files; not a read |
| Dangerous functions (per-engine denylist: `pg_read_file`, `LOAD_FILE`, `readfile`, `nextval`/`setval`, `pg_sleep`, `SLEEP`, `BENCHMARK`, …) | File access, sequence mutation, or DoS primitives |

### Not supported

The per-engine supported/not-supported matrix lives in
[CAPABILITIES.md](CAPABILITIES.md) and is enforced by
`crates/strut-sql-guard/src/capabilities.rs`. Unsupported cells are rejected
before execution; there is no silent subset.

## Error contract

Every tool failure carries `data: {code, message, retryable}`:

| Category | JSON-RPC code | `code` | Retryable |
|---|---|---|---|
| Validation (bad params, unknown source) | -32602 | `invalid_params` | no |
| Authorization policy denial | -32010 | `authorization_denied` | no |
| Guard denial / blocked statement | -32011 | `guard_denied` | no |
| Execution / connection failure | -32012 | `execution_error` | yes |

Successful calls never return an `{ok:false}` payload; batch items keep their
per-item `{ok, data|error}` shape because a batch is fail-soft by design.

## Caps

| Flag | Default | Meaning |
|------|---------|---------|
| `--max-rows` | 100 | Auto-inject `LIMIT` when missing; clamp explicit `LIMIT` above this value |
| `--max-bytes` | 65536 | Truncate columnar payload |
| `--query-timeout` | 10s | Per-query timeout |
| `--batch-concurrency` | 8 | Max parallel batch queries |
| `--pool-size` | 10 | sqlx pool per source |
| `--connect-timeout` | 2s | sqlx pool acquire / handshake after TCP preflight |
| `--deny-columns` | (empty) | Opt-in comma-separated column names to redact in results |
| `--allow-explain-analyze` | off | Allow `analyze_query_performance(analyze=true)` (executes the statement) |
| `STRUT_STACK_SQL_QUERY_HISTORY` | off | Local-principal session history; per-principal opt-in is `query_history = true` on a `[[grants]]` entry |
