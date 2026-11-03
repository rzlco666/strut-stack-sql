# Configuration — strut-stack-sql

## Resolution order (as implemented)

There are two mutually exclusive configuration paths. **The TOML path wins
whenever it is selected:**

1. **TOML multi-source — if `--config <file>` is given** (or the file exists
   at the path passed), the server loads *only* that file's sources. On this
   path the single-source CLI/env resolution below is **not** consulted:
   `--url` / `--url-env` / `DATABASE_URL` do not override TOML sources, and a
   TOML source without `url` / `url_env` is an error — it never silently
   inherits the global `DATABASE_URL`. Process-wide options (write mode,
   `--max-rows`, timeouts, deny-columns) still come from the CLI on both
   paths.
2. **Single-source — otherwise**, exactly one source named `default`,
   resolved in this order:
   1. CLI `--url`
   2. CLI `--url-env <VAR>` (the named environment variable)
   3. `.env` discovery (walk cwd → parents), then environment:
      - `DATABASE_URL`
      - `POSTGRES_URL` / `MYSQL_URL` / `SQLITE_URL`
      - `POSTGRES_*` / `PG*` connection parts
      - `MYSQL_*` connection parts
   4. **Ephemeral fallback** — if nothing resolves, the server falls back
      to in-memory SQLite so a first-run MCP handshake can succeed, but
      **never silently**: it prints a loud multi-line banner on stderr and
      announces the ephemeral mode to the client in `ServerInfo`.
      Acknowledge the mode explicitly with `--ephemeral` (below).

> There is **no** `MCP_SQL_CONFIG` environment variable. The TOML path is
> selected with the `--config` CLI flag only. (Older docs mentioned
> `MCP_SQL_CONFIG`; it never existed in the code and has been removed.)

### Ephemeral mode

```bash
strut-stack-sql --ephemeral          # or STRUT_STACK_SQL_EPHEMERAL=1
```

With no database configured, the server starts on an in-memory SQLite
(`sqlite::memory:`) so MCP `initialize` / `tools/list` succeed with zero
setup — handy for trying the server out. This is **never silent**: a loud
multi-line banner on stderr states that no real database is connected and
all data vanishes at exit, and the mode is announced to the client in
`ServerInfo`. Passing `--ephemeral` acknowledges the mode (and downgrades
the banner to a warning). Never use it for real work.

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
DATABASE_URL=sqlite://./data/app.db
DATABASE_URL=sqlite::memory:
```

```env
POSTGRES_HOST=localhost
POSTGRES_PORT=5432
POSTGRES_USER=app
POSTGRES_PASSWORD=<redacted>
POSTGRES_DB=app
```

## Multi-source TOML

The example file in this repo is
[`strut-stack-sql.example.toml`](../strut-stack-sql.example.toml) — *the*
example for `strut-stack-sql --config`. Copy it to `strut-stack-sql.toml`:

```toml
default_source = "app"

[[sources]]
name = "app"
url_env = "DATABASE_URL"

[[sources]]
name = "analytics"
url_env = "ANALYTICS_DATABASE_URL"
engine = "postgres"
```

Rules:

- **Prefer `url_env`** — it keeps credentials out of files. Every source in
  the shipped example uses `url_env` for exactly that reason; do not paste
  URLs with plaintext passwords into TOML.
- `default_source` is honored as written and **must name an existing
  source** (a typo is a startup error). When omitted, the default is
  deterministic in v2: the **lexicographically first** source name,
  chosen with a startup warning (previously this was hash-map order).
- Duplicate source names and unknown keys are startup errors (fail-closed).
- Pass `"source": "analytics"` in tool calls to target a non-default source.

## HTTP tokens

HTTP mode (`--http`) authenticates with API tokens from the environment:

```bash
STRUT_STACK_SQL_HTTP_TOKENS="alice:s3cr3t:readonly,bob:other-token:writes:main|reporting" \
  strut-stack-sql --http 0.0.0.0:8080
```

Format: **comma-separated** entries of the form
`name:token:mode[:source1|source2|...]`:

- `mode` is the token's **maximum** write mode: `readonly`, `writes`, or
  `ddl` (aliases `read`/`ro`, `write`/`dml`, `admin` are accepted). The
  effective mode is the *lower* of the process tier (CLI flags) and the
  token mode — a token can only lower what its holder may do.
- The optional 4th field is a **source allowlist** (`|`-separated source
  names); without it the token may use every source.
- Token secrets must not contain `:` or `,`. Duplicate token names are a
  startup error.
- Secrets may be stored as `sha256:<64 lowercase hex>` instead of plaintext:
  `alice:sha256:9f86d0…:readonly`. Raw and hashed secrets are both compared
  in constant time. Generate a digest with
  `printf %s "$TOKEN" | sha256sum`.

For a single quick token there is also `--http-token <secret>` (name `cli`,
mode = the process mode). Clients send `Authorization: Bearer <token>`.
Requests are rate-limited per token (`--http-rate-limit-per-minute`,
default 600, 0 = off).

## OAuth 2.1 resource server

Set both variables to validate Bearer JWTs from an external authorization
server:

```bash
STRUT_STACK_SQL_OAUTH_ISSUER="https://auth.example.com" \
STRUT_STACK_SQL_OAUTH_AUDIENCE="https://mcp.example.com/mcp" \
  strut-stack-sql --http 0.0.0.0:8080
```

- The server discovers `/.well-known/oauth-authorization-server` (falling back
  to `/.well-known/openid-configuration`), fetches the advertised JWKS, and
  validates **RS256** signature plus `iss`, `aud`, and `exp`.
- The principal id comes from the claim named by
  `STRUT_STACK_SQL_OAUTH_SUBJECT_CLAIM` (default `sub`). It is mapped through
  the same `[[grants]]` policy as API keys.
- `GET /.well-known/oauth-protected-resource` (and the resource-path variant)
  serves RFC 9728 metadata; 401 responses carry the `resource_metadata`
  challenge.
- Discovery/JWKS failure at startup makes OAuth tokens fail closed; API keys
  keep working. There is no built-in authorization server and no token
  issuance.
- Token material is never logged.

The HTTP transport pins MCP spec **2025-06-18**. An unsupported
`MCP-Protocol-Version` header is rejected with 400; an `initialize` body that
requests an older/unknown version is answered with 2025-06-18. Browser-style
`Origin` headers are checked for DNS rebinding: loopback origins must match the
`Host` header and port, and non-loopback origins are rejected unless listed in
`STRUT_STACK_SQL_ALLOWED_ORIGINS` (comma-separated exact origins). Requests
without an `Origin` header (normal MCP clients) are unaffected.

Binding a **non-loopback** address with no tokens configured is refused at
startup; the only override is the explicit, loudly warned
`--allow-unauthenticated-http` flag. Loopback binds without tokens work for
local development and print a warning. See
[SECURITY.md](SECURITY.md#http-token-authentication).

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
strut-stack-sql --allow-explain-analyze     # advisor may run EXPLAIN ANALYZE (executes SQL)
strut-stack-sql --workspace /path/to/project   # chdir + .env walk + SQLite relative paths
strut-stack-sql --eager-connect                # connect at startup (default: lazy)
strut-stack-sql --ephemeral                    # acknowledge the in-memory SQLite fallback
strut-stack-sql --http-token <secret>          # single HTTP bearer token (name "cli")
strut-stack-sql --http-rate-limit-per-minute 600
```

Numeric options are range-validated at startup: zero or absurd
values for `--max-rows`, `--pool-size`, timeouts, and `--batch-concurrency`
are rejected instead of being silently accepted.

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

Default is **columnar** — `MCP_SQL_FORMAT` only changes it when set:

```bash
MCP_SQL_FORMAT=columnar strut-stack-sql   # default: {cols, rows, meta}
MCP_SQL_FORMAT=rows strut-stack-sql       # row objects
MCP_SQL_FORMAT=auto strut-stack-sql       # row objects when ≤10 rows, else columnar
```

Per-query override on `execute_sql`: `"format": "rows" | "columnar" | "auto"`.

## Cursor IDE

See [CURSOR.md](CURSOR.md) for the official global launcher (`packaging/cursor-mcp-launcher.mjs`).

## Install-specific notes

| Channel | Config tip |
|---------|------------|
| curl / brew / releases | Put `DATABASE_URL` in project `.env` |
| Docker | Pass `-e DATABASE_URL=...` |
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
MCP_SQL_LOG=audit=info strut-stack-sql   # audit trail of executed SQL (see SECURITY.md)
```

Logs go to **stderr** (stdout is MCP stdio).
