# Cursor IDE setup — strut-stack-sql

Zero-config global MCP for MySQL, PostgreSQL, and SQLite using the official launcher in this repo.

## Prerequisites

1. Build or install the binary:

```bash
# Dev loop (updates ~/.local/bin for Cursor):
./scripts/install-local.sh

# Or cargo install (updates ~/.cargo/bin):
cargo install --path /path/to/strut-stack-sql --force
```

`cargo build --release` alone leaves `~/.local/bin` stale — always run `install-local.sh` (or `cargo install --path . --force`) before expecting Cursor to pick up a new binary.
2. Each project needs a `.env` with `DATABASE_URL` (or `MYSQL_*` / `POSTGRES_*` parts). See [CONFIGURATION.md](CONFIGURATION.md).

## Global `~/.cursor/mcp.json`

Copy [packaging/mcp.json.example](../packaging/mcp.json.example) and adjust paths:

```json
{
  "mcpServers": {
    "strut-stack-sql": {
      "type": "stdio",
      "command": "node",
      "args": [
        "/home/you/projects/strut-stack-sql/packaging/cursor-mcp-launcher.mjs",
        "${workspaceFolder}"
      ],
      "env": {
        "MCP_SQL_RUST_BIN": "/home/you/.cargo/bin/strut-stack-sql",
        "MCP_SQL_ARGS": "--full-tools --deny-columns password_hash,totp_secret_encrypted,code_hash"
      }
    }
  }
}
```

## How it works

1. Cursor passes `${workspaceFolder}` (or cwd fallback) to the launcher.
2. Launcher walks up from the workspace to find `.env`.
3. **TCP preflight** (500ms, override `MCP_SQL_PREFLIGHT_MS`): if MySQL/Postgres host is unreachable, prints a clear stderr message and exits before spawning Rust.
4. Spawns `strut-stack-sql --workspace <path> --full-tools --deny-columns …` with env injected.
5. Rust `chdir`s to workspace, loads `.env`, resolves SQLite relative paths (`sqlite://./data.db`).

## Lazy connect (default)

The MCP server **starts even when the database is down**. The first tool call runs a **≤500ms TCP preflight**, then sqlx connect (default **2s** `--connect-timeout`):

```
cannot connect to mysql://user:***@127.0.0.1:3306/db: TCP preflight failed in <500ms (...)
```

Use `--eager-connect` (via `MCP_SQL_ARGS`) to restore startup ping behavior.

## Switching workspaces

Open `edoc-pid`, `sample-next`, or `boilerplate-rust` — each project's `.env` is loaded automatically. No per-project `mcp.json` entry required.

## Troubleshooting

| Symptom | Fix |
|---------|-----|
| `no DATABASE_URL in .env` | Add `.env` in project root; launcher walks up from workspace |
| `cannot reach host:port` | Start MySQL/Postgres; launcher preflight failed |
| Wrong database | Check `DATABASE_URL` in that workspace's `.env` |
| `Connection closed` after 30s | Update to v0.5+ (lazy connect); remove old custom launcher |

## Optional: project-local override

Per-repo `.cursor/mcp.json` can point at the same launcher with a relative path if you prefer not to use global config.
