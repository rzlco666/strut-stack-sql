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

### 1. AST guard (`crates/strut-sql-guard/`)

- Dialects: `PostgreSqlDialect` / `MySqlDialect` / `SQLiteDialect`
- Reject empty / multi-statement strings
- Classify: Read / Dml / Ddl / Admin / Txn / Other (unknown → deny)
- Per-engine capability matrix (PRD-F3-09): unsupported statements are rejected before execution; see [CAPABILITIES.md](CAPABILITIES.md). `VACUUM`/`ANALYZE` are Admin; PG `CREATE SEQUENCE`/`COMMENT ON` are Ddl; SQLite read PRAGMAs are Read.
- Explicit deny labels: `COPY`, `CALL`, `ATTACH DATABASE`, `LOAD DATA`, `PREPARE`, `EXECUTE`
- Complexity limits: max 8 joins, max subquery depth 5 (checked across the whole statement tree, including subqueries and CTE bodies)
- Enforce against `WriteMode`
- Inject `LIMIT` on SELECT when missing; clamp explicit `LIMIT` above `--max-rows`
- `EXPLAIN ANALYZE` requires writes
- Batch `queries[]` items are each parsed as a single statement

**Bypass closure (v2):**

- **CTE bodies are classified recursively.** A `WITH x AS (DELETE … RETURNING …) SELECT …` is DML, not a read; the class of a query is the most dangerous class found anywhere in its tree (CTEs, subqueries, set operations).
- **`SELECT … INTO` / `INTO OUTFILE` / `INTO DUMPFILE` are not reads.** They create tables or files and are denied in read-only mode.
- **Dangerous functions are denied or tier-gated per engine** — file access (`pg_read_file`, `pg_ls_dir`, `lo_export`, MySQL `LOAD_FILE`, SQLite `readfile`/`writefile`), sequence mutation (`nextval`/`setval`), session/DoS functions (`pg_sleep`, `pg_terminate_backend`, MySQL `SLEEP`/`BENCHMARK`), and extension/plugin loaders (`dblink`, …). A function on the denylist can never ride along inside an otherwise innocent `SELECT`.
- **Parameterized row limits are clamped too.** `LIMIT ?` and `FETCH FIRST ? ROWS ONLY` are rewritten against `--max-rows` after parameter binding is accounted for — a parameter value can no longer widen a result past the cap.

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
| AllowDdl | `--allow-ddl` | + DROP/ALTER/TRUNCATE/CREATE … |

`GRANT` / `REVOKE` form their own guard class (**Admin**) and
are **not** unlocked by `--allow-ddl` alone: privilege management
additionally requires the explicit `--allow-grants` opt-in, so a schema-DDL
token can never silently become a privilege-escalation token. See
[TOOLS.md](TOOLS.md#supported-sql-surface).

Transaction control (`BEGIN`/`COMMIT`/`ROLLBACK`/`SAVEPOINT`) is always blocked at the guard, in every mode.

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
- Recommended Cursor args: `--full-tools --deny-columns password_hash,totp_secret_encrypted,code_hash` (see [CURSOR.md](CURSOR.md))
- Do not log full DSNs (`MCP_SQL_LOG` goes to stderr)
- Audit trail: see [Audit log](#audit-log) — `MCP_SQL_LOG=audit=info` enables it
- Do not put passwords in MCP client JSON `args`

### 5. Schema isolation

MySQL `search_objects` / `list_tables` / `list_columns` / `list_indexes` default to the **connected database** (`DATABASE()` or database name in URL). Cross-database search requires explicit `schema: "*"`.

### 6. DDL confirmation

`schema_mutate` and DDL alias tools (`drop_table`, `truncate_table`, `drop_column`) require `"confirm": true` for destructive actions, in addition to `--allow-ddl`. Aliases are listed only when both `--full-tools` and `--allow-ddl` are set.

## Release artifact integrity

GitHub Releases publish `SHA256SUMS` for all archives. Verify before install:

```bash
sha256sum -c SHA256SUMS --ignore-missing
```

[`install.sh`](../install.sh) verifies by default. See [INSTALL.md](INSTALL.md).

## Audit log

Executed SQL is audited through the `audit` tracing target:

```bash
MCP_SQL_LOG=audit=info strut-stack-sql
# combine with diagnostics, e.g. MCP_SQL_LOG=info,audit=info
```

Each executed statement emits one audit event on stderr with the statement
text, its guard classification, the effective write tier, the source name,
and the outcome. **Parameter values are never logged** — values bound through
`params` stay out of the audit trail, and tokens/credentials are never
written to any log. The filter is the tracing target `audit`, e.g.
`MCP_SQL_LOG=audit=info`.

## HTTP transport

`--http` serves Streamable HTTP at `/mcp`, plus two probe endpoints:

| Endpoint | Behavior |
|----------|----------|
| `GET /livez` | Static liveness — answers without touching any database |
| `GET /readyz` | Readiness from cached pool status; never returns raw database error text |
| `GET /healthz` | Compat alias of `/readyz` |

The v1 `/healthz` behavior (connect to every source on each probe and
return raw error strings to anonymous callers) is gone; the path remains
only as an alias of the safe readiness handler.

### HTTP token authentication

Configure API tokens with `STRUT_STACK_SQL_HTTP_TOKENS` (see
[CONFIGURATION.md](CONFIGURATION.md#http-tokens)):

```bash
STRUT_STACK_SQL_HTTP_TOKENS="alice:s3cr3t:readonly,bob:other-token:writes:main|reporting" \
  strut-stack-sql --http 0.0.0.0:8080
```

Format: comma-separated `name:token:mode[:source1|source2|...]` entries
(a single token can also be set with `--http-token`). Full syntax:
[CONFIGURATION.md](CONFIGURATION.md#http-tokens).

- Clients authenticate with `Authorization: Bearer <token>`; requests
  without a valid token are rejected before any tool runs.
- **Each token carries a maximum write mode** (`readonly`, `writes`, or
  `ddl`) and optionally a **source allowlist**. The effective mode for a
  call is the *lower* of the process tier (CLI flags) and the token's mode
  — a read-only token cannot write even if the server was started with
  `--allow-ddl`. Requests are rate-limited per token
  (`--http-rate-limit-per-minute`, default 600).
- **Binding a non-loopback address without any token configured is refused
  at startup** (the only override is the explicit, loudly warned
  `--allow-unauthenticated-http` flag). An unauthenticated SQL endpoint on
  a routable interface is never started by accident.
- Loopback binds (`127.0.0.1`) without tokens remain available for local
  development and print a startup warning.
- Token secrets may be stored as `sha256:<64 lowercase hex>` and are compared
  in constant time; raw secrets and digests are both accepted.
- OAuth 2.1: set `STRUT_STACK_SQL_OAUTH_ISSUER` and
  `STRUT_STACK_SQL_OAUTH_AUDIENCE` to validate RS256 Bearer JWTs from an
  external authorization server. The server discovers the issuer's JWKS,
  validates `iss`/`aud`/`exp`, maps a configurable claim (default `sub`) to a
  principal, and serves RFC 9728 protected-resource metadata at
  `/.well-known/oauth-protected-resource`. Discovery failure fails closed;
  API keys keep working. Token material is never logged.
- Terminate TLS in front of the server (reverse proxy or tunnel) whenever
  tokens cross a network you do not control.

## Release security gate (R1-06)

Three checks run before a release:

1. **Guard fuzz (`scripts/security/fuzz_guard.py`).** The server runs in the
   default read-only process mode against a *writable* SQLite source, so a
   guard bypass would actually mutate the database instead of failing on a
   read-only connection. The generator emits write attempts hidden in reads
   (CTE bodies, `SELECT … INTO`, `INTO OUTFILE`/`DUMPFILE`, `PRAGMA` writes,
   keyword comment-splitting, multi-statement smuggling, dangerous functions,
   unicode/whitespace variants) and records attempts, denials, any `ok: true`
   on a write attempt (a bypass), protocol failures, crashes, and a database
   digest before/after (table list, row counts, canary rows, file SHA-256,
   stray files).

   ```bash
   scripts/security/fuzz_guard.py --seconds 3600 --seed 20261103 \
       --out /tmp/guard-fuzz.json
   ```

   The gate passes only with `bypasses == 0`, no crashes, no protocol errors,
   and an unchanged database digest. Raw artifacts are attached to the release
   evidence, not regenerated for the docs.

   Freeze result (2026-11-03, seed 20261103, 3,603 s wall clock): **3,159,868
   attempts, 3,159,868 denied, 0 bypasses, 0 protocol errors, 0 crashes,
   database digest unchanged** — `scripts/security/results/guard-fuzz-2.0.0.json`
   records the counts and the binary SHA-256. Re-run with a fresh seed before
   any release whose guard code changed.

2. **No credentials in output, errors or logs.**
   `scripts/security/credential_leak_check.sh` drives a DSN with a password
   into a refused connection and an HTTP request with a wrong token, then
   scans stdout, stderr and the response body for either secret. It exits
   non-zero on any hit. Connection errors redact URL passwords, `list_sources`
   never returns user/password, audit events carry the statement text but
   never parameter values, and OAuth/token material is never logged.

3. **Cross-principal isolation.** With two principals granted two different
   sources, principal A cannot reach source B through *any* tool path —
   direct `execute_sql`, a batch on the other source, `search_objects`, or the
   advisor — and the denial neither returns rows nor echoes the other source's
   URL. Covered by
   `tests/http_integration.rs::principal_cannot_reach_a_source_it_was_not_granted`.

## Reporting

Guard bypass reports: GitHub issue with minimal SQL repro (no production credentials).
