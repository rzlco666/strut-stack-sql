# Security — strut-stack-sql

## Threat model

AI agents may emit destructive SQL (DROP, mass DELETE, multi-statement smuggling). The server must fail closed by default.

## Defense in depth

```mermaid
flowchart LR
  Q[Agent SQL] --> AST[sqlparser AST]
  AST -->|deny| X[Reject no DB]
  AST -->|allow| Mode[WriteMode check]
  Mode -->|deny| X
  Mode -->|allow| Sess[Session RO hooks]
  Sess --> DB[(Least-privilege DB user)]
```

### 1. AST guard (`src/guard/`)

- Dialects: `PostgreSqlDialect` / `MySqlDialect` / `SQLiteDialect`
- Reject empty / multi-statement strings
- Classify: Read / Dml / Ddl / Txn / Other (unknown → deny)
- Explicit deny labels: `COPY`, `CALL`, `ATTACH DATABASE`, `LOAD DATA`, `PREPARE`, `EXECUTE`
- Complexity limits: max 8 joins, max subquery depth 5
- Enforce against `WriteMode`
- Inject `LIMIT` on SELECT when missing; clamp explicit `LIMIT` above `--max-rows`
- `EXPLAIN ANALYZE` requires writes
- Batch `queries[]` items are each parsed as a single statement

### 1c. Timeouts

- Client: `tokio::time::timeout` around execute (default `--query-timeout 10`)
- Postgres: `SET statement_timeout` on each new pool connection
- MySQL: `SET SESSION max_execution_time` (SELECT) on connect
- SQLite: client timeout only (no server kill API via sqlx pool)

### 1b. Parameterized queries (`params`)

`execute_sql` and `analyze_query_performance` accept optional `params` for `?` placeholders (or native `$N` on PostgreSQL). Values bind via `db/bind.rs` — **never** concatenated into SQL.

**What params help with:** safe value binding when agents separate structure from user input.

**What params do not change:** AST guard still runs first; write tiers still apply; agents can send raw SQL without `params`. Multi-statement smuggling remains blocked.

### 2. Write tiers (CLI)

| Mode | Flag | Allowed |
|------|------|---------|
| ReadOnly | (default) | SELECT, SHOW, DESCRIBE, EXPLAIN |
| AllowWrites | `--allow-writes` | + INSERT/UPDATE/DELETE/MERGE |
| AllowDdl | `--allow-ddl` | + DROP/ALTER/TRUNCATE/CREATE/GRANT… |

Transaction control (`BEGIN`/`COMMIT`/`ROLLBACK`) is blocked at the guard.

### 3. Session hardening

- Postgres (read-only): `SET default_transaction_read_only = on`
- MySQL (read-only): `SET SESSION TRANSACTION READ ONLY`
- SQLite (read-only): `?mode=ro` on connect URL

### 4. Operational recommendation

Use a read-only DB role for agent workloads. Demo compose credentials (`demo/demo`) are for **local dev only**.

```sql
-- PostgreSQL example
CREATE ROLE mcp_ro LOGIN PASSWORD '...';
GRANT CONNECT ON DATABASE app TO mcp_ro;
GRANT USAGE ON SCHEMA public TO mcp_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO mcp_ro;
```

## Secrets

- Credentials from `.env` / env / TOML `url_env` — never from tool arguments
- Connection errors **redact passwords** in URLs (`mysql://user:***@host/db`)
- `list_sources` exposes engine/host/port/database only — never user/password
- Optional **opt-in** column deny-list (`--deny-columns` / `MCP_SQL_DENY_COLUMNS` / TOML) redacts matching cells as `"[redacted]"`
- Do not log full DSNs (`MCP_SQL_LOG` goes to stderr)
- Optional audit trail: `MCP_SQL_LOG=audit` logs executed SQL (no param values)
- Do not put passwords in MCP client JSON `args`

### 5. Schema isolation (v0.5+)

MySQL `search_objects` / `list_tables` / `list_columns` / `list_indexes` default to the **connected database** (`DATABASE()` or database name in URL). Cross-database search requires explicit `schema: "*"`.

### 6. DDL confirm (v0.5+)

`schema_mutate` and DDL alias tools (`drop_table`, `truncate_table`, `drop_column`) require `"confirm": true` for destructive actions, in addition to `--allow-ddl`. Aliases are listed only when both `--full-tools` and `--allow-ddl` are set.

## Release artifact integrity

GitHub Releases publish `SHA256SUMS` for all archives. Verify before install:

```bash
sha256sum -c SHA256SUMS --ignore-missing
```

[`install.sh`](../install.sh) verifies by default. See [INSTALL.md](INSTALL.md).

## HTTP transport

`--http` binds Streamable HTTP without OAuth in v1. Bind to `127.0.0.1` unless behind TLS + access controls.

## Reporting

Guard bypass reports: GitHub issue with minimal SQL repro (no production credentials).
