<p align="center">
  <img src="docs/assets/banner.svg" alt="Strut Stack SQL — Light strut. Extreme load." width="100%">
</p>

<p align="center">
  <a href="https://github.com/rzlco666/strut-stack-sql/releases/latest"><img src="https://img.shields.io/github/v/release/rzlco666/strut-stack-sql" alt="Latest release"></a>
  <a href="https://github.com/rzlco666/strut-stack-sql/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/rzlco666/strut-stack-sql/ci.yml?branch=main" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <a href="https://registry.modelcontextprotocol.io/v0/servers?search=io.github.rzlco666/strut-stack-sql"><img src="https://img.shields.io/badge/MCP%20Registry-io.github.rzlco666%2Fstrut--stack--sql-purple" alt="MCP Registry"></a>
  <img src="https://img.shields.io/badge/engines-PostgreSQL%20%C2%B7%20MySQL%20%C2%B7%20SQLite%20%C2%B7%20DuckDB%20%C2%B7%20ClickHouse%20%C2%B7%20SQL%20Server-informational" alt="PostgreSQL · MySQL · SQLite · DuckDB · ClickHouse · SQL Server">
</p>

<p align="center">
  <strong>Light strut. Extreme load.</strong><br>
  A token-efficient <a href="https://modelcontextprotocol.io">MCP</a> server that lets AI agents query
  <strong>PostgreSQL</strong>, <strong>MySQL</strong>, <strong>SQLite</strong>, <strong>DuckDB</strong>, <strong>ClickHouse</strong>, and <strong>SQL Server</strong> — safely, in one small Rust binary.
</p>

<p align="center">
  ⭐ If this saves you tokens, <strong>star the repo</strong> — it is the single biggest thing that helps other people find it.
</p>

---

## Install in one command — binary + AI setup included

This is the fastest path. It downloads the release binary (SHA256-verified), then
configures **every AI tool it finds on your machine** — Claude Code, Cursor, VS Code,
Claude Desktop, Codex, OpenCode, Windsurf, and Zed — so you do not hand-edit a single
JSON file. Existing entries for other servers are kept, and every file it touches is
backed up first.

```bash
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash -s -- --setup
```

With your database URL baked in (recommended):

```bash
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash -s -- --setup --database-url 'postgresql://user:pass@localhost:5432/mydb'
```

Other useful forms:

```bash
# Only specific tools
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash -s -- --setup --tools cursor,vscode,opencode

# Project-local configs (.cursor/mcp.json, .vscode/mcp.json, .mcp.json, opencode.json, .codex/config.toml)
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash -s -- --setup --project

# Binary already installed? Just do the setup step
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash -s -- --setup --skip-install

# Install only, no setup
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash
```

Then **restart your AI tool** and ask it:

> "List the tables in my database, then show me the 5 most recent orders."

You should see the tables listed, then the 5 most recent orders.

If you run the installer without a URL, configs reference `${env:DATABASE_URL}` (or
forward it, for Codex) — export `DATABASE_URL` in the environment that launches your
editor before starting it. Full installer reference: [docs/INSTALL.md](docs/INSTALL.md).

## One-click install

No terminal? These links open your editor with the server pre-filled. Replace the
placeholder database URL with your own where prompted.

| Client | One-click |
|---|---|
| **Cursor** | [Add to Cursor](https://cursor.com/install-mcp?name=strut-stack-sql&config=eyJjb21tYW5kIjoic3RydXQtc3RhY2stc3FsIiwiYXJncyI6WyItLXVybC1lbnYiLCJEQVRBQkFTRV9VUkwiLCItLWZ1bGwtdG9vbHMiXSwiZW52Ijp7IkRBVEFCQVNFX1VSTCI6IiR7ZW52OkRBVEFCQVNFX1VSTH0ifX0%3D) |
| **VS Code** | [Install in VS Code](https://insiders.vscode.dev/redirect/mcp/install?name=strut-stack-sql&config=%7B%22command%22%3A%22strut-stack-sql%22%2C%22args%22%3A%5B%22--url-env%22%2C%22DATABASE_URL%22%2C%22--full-tools%22%5D%2C%22env%22%3A%7B%22DATABASE_URL%22%3A%22%24%7Binput%3Adatabase_url%7D%22%7D%2C%22inputs%22%3A%5B%7B%22type%22%3A%22promptString%22%2C%22id%22%3A%22database_url%22%2C%22description%22%3A%22DATABASE_URL%20for%20strut-stack-sql%20%28e.g.%20postgresql%3A%2F%2Fuser%3Apassword%40localhost%3A5432%2Fdbname%29%22%2C%22password%22%3Atrue%7D%5D%7D) |
| **Claude Desktop** | Download the `.mcpb` bundle for your platform from [Releases](https://github.com/rzlco666/strut-stack-sql/releases/latest) and open it — it asks for `DATABASE_URL` and installs itself. |

GitHub does not render `cursor://` / `vscode:` links clickable. If a button above does
not open your editor, paste this into your browser's address bar instead:

```
vscode:mcp/install?%7B%22command%22%3A%22strut-stack-sql%22%2C%22args%22%3A%5B%22--url-env%22%2C%22DATABASE_URL%22%2C%22--full-tools%22%5D%2C%22env%22%3A%7B%22DATABASE_URL%22%3A%22%24%7Binput%3Adatabase_url%7D%22%7D%2C%22inputs%22%3A%5B%7B%22type%22%3A%22promptString%22%2C%22id%22%3A%22database_url%22%2C%22description%22%3A%22DATABASE_URL%20for%20strut-stack-sql%20%28e.g.%20postgresql%3A%2F%2Fuser%3Apassword%40localhost%3A5432%2Fdbname%29%22%2C%22password%22%3Atrue%7D%5D%7D
```

## Set up by hand (copy-paste)

Prefer the installer above. If you configure by hand, every block below is
**read-only by default**: 13 tools (3 core + 10 introspection), no writes, no DDL.
Replace `DATABASE_URL` with your real URL, or keep `--url-env DATABASE_URL` and
export it. Every block is also machine-checked in [docs/CLIENTS.md](docs/CLIENTS.md).

### Claude Code

```bash
claude mcp add --scope user --transport stdio \
  --env DATABASE_URL='postgresql://user:pass@localhost:5432/mydb' \
  strut-stack-sql -- strut-stack-sql --url-env DATABASE_URL --full-tools
```

Or in `.mcp.json` (project) — Claude Code expands `${VAR}` here:

```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "type": "stdio",
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": { "DATABASE_URL": "postgresql://user:pass@localhost:5432/mydb" }
    }
  }
}
```

Check it with `claude mcp get strut-stack-sql` (or `/mcp` inside a session).

### Cursor

Global `~/.cursor/mcp.json`, or project `.cursor/mcp.json`. Cursor interpolates
`${env:NAME}`, so this works with an exported `DATABASE_URL` and no secret in the file:

```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "type": "stdio",
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": { "DATABASE_URL": "${env:DATABASE_URL}" }
    }
  }
}
```

### VS Code

Workspace `.vscode/mcp.json` (or the user configuration via **MCP: Open User
Configuration**). Note: VS Code uses a top-level `servers` object, **not** `mcpServers` —
do not paste a Cursor block here unchanged:

```json
{
  "servers": {
    "strut-stack-sql": {
      "type": "stdio",
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": { "DATABASE_URL": "${env:DATABASE_URL}" }
    }
  }
}
```

### Claude Desktop

`claude_desktop_config.json` (`~/Library/Application Support/Claude/` on macOS,
`~/.config/Claude/` on Linux):

```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": { "DATABASE_URL": "postgresql://user:pass@localhost:5432/mydb" }
    }
  }
}
```

### Codex

`~/.codex/config.toml` (or project `.codex/config.toml`). Codex uses TOML, not JSON:

```toml
[mcp_servers.strut-stack-sql]
command = "strut-stack-sql"
args = ["--url-env", "DATABASE_URL", "--full-tools"]
env = { DATABASE_URL = "postgresql://user:pass@localhost:5432/mydb" }
startup_timeout_sec = 20
```

No URL in the file? Use `env_vars = ["DATABASE_URL"]` instead of `env` to forward
it from Codex's environment.

### OpenCode

The CLI is the version-proof path (it writes the right shape for your OpenCode):

```bash
opencode mcp add strut-stack-sql --env DATABASE_URL='postgresql://user:pass@localhost:5432/mydb' -- strut-stack-sql --url-env DATABASE_URL --full-tools
```

Or in `opencode.json` (global `~/.config/opencode/opencode.json`, or project root).
`command` is one array and env vars go in `environment`, not `env`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "strut-stack-sql": {
      "type": "local",
      "command": ["strut-stack-sql", "--url-env", "DATABASE_URL", "--full-tools"],
      "enabled": true,
      "environment": { "DATABASE_URL": "postgresql://user:pass@localhost:5432/mydb" },
      "timeout": 30000
    }
  }
}
```

Verify with `opencode mcp list`. On OpenCode v2, if the server does not appear,
nest it under `mcp.servers` instead of directly under `mcp`.

### Windsurf

`~/.config/devin/mcp_config.json` (current builds; older builds read
`~/.codeium/windsurf/mcp_config.json` — use whichever **Open MCP config file** in
Cascade opens):

```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": { "DATABASE_URL": "postgresql://user:pass@localhost:5432/mydb" }
    }
  }
}
```

Press refresh in Cascade after editing.

### Zed

`~/.config/zed/settings.json` (merged under `context_servers`, everything else kept):

```json
{
  "context_servers": {
    "strut-stack-sql": {
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": { "DATABASE_URL": "postgresql://user:pass@localhost:5432/mydb" }
    }
  }
}
```

### Any other MCP client

This is the whole contract — stdio, one binary, one env var:

```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": { "DATABASE_URL": "postgresql://user:pass@localhost:5432/mydb" }
    }
  }
}
```

## Try it

Once your AI tool shows the server as connected, these prompts exercise the whole
surface. Start read-only; writes and DDL stay off until you explicitly enable them
(see [Tools](#tools--exactly-20)).

```
"List the tables in my database, with row counts."
"Show me the schema of the orders table, including its foreign keys."
"Why is this query slow, and which index would fix it?"
  (paste any SELECT — analyze_query_performance returns an EXPLAIN tree,
   tiered findings, and index advice; the advice is text only, never executed)
"Find every table that references customers.id."
```

What the agent sees: `execute_sql` returns columnar JSON — column names once, compact meta:

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

Smoke-test the binary itself (no database needed) — a JSON `initialize` response
means the server speaks MCP:

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  | strut-stack-sql --url 'sqlite::memory:' --full-tools
```

## Connect your database

One URL, one flag. Credentials belong in the environment (or `.env` discovery in
your project), never in a pasted config — prefer `--url-env DATABASE_URL`.

| Engine | URL format | Notes |
|---|---|---|
| PostgreSQL | `postgresql://user:pass@host:5432/dbname` | |
| MySQL / MariaDB | `mysql://user:pass@host:3306/dbname` | `mariadb` engine hint maps here |
| SQLite | `sqlite:///absolute/path/app.db`, `sqlite::memory:` | Constrain files with `--sqlite-allowlist DIR[,DIR…]` |
| DuckDB | `duckdb:///absolute/path/analytics.duckdb`, `duckdb::memory:` | Embedded engine; constrain files with `--duckdb-allowlist DIR[,DIR…]`; `:memory:` is read-only via the guard |
| ClickHouse | `clickhouse://host:8123/db`, `clickhouses://host:8443/db` | HTTP interface; `s` = TLS |
| SQL Server | `sqlserver://user:pass@host:1433/dbname` (`mssql://` alias) | TLS + certificate validation default **on** |

```bash
# Single database, from the environment
DATABASE_URL='postgresql://user:pass@localhost:5432/app' strut-stack-sql --full-tools

# Explicit flag instead of the environment
strut-stack-sql --url-env DATABASE_URL --full-tools

# Project .env discovery (the server reads .env in your workspace)
strut-stack-sql
```

Multiple databases, per-principal grants, row/byte/timeout ceilings, and redaction
live in one TOML file — see [`strut-stack-sql.example.toml`](strut-stack-sql.example.toml):

```toml
# strut-stack-sql.toml
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

Full CLI / env / TOML reference: [docs/CONFIGURATION.md](docs/CONFIGURATION.md).
Per-engine notes: [Postgres](docs/POSTGRES.md) · [MySQL](docs/MYSQL.md) ·
[SQLite](docs/SQLITE.md) · [DuckDB](docs/DUCKDB.md) · [SQL Server](docs/CONFIGURATION.md#sql-server).

## Why

Most SQL MCP servers make agents pay three taxes:

1. **Token tax** — every tool schema and every row-as-object result burns context.
2. **Safety tax** — raw SQL passthrough means one hallucinated `DROP TABLE` is a production incident.
3. **Footprint tax** — a Node/Python runtime just to run `SELECT`.

Strut Stack SQL does not charge them:

- **3 tools by default — the `lite` profile.** The other 17 are opt-in: 10 introspection tools with `--full-tools`, and 7 DDL tools only when `--full-tools --allow-ddl` is set. A fresh install that shows three tools is working as designed.
- **Columnar results** — `{cols, rows, meta}` sends column names once, not once per row.
- **Every statement passes one AST guard** — parsed with `sqlparser`, classified recursively (including CTE bodies), checked against a tier, LIMIT-clamped, and rejected *before* a pool connection is checked out.
- **One static binary, six engines**, behind a stable driver contract.
- **Measured, not claimed** — idle RSS under 8 MB, `tools/list` at 2,643 bytes in lite mode (625 tokens `o200k_base`; 10,853 bytes / 2,549 tokens in full mode), cold start to `initialize` at 2.52 ms p95 over stdio. Token numbers are v2.0.1 (2026-11-05); runtime numbers are v2.0.0 (2026-11-02). Full numbers and their limits: [Benchmarks](#benchmarks-measured).

## Tools — exactly 20

| Surface | Flag | Tools |
|---|---|---|
| **Core** (always) | — | `search_objects`, `execute_sql`, `analyze_query_performance` |
| **Introspection** (10) | `--full-tools` | `list_sources`, `list_schemas`, `list_tables`, `describe_table`, `list_indexes`, `list_foreign_keys`, `schema_overview`, `get_relationship_graph`, `suggest_joins`, `get_schema_context` |
| **DDL** (7) | `--full-tools --allow-ddl` | `schema_mutate`, `create_table`, `drop_table`, `add_column`, `alter_column`, `drop_column`, `truncate_table` |

- `search_objects` — ranked progressive schema discovery (exact > prefix > substring > comment match).
- `execute_sql` — single statement, bound parameters, or a governed concurrent batch; columnar / rows / auto output; cursor pagination for large results.
- `analyze_query_performance` — normalized EXPLAIN tree, tiered findings, index advice (advice text only; never executed).
- `get_relationship_graph` / `suggest_joins` / `get_schema_context` — FK graph, shortest join paths, token-budgeted schema digest.
- DDL aliases require `"confirm": true` for destructive actions.

Profiles are presets over the same flags — no second authorization system:

| Profile | Write mode | `--full-tools` | Approvals |
|---|---|---|---|
| `lite` | read-only | no | no |
| `standard` | read-only | yes | no |
| `full` | writes | yes | no |
| `admin` | ddl | yes | required |

```bash
strut-stack-sql --profile standard   # read-only, 13 tools (3 core + 10 introspection)
```

`list_tools` and invocation use the same authorization function, so a tool hidden from the list is
also rejected when called directly. Full schemas and error contract: [docs/TOOLS.md](docs/TOOLS.md).

## Safety — read-only by default, fail-closed everywhere else

- **AST guard before pool checkout** — one statement per string; unknown statement types are denied, not guessed.
- **Recursive classification** — writes hidden in CTEs (`WITH … DELETE … RETURNING … SELECT …`), `SELECT … INTO`, `INTO OUTFILE`/`DUMPFILE`, row-locking clauses, and dangerous functions (`pg_read_file`, `LOAD_FILE`, `pg_sleep`, …) are never counted as reads.
- **Per-principal grants** — `read < writes < ddl < admin` per principal × source, with optional row/byte/timeout ceilings. The process flags (`--allow-writes`, `--allow-ddl`) are a ceiling no grant can exceed.
- **Admin gate** — `GRANT`/`REVOKE` use the separate `Admin` tier and additionally require `--allow-grants`.
- **Column redaction, not denial** — `--deny-columns password_hash,totp_secret` replaces matching **values** with `"[redacted]"`: exact, case-insensitive column-name match, column names stay visible. An alias (`SELECT password AS p`) evades it, so treat it as a seat-belt, not an access-control boundary — use a read-only database user and views that never select the secret. DuckDB's guard additionally blocks `read_csv`, `read_text`, `ATTACH`, `INSTALL`, and `LOAD` so an agent cannot read files outside the allowlist.
- **Dry runs and explicit consent** — DDL accepts `dry_run: true` (parse, classify, and report without executing), destructive aliases require `confirm: true`, and the `admin` profile adds single-use approvals (`approval_id` / MCP elicitation) on top.
- **Per-database allowlist (opt-in)** — `--allow-databases app,analytics` denies cross-database table qualifiers, hides other databases from `list_schemas`, and rejects `schema: "*"`; unqualified names still resolve on the connected database, so pair it with a database-scoped credential ([limits](docs/CONFIGURATION.md#hardening)).
- **HTTP auth** — `STRUT_STACK_SQL_HTTP_TOKENS` entries are `name:secret:mode[:source…]`; secrets may be stored as `sha256:<64 hex>` and are compared in constant time. OAuth 2.1 resource-server mode validates RS256 JWTs against the issuer's JWKS and fails closed if discovery fails. A non-loopback HTTP bind with no auth fails startup unless `--allow-unauthenticated-http` is passed explicitly.

Remote HTTP mode:

```bash
STRUT_STACK_SQL_HTTP_TOKENS="alice:s3cr3t:readonly" \
  strut-stack-sql --http 0.0.0.0:8080
```

Full model: [docs/SECURITY.md](docs/SECURITY.md). Safe probes: `/livez` never touches a
database; `/readyz` uses cached pool status and never returns raw database errors.

## Security boundary and limitations

Defense in depth, not a sandbox. The controls in the Safety section above are real, but none of them replaces least-privilege database accounts, network restrictions, and review of SQL privileges.

- **The guard parses SQL; it does not understand intent.** Statements are parsed with `sqlparser` into an AST and classified recursively (including CTE bodies); anything the parser cannot represent is rejected, not guessed. That is a policy check on the statement's shape — not a proof that the query is safe to run against your data.
- **Database privileges remain decisive.** Run the server with a dedicated, least-privilege database account — never an administrative one — and isolate demonstration environments. A read-only database user and views that never select the secret are the real boundary; `--deny-columns` is a seat-belt, not a wall (an alias evades the name match).
- **Treat credentials as secrets.** Never commit `.env` files, real token hashes, raw tokens, or logs containing sensitive SQL.
- **Transport needs deployment security.** Bind locally for development. The HTTP transport must not be exposed publicly without a properly secured TLS / reverse-proxy and network policy — a non-loopback bind without auth fails startup unless `--allow-unauthenticated-http` is passed explicitly.
- **There is no persistent audit log yet.** Tool calls and SQL text can appear in logs — control log access, rotation, retention, and redaction according to your environment. A JSONL audit log with agent identity is on the roadmap, not in this release.
- **This is not a security certification.** Review and test it before any production deployment. Data returned through tools must be treated as untrusted content, not as instructions to the agent.

What exists today, what is still on the roadmap, and how to report a vulnerability (privately): [SECURITY.md](SECURITY.md). The full threat model lives in [docs/SECURITY.md](docs/SECURITY.md).

## More ways to install

The installer above is recommended. Everything below installs the same binary;
full matrix and release asset names: [docs/INSTALL.md](docs/INSTALL.md).

```bash
# Homebrew (macOS)
brew install rzlco666/tap/strut-stack-sql

# Windows
scoop install rzlco666/strut-stack-sql
# or
winget install rzlco666.strut-stack-sql

# Docker (no install at all)
docker run --rm -i -e DATABASE_URL='postgresql://user:pass@host:5432/db' \
  ghcr.io/rzlco666/strut-stack-sql:latest

# From source (crates.io publication is off)
cargo install --path . --bins
```

MCPB bundles for every platform are attached to each
[GitHub Release](https://github.com/rzlco666/strut-stack-sql/releases/latest) and described by
[`server.json`](server.json) for the MCP Registry.

## Open source

The whole stack is [MIT](LICENSE) licensed: one static Rust binary for six engines
(PostgreSQL, MySQL, SQLite, DuckDB, ClickHouse, SQL Server), with the guard, formatter, tools,
server, and drivers as separate crates. Choose a surface with `--profile lite|standard|full|admin`
instead of maintaining four builds — profiles are presets over the same flags and grants, not a
second authorization system ([docs/PROFILES.md](docs/PROFILES.md)).

- Security model and threat model: [docs/SECURITY.md](docs/SECURITY.md).
- Install channels: one-line installer, Homebrew, Scoop, winget, Docker, `cargo install --path .`,
  and per-platform MCPB bundles ([docs/INSTALL.md](docs/INSTALL.md)).
- Contributions: [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md).

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
  DriverAPI --> DK[strut-sql-driver-duckdb]
  DriverAPI --> CH[strut-sql-driver-clickhouse]
  DriverAPI --> MS[strut-sql-driver-sqlserver]
  PG --> DB[(PostgreSQL)]
  MY --> DB2[(MySQL)]
  SQ --> DB3[(SQLite)]
  DK --> DB4[(DuckDB file / memory)]
  CH --> DB5[(ClickHouse HTTP)]
  MS --> DB6[(SQL Server)]
  Tools --> Format[strut-sql-format]
  Server --> Core[strut-sql-core]
```

Thirteen crates plus a thin binary. The core, guard, format, tools, and server crates never import a
database library; only a driver crate does. The driver boundary is one trait
(`EngineDriver` in `crates/strut-sql-driver-api`): drivers own connections, execution, and
catalogs, while dialect selection, capability checks, authorization, and tool SQL generation stay
shared. Full crate map: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Benchmarks (measured)

Raw artifact: [`scripts/benchmark/results/2.0.0.json`](scripts/benchmark/results/2.0.0.json) —
2026-11-02, WSL2 Ubuntu 24.04, Intel i5-13420H (6 vCPU), 5.79 GB RAM, SQLite temp database,
n=120 iterations after 20 warmups, 20 cold-start runs. Reproduce with
`./scripts/benchmark/run-bench.sh`. The table below is the v2.0.0 runtime benchmark — for the
v2.0.1 token numbers, see [Token benchmark](#token-benchmark-v201) below.

| Metric | Target | stdio | HTTP |
|---|---:|---:|---:|
| Idle RSS | ≤ 15 MB | **7.93 MB** ✅ | **8.78 MB** ✅ |
| Cold start (spawn → `initialize`) p95 | ≤ 25 ms | **2.52 ms** ✅ | **41.74 ms** ❌ |
| `tools/list` (core mode) | ≤ 3,072 B | **3,039 B** ✅ | **3,039 B** ✅ |
| 100 rows × 8 cols (before truncation) | ≤ 70 KB | **7,526 B** ✅ | **7,526 B** ✅ |
| `execute_sql` SELECT by PK, p50 | ≤ 1 ms | **0.80 ms** ✅ | **1.68 ms** ❌ |

Two measurement classes are kept apart instead of blended:

- **In-process** — engine work on a local SQLite file. `tools/list` is **3,039 bytes** (v2.0.0) for the core
  surface, and a 100 × 8 result is **7,526 bytes** of columnar text before truncation (column names
  once, not once per row). These numbers describe payload shape, not network cost.
- **Roundtrip over stdio/HTTP** — process spawn → `initialize`, plus one request/response per
  `execute_sql`. The 0.80 ms stdio p50 includes local SQLite execution and JSON-RPC framing; the
  harness cannot split tool overhead from database time on an in-process file.

The two HTTP misses are real and are not rounded away: cold start includes listener readiness and
the session handshake, and each HTTP `execute_sql` is a fresh request, so transport framing
dominates. PostgreSQL and MySQL were not measured (no live server on the reference machine) and
are recorded as `not_run`, together with the competitor comparison. Methodology and limits:
[docs/BENCHMARKS.md](docs/BENCHMARKS.md).

### Token benchmark (v2.0.1)

Measured 2026-11-05 (run 1) and independently re-run 2026-11-06 (run 2, byte-identical payloads)
against the v2.0.1 release binary (`strut-stack-sql 2.0.1`, commit `805d18f`, MCP protocol
2025-06-18, tiktoken 0.14.0). Tokens count only what the model sees — per-tool `name` +
`description` + `inputSchema` (method: octomind.run, 2026-10-02). These are schema tokens, not
session tokens and not a Claude bill. Full method, per-tool tables, and reproduction steps:
[docs/TOKEN-BENCHMARK.md](docs/TOKEN-BENCHMARK.md).

| Profile | Tools | Canonical (name+desc+schema) | Tokens `o200k_base` | Tokens `cl100k_base` | Byte cap (CI) | Token budget |
|---|---|---|---|---|---|---|
| lite | 3 | 2,629 B | 625 | 605 | ≤ 3,072 B ✅ | ≤ 750 |
| standard | 13 | 5,655 B | 1,313 | 1,279 | — | — |
| full | 20 | 10,822 B | 2,549 | 2,479 | ≤ 12,000 B ✅ | ≤ 3,000 |

Raw `tools/list` payloads: lite 2,643 B, standard 5,679 B, full 10,853 B. The old headline figure
of 3,039 bytes was the v2.0.0 payload; it does not apply to v2.0.1. Token budgets (lite ≤ 750,
full ≤ 3,000 `o200k_base`) are adopted design budgets; the CI job enforcing them is follow-up
work, not yet a gate.

## Protocol and clients

- **Pinned MCP specification: 2025-06-18.** Version negotiation, `tools/list`, `tools/call`, error shapes, Streamable HTTP sessions, auth metadata, and DNS-rebinding `Origin` checks are exercised by a runnable gate. Older requested versions are answered with 2025-06-18; JSON-RPC batches are rejected.
- **Eight-client setup, six-client checked matrix:** the installer and the copy-paste blocks above cover Claude Code, Cursor, VS Code, Claude Desktop, Codex, OpenCode, Windsurf, and Zed. The six-client matrix in [docs/CLIENTS.md](docs/CLIENTS.md) (Cursor, Claude Code, VS Code, Codex, Windsurf, OpenCode) has every config block parsed and the minimal stdio command smoke-run in CI.
- **Historical 2.0.0 conformance results:** [docs/CONFORMANCE.md](docs/CONFORMANCE.md).

## Troubleshooting

| Symptom | Fix |
|---|---|
| Agent says the server is not connected | Restart the editor after editing its config — most clients only read MCP configs at startup. Then check `claude mcp get strut-stack-sql` / `opencode mcp list` / the editor's MCP panel. |
| "No database" / connection errors | `DATABASE_URL` must be exported in the **environment that launches the editor**, not just your terminal. If in doubt, put the literal URL in the config's `env` block. |
| Binary not found | The installer puts it in `~/.local/bin` — add that to `PATH`, or use the absolute path in your config's `command` field (the `--setup` installer does this automatically). |
| Writes/DDL rejected | Expected: read-only is the default. Add `--allow-writes` (writes) or `--full-tools --allow-ddl` (DDL) — or `--profile full` / `--profile admin` — and confirm the grant tier allows it. |
| DuckDB cannot read a file | The file must be inside `--duckdb-allowlist` (same for SQLite with `--sqlite-allowlist`); the guard blocks everything outside it, by design. |

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
| [docs/POSTGRES.md](docs/POSTGRES.md) · [docs/MYSQL.md](docs/MYSQL.md) · [docs/SQLITE.md](docs/SQLITE.md) · [docs/DUCKDB.md](docs/DUCKDB.md) · [SQL Server](docs/CONFIGURATION.md#sql-server) | Per-engine notes |
| [docs/MCP_REGISTRY.md](docs/MCP_REGISTRY.md) | MCP Registry / MCPB publishing |
| [CHANGELOG.md](CHANGELOG.md) | What changed in 2.0.0 (breaking: no v1 compatibility layer, `mcp-sql-rust` alias removed) |

## Contributing

Issues and PRs welcome — start with [docs/CONTRIBUTING.md](docs/CONTRIBUTING.md). Include a minimal
SQL repro for guard issues (never production credentials).

## License

[MIT](LICENSE)

---

<p align="center">
  Built for agents that should ask the database, not guess it.<br>
  ⭐ <strong>Star it</strong> if it earns its place in your stack — and tell one person who still pastes SQL into chat.
</p>
