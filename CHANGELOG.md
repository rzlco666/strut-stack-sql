# Changelog

## [2.0.0] — 2026-11-03

Major, breaking release. There is no v1 compatibility layer: the tool surface,
configuration schema, and result/error contracts are the v2 ones documented in
`docs/`. A v1 deployment must be reconfigured from the sections below; the
step-by-step mapping is in
[docs/MIGRATION_V1_TO_V2.md](docs/MIGRATION_V1_TO_V2.md).

### Added

- **10-crate workspace plus a thin binary.** `strut-sql-driver-api` defines the
  public `EngineDriver` contract and engine-neutral value types;
  `strut-sql-driver-{postgres,mysql,sqlite}` implement it; `strut-sql-core`,
  `strut-sql-guard`, `strut-sql-format`, `strut-sql-tools`, and
  `strut-sql-server` hold the engine-independent product; and
  `strut-sql-driver-conformance` is the shared Driver Conformance Test Kit.
  Database libraries are confined to driver crates.
- **Per-caller authorization (Principal + Policy).** Grants are evaluated per
  principal × source × tier (`read < writes < ddl < admin`) with optional
  row/byte/timeout ceilings. One `authorize()` function gates every tool
  invocation and drives `list_tools`, so a hidden tool is also denied when
  called directly. `GRANT`/`REVOKE` are a separate `Admin` tier behind
  `--allow-grants`; the process write mode remains a hard ceiling.
- **HTTP authentication.** API keys are accepted raw or as `sha256:<hex>`
  digests and compared in constant time; each key carries a maximum tier and
  optional source allowlist. Non-loopback binds without auth are refused at
  startup unless `--allow-unauthenticated-http` is explicit. Origin/Host
  checks defend against DNS rebinding.
- **OAuth 2.1 resource server.** With `STRUT_STACK_SQL_OAUTH_ISSUER` and
  `STRUT_STACK_SQL_OAUTH_AUDIENCE`, access tokens are validated as RS256 JWTs
  against the issuer's discovered JWKS (`iss`/`aud`/`exp`), mapped to a
  principal via a configurable claim, and advertised through RFC 9728
  protected-resource metadata. Discovery failure fails closed; token material
  is never logged.
- **Schema intelligence (PRD-F3).** Ranked `search_objects`, relation-aware
  `schema_overview`, and three new tools — `get_relationship_graph`,
  `suggest_joins`, `get_schema_context` — bring the surface to exactly 20
  tools. All are read-tier, guarded, audited, and honor source/deny-column
  policy.
- **Performance advisor v2.** A uniform plan tree across the three engines,
  tiered `issues` (`info`/`warning`/`critical`), dominant-cost nodes, row
  estimate misses, leading-wildcard LIKE, AST anti-patterns, and index advice
  as `CREATE INDEX …` text with triggering node, estimate, and confidence.
  Advice is never executed. `EXPLAIN ANALYZE` is opt-in via `analyze: true`
  plus `--allow-explain-analyze`.
- **Schema statistics and history.** Driver `table_stats` with honest
  provenance (planner/catalog/unavailable), table/index sizes where reported,
  index coverage for advisor rules; opt-in per-principal in-memory query
  history (normalized hashes, no parameter values) for N+1 and repeated-slow
  suggestions.
- **Schema cache** keyed by source/engine/schema with TTL and invalidation
  after DDL issued through the server; filter and pagination pushed into the
  driver's catalog SQL; set-based SQLite introspection removes the N+1
  `PRAGMA` chains.
- **Governed batch execution**: statement count, total wall-clock deadline,
  and cross-item byte ceiling, with per-item fail-soft unless `--fail-fast`.
- **Rich type codecs**: UUID, timestamps, high-precision decimals, JSON,
  binary/bytea, PostgreSQL arrays; SQLite BLOB as hex; MySQL BOOL as JSON
  bool; VARBINARY/BLOB remain hex.
- **MCP conformance gate and six-client matrix.** `scripts/conformance/run.sh`
  covers stdio and Streamable HTTP (lifecycle, 20-tool listing, calls, error
  shapes, batching, auth/OAuth metadata, sessions, Origin checks); every
  config block in `docs/CLIENTS.md` is parsed and the minimal stdio command is
  smoke-run. The pinned spec is **2025-06-18**.
- **Reproducible benchmark harness** (`scripts/benchmark/bench.py`) with raw
  results published at `scripts/benchmark/results/2.0.0.json`.

- **Cursor (keyset) pagination for `execute_sql`.** The first page with
  `page_size` returns an opaque HMAC-signed `meta.next_cursor`; sending it with
  the same SQL and parameters continues the walk without OFFSET, so an insert
  or delete between pages cannot duplicate or skip a row. Cursors are bound to
  the source, SQL and parameter values, expire after 10 minutes, and fail with
  `cursor_expired` / `cursor_invalid` / `cursor_*` codes. Cursor mode requires
  a single-table SELECT ordered by one unique column that is also selected;
  other first-page requests keep the legacy offset behaviour.
- **Destructive-DDL approvals.** With `--require-approval`, drop/truncate/
  drop-column actions (tool wrappers and raw `execute_sql`) return
  `approval_required` with a single-use id that expires after 15 minutes.
  Approvals are accepted through the MCP elicitation flow when the client
  supports it, or via the HTTP queue (`GET /approvals`,
  `POST /approvals/{id}/accept|reject`), bound to the requesting principal.
  `confirm: true` stays a legacy confirmation flag and is never treated as an
  approval.
- **Table allow/deny, row filters and column masking.** `[[grants]]` gained
  `allow_tables`, `deny_tables`, `row_filters` and `mask` (partial | hash |
  null). Deny wins over allow; row filters are injected into the AST as
  `AND (predicate)` and a statement where a filter cannot be applied is
  rejected instead of running unfiltered. Enforcement covers `execute_sql`
  (single, batch, cursor, summary), `/export`, the advisor and every schema
  discovery tool.
- **HTTP export (NDJSON / CSV).** `POST /export` streams rows with the same
  authentication and Policy authorization as `/mcp`, a forced read-only write
  mode, `--max-rows`/`--max-bytes` caps, response chunks bounded by
  `--max-bytes`, and deny-column redaction. CSV writes NULL as an empty field
  and advertises `x-meta-null-as-empty: true`. stdio stays paginated JSON.
- **Server-side summary mode.** `execute_sql({summary: …})` builds `count`,
  `sample` or `aggregate` (count/avg/sum/min/max, optional `group_by`) without
  accepting SQL from the caller. Identifiers are validated against the catalog
  and quoted; deny-listed columns are rejected in group keys, metrics and
  sample projections. Counts and aggregates are marked `exact: true`; a sample
  is marked `exact: false` and never presented as a total.

### Changed (breaking)

- **Tool surface is exactly 20**: 3 core tools always; 10 introspection tools
  with `--full-tools`; 7 DDL tools only with `--full-tools --allow-ddl`.
  v1 tool names and gating no longer apply.
- **Configuration v2**: multi-source TOML with `[[principals]]` and
  `[[grants]]`; unknown keys, duplicate source names, and a dangling
  `default_source` are startup errors; an omitted default is the
  lexicographically first source; a TOML source without `url`/`url_env` is an
  error and never inherits another source's credentials; numeric options are
  range-validated.
- **Result and error contracts**: every failure is categorized as
  `invalid_params`, `authorization_denied`, `guard_denied`, or retryable
  `execution_error` with structured `{code,message,retryable}` data; success
  payloads never carry `{ok:false}`. Pagination reports
  `page_offset`/`page_size`/`has_more`/`total_fetched`.
- **One binary only.** The legacy `mcp-sql-rust` binary target, its install
  symlink, and the duplicate Homebrew/Scoop/winget manifests were removed;
  every script, launcher, container, and document now targets
  `strut-stack-sql`.

### Fixed

- **Guard bypass family (Tahap A)**: CTE bodies containing
  INSERT/UPDATE/DELETE are classified recursively as DML;
  `SELECT … INTO`, `INTO OUTFILE`, and `INTO DUMPFILE` are never reads;
  dangerous functions are denied or tier-gated per engine; parameterized
  `LIMIT ?` and `FETCH FIRST` are clamped; row-locking clauses and unknown
  statement classes are denied; GRANT/REVOKE moved to `Admin`.
- **Credential and identifier safety**: URL redaction handles passwords with
  `@`/`:` and sensitive query parameters; URL parts are percent-encoded;
  SQLite attached-schema names are quoted correctly; cross-source credential
  inheritance was removed.
- **Data fidelity**: PostgreSQL `uuid`/`timestamptz`/`inet`/`citext` decode as
  JSON strings; MySQL `DECIMAL`/`BIGINT UNSIGNED`/aggregates stay numeric
  without float round-trip; MySQL text vs BINARY/VARBINARY decode is correct;
  SQLite BLOB is hex instead of silent `Null`; `approx_rows` no longer reports
  fake zeroes for never-analyzed PostgreSQL.
- **Robustness**: `search_objects` pagination past the end returns an empty
  page instead of panicking; FK/index grouping is keyed by
  (schema, table, name); pool-access `.expect()` panics were replaced with
  structured errors; a single 10 MB cell cannot exceed the result byte cap.
- **Advisor defects**: the dead `_warnings` parameter, MySQL double-counting
  walker, and hardcoded SQLite index column were removed.

### Performance

Measured at the 2.0.0 freeze on the documented WSL2 reference machine
(n=120 after 20 warmups, 20 cold starts; raw JSON in
`scripts/benchmark/results/2.0.0.json`, including the binary sha256):

- **stdio meets every PRD-F2-12 target**: idle RSS 8.05 MB (≤ 15), cold-start
  p95 7.43 ms (≤ 25), core `tools/list` 3,039 B (≤ 3,072 B), 100×8 result
  7,526 B (≤ 70 KB), SELECT-by-PK p50 0.99 ms (≤ 1).
- **HTTP now meets its own declared targets**: cold-start p95 46.98 ms
  (≤ 60 ms) and SELECT-by-PK p50 2.28 ms (≤ 5 ms). Setting `TCP_NODELAY` on
  accepted connections removed a ~40 ms Nagle/delayed-ACK penalty per
  `tools/call` on reused connections. The HTTP cold start cannot reuse the
  stdio budget: on this host the kernel `bind()` of a listening socket alone
  measures ~23 ms p50 (reproducible in plain Python), so the per-transport
  targets are declared in the artifact instead of rounding the difference
  away.
- **Full 20-tool surface**: `tools/list` with `--full-tools --allow-ddl` is
  11,003 B against a 12,000 B CI cap; the core surface is 3,039 B against
  3,072 B.
- **PostgreSQL 16 and MySQL 8.4 are measured**, not `not_run`: SELECT-by-PK
  p50 2.44 ms / 2.99 ms (p95 3.78 / 4.27 ms), 0 errors, same 7,526 B payload,
  using fixtures seeded through the compose stack.
- **Size gate**: the stripped Linux x64 release binary is 10.6 MB, under the
  16 MB release budget.
- The competitor comparison stays `not_run`: `npx`/Node is not installed on the
  reference machine, and the pinned rivals also need a live PostgreSQL server.
- Run-to-run `execute_sql` p50 varies with host load (0.80–1.47 ms observed for
  the same binary); the range is stated in [BENCHMARKS.md](docs/BENCHMARKS.md)
  rather than quoting only the best run.

## [1.1.0] — 2026-11-02

Hardening release: closes every guard-bypass and correctness finding from the
full-code audit of v1.0.4, gives the HTTP transport real authentication, and
re-syncs all packaging channels and public docs with reality. No tool names or
result shapes changed; configuration resolution is stricter (see Changed).

### Security

- **Guard: writes hidden in CTEs are no longer "reads".** Classification now
  inspects the body of `WITH …` queries recursively — a CTE containing
  `INSERT`/`UPDATE`/`DELETE … RETURNING` is classified as DML and requires
  `--allow-writes`, in every engine dialect.
- **Guard: `SELECT … INTO`, `INTO OUTFILE` / `INTO DUMPFILE` rejected** as
  reads; they create tables/files and are denied unless the corresponding
  write tier is active.
- **Dangerous-function denylist (per engine).** File, sequence, session, and
  DoS functions are blocked in read paths and gated behind write tiers where
  legitimate: PostgreSQL `pg_read_file`, `pg_ls_dir`, `lo_export`, `dblink`,
  `nextval`/`setval`, `pg_sleep`, `pg_terminate_backend`, …; MySQL
  `LOAD_FILE`, `SLEEP`, `BENCHMARK`, …; SQLite `readfile`/`writefile`, …
- **Parameterized `LIMIT ?` and `FETCH FIRST` are clamped** to `--max-rows`
  exactly like literal limits; parameter values can no longer widen a result
  past the cap.
- **HTTP API-token authentication.** `--http` now accepts API tokens via
  `STRUT_STACK_SQL_HTTP_TOKENS` (comma-separated
  `name:token:mode[:source1|source2]` entries; `--http-token` sets a single
  token). Each token carries a **maximum write mode**
  (`readonly` / `writes` / `ddl`) — and optionally a source allowlist — that
  caps what its holder can do, regardless of client requests; the effective
  mode is the lower of the process tier and the token mode. Clients send
  `Authorization: Bearer <token>`. A per-token rate limit
  (`--http-rate-limit-per-minute`, default 600) applies.
- **Unsafe binds refused.** Starting HTTP on a non-loopback address without
  any configured token exits with an error instead of silently serving an
  unauthenticated SQL endpoint; the only escape is the explicit, loudly
  warned `--allow-unauthenticated-http` flag.
- **Safe health endpoints.** New `/livez` (static, never connects) and
  `/readyz` (cached pool status, generic error mapping); `/healthz` remains
  as a compat alias of the safe `/readyz`. Neither returns raw database
  error text to anonymous callers.
- **Strong URL redaction.** URL parsing is rewritten: passwords containing
  `@` or `:` and sensitive query parameters are reliably redacted in errors
  and logs; credential parts assembled from `*_HOST`/`*_USER`/… env vars are
  URL-encoded instead of interpolated raw.
- **SQLite identifier quoting.** The attached-schema introspection query no
  longer interpolates an unquoted schema name into `sqlite_master` SQL.
- **Cross-source credential inheritance removed.** TOML sources without
  `url`/`url_env` no longer silently inherit the global `DATABASE_URL`.

### Added

- **Audit log.** Every executed statement is emitted on the `audit` tracing
  target — enable with `MCP_SQL_LOG=audit=info`. Statement text only;
  parameter values are never logged.
- **Schema cache.** Introspection results are cached per source (with
  invalidation on DDL), removing repeated full-catalog scans.
- **`--ephemeral` acknowledgement** (env `STRUT_STACK_SQL_EPHEMERAL=1`) for
  the no-database fallback: an in-memory SQLite that is announced by a loud
  startup banner and in `ServerInfo`. See Changed.

### Changed

- **Configuration is fail-closed and deterministic.** Invalid configuration
  is an error at startup: unknown TOML keys are rejected
  (`deny_unknown_fields`), duplicate source names are a hard error, a named
  `default_source` must exist, and an unnamed default is chosen
  deterministically (lexicographically first source, with a warning) instead
  of by hash-map coin flip; numeric CLI options are range-validated.
- **No more silent ephemeral fallback.** Previously, a missing `DATABASE_URL`
  quietly started the server on `sqlite::memory:`, masking misconfiguration
  as a healthy server. The fallback still exists for first-run MCP
  handshakes, but it is **never silent** now: a loud multi-line banner on
  stderr, announced to the client in `ServerInfo`, and `--ephemeral`
  (or `STRUT_STACK_SQL_EPHEMERAL=1`) to acknowledge it explicitly.
- **Documentation synced to the code** (`docs/CONFIGURATION.md`,
  `docs/SECURITY.md`, `docs/TOOLS.md`, `.env.example`): config precedence,
  the supported/rejected SQL surface, HTTP token auth, and the audit log are
  documented as implemented. The ghost env var `MCP_SQL_CONFIG` is gone
  (the TOML path is a CLI flag: `--config`).

### Fixed

- **`search_objects` panic** when `offset` exceeded the total result count
  (`Vec::drain` out of range) — now returns an empty page.
- **Foreign-key / index grouping collisions.** Introspection grouping is
  keyed by (schema, table, name) instead of the bare constraint/index name,
  so same-named indexes on different tables no longer merge.
- **Value decoding fidelity.** BLOB/binary cells are encoded as hex instead
  of lossy text, and decimal values keep full precision (no float
  round-trip).
- **SQLite N+1 introspection.** `index_list`/`index_info` and per-table FK
  lookups in schema overview are batched/cached instead of one query per
  table per call.
- **Packaging manifests were stuck at 0.4.0 with a retired artifact naming
  scheme.** Homebrew, Scoop, and winget manifests now target the current
  release asset names (`strut-stack-sql-{linux,macos,windows}-…`), and the
  `scripts/bump-*.sh` helpers were rewritten to match the current repo,
  artifact names, and hash placeholders, so hashes can be filled in
  mechanically after each release.
- **`server.json` gains the Windows package** (the release matrix ships a
  Windows x64 binary) and matches `mcpb/manifest.json` at 1.1.0.

## 1.0.4 — 2026-07-24

P0: make `approx_rows` omit path actually run on live Postgres.

### Fixes

- **`schema_overview.approx_rows`:** decode `pg_class.reltuples` as Postgres `real` (`f32`) / `::float8` — `try_get::<f64>` alone failed silently, so never-analyzed (`-1`) never reached `pg_approx_rows` and stale `n_live_tup=0` still emitted fake `0`.

## 1.0.3 — 2026-07-24

Correctness patch: never emit fake `approx_rows: 0` on small/never-analyzed Postgres.

### Fixes

- **`schema_overview.approx_rows`:** if `reltuples < 0` (never ANALYZE'd), omit even when `n_live_tup == 0` (stale stats collector); only trust positive `n_live_tup` until analyzed. `meta.row_counts = "approx"` unchanged.

### DX / ops

- **`scripts/install-local.sh`:** also sync `$CARGO_HOME/bin` (`~/.cargo/bin`) so manual CLI matches Cursor/`~/.local/bin`

## 1.0.2 — 2026-07-24

Scorecard polish: honest approx rows, quieter indexes, install hygiene.

### Fixes

- **`schema_overview.approx_rows`:** never fake `0` from never-analyzed Postgres (`reltuples < 0` → omit). Prefer `n_live_tup`; `meta.row_counts = "approx"`.
- **`list_indexes`:** flat `{schema,table,name,columns,unique}` — no nested `indexes[]`, no full `CREATE INDEX` in `data_type`.

### DX / ops

- MCP `get_info` instructions include `CARGO_PKG_VERSION`
- **`scripts/install-local.sh`:** release build → `~/.local/bin` (repo `cargo build` alone does not update the Cursor binary)
- Cursor example args include `--deny-columns password_hash,totp_secret_encrypted,code_hash`

## 1.0.1 — 2026-07-24

Correctness + agent DX patch from live MCP scorecard.

### P0

- **Postgres typed cells:** `uuid` / `timestamptz` / `inet` / `citext` serialize as JSON strings (no silent `null`)
- **Index columns:** `describe_table` / `list_indexes` fill `indexes[].columns` on Postgres

### P1

- **Domain types:** `USER-DEFINED` resolved to `udt_name` (`citext`, …)
- **`list_sources`:** returns `engine` / `host` / `port` / `database` (no credentials)
- **Batch order:** `execute_sql` `queries[]` results preserve input order
- **`analyze_query_performance`:** rejects SQL already wrapped in `EXPLAIN`

### P2

- **Opt-in column deny-list:** `--deny-columns` / `MCP_SQL_DENY_COLUMNS` / TOML → `"[redacted]"` + `meta.redacted_columns`
- **`schema_overview`:** one-shot tables + approx row counts + FKs (`--full-tools`)

## 1.0.0 — 2026-07-24

**Strut Stack SQL** — first stable release (rebrand from `mcp-sql-rust`).

### Product

- Binary / crate: `strut-stack-sql` (compat alias `mcp-sql-rust`)
- Repo: https://github.com/rzlco666/strut-stack-sql
- Default result format: **columnar** (token-efficient)
- One-command install + friendly release asset names (`strut-stack-sql-linux-x64.tar.gz`, …)

### Reliability

- Cursor launcher never blocks MCP initialize on missing `.env` / dead DB (warn + lazy connect)
- Server starts without `DATABASE_URL` via ephemeral `sqlite::memory:`
- Env discovery prefers `apps/api/.env` and other nested paths

### Safety

- Explicit deny: `COPY`, `CALL`, `ATTACH DATABASE`, `LOAD DATA`, `PREPARE`, `EXECUTE`
- Query complexity limits (joins / subquery depth)
- PG `statement_timeout` + MySQL `max_execution_time` on pool connect (plus client timeout)

### Footprint

- Release profile: `opt-level=3`, thin LTO, strip, `panic=abort`
- Linux x64 stripped binary ≈ **15 MB** (three engines)

## 0.5.2 — 2026-07-14

### P0 — Metadata

- **`describe_table` index `unique`:** no longer hardcodes `false`; preserves uniqueness from MySQL/`SQLite`/`Postgres` index metadata (Postgres via `CREATE UNIQUE INDEX` in `indexdef`).

### P1 — Schema/DDL UX

- **DDL aliases** (`create_table`, `drop_table`, `add_column`, `alter_column`, `drop_column`, `truncate_table`) thin-wrap `schema_mutate`.
- **Tool list gating:** DDL tools (including `schema_mutate`) listed only with `--full-tools` **and** `--allow-ddl`.
- **`alter_column` dialects:** MySQL `MODIFY COLUMN`, Postgres `ALTER … TYPE`, SQLite clear unsupported error.

### P1 — Connect UX

- **TCP preflight ≤500ms** before sqlx pool connect (MySQL/Postgres); launcher `PREFLIGHT_TIMEOUT_MS` default **500** (`MCP_SQL_PREFLIGHT_MS` override).

## 0.5.1 — 2026-07-14

### P0 — Type fidelity

- **MySQL text decode:** `VARCHAR` / `CHAR` / `TEXT` / `ENUM` / `SET` / `JSON` in `execute_sql` serialize as UTF-8 JSON strings (no longer hex). Binary types (`BLOB` / `BINARY` / `VARBINARY`) remain hex.
- Shared MySQL text helpers used by schema tools and execute path (`mysql_decode_text_by_*`).

### P1 — Connect UX

- **`--connect-timeout SECS`** (default **2s**, was fixed 5s) for pool acquire timeout on first lazy connect.

## 0.5.0 — 2026-07-13

### P0 — Blockers

- **Type fidelity:** MySQL `DECIMAL` / `BIGINT UNSIGNED` / aggregates (`SUM`, `ROUND`, `information_schema`) serialize as JSON numbers via `bigdecimal` decoding.
- **Lazy connect (default):** MCP starts without blocking on DB; 5s connect timeout per tool call. `--eager-connect` restores startup ping.
- **Schema isolation:** `search_objects` / `list_tables` default to current database; use `schema: "*"` to search all schemas on MySQL.
- **Cursor setup:** Official `packaging/cursor-mcp-launcher.mjs` with TCP preflight, `--workspace`, and [docs/CURSOR.md](docs/CURSOR.md).

### P1 — Parity

- **`schema_mutate`** tool (behind `--full-tools` + `--allow-ddl`) with `confirm: true` for destructive ops.
- **`list_foreign_keys`** tool for MySQL, PostgreSQL, SQLite.
- **`describe_table`** enriched: `key`, `extra`, `comment`, `default` (MySQL via `SHOW FULL COLUMNS`).
- **`list_tables`** pagination (`offset`, `limit`, `has_more` meta).
- **`execute_sql` `format`:** `auto` | `columnar` | `rows` (+ `MCP_SQL_FORMAT` env).
- **Security:** URL password redaction in connect errors; `MCP_SQL_LOG=audit` SQL audit trail.

### P2 — Polish

- Benchmark script: `scripts/benchmark/run-mysql-vs-mcp-mysql.sh`
- [docs/POSTGRES.md](docs/POSTGRES.md), updated [docs/MYSQL.md](docs/MYSQL.md), [docs/CURSOR.md](docs/CURSOR.md)

## 0.4.0

- HTTP transport, server-side pagination, MCP Registry packaging.
