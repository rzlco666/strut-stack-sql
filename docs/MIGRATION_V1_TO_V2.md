# Migrating a V1 deployment to 2.0.0

V2 is a breaking release. There is no compatibility shim: the binary name, the
configuration surface, the tool gating and the result/error contracts are the
V2 ones. This guide maps a working V1 deployment onto V2 and states what was
verified on a reconstructed V1 configuration.

> **Alias status.** V1.0/V1.1 also installed an `mcp-sql-rust` binary and
> packaging entries under that name. **2.0.0 removes them**: no alias binary,
> no install symlink, no duplicate Homebrew/Scoop/winget manifest. The earlier
> PRD supplement proposed deprecating the alias in 2.0 and removing it in 3.0;
> the shipped tree removes it in 2.0. This document states what the release
> does, not what the supplement proposed.

## 1. Binary and install

| V1 | V2 |
|---|---|
| `mcp-sql-rust` | `strut-stack-sql` (the only binary) |
| `install.sh` installed a `mcp-sql-rust` symlink | no symlink |
| `packaging/homebrew/mcp-sql-rust.rb` | removed; use `strut-stack-sql.rb` |
| `packaging/scoop/mcp-sql-rust.json` | removed |
| `packaging/winget/rzlco666.mcp-sql-rust.yaml` | removed |

Every client configuration that names `mcp-sql-rust` must be renamed. Cursor,
VS Code, Claude Desktop and generic MCP client blocks live in
[CLIENTS.md](CLIENTS.md); the `command` value is the only change.

## 2. Configuration

Sources keep their shape; process-wide options move to the CLI. V2 rejects
unknown TOML keys instead of ignoring them, so a V1 file with process options
fails loudly at startup:

```text
$ strut-stack-sql --config v1.toml
Error: parsing v1.toml
Caused by:
    TOML parse error at line 4, column 1
      |
    4 | max_rows = 50
      | ^^^^^^^^
    unknown field `max_rows`, expected one of `default_source`, `sources`,
    `deny_columns`, `principals`, `grants`
```

Allowed top-level keys in V2: `default_source`, `sources`, `deny_columns`,
`principals`, `grants`. Allowed source keys: `name`, `url`, `url_env`,
`engine`.

| V1 location | V2 location |
|---|---|
| `max_rows` in TOML | `--max-rows` (or a `[[grants]]` ceiling) |
| `max_bytes`, timeouts, batch limits | CLI flags (`--max-bytes`, `--query-timeout`, `--batch-*`) |
| single `DATABASE_URL` | still works (single source named `default`) |
| `[[sources]]` with `url`/`url_env` | unchanged |
| HTTP tokens | `STRUT_STACK_SQL_HTTP_TOKENS` or `--http-token`; tokens may be `sha256:<hex>` |
| per-token source allowlist | `name:token:mode:source1|source2` |
| new in V2 | `[[principals]]` and `[[grants]]` (principal × source × tier) |

A sources-only V1 file was tested against the V2 binary and keeps working:
both sources are listed by `list_sources`, the default source is honoured, and
`--full-tools` still exposes the introspection tools.

## 3. Tool surface and gating

V2 exposes **exactly 20 tools**:

- 3 core tools always: `search_objects`, `execute_sql`,
  `analyze_query_performance`;
- 10 introspection tools with `--full-tools`;
- 7 DDL tools with `--full-tools --allow-ddl`;
- `GRANT`/`REVOKE` additionally require `--allow-grants` and an Admin grant.

All 17 V1.1.0 tool names survive unchanged — `search_objects`,
`execute_sql`, `analyze_query_performance`, `list_sources`, `list_schemas`,
`list_tables`, `describe_table`, `list_indexes`, `list_foreign_keys`,
`schema_overview`, `create_table`, `drop_table`, `truncate_table`,
`add_column`, `alter_column`, `drop_column`, `schema_mutate`. V2 adds
`get_relationship_graph`, `suggest_joins` and `get_schema_context`. What
changed is authorization: every call is now checked against the caller's
grant in addition to `--full-tools`/`--allow-ddl`, and `tools/list` agrees
with invocation — a tool hidden by policy is also denied when called
directly.

## 4. Results and errors

| V1 | V2 |
|---|---|
| ad-hoc error strings | structured `{code, message, retryable}` data |
| `{ok:false}` possible in success payloads | never; failures are `isError` tool results or JSON-RPC errors |
| pagination fields | `page_offset`, `page_size`, `has_more`, `total_fetched` |
| truncation | columnar payload truncated at `--max-bytes`, `meta.truncated` set |

Error categories: `invalid_params`, `authorization_denied`, `guard_denied`,
retryable `execution_error`.

## 5. HTTP authentication

- Non-loopback binds without authentication are **refused at startup** unless
  `--allow-unauthenticated-http` is explicit.
- API keys are accepted raw or as `sha256:<hex>` digests and compared in
  constant time.
- OAuth 2.1 resource-server mode: `STRUT_STACK_SQL_OAUTH_ISSUER`,
  `STRUT_STACK_SQL_OAUTH_AUDIENCE`; discovery failure fails closed for bearer
  tokens, API keys keep working.
- Origin/Host checks defend against DNS rebinding.

## 6. History and audit

- Default audit events are one line per tool call with source, tool, outcome
  and duration.
- Query history (opt-in per grant, `query_history = true`) stores normalized
  SQL **hashes** and durations only — never SQL text or parameter values.
- No credential ever appears in results, errors or logs; URL redaction covers
  passwords containing `@`/`:` and sensitive query parameters.

## 7. Migration checklist

1. Replace `mcp-sql-rust` with `strut-stack-sql` in every client config.
2. Move process options out of TOML into CLI flags or `[[grants]]` ceilings;
   start once and fix any `unknown field` error the parser reports.
3. Add `[[principals]]`/`[[grants]]` if the deployment served more than one
   caller; without them a local operator principal gets the process ceiling.
4. Re-check tool gating: `--full-tools` and `--allow-ddl` are now the only
   switches that widen the surface, and `--allow-grants` gates Admin SQL.
5. Re-run the client against the new server and compare
   `tools/list`/`tools/call` payloads; the error shape changed from strings to
   structured data.
