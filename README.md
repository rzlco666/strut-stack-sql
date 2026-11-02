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
</p>

<p align="center">
  <strong>Light strut. Extreme load.</strong><br>
  A token-efficient <a href="https://modelcontextprotocol.io">MCP</a> server that lets AI agents query
  <strong>PostgreSQL</strong>, <strong>MySQL</strong>, and <strong>SQLite</strong> — safely, in one small Rust binary.
</p>

---

## Why

Most SQL MCP servers make agents pay three taxes:

1. **Token tax** — every tool schema and every row-as-object result burns context window.
2. **Safety tax** — raw SQL passthrough means one hallucinated `DROP TABLE` is a production incident.
3. **Footprint tax** — a Node/Python runtime just to run `SELECT`.

Strut Stack SQL is built to not charge them:

- **Default surface is 3 tools.** More tools exist (`--full-tools`), but they stay out of the agent's context until you ask for them.
- **Results are columnar** — `{cols, rows, meta}` sends the column names once, not once per row.
- **Every statement passes an AST guard first** — parsed with `sqlparser`, classified, checked against a write tier, LIMIT-clamped, and rejected *before* a pool connection is even checked out.
- **One static binary, three engines.** No runtime, no interpreter, nothing to `npx`.

Part of the [StrutStack](https://github.com/rzlco666/strut-stack) ecosystem; fully usable standalone with any MCP client. Formerly published as `mcp-sql-rust` (a compat binary alias still ships in v1.x).

## How it compares

Feature comparison against the common categories of SQL MCP servers. No benchmark claims here — for measured numbers, see [docs/BENCHMARKS.md](docs/BENCHMARKS.md), which labels exactly what was measured, where, and when.

| Capability | **Strut Stack SQL** | Typical Node/Python SQL MCP | DB-vendor official MCP |
|---|---|---|---|
| Engines in one server | PostgreSQL + MySQL + SQLite | Usually 1 | 1 (the vendor's) |
| Default tool count | **3** (more behind `--full-tools`) | Often 10+ | Varies |
| Result format | Columnar `{cols, rows, meta}` + caps | Row objects | Row objects |
| SQL safety | AST guard: classification, write tiers, function denylist, LIMIT clamp, complexity limits | Mostly string checks or none | Varies |
| Write control | Read-only by default; `--allow-writes` / `--allow-ddl` tiers; per-token modes on HTTP | Often all-or-nothing | Varies |
| Remote transport | Streamable HTTP **with API-token auth** + stdio | stdio, or HTTP without auth | Varies |
| Runtime needed | None (single Rust binary) | Node.js / Python | Varies |
| Multi-database sources | Yes (TOML multi-source) | Rare | No |

## Features

- **3 core tools** — `search_objects` (progressive schema discovery), `execute_sql` (single, parameterized, or concurrent batch), `analyze_query_performance` (distilled EXPLAIN summaries). Full reference: [docs/TOOLS.md](docs/TOOLS.md).
- **AST SQL guard** — single-statement enforcement; Read / DML / DDL classification (including writes hidden inside CTEs); dangerous-function denylist per engine; LIMIT injection & clamping (including parameterized limits); join/subquery complexity caps; transaction control always rejected.
- **Write tiers** — read-only by default. `--allow-writes` for DML, `--allow-ddl` for DDL, destructive DDL tools additionally require `"confirm": true`.
- **Three engines, one binary** — PostgreSQL, MySQL, SQLite via a shared introspection layer: schemas, tables, columns, indexes, foreign keys, approximate row counts, typed value fidelity (UUID, timestamps, decimals, blobs).
- **Two transports, both first-class**
  - **stdio** for local agents (Cursor, Claude Code, VS Code, …).
  - **Streamable HTTP** (`--http`) with **API-token authentication and a write mode per token**, safe `/livez` + `/readyz` probes, and a hard rule: binding a non-loopback address without tokens is refused.
- **Token-efficient results** — columnar JSON by default, `--max-rows` / `--max-bytes` caps, server-side pagination, optional per-column redaction (`--deny-columns`).
- **Audit log** — executed SQL is emitted on the `audit` tracing target (statement text only, never parameter values).
- **Multi-source** — one server, several named databases via TOML config; pick per call with `"source": "..."`.
- **Lazy connect** — starts instantly, connects on first tool call (TCP preflight ≤ 500 ms); `--eager-connect` if you want fail-fast at boot.
- **No silent fake database** — with no database configured, the server can still start on an ephemeral in-memory SQLite (so MCP clients can connect), but it is *never silent* about it: a loud multi-line banner on stderr and the mode is announced to the client in `ServerInfo`. `--ephemeral` acknowledges the mode explicitly.

## Quick start

### Install (one command)

```bash
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash
strut-stack-sql --version
```

Installs to `~/.local/bin` (plus the `mcp-sql-rust` compat alias). Other channels — Homebrew, Scoop, winget, Docker, cargo-binstall, MCPB bundles: [docs/INSTALL.md](docs/INSTALL.md).

### Configure your client

Project `.env`:

```env
DATABASE_URL=postgresql://user:password@localhost:5432/app
```

**Cursor** (`.cursor/mcp.json`) — also see [docs/CURSOR.md](docs/CURSOR.md) for the workspace launcher:

```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "command": "strut-stack-sql",
      "args": ["--full-tools"],
      "cwd": "${workspaceFolder}"
    }
  }
}
```

**Claude Code**:

```bash
claude mcp add strut-stack-sql -- strut-stack-sql --full-tools
```

**VS Code** (`.vscode/mcp.json`):

```json
{
  "servers": {
    "strut-stack-sql": {
      "command": "strut-stack-sql",
      "args": ["--full-tools"]
    }
  }
}
```

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

### Run it as a shared HTTP server

```bash
STRUT_STACK_SQL_HTTP_TOKENS="alice:s3cr3t:readonly,bob:other-token:writes" \
  strut-stack-sql --http 0.0.0.0:8080
```

Tokens are `name:token:mode` entries (optionally source-scoped), each bound to a maximum write mode — a read-only token can never write, no matter what the client asks for. Details: [docs/SECURITY.md](docs/SECURITY.md), [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Security

Read-only by default, fail-closed everywhere else:

- AST guard runs **before** pool checkout; unknown statement types are denied, not guessed.
- Writes hidden in CTEs (`WITH … DELETE … SELECT`), `SELECT … INTO`, and dangerous functions (`pg_read_file`, `LOAD_FILE`, `pg_sleep`, …) are blocked or classified as writes — never as reads.
- Read-only mode is *also* enforced at the session level (PostgreSQL `default_transaction_read_only`, MySQL `TRANSACTION READ ONLY`, SQLite `mode=ro`) — use a least-privilege database role as the third layer.
- HTTP mode requires API tokens for non-loopback binds; `/livez` and `/readyz` never leak raw database errors.
- Credentials come from env / `.env` / TOML `url_env` — never from tool arguments; passwords are redacted from error messages and `list_sources` output.

Full model: [docs/SECURITY.md](docs/SECURITY.md).

## CLI at a glance

```bash
strut-stack-sql                          # stdio, read-only (default)
strut-stack-sql --allow-writes           # + INSERT/UPDATE/DELETE/MERGE
strut-stack-sql --allow-ddl              # + DDL (implies writes)
strut-stack-sql --full-tools             # + schema introspection tools
strut-stack-sql --http 127.0.0.1:8080    # Streamable HTTP (/mcp, /livez, /readyz)
strut-stack-sql --config ./strut-stack-sql.toml   # multi-source
strut-stack-sql --max-rows 50 --max-bytes 32768   # result caps
```

Full reference: [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

## Documentation

| Doc | Purpose |
|---|---|
| [docs/QUICKSTART.md](docs/QUICKSTART.md) | First run, step by step |
| [docs/INSTALL.md](docs/INSTALL.md) | All install channels |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | CLI / env / TOML, precedence, HTTP tokens |
| [docs/TOOLS.md](docs/TOOLS.md) | Tool schemas + the supported SQL surface |
| [docs/SECURITY.md](docs/SECURITY.md) | Guard, write tiers, HTTP auth, audit log |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Module map |
| [docs/BENCHMARKS.md](docs/BENCHMARKS.md) | Measured numbers — with their limits stated |
| [docs/TEST_REPORT.md](docs/TEST_REPORT.md) | What the test suites actually cover |
| [docs/POSTGRES.md](docs/POSTGRES.md) · [docs/MYSQL.md](docs/MYSQL.md) · [docs/SQLITE.md](docs/SQLITE.md) | Per-engine notes |
| [docs/MCP_REGISTRY.md](docs/MCP_REGISTRY.md) | MCP Registry / MCPB publishing |

## Roadmap

**Now (v1.1.x)** — hardening: guard bypass closure, HTTP token auth, audit log, deterministic fail-closed config, packaging channels synced to the current release.

**Next (v2.0)** — a breaking, ground-up step (no v1 compatibility layer):

- **Pluggable driver architecture** — PostgreSQL/MySQL/SQLite stay first-party; other engines become community drivers behind a stable `EngineDriver` trait.
- **HTTP remote, production-grade** — OAuth 2.1 on top of the token model, per-principal authorization enforced at tool invocation.
- **Schema intelligence** — relationship graph, join suggestions, token-budgeted schema context for agents.
- **Query performance advisor** — deeper EXPLAIN analysis and index recommendations.
- **Spec conformance as a release gate**, with per-client optimization for Cursor, Claude Code, VS Code, Codex, Windsurf, and OpenCode.

## Contributing

Issues and PRs welcome — start with [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md). Please include a minimal SQL repro for guard issues (never production credentials).

## License

[MIT](LICENSE)
