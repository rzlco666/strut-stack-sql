# Contributing — strut-stack-sql

## Development

1. Fork / clone https://github.com/rzlco666/strut-stack-sql
2. Rust 1.88+
3. **Recommended:** Dev Container or `docker compose up -d` (see [QUICKSTART.md](QUICKSTART.md))
4. `cp .env.example .env`
5. `cargo test` and `cargo build --release`

### Databases for integration tests

```bash
# Compose (default ports 5433 / 3307)
docker compose up -d

# PostgreSQL — use your own URL or compose
DATABASE_URL=postgresql://demo:demo@localhost:5433/demo cargo test

# MySQL integration (ignored by default)
MYSQL_DATABASE_URL=mysql://demo:demo@localhost:3307/demo \
  cargo test --test mysql_integration -- --ignored

# SQLite (in-process, no Docker)
cargo test --test sqlite_integration

# SQL Server — disposable database only; these tests create/drop objects.
# Provision SQLSERVER_DATABASE_URL securely (sqlserver:// or mssql://).
# It is explicit test configuration, not automatic server env discovery.
CARGO_BUILD_JOBS=1 cargo +1.99 test -p strut-sql-driver-sqlserver \
  --test conformance -- --ignored --test-threads=1

# Root tool/guard/policy/catalog/plan integration against SQL Server:
CARGO_BUILD_JOBS=1 cargo +1.99 test --test sqlserver_integration \
  -- --ignored --test-threads=1
```

Both SQL Server commands require the exported `SQLSERVER_DATABASE_URL`.
Do not run them against production or a shared database: fixtures create and
drop named tables/routines. The login needs fixture DDL/DML and `SHOWPLAN`
permissions. TLS/certificate validation defaults remain enabled; a disposable
self-signed development server requires explicit
`trust_server_certificate=true` in the URL, not a code default change.
See [SQL Server configuration and limitations](CONFIGURATION.md#sql-server).
These are runnable gates, not a claim that the current worktree passed them.

### Benchmarks

```bash
./scripts/benchmark/run.sh
```

See [BENCHMARKS.md](BENCHMARKS.md).

### Local AI agent setup (optional)

Agent workflow files may be local-only. Templates: [`docs/templates/agent/`](templates/agent/).

```bash
./scripts/setup-agent-dev.sh
```

## Commit style

Conventional Commits preferred:

```
feat(guard): block EXPLAIN ANALYZE without --allow-writes
fix(exec): preserve nulls in columnar decode
docs: add INSTALL.md and compose quickstart
chore: release v2.0.0 branding
```

## Pull requests

- Describe **why**, not only what
- Link issues
- Include test plan checklist
- Do not commit `.env`, credentials, or `.codegraph/`

## Code review focus

1. SQL guard integrity (deny before pool checkout)
2. Token efficiency (tool surface + columnar shape)
3. No secret leakage
4. Six-engine correctness (MySQL + PostgreSQL + SQLite + DuckDB + SQL Server + ClickHouse), including engine-specific guard syntax, bind/codec behavior, catalogs and XML plans

## Packaging maintenance (release manager)

After tagging `vX.Y.Z`:

```bash
./scripts/update-server-json-sha.sh X.Y.Z
./scripts/bump-homebrew-formula.sh X.Y.Z    # copy to homebrew-tap
./scripts/bump-windows-manifests.sh X.Y.Z   # copy to scoop-bucket / winget-pkgs
```

## License

MIT
