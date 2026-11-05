# Test Report — strut-stack-sql 2.0.0

The release-freeze census below is historical: it predates the SQL Server
driver. The later [SQL Server R3-02 verification](#sql-server-r3-02-verification)
records exercised feature gates separately; do not quote the old census as
the current workspace total.

- **Version:** 2.0.0
- **Counted on:** 2026-11-04 (final release-freeze recount, `cargo test --workspace`)
- **Counting method:** `grep -rnE "^\s*#\[(tokio::)?test\b" --include=*.rs crates src tests`
  for test attributes, then `grep -rnE "^\s*#\[ignore" …` for tests that need a live
  engine, and finally the per-binary `test result:` lines of an actual
  `cargo test --workspace` run. The final count is **398 functions, 381 run**;
  `#[ignore]`d tests are 17: 9 MySQL integration, 5 HTTP integration (live
  PostgreSQL/MySQL), 2 driver-conformance cases and the pgvector case.
  Recount at every release freeze before quoting externally.

## Totals

| | Count |
|---|---:|
| Test functions in crate `src/` and `crates/*/tests/` | 264 |
| Test functions in top-level `tests/` | 134 |
| **Total** | **398** |
| Runnable by default (`cargo test --workspace`) | 381 |
| `#[ignore]`d — need a live engine (incl. pgvector) | 17 |

## Coverage matrix (per area)

| Area | Files | Tests | Runs where |
|---|---|---:|---|
| Guard & classification | `guard/classify.rs` (12), `guard/lib.rs` (38), `guard/params.rs` (15), `guard/patterns.rs` (4), `guard/capabilities.rs` (2) | 71 | Default |
| Core config, policy, approvals, plans, limits | `core/approvals.rs` (4), `auth.rs` (13), `config.rs` (25), `limits.rs` (2), `migrations.rs` (5), `profiles.rs` (3), `redact.rs` (6), `registry.rs` (3) | 61 | Default |
| Tool handlers | `tools/bind.rs` (8), `cursor`/`keyset`/`summary` (13), `ddl.rs` (9), `exec.rs` (10), `explain.rs` (6), `graph.rs` (4), `history.rs` (2), `introspect.rs` (11) | 63 | Default |
| Driver API | `driver-api/src/lib.rs` | 3 | Default |
| Driver crates | PostgreSQL (12 + conformance 1 ignored + pgvector 1 ignored), MySQL (12 + conformance 1 ignored), SQLite (7 + conformance 1) | 35 | Default; ignored cases need DSNs |
| Server, auth, OAuth, export, resources | `server/src/lib.rs` (1), `export.rs` (3), `oauth.rs` (4), `server/tests/oauth_server.rs` (11) | 19 | Default |
| Result formatting | `format/src/columnar.rs` | 12 | Default |
| F3 schema intelligence | `tests/f3_intelligence.rs` | 10 | Default |
| SQLite integration | `tests/sqlite_integration.rs` | 9 | Default |
| HTTP / handler integration | `tests/http_integration.rs` | 50 (5 ignored) | Default + Docker CI |
| MySQL integration | `tests/mysql_integration.rs` | 9 (all ignored) | Only with `MYSQL_DATABASE_URL` |
| Cursor pagination | `tests/cursor_pagination.rs` | 6 | Default |
| Summary mode | `tests/summary_mode.rs` | 7 | Default |
| Table rules / masking | `tests/table_policy.rs` | 6 | Default |
| Approvals | `tests/approvals.rs` | 6 | Default |
| Migrations / dry run | `tests/migrations.rs` | 4 | Default |
| Hardening | `tests/hardening.rs` | 4 | Default |
| CLI tools (doctor/migrate/profiles) | `tests/cli_tools.rs` | 9 | Default |
| Standalone unit tests | `tests/unit.rs` | 14 | Default |

## What runs in CI

| Job | What it runs |
|---|---|
| `test` | `cargo test --all-targets`, then `cargo build --release` |
| `conformance` | `scripts/conformance/run.sh` + `scripts/conformance/check_clients.py` against the release binary |
| `integration-http-docker` | Seeds PostgreSQL 16 + MySQL 8.4 service containers and runs the ignored HTTP integration tests against both |
| `integration-mysql` (main pushes only) | The ignored MySQL integration tests when `MYSQL_DATABASE_URL` is set; skips loudly otherwise |
| `lint` | `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo audit` |

## Regression families covered

- **Guard bypasses**: CTE writes, `SELECT … INTO`, `INTO OUTFILE`/`DUMPFILE`,
  dangerous functions per engine, row-locking clauses, parameterized LIMIT and
  `FETCH FIRST`, GRANT/REVOKE as Admin.
- **Authorization**: principal × source × tier matrix, direct invocation of a
  tool hidden from `list_tools`, Admin gate, non-loopback unauthenticated bind
  refusal, token hashing and constant-time comparison.
- **OAuth**: JWKS validation (`iss`/`aud`/`exp`), claim mapping, RFC 9728
  metadata and challenges, fail-closed discovery.
- **Type fidelity**: UUID, timestamps, high-precision decimals, JSON, bytea /
  BLOB/VARBINARY, PostgreSQL arrays, MySQL BOOL.
- **Robustness**: pagination offsets at and past the end, one oversized cell,
  wrong pool variants, FK/index grouping across schemas, schema-cache
  invalidation after DDL.
- **Conformance**: stdio + HTTP lifecycle, 3-tool default and 20-tool
  full+DDL surfaces, error shapes, batching, sessions, Origin checks, and the
  six-client config matrix.

## Known gaps (stated, not hidden)

- There is no coverage/line-percentage gate in CI.
- Placeholder-scanner property-based tests do not exist; example-based tests
  cover escape/comment/dollar-quote cases.
- MySQL and PostgreSQL integration tests require a live server; the Docker CI
  job supplies both, but fork PRs without secrets skip the secret-gated job.
- Performance is not asserted by `cargo test`; see
  [BENCHMARKS.md](BENCHMARKS.md) for measured numbers and their limits.

## SQL Server R3-02 verification

Local feature verification on 2026-11-04, `feat/sql-r3-sqlserver`, using
`cargo +1.99` (1.99.1), `CARGO_BUILD_JOBS=1`, and a debug workspace binary.
The live database was a disposable SQL Server 2022 Developer CU27-GDR
16.0.4305.5 container bound only to loopback; no production server was used.

| Gate | Observed result | Exit |
|---|---|---:|
| `cargo +1.99 fmt --all -- --check` | Passed | 0 |
| `cargo +1.99 check --workspace --all-targets` | Passed | 0 |
| `cargo +1.99 clippy --workspace --all-targets -- -D warnings` | Passed | 0 |
| `cargo +1.99 test --workspace --all-targets` | 436 passed, 28 ignored, 31 suites | 0 |
| `cargo +1.99 test --workspace --doc` | Passed, 12 suites | 0 |
| SQL Server driver live conformance/boundaries | 6 passed | 0 |
| Root `sqlserver_integration` live cases | 5 passed | 0 |
| Real SQL Server MCP stdio smoke | 37 scenarios, all 20 existing tool names exercised; harness completed without exceptions | — |
| Repo MCP conformance, SQLite fixtures | 51 passed, 0 failed, 3 optional | 0 |
| Client configuration matrix | 16 marked snippets checked; smoke passed | 0 |
| `cargo +1.99 audit` | 396 dependencies scanned; 4 inherited warnings | 0 |

Live cases prove command affected counts and catalog composite keys, result
codecs/empty metadata/caps, cancellation-safe connection discard, bound
estimated/actual XML plans and mode cleanup, policy/masking/pagination,
quoted dotted/bracket identifiers and cursor lookup, XML set-query caps,
T-SQL money literals, and MERGE terminators after line comments.
The real-process smoke exercises DDL, queries,
summaries, all catalog tools, plans, and filesystem-DMF denial.
A killed idle connection is replaced before the next user statement. A
separate disposable-server check enables the NOCOUNT login default and
proves the first bound INSERT still reports one affected row; the server
default is restored afterward.

Machine evidence is generated under `target/conformance/`:
`sqlserver-live-mcp.json`, `sqlserver-final-report.json`, and
`sqlserver-final-clients.json`. The MCP conformance runner's SQLite fixtures
are not evidence of SQL Server integration; the explicit live cases and
SQL Server MCP smoke provide that evidence.

Audit warnings are inherited from the base lockfile: `event-listener 5.4.1`
(`RUSTSEC-2026-0221`), `tokio 1.52.3` (`RUSTSEC-2026-0386`),
and yanked `chacha20 0.10.1` / `spin 0.9.8`. No advisory ignore was added.
Release optimization, a public server, and production credentials were not
part of this local verification.
