# Installation

All install paths for **strut-stack-sql**. Pick one channel; they install the same binary.

## Quick pick

| Channel | Best for |
|---------|----------|
| [curl install script](#curl-install-script) | Linux/macOS, **recommended** |
| [Docker](#docker-ghcr) | Servers, CI |
| [Windows .zip](#windows) | Windows |
| [MCPB](#mcpb) | MCP Registry clients |
| [From source](#from-source) | Contributors and custom builds |

## curl install script

```bash
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash
strut-stack-sql --version
```

Options:

```bash
curl -fsSL .../install.sh | bash -s -- --version 2.0.2 --prefix ~/.local
```

Installs `~/.local/bin/strut-stack-sql`. Add `~/.local/bin` to `PATH` if needed.

### One command: install + AI setup

`--setup` installs the binary and then configures every supported AI tool it
detects (Claude Code, Cursor, VS Code, Claude Desktop, Codex, OpenCode, Windsurf,
Zed). Existing entries for other servers are kept, and every file it touches is
backed up first. It never uploads anything.

```bash
# Install + configure every detected tool
curl -fsSL .../install.sh | bash -s -- --setup

# With your database URL baked into the tool configs
curl -fsSL .../install.sh | bash -s -- --setup --database-url 'postgresql://user:pass@localhost:5432/mydb'

# Only specific tools, project-local configs, or setup-only (binary installed)
curl -fsSL .../install.sh | bash -s -- --setup --tools cursor,vscode,opencode
curl -fsSL .../install.sh | bash -s -- --setup --project
curl -fsSL .../install.sh | bash -s -- --setup --skip-install
```

Without `--database-url` (and without `DATABASE_URL` exported), configs reference
`${env:DATABASE_URL}` where the tool supports it (Codex forwards it instead).
Run `install.sh --help` for every flag.

### Release asset names

| Platform | Asset |
|----------|--------|
| Linux x64 | `strut-stack-sql-linux-x64.tar.gz` |
| Linux arm64 | `strut-stack-sql-linux-arm64.tar.gz` |
| macOS x64 | `strut-stack-sql-macos-x64.tar.gz` |
| macOS arm64 | `strut-stack-sql-macos-arm64.tar.gz` |
| Windows x64 | `strut-stack-sql-windows-x64.zip` |

Each archive contains a single `strut-stack-sql` binary. `SHA256SUMS` is published on every release.

## Docker (GHCR)

```bash
docker run --rm -i -e DATABASE_URL=sqlite:///data/app.db \
  ghcr.io/rzlco666/strut-stack-sql:latest --url-env DATABASE_URL
```

The image defaults to HTTP mode on port 8000;
without `--http` the image runs in stdio mode.

## Windows

Download `strut-stack-sql-windows-x64.zip` from the
[latest release](https://github.com/rzlco666/strut-stack-sql/releases/latest),
unpack it, and add the folder to `PATH`.

## MCPB

Each GitHub Release attaches `strut-stack-sql-<platform>.mcpb` bundles. They
are the packages referenced by [`server.json`](../server.json) for the MCP
Registry; install them through a client that supports MCPB, or unpack the
bundle and run the included `strut-stack-sql` binary.

## From source

```bash
git clone https://github.com/rzlco666/strut-stack-sql.git
cd strut-stack-sql
cargo install --path . --bins
```

### Local Cursor binary (`~/.local/bin`)

`cargo build --release` writes `target/release/` only. To refresh the binary Cursor uses from `~/.local/bin`:

```bash
./scripts/install-local.sh
# PREFIX=~/.local by default; override: PREFIX=/usr/local ./scripts/install-local.sh
strut-stack-sql --version
```

Or: `cargo install --path . --force` (installs to `~/.cargo/bin`).

## After install

```bash
strut-stack-sql --version   # strut-stack-sql 2.0.2
# Cursor: see README quick start
```

Version 2.0.0 ships only the `strut-stack-sql` binary; the v1
`mcp-sql-rust` alias is gone. Replace old commands and configs with
`strut-stack-sql`.
