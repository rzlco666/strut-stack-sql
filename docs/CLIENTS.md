# Client configuration — six-client matrix

Copy-paste configuration for the six coding-agent clients named in PRD §6.2
(Cursor, Claude Code, VS Code, Codex, Windsurf, OpenCode). Every JSON/TOML
block in this file is machine-checked by
[`scripts/conformance/check_clients.py`](../scripts/conformance/check_clients.py),
and the minimal stdio block is spawned by that checker to prove the documented
command line completes `initialize` + `tools/list`.

Supported MCP specification version: **2025-06-18** (pinned; see
[CONFORMANCE.md](CONFORMANCE.md)). Clients that negotiate an older version are
answered with 2025-06-18; JSON-RPC batches are rejected.

## The binary

The executable is **`strut-stack-sql`**. It speaks MCP over stdio by default and
over Streamable HTTP with `--http`. Relevant flags (verified with `--help`):

| Flag | Meaning |
|---|---|
| `--url-env DATABASE_URL` | Read the connection URL from an environment variable |
| `--url URL` | Connection URL literal (avoid passwords in files) |
| `--config FILE` | Multi-source TOML configuration |
| `--workspace DIR` | Workspace root for `.env` discovery and relative SQLite paths |
| `--full-tools` | Expose the full 20-tool surface (requires `--allow-ddl` for the 7 DDL tools) |
| `--deny-columns a,b` | Redact matching result columns |
| `--allow-writes` | Allow INSERT/UPDATE/DELETE — **not used in the safe examples below** |
| `--allow-ddl` | Allow DDL (implies writes) — **not used in the safe examples below** |
| `--http ADDR` | Streamable HTTP bind address, e.g. `127.0.0.1:8080` |
| `--http-token TOKEN` | Bearer token for HTTP mode (or `STRUT_STACK_SQL_HTTP_TOKENS`) |
| `--allow-unauthenticated-http` | Dangerous escape hatch; never use on a non-loopback bind |

The examples are **stdio read-only** by default. `--full-tools` is read-only
introspection; it does not enable writes. Write/DDL tiers are discussed at the
end and are deliberately not shown as copy-paste configs.

## Minimal stdio config (smoke-checked)

This is the smallest invocation the CI checker runs verbatim. It uses an
empty in-memory SQLite database so it needs no local files; replace the `--url`
value with your real database (or use `--url-env DATABASE_URL` as in the
per-client examples).

<!-- conformance: client=generic transport=stdio format=json smoke=true -->
```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "type": "stdio",
      "command": "strut-stack-sql",
      "args": ["--url", "sqlite::memory:", "--full-tools"]
    }
  }
}
```

## Cursor

Project config: `.cursor/mcp.json`; global config: `~/.cursor/mcp.json`.
Cursor interpolates `${env:NAME}`, `${workspaceFolder}`, `${userHome}`, and
related variables in `command`, `args`, `env`, `url`, and `headers`. For stdio
servers it also supports `envFile`.

<!-- conformance: client=cursor transport=stdio format=json -->
```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "type": "stdio",
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools", "--deny-columns", "password_hash,totp_secret"],
      "env": {
        "DATABASE_URL": "sqlite:///absolute/path/to/app.db"
      }
    }
  }
}
```

Remote Streamable HTTP (Cursor reads RFC 9728 metadata and can run the OAuth
flow; a static bearer token is shown here):

<!-- conformance: client=cursor transport=http format=json -->
```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${env:STRUT_STACK_SQL_TOKEN}"
      }
    }
  }
}
```

Caveats:

- `env` values are resolved by Cursor, not by your shell profile. To reuse an
  existing `.env`, prefer `"envFile": "${workspaceFolder}/.env"` on the stdio
  entry instead of duplicating the URL.
- For nested `.env` discovery (`apps/api/.env`, `backend/.env`, …, then parent
  directories), the optional Node launcher at
  [`packaging/cursor-mcp-launcher.mjs`](../packaging/cursor-mcp-launcher.mjs)
  still works with the v2 binary: set `STRUT_STACK_SQL_BIN` to the binary and
  `MCP_SQL_ARGS` to the extra flags. It requires Node.js; the direct `command`
  config above does not.
- `--deny-columns` is an opt-in redaction list; keep it in `args` when the
  database contains secrets the agent should never read.
- Remote entries use `url`, not `command`; do not add `"type": "stdio"`.

## Claude Code

Project file: `.mcp.json` at the repository root (or user scope through the
CLI). Claude Code expands `${VAR}` in `.mcp.json`; a `url` entry **must** carry
`"type": "http"` or Claude Code reads it as a stdio entry and fails.

### stdio

<!-- conformance: client=claude-code transport=stdio format=json -->
```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "type": "stdio",
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": {
        "DATABASE_URL": "sqlite:///absolute/path/to/app.db"
      }
    }
  }
}
```

Verified CLI equivalent (the `--` separator is required so Claude Code does not
parse the server's flags):

<!-- conformance: client=claude-code transport=stdio format=shell -->
```bash
claude mcp add --scope user --transport stdio \
  --env DATABASE_URL=sqlite:///absolute/path/to/app.db \
  strut-stack-sql -- strut-stack-sql --url-env DATABASE_URL --full-tools
```

### HTTP with a bearer token

<!-- conformance: client=claude-code transport=http format=json -->
```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${STRUT_STACK_SQL_TOKEN}"
      }
    }
  }
}
```

<!-- conformance: client=claude-code transport=http format=shell -->
```bash
claude mcp add --transport http strut-stack-sql https://mcp.example.com/mcp \
  --header "Authorization: Bearer $STRUT_STACK_SQL_TOKEN"
```

Caveats:

- Use `"type": "http"` (or its alias `"streamable-http"`) whenever `url` is
  present. `"sse"` is the deprecated transport.
- `--env KEY=value` must appear before the server name/`--`; `--` separates
  Claude Code options from the server command.
- `claude mcp get strut-stack-sql` (or `/mcp`) shows the connection status.

## VS Code

Workspace config: `.vscode/mcp.json`. **VS Code uses a top-level `servers`
object, not `mcpServers`.** This is an intentional format difference from
Cursor; do not paste a Cursor block into VS Code unchanged. VS Code supports
`${env:NAME}` and interactive `${input:name}` variables.

<!-- conformance: client=vscode transport=stdio format=json -->
```json
{
  "servers": {
    "strut-stack-sql": {
      "type": "stdio",
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": {
        "DATABASE_URL": "${env:DATABASE_URL}"
      }
    }
  }
}
```

<!-- conformance: client=vscode transport=http format=json -->
```json
{
  "servers": {
    "strut-stack-sql": {
      "type": "http",
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${env:STRUT_STACK_SQL_TOKEN}"
      }
    }
  }
}
```

Caveats:

- If the token or database URL is not exported in the environment that launches
  VS Code, define an `inputs` entry and reference `${input:...}`. `${env:NAME}`
  reads the environment of the host running the extension, not a terminal.
- VS Code tries Streamable HTTP first and falls back to SSE. The server only
  implements Streamable HTTP; the fallback is irrelevant here.
- The portable Copilot format (`.mcp.json` with `mcpServers`) is different from
  the VS Code format above; only the `servers` block is checked here.

## Codex

Config file: `~/.codex/config.toml` (or project-scoped `.codex/config.toml` in a
trusted project). Codex uses TOML tables, not JSON.

<!-- conformance: client=codex transport=stdio format=toml -->
```toml
[mcp_servers.strut-stack-sql]
command = "strut-stack-sql"
args = ["--url-env", "DATABASE_URL", "--full-tools"]
env = { DATABASE_URL = "sqlite:///absolute/path/to/app.db" }
startup_timeout_sec = 20
```

Streamable HTTP with a bearer token from the environment:

<!-- conformance: client=codex transport=http format=toml -->
```toml
[mcp_servers.strut-stack-sql]
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "STRUT_STACK_SQL_TOKEN"
tool_timeout_sec = 120
```

Caveats:

- `bearer_token_env_var` names an environment variable; it does not store the
  token. Use `http_headers` or `env_http_headers` for other header schemes.
- `env_vars = ["DATABASE_URL"]` forwards selected variables from Codex's local
  environment instead of setting them in the file.
- Codex reads the MCP `instructions` field from `initialize`; this server emits
  a compact usage hint there.

## Windsurf

Config file (current builds): `~/.config/devin/mcp_config.json`. The editor's
MCP discovery still reads the legacy build-specific path
`~/.codeium/windsurf/mcp_config.json`; use the file opened by **Open MCP config
file** in Cascade if you are unsure which path your build uses.

<!-- conformance: client=windsurf transport=stdio format=json -->
```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "command": "strut-stack-sql",
      "args": ["--url-env", "DATABASE_URL", "--full-tools"],
      "env": {
        "DATABASE_URL": "sqlite:///absolute/path/to/app.db"
      }
    }
  }
}
```

<!-- conformance: client=windsurf transport=http format=json -->
```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "serverUrl": "https://mcp.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${env:STRUT_STACK_SQL_TOKEN}"
      }
    }
  }
}
```

Caveats:

- Remote entries use `serverUrl` (or `url`); stdio entries use `command`.
- Windsurf supports OAuth for remote HTTP/SSE servers but **not** for stdio;
  pass stdio credentials through `env` or `args`.
- Cascade interpolates `${env:NAME}` in `command`, `args`, `env`, `serverUrl`,
  `url`, and `headers`.
- Press the refresh button in Cascade after editing the file.

## OpenCode

Config file: `opencode.json` at the workspace root. OpenCode uses an `mcp`
object with `type: "local"` or `type: "remote"`. The repository's existing agent
template ([`docs/templates/agent/opencode.json.template`](templates/agent/opencode.json.template))
still supplies the `instructions` and `command` blocks; add the `mcp` entry
alongside them.

<!-- conformance: client=opencode transport=stdio format=json -->
```json
{
  "$schema": "https://opencode.ai/config.json",
  "instructions": ["AGENTS.md", ".cursor/rules/*.mdc", "docs/DEV_WORKFLOW.md"],
  "mcp": {
    "strut-stack-sql": {
      "type": "local",
      "command": ["strut-stack-sql", "--url-env", "DATABASE_URL", "--full-tools"],
      "enabled": true,
      "environment": {
        "DATABASE_URL": "sqlite:///absolute/path/to/app.db"
      },
      "timeout": 30000
    }
  }
}
```

<!-- conformance: client=opencode transport=http format=json -->
```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "strut-stack-sql": {
      "type": "remote",
      "url": "https://mcp.example.com/mcp",
      "enabled": true,
      "oauth": false,
      "headers": {
        "Authorization": "Bearer {env:STRUT_STACK_SQL_TOKEN}"
      }
    }
  }
}
```

Caveats:

- `command` is an **array**, and environment variables use `environment`, not
  `env`. These names differ from Cursor/Windsurf.
- Set `"oauth": false` when using a static bearer token; otherwise OpenCode may
  start an OAuth flow on the first 401.
- `timeout` is in milliseconds. The default is 30 s; raise it for long queries.

## HTTP server side

Every HTTP example above expects the server to be reachable at
`https://mcp.example.com/mcp`. Run it with authentication — a non-loopback bind
without authentication is refused at startup:

<!-- conformance: client=server transport=http format=shell -->
```bash
export STRUT_STACK_SQL_TOKEN="$(openssl rand -hex 32)"
strut-stack-sql --config ./strut-stack-sql.toml \
  --http 0.0.0.0:8080 --http-token "$STRUT_STACK_SQL_TOKEN"
```

- The MCP endpoint is `/mcp`; `/livez` and `/readyz` are for probes.
- Terminate TLS at a reverse proxy; the binary serves plain HTTP.
- Named tokens with per-token modes/sources:
  `STRUT_STACK_SQL_HTTP_TOKENS=name:token:mode[:src1|src2],...`.
- Browser-origin requests are checked for DNS rebinding. Non-loopback origins
  must be listed in `STRUT_STACK_SQL_ALLOWED_ORIGINS` (comma-separated exact
  origins) or they are rejected with 403. Non-browser clients (no `Origin`
  header) are unaffected.

## Full 20-tool surface and risk

Without `--full-tools` the server exposes three token-efficient tools:
`search_objects`, `execute_sql`, `analyze_query_performance`. `--full-tools`
adds ten read-only introspection tools (13 total). The remaining seven DDL
tools appear only with **both** `--full-tools` **and** `--allow-ddl`.
`--allow-ddl` implies `--allow-writes`.

Do not expose a write or DDL tier to an untrusted agent. If you must, combine it
with HTTP authentication, a least-privilege database account, and
`--deny-columns`; the server's guard is a defense-in-depth layer, not a
replacement for database grants. No copy-paste example in this document enables
writes.
