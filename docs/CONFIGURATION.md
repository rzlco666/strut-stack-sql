# Configuration — strut-stack-sql

## Resolution order

1. CLI `--url` / `--url-env`
2. Walk cwd → parents for `.env`, then:
   - `DATABASE_URL`
   - `POSTGRES_URL` / `MYSQL_URL` / `SQLITE_URL`
   - `POSTGRES_*` / `PG*` parts
   - `MYSQL_*` parts
3. `--config strut-stack-sql.toml` multi-source
4. Error listing searched paths

## Docker Compose (local dev)

```bash
docker compose up -d
cp .env.example .env
```

Default from [`.env.example`](../.env.example):

```env
DATABASE_URL=postgresql://demo:demo@localhost:5433/demo
# DATABASE_URL=mysql://demo:demo@localhost:3307/demo
```

Inside Dev Container, use hostnames `postgres` / `mysql` (see [QUICKSTART.md](QUICKSTART.md)).

## `.env` examples

```env
DATABASE_URL=postgresql://user:pass@localhost:5432/app
```

```env
DATABASE_URL=mysql://user:pass@localhost:3306/app
```

```env
DATABASE_URL=sqlite::memory:
DATABASE_URL=sqlite://./data/app.db
```

```env
POSTGRES_HOST=localhost
POSTGRES_PORT=5432
POSTGRES_USER=app
POSTGRES_PASSWORD=secret
POSTGRES_DB=app
```

## Multi-source TOML

See [`strut-stack-sql.example.toml`](../strut-stack-sql.example.toml):

```toml
default_source = "app"
deny_columns = ["password_hash"]

[[sources]]
name = "app"
url_env = "DATABASE_URL"

[[sources]]
name = "analytics"
url = "postgresql://readonly:secret@localhost:5432/analytics"
engine = "postgres"
```

Pass `"source": "analytics"` in tool calls.

## CLI reference

```bash
strut-stack-sql --version
strut-stack-sql                          # stdio, .env, read-only
strut-stack-sql --allow-writes
strut-stack-sql --allow-ddl
strut-stack-sql --full-tools
strut-stack-sql --http 127.0.0.1:8080
strut-stack-sql --config ./strut-stack-sql.toml
strut-stack-sql --url "$DATABASE_URL"
strut-stack-sql --max-rows 50 --max-bytes 32768
strut-stack-sql --pool-size 16 --query-timeout 15 --connect-timeout 2
strut-stack-sql --batch-concurrency 4 --fail-fast
strut-stack-sql --deny-columns password_hash,totp_secret
strut-stack-sql --workspace /path/to/project   # chdir + .env walk + SQLite relative paths
strut-stack-sql --eager-connect                # connect at startup (default: lazy)
```

## Column deny-list (opt-in)

Empty by default (no silent redaction). Resolve order:

1. CLI `--deny-columns a,b`
2. Env `MCP_SQL_DENY_COLUMNS=a,b`
3. TOML `deny_columns = ["a", "b"]`

Matched column names (case-insensitive) are replaced with `"[redacted]"` in `execute_sql` results; `meta.redacted_columns` lists hits.

## Lazy connect

By default the server **does not** connect to the database at startup. The first tool call establishes the pool:

1. **TCP preflight** (MySQL/Postgres) — ≤500ms connect probe; fails fast if host/port unreachable
2. **sqlx pool** — default **2s** acquire timeout via `--connect-timeout`

Errors redact passwords:

```
cannot connect to mysql://user:***@127.0.0.1:3306/db: TCP preflight failed in <500ms (...)
```

Cursor launcher (`packaging/cursor-mcp-launcher.mjs`) also probes TCP before spawn (default 500ms; override with `MCP_SQL_PREFLIGHT_MS`).

Use `--eager-connect` to ping all sources before serving MCP (previous behavior).

## Response format

```bash
MCP_SQL_FORMAT=auto|columnar|rows strut-stack-sql
```

Per-query override on `execute_sql`: `"format": "rows"`. `auto` returns row objects when ≤10 rows, else columnar.

## Cursor IDE

See [CURSOR.md](CURSOR.md) for the official global launcher (`packaging/cursor-mcp-launcher.mjs`).

## Install-specific notes

| Channel | Config tip |
|---------|------------|
| curl / brew / releases | Put `DATABASE_URL` in project `.env` |
| Docker | Pass `-e DATABASE_URL=...` to `docker run` |
| MCP Registry `.mcpb` | Set `DATABASE_URL` in client env |
| Dev Container | Pre-set in `devcontainer.json` / compose |

Full install matrix: [INSTALL.md](INSTALL.md)

## Cursor MCP config

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

With writes (local only):

```json
{
  "mcpServers": {
    "sql": {
      "command": "strut-stack-sql",
      "args": ["--allow-writes"]
    }
  }
}
```

Put secrets in project `.env`, not in `args`.

## Logging

```bash
MCP_SQL_LOG=debug strut-stack-sql
```

Logs go to **stderr** (stdout is MCP stdio).
