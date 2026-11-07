# Architecture — strut-stack-sql

## Purpose

Expose MySQL, PostgreSQL, SQLite, DuckDB, SQL Server, and ClickHouse to AI agents over the
[Model Context Protocol](https://modelcontextprotocol.io) with:

- a small default tool surface and columnar results (token efficiency);
- one AST guard for every SQL path (safety);
- an engine-neutral driver contract (pluggable engines);
- per-principal authorization on stdio and HTTP (identity, not just transport).

## High-level flow

```mermaid
flowchart TB
  Agent[AI agent] -->|stdio or HTTP /mcp| Server[strut-sql-server]
  Server --> Auth[API key / OAuth principal]
  Auth --> Authz[Policy::authorize at invocation]
  Authz --> Tools[strut-sql-tools handlers]
  Tools --> Guard[strut-sql-guard AST pipeline]
  Guard -->|deny| Err[Error, no pool checkout]
  Guard -->|allow| DriverAPI[EngineDriver trait]
  DriverAPI --> PG[strut-sql-driver-postgres]
  DriverAPI --> MY[strut-sql-driver-mysql]
  DriverAPI --> SQ[strut-sql-driver-sqlite]
  DriverAPI --> DK[strut-sql-driver-duckdb]
  DriverAPI --> CH[strut-sql-driver-clickhouse]
  DriverAPI --> MS[strut-sql-driver-sqlserver]
  PG --> DB1[(PostgreSQL)]
  MY --> DB2[(MySQL)]
  SQ --> DB3[(SQLite)]
  DK --> DB4[(DuckDB file / memory)]
  CH --> DB5[(ClickHouse HTTP)]
  MS --> DB6[(SQL Server)]
  Tools --> Format[strut-sql-format columnar / rows]
  Format --> Agent
```

## Workspace layout

One workspace, one shipped binary (`strut-stack-sql`). Thirteen crates carry the
product; the binary crate is a thin entry point.

```
src/                    # binary: CLI parse, tracing, transport choice
crates/
  strut-sql-driver-api/         # EngineDriver contract, neutral types
  strut-sql-driver-postgres/    # PostgreSQL driver
  strut-sql-driver-mysql/       # MySQL driver
  strut-sql-driver-sqlite/      # SQLite driver
  strut-sql-driver-duckdb/      # DuckDB driver (embedded)
  strut-sql-driver-clickhouse/  # ClickHouse driver (HTTP interface)
  strut-sql-driver-sqlserver/   # SQL Server driver (Tiberius/Rustls)
  strut-sql-driver-conformance/ # Driver Conformance Test Kit
  strut-sql-core/               # config, registries, Principal/Policy, drivers
  strut-sql-guard/              # classification, denylist, row caps, placeholders
  strut-sql-format/             # columnar/rows, pagination, truncation
  strut-sql-tools/              # the 20 tool handlers
  strut-sql-server/             # rmcp stdio + Streamable HTTP, auth, health
```

Dependency rule: a database library (for example `sqlx`) may appear only in a
driver crate. Core, guard, format, tools, and server operate on the neutral
types from `strut-sql-driver-api`.

## Key types

| Type | Crate | Role |
|---|---|---|
| `EngineDriver` | `strut-sql-driver-api` | Driver construction, execution, catalogs, capability notes and optional session-scoped explain |
| `SqlValue`, `ColumnMeta`, `QueryResult` | `strut-sql-driver-api` | Engine-neutral values, column metadata and query results |
| `PlanNode`, `ExplainSummary` | `strut-sql-tools` | Normalized plan and advisor output |
| `AppConfig` | `strut-sql-core` | Validated runtime settings, sources, policy |
| `Principal`, `Policy`, `Tier` | `strut-sql-core` | Caller identity and grants (`read < writes < ddl < admin`) |
| `PreparedSql` | `strut-sql-guard` | Guard-approved SQL + classification + row-bound decisions |
| `ColumnarResult` | `strut-sql-format` | Token-efficient result payload |
| `McpSqlServer` | `strut-sql-server` | MCP service, invocation authorization, transports |

## Request path (`execute_sql`)

1. Resolve `source` to a configured `ResolvedSource` (unknown name is a validation error).
2. Classify the SQL recursively and enforce the required tier; derive the effective grant for the authenticated principal.
3. `authorize(principal, tool, source, tier)` at the invocation boundary — the same function used by `list_tools`.
4. Parse with the engine dialect, reject unsupported statements, deny dangerous functions, inject/clamp row bounds (`LIMIT`/`FETCH`, or SQL Server `TOP`/`OFFSET FETCH`).
5. On deny: return a typed error **without** checking out a pool connection.
6. Bind parameters through the driver's neutral codec; execute with the per-source timeout.
7. Decode rows to neutral values, format columnar/rows, truncate per cell and per result.
8. Emit one structured audit event; return the MCP text content block.

## Batch path

`execute_sql(queries: [...])` validates each item independently, preserves input
order while running with bounded concurrency, and enforces the configured
statement count, total deadline, and cross-item byte ceiling. Failures are
fail-soft per item unless `--fail-fast`.

## SQL Server integration

`EngineKind::SqlServer` serializes as `sqlserver`. The core registry constructs
`strut-sql-driver-sqlserver` for `sqlserver://` and `mssql://` URLs; driver
dependencies are Tiberius 0.13 with Rustls, TDS 7.3, chrono and bigdecimal.
No database-library types cross the driver boundary.

The shared guard selects `MsSqlDialect`, performs full-AST SQL Server safety
checks, and preserves table rules, row filters, masks and bind ordering through
pagination rewrites. The tools generate bracket-quoted DDL and normalize XML
execution plans. There are still exactly 20 tools, not a separate SQL Server
tool surface.

`EngineDriver::explain(&BoundQuery, analyze)` has a default `Unsupported`
implementation. SQL Server overrides it to manage `SHOWPLAN_XML` or
`STATISTICS XML` on the same checked-out connection as the query and restore
session state before reuse. Existing engines keep their previous tool-owned
EXPLAIN SQL paths. XML normalization and advice belong to tools, not the driver.

Catalogs use `sys.*` metadata in the connected database. SQL Server `schema`
means a schema such as `dbo`, not a database; omitted scope resolves through
`SCHEMA_NAME()`. Composite key ordering is retained. Catalog pages use shared
filter/page defaults where no driver pushdown is implemented; they are not
claimed as universally server-paginated.

Connection/TLS settings and execution/codec/statistics limitations:
[CONFIGURATION.md — SQL Server](CONFIGURATION.md#sql-server).


## Authorization path

- **stdio**: a synthesized local operator principal uses the same
  `Policy::authorize` code path as HTTP; there is no bypass.
- **HTTP API key**: token name → principal id, token mode → maximum tier,
  optional source allowlist → grants. Secrets may be stored as
  `sha256:<hex>` and are compared in constant time.
- **OAuth 2.1**: RS256 JWT validation against the issuer's JWKS
  (`iss`/`aud`/`exp`), with the principal id from a configurable claim.
- **Admin gate**: `GRANT`/`REVOKE` require the Admin tier and
  `--allow-grants`; a DDL grant alone never widens to privilege management.
- **Table rules**: the effective grant also carries `allow_tables`,
  `deny_tables`, `row_filters` and `mask`. The guard checks every referenced
  table and rewrites filtered tables as derived subqueries, so the raw SQL
  path cannot bypass them.

## Transports

| Mode | Flag | Notes |
|---|---|---|
| stdio | (default) | MCP over stdin/stdout; diagnostics and audit on stderr |
| Streamable HTTP | `--http 127.0.0.1:8080` | `/mcp`, `/livez`, `/readyz`, OAuth resource metadata; sessions bound to a principal |
| HTTP export | `POST /export` | NDJSON/CSV streaming; same auth and Policy as `/mcp`, forced read-only, chunked at `--max-bytes` |

The server pins MCP specification **2025-06-18**. Older requested versions are
answered with 2025-06-18; unsupported `MCP-Protocol-Version` headers are
rejected with 400; JSON-RPC batches are rejected.

## Distribution

| Channel | Artifact / entry |
|---|---|
| GitHub Releases | `strut-stack-sql-<platform>` archives, `.mcpb` bundles, `SHA256SUMS` |
| curl installer | [`install.sh`](../install.sh) |
| Homebrew | [`packaging/homebrew/strut-stack-sql.rb`](../packaging/homebrew/strut-stack-sql.rb) |
| Scoop / winget | [`packaging/scoop/`](../packaging/scoop/), [`packaging/winget/`](../packaging/winget/) |
| Docker | [`Dockerfile`](../Dockerfile) → `ghcr.io/rzlco666/strut-stack-sql` |
| MCP Registry | [`server.json`](../server.json) + [`mcpb/manifest.json`](../mcpb/manifest.json) |

See [INSTALL.md](INSTALL.md) and [MCP_REGISTRY.md](MCP_REGISTRY.md).

## Local development

| Component | Path |
|---|---|
| Compose stack | [`docker-compose.yml`](../docker-compose.yml) |
| Seed SQL | [`docker/seed/`](../docker/seed/) |
| Dev Container | [`.devcontainer/`](../.devcontainer/) |
| Conformance gate | [`scripts/conformance/run.sh`](../scripts/conformance/run.sh) |
| Benchmark harness | [`scripts/benchmark/run-bench.sh`](../scripts/benchmark/run-bench.sh) |

PostgreSQL is exposed on host port **5433**, MySQL on **3307** (avoids
conflicts with local installations).
