# Changelog

## 1.0.4 — 2026-07-24

P0: make `approx_rows` omit path actually run on live Postgres.

### Fixes

- **`schema_overview.approx_rows`:** decode `pg_class.reltuples` as Postgres `real` (`f32`) / `::float8` — `try_get::<f64>` alone failed silently, so never-analyzed (`-1`) never reached `pg_approx_rows` and stale `n_live_tup=0` still emitted fake `0`.

## 1.0.3 — 2026-07-24

Correctness patch: never emit fake `approx_rows: 0` on small/never-analyzed Postgres.

### Fixes

- **`schema_overview.approx_rows`:** if `reltuples < 0` (never ANALYZE'd), omit even when `n_live_tup == 0` (stale stats collector); only trust positive `n_live_tup` until analyzed. `meta.row_counts = "approx"` unchanged.

### DX / ops

- **`scripts/install-local.sh`:** also sync `$CARGO_HOME/bin` (`~/.cargo/bin`) so manual CLI matches Cursor/`~/.local/bin`

## 1.0.2 — 2026-07-24

Scorecard polish: honest approx rows, quieter indexes, install hygiene.

### Fixes

- **`schema_overview.approx_rows`:** never fake `0` from never-analyzed Postgres (`reltuples < 0` → omit). Prefer `n_live_tup`; `meta.row_counts = "approx"`.
- **`list_indexes`:** flat `{schema,table,name,columns,unique}` — no nested `indexes[]`, no full `CREATE INDEX` in `data_type`.

### DX / ops

- MCP `get_info` instructions include `CARGO_PKG_VERSION`
- **`scripts/install-local.sh`:** release build → `~/.local/bin` (repo `cargo build` alone does not update the Cursor binary)
- Cursor example args include `--deny-columns password_hash,totp_secret_encrypted,code_hash`

## 1.0.1 — 2026-07-24

Correctness + agent DX patch from live MCP scorecard.

### P0

- **Postgres typed cells:** `uuid` / `timestamptz` / `inet` / `citext` serialize as JSON strings (no silent `null`)
- **Index columns:** `describe_table` / `list_indexes` fill `indexes[].columns` on Postgres

### P1

- **Domain types:** `USER-DEFINED` resolved to `udt_name` (`citext`, …)
- **`list_sources`:** returns `engine` / `host` / `port` / `database` (no credentials)
- **Batch order:** `execute_sql` `queries[]` results preserve input order
- **`analyze_query_performance`:** rejects SQL already wrapped in `EXPLAIN`

### P2

- **Opt-in column deny-list:** `--deny-columns` / `MCP_SQL_DENY_COLUMNS` / TOML → `"[redacted]"` + `meta.redacted_columns`
- **`schema_overview`:** one-shot tables + approx row counts + FKs (`--full-tools`)

## 1.0.0 — 2026-07-24

**Strut Stack SQL** — first stable release (rebrand from `mcp-sql-rust`).

### Product

- Binary / crate: `strut-stack-sql` (compat alias `mcp-sql-rust`)
- Repo: https://github.com/rzlco666/strut-stack-sql
- Default result format: **columnar** (token-efficient)
- One-command install + friendly release asset names (`strut-stack-sql-linux-x64.tar.gz`, …)

### Reliability

- Cursor launcher never blocks MCP initialize on missing `.env` / dead DB (warn + lazy connect)
- Server starts without `DATABASE_URL` via ephemeral `sqlite::memory:`
- Env discovery prefers `apps/api/.env` and other nested paths

### Safety

- Explicit deny: `COPY`, `CALL`, `ATTACH DATABASE`, `LOAD DATA`, `PREPARE`, `EXECUTE`
- Query complexity limits (joins / subquery depth)
- PG `statement_timeout` + MySQL `max_execution_time` on pool connect (plus client timeout)

### Footprint

- Release profile: `opt-level=3`, thin LTO, strip, `panic=abort`
- Linux x64 stripped binary ≈ **15 MB** (three engines)

## 0.5.2 — 2026-07-14

### P0 — Metadata

- **`describe_table` index `unique`:** no longer hardcodes `false`; preserves uniqueness from MySQL/`SQLite`/`Postgres` index metadata (Postgres via `CREATE UNIQUE INDEX` in `indexdef`).

### P1 — Schema/DDL UX

- **DDL aliases** (`create_table`, `drop_table`, `add_column`, `alter_column`, `drop_column`, `truncate_table`) thin-wrap `schema_mutate`.
- **Tool list gating:** DDL tools (including `schema_mutate`) listed only with `--full-tools` **and** `--allow-ddl`.
- **`alter_column` dialects:** MySQL `MODIFY COLUMN`, Postgres `ALTER … TYPE`, SQLite clear unsupported error.

### P1 — Connect UX

- **TCP preflight ≤500ms** before sqlx pool connect (MySQL/Postgres); launcher `PREFLIGHT_TIMEOUT_MS` default **500** (`MCP_SQL_PREFLIGHT_MS` override).

## 0.5.1 — 2026-07-14

### P0 — Type fidelity

- **MySQL text decode:** `VARCHAR` / `CHAR` / `TEXT` / `ENUM` / `SET` / `JSON` in `execute_sql` serialize as UTF-8 JSON strings (no longer hex). Binary types (`BLOB` / `BINARY` / `VARBINARY`) remain hex.
- Shared MySQL text helpers used by schema tools and execute path (`mysql_decode_text_by_*`).

### P1 — Connect UX

- **`--connect-timeout SECS`** (default **2s**, was fixed 5s) for pool acquire timeout on first lazy connect.

## 0.5.0 — 2026-07-13

### P0 — Blockers

- **Type fidelity:** MySQL `DECIMAL` / `BIGINT UNSIGNED` / aggregates (`SUM`, `ROUND`, `information_schema`) serialize as JSON numbers via `bigdecimal` decoding.
- **Lazy connect (default):** MCP starts without blocking on DB; 5s connect timeout per tool call. `--eager-connect` restores startup ping.
- **Schema isolation:** `search_objects` / `list_tables` default to current database; use `schema: "*"` to search all schemas on MySQL.
- **Cursor setup:** Official `packaging/cursor-mcp-launcher.mjs` with TCP preflight, `--workspace`, and [docs/CURSOR.md](docs/CURSOR.md).

### P1 — Parity

- **`schema_mutate`** tool (behind `--full-tools` + `--allow-ddl`) with `confirm: true` for destructive ops.
- **`list_foreign_keys`** tool for MySQL, PostgreSQL, SQLite.
- **`describe_table`** enriched: `key`, `extra`, `comment`, `default` (MySQL via `SHOW FULL COLUMNS`).
- **`list_tables`** pagination (`offset`, `limit`, `has_more` meta).
- **`execute_sql` `format`:** `auto` | `columnar` | `rows` (+ `MCP_SQL_FORMAT` env).
- **Security:** URL password redaction in connect errors; `MCP_SQL_LOG=audit` SQL audit trail.

### P2 — Polish

- Benchmark script: `scripts/benchmark/run-mysql-vs-mcp-mysql.sh`
- [docs/POSTGRES.md](docs/POSTGRES.md), updated [docs/MYSQL.md](docs/MYSQL.md), [docs/CURSOR.md](docs/CURSOR.md)

## 0.4.0

- HTTP transport, server-side pagination, MCP Registry packaging.
