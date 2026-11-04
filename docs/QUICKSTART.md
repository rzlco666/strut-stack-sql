# Quickstart

Get **strut-stack-sql** talking to real databases in about a minute.

## Prerequisites

- [Docker](https://docs.docker.com/get-docker/) + Docker Compose
- [Cursor](https://cursor.com) or another MCP client
- Rust 1.88+ (or a [pre-built binary](INSTALL.md))

## Option A — Dev Container (recommended)

1. Clone the repo and open in Cursor / VS Code
2. **Reopen in Container** (`.devcontainer/devcontainer.json` starts Postgres + MySQL)
3. Wait for `postCreateCommand` (`cargo build`) to finish
4. Copy env: `cp .env.example .env`
5. Add MCP config (see below)

First container build may take ~3 minutes.

## Option B — Docker Compose only

```bash
git clone https://github.com/rzlco666/strut-stack-sql.git
cd strut-stack-sql
docker compose up -d
cp .env.example .env
cargo build --release
```

Wait for healthchecks:

```bash
docker compose ps
```

Host ports: Postgres **5433**, MySQL **3307** (see `.env.example`). Inside Dev Container use service hostnames `postgres` / `mysql` on default container ports.

## MCP client config (Cursor)

Project `.cursor/mcp.json` or user MCP settings:

```json
{
  "mcpServers": {
    "sql": {
      "command": "/workspaces/strut-stack-sql/target/debug/strut-stack-sql",
      "args": [],
      "env": {
        "DATABASE_URL": "postgresql://demo:demo@postgres:5432/demo"
      }
    }
  }
}
```

In Dev Container, use hostname `postgres` / `mysql` instead of `localhost`.

For a release binary on the host:

```json
{
  "mcpServers": {
    "sql": {
      "command": "strut-stack-sql",
      "args": []
    }
  }
}
```

## Try the default tools

With read-only mode (default), run queries against seed data:

```sql
-- search_objects
{ "object_type": "table", "keyword": "users" }

-- execute_sql
{ "sql": "SELECT id, email, name FROM demo.users WHERE id = $1", "params": [42] }

-- analyze_query_performance
{ "sql": "SELECT * FROM demo.orders WHERE user_id = $1", "params": [7] }
```

MySQL uses `?` placeholders instead of `$1`.

### Pagination (large result sets)

```json
{
  "sql": "SELECT id, email FROM demo.users ORDER BY id",
  "page_offset": 0,
  "page_size": 50
}
```

Repeat with `page_offset: 50`, `100`, … while `meta.has_more` is true.

### Switch to MySQL

```env
DATABASE_URL=mysql://demo:demo@localhost:3307/demo
```

Or in Dev Container: `mysql://demo:demo@mysql:3306/demo`

## Seed data

| Table | Rows | Notes |
|-------|------|-------|
| `users` | 200 | indexed email |
| `products` | 50 | SKU + price |
| `orders` | 1000 | FK to users/products; index on `user_id`, `status` |

Schema: `demo` (Postgres) / database `demo` (MySQL).

## Writes (optional)

Default is read-only. To demo INSERT:

```bash
strut-stack-sql --allow-writes
```

DDL (`CREATE TABLE`, etc.) requires `--allow-ddl`. See [SECURITY.md](SECURITY.md).

## HTTP mode (optional)

```bash
strut-stack-sql --http 127.0.0.1:8080
curl -s http://127.0.0.1:8080/healthz
```

## Next steps

- [INSTALL.md](INSTALL.md) — all install channels
- [TOOLS.md](TOOLS.md) — tool schemas and examples
- [CONFIGURATION.md](CONFIGURATION.md) — env, TOML, multi-source
- [BENCHMARKS.md](BENCHMARKS.md) — performance vs other MCP SQL servers
