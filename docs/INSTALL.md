# Installation

All install paths for **strut-stack-sql**. Pick one channel; they install the same binary.

## Quick pick

| Channel | Best for |
|---------|----------|
| [curl install script](#curl-install-script) | Linux/macOS, **recommended** |
| [Homebrew](#homebrew) | macOS |
| [Docker](#docker-ghcr) | Servers, CI |
| [Scoop / winget](#windows) | Windows |
| [MCPB](#mcpb) | MCP Registry clients |
| [From source](#from-source) | Contributors and custom builds |

## curl install script

```bash
curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash
strut-stack-sql --version
```

Options:

```bash
curl -fsSL .../install.sh | bash -s -- --version 2.0.0 --prefix ~/.local
```

Installs `~/.local/bin/strut-stack-sql`. Add `~/.local/bin` to `PATH` if needed.

### Release asset names

| Platform | Asset |
|----------|--------|
| Linux x64 | `strut-stack-sql-linux-x64.tar.gz` |
| Linux arm64 | `strut-stack-sql-linux-arm64.tar.gz` |
| macOS x64 | `strut-stack-sql-macos-x64.tar.gz` |
| macOS arm64 | `strut-stack-sql-macos-arm64.tar.gz` |
| Windows x64 | `strut-stack-sql-windows-x64.zip` |

Each archive contains a single `strut-stack-sql` binary. `SHA256SUMS` is published on every release.

## Homebrew

```bash
brew install rzlco666/tap/strut-stack-sql
```

## Docker (GHCR)

```bash
docker run --rm -i -e DATABASE_URL=... ghcr.io/rzlco666/strut-stack-sql:latest
```

## Windows

```bash
scoop install rzlco666/strut-stack-sql
# or
winget install rzlco666.strut-stack-sql
```

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
strut-stack-sql --version   # strut-stack-sql 2.0.0
# Cursor: see README quick start
```

Version 2.0.0 ships only the `strut-stack-sql` binary; the v1
`mcp-sql-rust` alias is gone. Replace old commands and configs with
`strut-stack-sql`.
