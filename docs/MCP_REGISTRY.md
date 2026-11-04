# MCP Registry — strut-stack-sql

Publishes to the [MCP Registry](https://registry.modelcontextprotocol.io) using **MCPB** bundles on [GitHub Releases](https://github.com/rzlco666/strut-stack-sql/releases).

Registry name:

```text
io.github.rzlco666/strut-stack-sql
```

## Release artifacts

| Asset | Purpose |
|-------|---------|
| `strut-stack-sql-<target>.tar.gz` | Linux/macOS binary archive |
| `strut-stack-sql-x86_64-pc-windows-msvc.zip` | Windows binary archive |
| `SHA256SUMS` | Checksums for all release files |
| `strut-stack-sql-<target>.mcpb` | MCP Registry bundle per platform |

Archive naming is consistent across platforms. See [INSTALL.md](INSTALL.md) for non-registry install paths.

## Files in repo

| File | Purpose |
|------|---------|
| [`server.json`](../server.json) | Registry manifest (`registryType: mcpb`) |
| [`mcpb/manifest.json`](../mcpb/manifest.json) | MCPB template |
| [`.github/workflows/release.yml`](../.github/workflows/release.yml) | Build archives + MCPB + SHA256SUMS |
| [`.github/workflows/registry-publish.yml`](../.github/workflows/registry-publish.yml) | Auto-publish on release |

Crates.io is **not** used (`publish = false`). Use GitHub Releases, Homebrew tap, or `cargo binstall`.

## Automated publish

On GitHub Release (`release: published`) or after Release workflow completes:

1. Download `mcp-publisher`
2. Sync `server.json` version from tag
3. `mcp-publisher login github-oidc`
4. `mcp-publisher publish`

Manual fallback: `workflow_dispatch` on `registry-publish.yml`.

## Freeze state

`server.json` ships with `fileSha256: "UPDATE_AFTER_RELEASE"` placeholders,
because the bundles are built per platform by the release workflow and the
hash of a local build is not the hash of the published artifact.
`scripts/update-server-json-sha.sh <version>` replaces them from that
release's `SHA256SUMS`, and `registry-publish.yml` now refuses to publish an
empty or placeholder hash (or duplicate hashes across platforms) instead of
shipping an unverifiable entry.

## Verify

```bash
curl -s "https://registry.modelcontextprotocol.io/v0/servers?search=io.github.rzlco666/strut-stack-sql"
```

## Manual publish

1. Install `mcp-publisher` (see [MCP Registry docs](https://github.com/modelcontextprotocol/registry))
2. After release, patch MCPB hashes:

```bash
./scripts/update-server-json-sha.sh 2.0.0
```

3. Authenticate (interactive): `mcp-publisher login github`
4. `mcp-publisher validate server.json && mcp-publisher publish`
## Cursor install (binary)

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

Set `DATABASE_URL` in environment or project `.env`.

## Integrity

Release `SHA256SUMS` covers archives and MCPB files. Registry manifest stores `fileSha256` for the primary MCPB package. Verify downloads per [INSTALL.md](INSTALL.md).
