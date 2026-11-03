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

## Table rules, row filters and masking (`[[grants]]`)

A grant can narrow what its principal sees beyond the tier. All four fields are
optional and additive across grants that match the same principal/source:

```toml
[[grants]]
principal   = "analyst"
source      = "app"
max_tier    = "read"
allow_tables = ["public.orders", "public.customers"]   # non-empty = only these
deny_tables  = ["public.payroll", "customers"]         # always wins over allow
row_filters  = [{ table = "public.orders", predicate = "tenant_id = 7" }]
mask         = [
  { column = "email", mode = "partial" },                    # a***@example.com
  { table = "public.customers", column = "tax_id", mode = "hash" },
  { column = "phone", mode = "null" },
]
```

- **Deny wins over allow.** With both set, a denied table is unreachable even
  if it is also listed in `allow_tables`; an empty `allow_tables` means "every
  table not denied". Schema-qualified names are matched against the schema
  part; an unqualified name matches any schema.
- **Row filters are injected into the AST**, as `AND (predicate)` around the
  table reference, so aliases, joins, CTEs and existing `WHERE` clauses keep
  working. The predicate must be a single expression (no `;`, no comments) and
  is validated by the guard. A statement family where the filter cannot be
  applied — DML/DDL on a filtered table, or a shape the rewriter cannot see —
  is **rejected** with `guard_denied` rather than run unfiltered.
- **Masks apply to returned cells**: `partial` keeps a recognisable prefix
  (first character plus the domain for email-like values), `hash` replaces the
  value with its SHA-256 hex digest, `null` replaces it with JSON null. Masked
  columns are listed in `meta.redacted_columns`. A table-scoped mask wins for
  its table; an unscoped mask applies to that column name on every table.
- **Enforcement points**: `execute_sql` (single, batch, cursor and summary),
  HTTP `/export`, the EXPLAIN advisor, and the schema tools — `list_tables`,
  `describe_table`, `list_indexes`, `list_foreign_keys`, `schema_overview`,
  `search_objects`, `get_schema_context`, `suggest_joins` and
  `get_relationship_graph` never reveal a table the caller cannot query.
- Denials name the table and the principal's limits, never another tenant's
  rows or the filter's hidden predicate values.

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

## Destructive DDL approvals (R2-05)

Start the server with `--require-approval` (or
`STRUT_STACK_SQL_REQUIRE_APPROVAL=1`) to require an accepted approval for
`drop_table`, `truncate_table`, `drop_column`, and the matching `schema_mutate`
actions, including the same statements sent through `execute_sql`.

Flow:

1. The call fails with `approval_required` (-32015) and `data.approval_id`,
   `data.expires_unix`, `data.accept_path`. Nothing is executed or locked.
2. The caller (or an operator on the same machine) decides it:

   ```bash
   curl -sS -H "authorization: Bearer $TOKEN" http://127.0.0.1:8080/approvals
   curl -sS -X POST -H "authorization: Bearer $TOKEN" \
        http://127.0.0.1:8080/approvals/<id>/accept
   ```

3. The same principal retries the call with `approval_id: "<id>"` and, for the
   DDL tools, `confirm: true`. The approval is consumed by that one execution.

Rules:

- Queue entries expire after **15 minutes** and are single-use; a replay
  returns `approval_rejected`.
- Decisions are bound to the requesting principal. Another principal cannot
  accept or reject an entry, and the API does not reveal whether an id exists.
- Clients that support MCP elicitation are asked directly by the server; the
  accepted answer becomes the same kind of single-use approval. Clients
  without elicitation use the HTTP queue.
- **`confirm: true` is not an approval.** It remains the legacy confirmation
  flag required by the DDL tools, is never recorded in the queue and never
  satisfies a pending approval.

## Maintenance commands

```bash
strut-stack-sql doctor --config strut-stack-sql.toml   # checks + exit code
strut-stack-sql migrate --from-v1 --input v1.toml      # dry run, prints V2 TOML
strut-stack-sql migrate --from-v1 --input v1.toml --write --out strut-stack-sql.toml
```

`doctor` loads the real configuration (the same `--config`/`--url`/env resolution
the server uses), then checks every source with a real connection and reports
capability degradations. Exit codes: `0` all checks passed, `1` configuration
could not be loaded, `2` at least one source failed to connect, `3` reachable
but degraded. URLs are redacted in its output.

`migrate --from-v1` translates a V1 TOML: sources and `deny_columns` carry
over, top-level V1 process options are reported with the CLI flag that
replaces them (`max_rows` → `--max-rows`, …), and the binary rename is stated
once. It never touches a database and **never overwrites a file** unless
`--write` is passed; without `--write` it prints the translated file.

## Observability (OTel-shaped fields)

Query events carry OpenTelemetry-style attributes on the `query executed`
tracing event, so a `tracing-opentelemetry` layer can export them without
renaming anything:

| Field | Value |
|---|---|
| `otel.name` | `execute_sql` |
| `db.system` | engine (postgresql/mysql/sqlite) |
| `db.operation` | `execute` |
| `strut.source` | configured source name |
| `strut.principal` | 12 hex chars of SHA-256 of the principal id |
| `strut.rows` | rows returned |
| `strut.duration_ms` | wall clock |
| `db.statement` | prepared SQL **only** with `--trace-sql` |

Cardinality and secrecy guards: parameter values are never emitted, the
principal is hashed (never an email/token name), and `--trace-sql` is ignored
whenever the caller's grant carries table rules or row filters (the scoped
config clears it), because the statement may name a table the caller cannot
access. There is no bundled exporter; point your tracing subscriber at an OTLP
pipeline.

## HTTP export (NDJSON / CSV)

`POST /export` streams rows for bulk export without going through JSON-RPC.
stdio stays paginated JSON; this endpoint exists only when `--http` is used.

```bash
curl -sS -X POST http://127.0.0.1:8080/export \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"sql":"SELECT id, email FROM users ORDER BY id","format":"ndjson"}'
```

| Field | Notes |
|---|---|
| `sql` | one read-only statement (same guard as `execute_sql`) |
| `params` | optional bound values |
| `source` | optional source name |
| `format` | `ndjson` or `csv` |

- Authentication and authorization are the same as `/mcp`: bearer token or
  OAuth, then `Policy::authorize` for `execute_sql` at **read** tier on the
  requested source. A caller without a grant gets `403 authorization_denied`.
- The statement runs with a forced read-only write mode, so DML/DDL is denied
  with `403 guard_denied` even on a DDL-tier token.
- Rows are capped by `--max-rows`; the response is chunked so each chunk stays
  within `--max-bytes` (`content-type: application/x-ndjson` or `text/csv`).
- `--deny-columns` redaction applies to exported cells.
- NDJSON: one JSON object per row. CSV: header row, RFC 4180 quoting, and
  **NULL is written as an empty field**; the response carries
  `x-meta-null-as-empty: true` so clients can tell that apart from a missing
  header. `x-meta-rows` and `x-meta-columns` describe the stream.
- The audit log records one `export` event per request (source, principal,
  rows, columns, format) — never parameter values.

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
