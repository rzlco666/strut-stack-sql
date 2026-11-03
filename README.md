<p align="center">
  <img src="docs/assets/banner.svg" alt="Strut Stack SQL — Light strut. Extreme load." width="100%">
</p>

<p align="center">
  <a href="https://github.com/rzlco666/strut-stack-sql/releases/latest"><img src="https://img.shields.io/github/v/release/rzlco666/strut-stack-sql" alt="Latest release"></a>
  <a href="https://github.com/rzlco666/strut-stack-sql/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/rzlco666/strut-stack-sql/ci.yml?branch=main" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <a href="https://registry.modelcontextprotocol.io/v0/servers?search=io.github.rzlco666/strut-stack-sql"><img src="https://img.shields.io/badge/MCP%20Registry-io.github.rzlco666%2Fstrut--stack--sql-purple" alt="MCP Registry"></a>
  <img src="https://img.shields.io/badge/Rust-1.88%2B-orange" alt="Rust 1.88+">
  <img src="https://img.shields.io/badge/engines-PostgreSQL%20%C2%B7%20MySQL%20%C2%B7%20SQLite-informational" alt="PostgreSQL · MySQL · SQLite">
  <img src="https://img.shields.io/badge/version-2.0.0-success" alt="Version 2.0.0">
</p>

<p align="center">
  <strong>Light strut. Extreme load.</strong><br>
  A token-efficient <a href="https://modelcontextprotocol.io">MCP</a> server that lets AI agents query
  <strong>PostgreSQL</strong>, <strong>MySQL</strong>, and <strong>SQLite</strong> — safely, in one small Rust binary.
</p>

---

**Version 2.0.0** is a ground-up, breaking release: a pluggable driver architecture, per-caller
authorization, OAuth 2.1 for remote HTTP, schema intelligence, an EXPLAIN advisor, and a pinned
MCP conformance gate. There is no v1 compatibility layer; the migration note is in
[CHANGELOG.md](CHANGELOG.md#200--2026-11-02).

## Why

Most SQL MCP servers make agents pay three taxes:

1. **Token tax** — every tool schema and every row-as-object result burns context.
2. **Safety tax** — raw SQL passthrough means one hallucinated `DROP TABLE` is a production incident.
3. **Footprint tax** — a Node/Python runtime just to run `SELECT`.

Strut Stack SQL does not charge them:

- **3 tools by default.** The other 17 are opt-in: 10 introspection tools with `--full-tools`, and 7 DDL tools only when `--full-tools --allow-ddl` is set.
- **Columnar results** — `{cols, rows, meta}` sends column names once, not once per row.
- **Every statement passes one AST guard** — parsed with `sqlparser`, classified recursively (including CTE bodies), checked against a tier, LIMIT-clamped, and rejected *before* a pool connection is checked out.
- **One static binary, three engines**, behind a stable driver contract.

## Install

The release artifact is a single binary named `strut-stack-sql`. All of these paths exist in this
repository or its release pipeline:

```bash
# From source (this workspace; crates.io publication is off)
cargo install --path . --bins

# Release binary (Linux/macOS/Windows archives + SHA256SUMS)
#   https://github.com/rzlco666/strut-stack-sql/releases/latest

# One-command installer (verifies SHA256SUMS)
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash

# Container image (built from this Dockerfile)
docker run --rm -i -e DATABASE_URL=... ghcr.io/rzlco666/strut-stack-sql:latest
```

MCPB bundles are attached to each GitHub Release and described by
[`server.json`](server.json) for the MCP Registry. Full matrix, including Homebrew, Scoop,
winget, and MCPB: [docs/INSTALL.md](docs/INSTALL.md).

## Quick start

```bash
# stdio, read-only, .env discovery
strut-stack-sql

# add the 10 introspection tools (13 total; 20 with --allow-ddl)
strut-stack-sql --full-tools

# remote HTTP with a least-privilege API key
STRUT_STACK_SQL_HTTP_TOKENS="alice:s3cr3t:readonly" \
  strut-stack-sql --http 0.0.0.0:8080
```

Project `.env`:

```env
DATABASE_URL=postgresql://user:password@localhost:5432/app
```

**Cursor** (`.cursor/mcp.json`):

```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "command": "strut-stack-sql",
      "args": ["--full-tools"]
    }
  }
}
```

Paste-ready configs for Cursor, Claude Code, VS Code, Codex, Windsurf, and OpenCode:
[docs/CLIENTS.md](docs/CLIENTS.md).

### What the agent sees

`execute_sql` returns columnar JSON — column names once, compact meta:

```json
{
  "ok": true,
  "data": {
    "cols": ["id", "email", "name"],
    "rows": [[42, "ada@example.com", "Ada"]],
    "meta": { "n": 1, "truncated": false, "limit_injected": true }
  }
}
```

## Tools — exactly 20

| Surface | Flag | Tools |
|---|---|---|
| **Core** (always) | — | `search_objects`, `execute_sql`, `analyze_query_performance` |
| **Introspection** (10) | `--full-tools` | `list_sources`, `list_schemas`, `list_tables`, `describe_table`, `list_indexes`, `list_foreign_keys`, `schema_overview`, `get_relationship_graph`, `suggest_joins`, `get_schema_context` |
| **DDL** (7) | `--full-tools --allow-ddl` | `schema_mutate`, `create_table`, `drop_table`, `add_column`, `alter_column`, `drop_column`, `truncate_table` |

- `search_objects` — ranked progressive schema discovery (exact > prefix > substring > comment match).
- `execute_sql` — single statement, bound parameters, or a governed concurrent batch; columnar / rows / auto output.
- `analyze_query_performance` — normalized EXPLAIN tree, tiered findings, index advice (advice text only; never executed).
- `get_relationship_graph` / `suggest_joins` / `get_schema_context` — FK graph, shortest join paths, token-budgeted schema digest.
- DDL aliases require `"confirm": true` for destructive actions.

`list_tools` and invocation use the same authorization function, so a tool hidden from the list is
also rejected when called directly. Full schemas and error contract: [docs/TOOLS.md](docs/TOOLS.md).

## Architecture

```mermaid
flowchart LR
  Agent[AI agent] -->|stdio / Streamable HTTP| Server[strut-sql-server]
  Server --> Tools[strut-sql-tools]
  Tools --> Guard[strut-sql-guard]
  Guard -->|allow| DriverAPI[strut-sql-driver-api]
  Guard -->|deny| NoDB[Reject before pool checkout]
  DriverAPI --> PG[strut-sql-driver-postgres]
  DriverAPI --> MY[strut-sql-driver-mysql]
  DriverAPI --> SQ[strut-sql-driver-sqlite]
  PG --> DB[(PostgreSQL)]
  MY --> DB2[(MySQL)]
  SQ --> DB3[(SQLite)]
  Tools --> Format[strut-sql-format]
  Server --> Core[strut-sql-core]
```

Ten crates plus a thin binary. The core, guard, format, tools, and server crates never import a
database library; only a driver crate does.

| Crate | Responsibility |
|---|---|
| `strut-stack-sql` (binary) | CLI parse, tracing, transport selection |
| `strut-sql-driver-api` | Public `EngineDriver` contract, neutral `SqlValue`/`ColumnMeta`/`PlanTree` types, capabilities, dialect, dangerous-function data |
| `strut-sql-driver-postgres` | PostgreSQL driver |
| `strut-sql-driver-mysql` | MySQL driver (`mysql://`; engine hint `mariadb` maps to it) |
| `strut-sql-driver-sqlite` | SQLite driver |
| `strut-sql-driver-conformance` | Shared Driver Conformance Test Kit |
| `strut-sql-core` | Config v2, source/driver registries, `Principal` + `Policy`, audit, pool ownership |
| `strut-sql-guard` | Recursive AST classification, function denylist, LIMIT management, complexity, placeholder scanner |
| `strut-sql-format` | Columnar/rows results, pagination, per-cell and whole-result truncation |
| `strut-sql-tools` | The 20 MCP tool handlers |
| `strut-sql-server` | `rmcp` stdio + Streamable HTTP, auth middleware, invocation authorization, `/livez` + `/readyz` |

The driver boundary:

```rust
#[async_trait]
pub trait EngineDriver: Send + Sync {
    fn kind(&self) -> EngineKind;
    fn capabilities(&self) -> DriverCapabilities;
    fn dialect(&self) -> DialectSpec;

    async fn open_pool(&self, url: &RedactedUrl, opts: PoolOptions) -> Result<DriverPool, DriverError>;
    async fn execute(&self, pool: &DriverPool, prepared: &PreparedStatement) -> Result<RawResult, DriverError>;
    async fn list_schemas(&self, pool: &DriverPool, filter: &SchemaFilter) -> Result<Vec<SchemaObject>, DriverError>;
    async fn explain(&self, pool: &DriverPool, prepared: &PreparedStatement) -> Result<PlanTree, DriverError>;

    fn dangerous_functions(&self) -> &'static [DangerousFn];
    fn quote_ident(&self, ident: &str) -> String;
}
```

(Abbreviated; the full contract is in `crates/strut-sql-driver-api`.) A new engine is a crate that
implements this trait plus one registry entry; the core, guard, format, tools, and server crates
stay free of database-library types.

## Security and auth

Read-only by default, fail-closed everywhere else.

- **AST guard before pool checkout** — one statement per string; unknown statement types are denied, not guessed.
- **Recursive classification** — writes hidden in CTEs (`WITH … DELETE … RETURNING … SELECT …`), `SELECT … INTO`, `INTO OUTFILE`/`DUMPFILE`, row-locking clauses, and dangerous functions (`pg_read_file`, `LOAD_FILE`, `pg_sleep`, …) are never counted as reads.
- **Per-principal grants** — `read < writes < ddl < admin` per principal × source, with optional row/byte/timeout ceilings. The process flags (`--allow-writes`, `--allow-ddl`) are a ceiling no grant can exceed.
- **Admin gate** — `GRANT`/`REVOKE` use the separate `Admin` tier and additionally require `--allow-grants`.
- **HTTP API keys** — `STRUT_STACK_SQL_HTTP_TOKENS` entries are `name:secret:mode[:source|source…]`. Secrets may be stored as `sha256:<64 hex>` and are compared in constant time. A token's mode and source allowlist cap every request.
- **OAuth 2.1 resource server** — with `STRUT_STACK_SQL_OAUTH_ISSUER` + `STRUT_STACK_SQL_OAUTH_AUDIENCE`, the server validates RS256 Bearer JWTs against the issuer's JWKS (`iss`/`aud`/`exp`), maps a configurable claim to the principal, serves RFC 9728 protected-resource metadata, and fails closed if discovery fails. Tokens are never logged.
- **Unsafe binds refused** — a non-loopback HTTP bind with no auth fails startup unless `--allow-unauthenticated-http` is passed explicitly (with a loud warning).
- **Safe probes** — `/livez` never touches a database; `/readyz` uses cached pool status and never returns raw database errors.

Full model: [docs/SECURITY.md](docs/SECURITY.md).

## Configuration

Resolution is layered and validated at startup. Prefer `url_env` so credentials stay in the
environment; a TOML source without `url`/`url_env` is an error, never a silent fallback.

```toml
# strut-stack-sql.toml — see strut-stack-sql.example.toml
default_source = "app"

[[sources]]
name = "app"
url_env = "DATABASE_URL"

[[principals]]
id = "alice"

[[grants]]
principal = "alice"
source = "app"          # or "*"
max_tier = "writes"     # read | writes | ddl | admin
max_rows = 200
max_bytes = 65536
```

```bash
strut-stack-sql --config ./strut-stack-sql.toml
```

CLI flags for caps, timeouts, pools, batch governance, redaction, and transports:
[docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Benchmarks (measured)

Raw artifact: [`scripts/benchmark/results/2.0.0.json`](scripts/benchmark/results/2.0.0.json) —
2026-11-02, WSL2 Ubuntu 24.04, Intel i5-13420H (6 vCPU), 5.79 GB RAM, SQLite temp database,
n=120 iterations after 20 warmups, 20 cold-start runs. Reproduce with
`./scripts/benchmark/run-bench.sh`.

| Metric | Target | stdio | HTTP |
|---|---:|---:|---:|
| Idle RSS | ≤ 15 MB | **7.93 MB** ✅ | **8.78 MB** ✅ |
| Cold start (spawn → `initialize`) p95 | ≤ 25 ms | **2.52 ms** ✅ | **41.74 ms** ❌ |
| `tools/list` (core mode) | ≤ 3,072 B | **3,039 B** ✅ | **3,039 B** ✅ |
| 100 rows × 8 cols (before truncation) | ≤ 70 KB | **7,526 B** ✅ | **7,526 B** ✅ |
| `execute_sql` SELECT by PK, p50 | ≤ 1 ms | **0.80 ms** ✅ | **1.68 ms** ❌ |

The two HTTP misses are real and are not rounded away: cold start includes listener readiness and
the session handshake, and each HTTP `execute_sql` is a fresh request, so transport framing
dominates. The stripped Linux x64 binary is 10.6 MB (release size budget: 16 MB). PostgreSQL and
MySQL were not measured (no live server on the reference machine) and
are recorded as `not_run`, together with the competitor comparison. Methodology and limits:
[docs/BENCHMARKS.md](docs/BENCHMARKS.md).

## Protocol and clients

- **Pinned MCP specification: 2025-06-18.** Version negotiation, `tools/list`, `tools/call`, error shapes, Streamable HTTP sessions, auth metadata, and DNS-rebinding `Origin` checks are exercised by a runnable gate. Older requested versions are answered with 2025-06-18; JSON-RPC batches are rejected.
- **Six-client matrix:** Cursor, Claude Code, VS Code, Codex, Windsurf, OpenCode — every config block is parsed and the minimal stdio command is smoke-run in CI.
- **Conformance results:** [docs/CONFORMANCE.md](docs/CONFORMANCE.md); client configs: [docs/CLIENTS.md](docs/CLIENTS.md).

## Documentation

| Doc | Purpose |
|---|---|
| [docs/QUICKSTART.md](docs/QUICKSTART.md) | First run, step by step |
| [docs/INSTALL.md](docs/INSTALL.md) | Install channels and release assets |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | CLI / env / TOML, precedence, tokens, OAuth |
| [docs/CLIENTS.md](docs/CLIENTS.md) | Paste-ready configs for six clients |
| [docs/TOOLS.md](docs/TOOLS.md) | Tool schemas, supported SQL surface, error contract |
| [docs/CAPABILITIES.md](docs/CAPABILITIES.md) | Per-engine statement matrix |
| [docs/SECURITY.md](docs/SECURITY.md) | Guard, tiers, HTTP auth, audit log |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Workspace/crate map and request path |
| [docs/CONFORMANCE.md](docs/CONFORMANCE.md) | MCP conformance gate and results |
| [docs/BENCHMARKS.md](docs/BENCHMARKS.md) | Measurements with their limits stated |
| [docs/TEST_REPORT.md](docs/TEST_REPORT.md) | What the test suites cover |
| [docs/POSTGRES.md](docs/POSTGRES.md) · [docs/MYSQL.md](docs/MYSQL.md) · [docs/SQLITE.md](docs/SQLITE.md) | Per-engine notes |
| [docs/MCP_REGISTRY.md](docs/MCP_REGISTRY.md) | MCP Registry / MCPB publishing |

## Contributing

Issues and PRs welcome — start with [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md). Include a minimal
SQL repro for guard issues (never production credentials).

## License

[MIT](LICENSE)
