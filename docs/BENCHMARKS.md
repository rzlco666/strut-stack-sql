# Benchmarks

Reproducible performance comparison of **strut-stack-sql** against other MCP SQL servers on PostgreSQL.

> ## ⚠ Historical numbers — read this first
>
> Every number on this page was measured on **v0.4.0** (PostgreSQL harness,
> 2026-07-11), on **one machine** (WSL2 laptop, see below), with **n=15**
> iterations. They have **not** been regenerated for v1.1.0 and must not be
> quoted as v1.1.0 performance. They are kept because the raw artifact
> (`scripts/benchmark/benchmark-results.json`) exists and the methodology is
> documented, so anyone can re-run and compare. Regenerating this page with
> current-version numbers on stated hardware is planned work.
>
> The previous MySQL results table has been **removed**: it was a manual
> markdown table with no raw artifact in this repository, so it could not
> be verified. See "Removed: unverified MySQL numbers" at the bottom.

## MySQL comparison scripts

```bash
MYSQL_DATABASE_URL=mysql://demo:demo@localhost:3307/demo \
  ./scripts/benchmark/run-mysql-vs-mcp-mysql.sh
```

Caution: despite its name, `run-mysql-vs-mcp-mysql.sh` is a smoke script,
not a head-to-head benchmark — treat anything it prints as a local timing
sample of this server only. See also
[`run-mysql.sh`](../scripts/benchmark/run-mysql.sh) for the single-server
MySQL harness.

## Summary (reference run)

Measured on **WSL2 Ubuntu 24.04** (`LAPTOP-OC7HTPEU`), `docker compose` seed DB, pinned versions in [`scripts/benchmark/versions.lock`](../scripts/benchmark/versions.lock).

| Server | Idle RSS (MB) | Load RSS (MB) | Cold start (ms) | p50 tool (ms) | tools/list (bytes) |
|--------|---------------|---------------|-----------------|---------------|-------------------|
| **strut-stack-sql** | **10.4** | **12.8** | **13.5** | 0.88 | 2509 |
| server-postgres | 100.4 | 100.4 | 563.6 | 1.48 | 141 |
| postgres-mysql-mcp-server | 100.9 | 100.9 | 553.3 | 0.32 | 1969 |

Raw JSON: [`scripts/benchmark/benchmark-results.json`](../scripts/benchmark/benchmark-results.json) (2026-07-11).

> Re-run `./scripts/benchmark/run.sh` on your machine for authoritative numbers. Absolute latency includes local DB round-trip.

## Why these metrics

| Metric | Method | Rationale |
|--------|--------|-----------|
| **Idle RSS** | `/proc/<pid>/status` VmRSS, 2s after start | Memory at rest |
| **Load RSS** | Same after warmup + measured iterations | Steady-state under load |
| **Cold start** | Spawn → first `tools/list` response | First-use UX |
| **p50 tool latency** | `execute_sql` / `query` equivalent | Core MCP path |
| **tools/list bytes** | JSON serialized `tools/list` result | Tool-discovery token cost |

## Competitors

| Server | Package | Stack |
|--------|---------|-------|
| strut-stack-sql | this repo | Rust |
| server-postgres | `@modelcontextprotocol/server-postgres@0.6.2` | Node/TS |
| postgres-mysql-mcp-server | `postgres-mysql-mcp-server@1.0.3` | Node/TS |

**Scope:** PostgreSQL only, same seed ([`docker/seed/postgres.sql`](../docker/seed/postgres.sql)), same query:

```sql
SELECT id, email, name FROM demo.users WHERE id = 42
```

`server-postgres` receives the DSN as a **CLI argument** (upstream requirement), not only `DATABASE_URL` env.

## Run locally

```bash
docker compose up -d
cargo build --release
./scripts/benchmark/run.sh
```

Requirements: Node.js 20+ (`npx`), Linux recommended for RSS.

### Harness files

| File | Role |
|------|------|
| [`mcp-client.mjs`](../scripts/benchmark/mcp-client.mjs) | NDJSON MCP stdio client |
| [`strut-stack-sql.sh`](../scripts/benchmark/strut-stack-sql.sh) | Single-server smoke |
| [`run.sh`](../scripts/benchmark/run.sh) | Full comparison orchestrator |
| [`versions.lock`](../scripts/benchmark/versions.lock) | Pinned rival versions |

## Interpreting results

- **RSS** — Rust binary + sqlx pool vs Node `npx` cold start footprint
- **Cold start** — includes rival package spawn; strut-stack-sql is a native binary
- **p50** — dominated by Postgres latency on localhost; compare relative overhead cautiously
- **tools/list** — strut-stack-sql lists richer schemas (3 tools with full descriptions); byte size is not the only token story

## Changelog

| Date | Change |
|------|--------|
| v0.5.2 | `describe_table` unique flag fix; DDL aliases + write-mode tool gating; TCP preflight ≤500ms |
| v0.5.1 | MySQL text UTF-8 in `execute_sql` (hex only for BINARY/BLOB); connect timeout default 2s |
| v0.5.0 | BIGINT/DECIMAL fidelity, lazy connect, schema isolation, pagination, `schema_mutate` |
| v0.4.0 | Initial harness + WSL2 reference numbers |

## Removed: unverified MySQL numbers

Earlier editions of this page carried a "MySQL localhost (v0.4.0
reference)" table (single-SELECT averages, batch timings, `describe_table`
and `search_objects` timings). Those numbers were recorded manually, no raw
measurement artifact was ever committed to `scripts/benchmark/`, and they
cannot be reproduced from this repository. They were removed in the 1.1.0
documentation audit rather than kept as unverifiable marketing.

To produce defensible MySQL numbers, run the harness yourself and commit
the raw output alongside any table derived from it:

```bash
docker compose up -d mysql
cargo build --release
./scripts/benchmark/run-mysql.sh
```
