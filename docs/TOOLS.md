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
| `offset` | number? | default 0 |
| `limit` | number? | default 50, max 200 |

### `execute_sql`

Run SQL or concurrent batch.

| Param | Type | Notes |
|-------|------|-------|
| `sql` | string? | single statement |
| `params` | array? | bound values for `?` placeholders (single-query mode) |
| `page_offset` | number? | row offset after guard LIMIT (single-query only; default 0) |
| `page_size` | number? | rows per page (single-query only; default `--max-rows`) |
| `format` | `auto` \| `columnar` \| `rows`? | response shape (default `auto`: row objects if ≤10 rows) |
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
  "total_cost": 12.5,
  "plan_rows": 1000,
  "warnings": ["Sequential scan detected — consider adding an index"],
  "nodes": [{ "node_type": "Seq Scan", "relation": "users", "issues": ["full table scan"] }]
}
```

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
| `schema_overview` | One-shot: tables + approx row counts (`meta.row_counts=approx`; omit unknown; use `COUNT(*)` for exact) + FKs (cap 200 tables) |

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
