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
```

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
4. Tri-engine correctness (PG + MySQL + SQLite)

## Packaging maintenance (release manager)

After tagging `vX.Y.Z`:

```bash
./scripts/update-server-json-sha.sh X.Y.Z
./scripts/bump-homebrew-formula.sh X.Y.Z    # copy to homebrew-tap
./scripts/bump-windows-manifests.sh X.Y.Z   # copy to scoop-bucket / winget-pkgs
```

## License

MIT
