# Test Report — strut-stack-sql 2.0.0

What the automated tests in this repository actually cover, counted from the
code — no scorecards, no self-assessment. If a number here disagrees with a
fresh count from the tree, the tree wins and this file is the bug.

- **Version:** 2.0.0
- **Counted on:** 2026-11-03 (release-freeze recount, `cargo test --workspace`)
- **Counting method:** `grep -rnE "^\s*#\[(tokio::)?test\b" --include=*.rs crates src tests`
  for test attributes, then `grep -rnE "^\s*#\[ignore" …` for tests that need a live
  engine, and finally the per-binary `test result:` lines of an actual
  `cargo test --workspace` run. All three agree: 322 functions, 306 run.
  The unanchored form of the grep (`grep -rn "#\[ignore"`) reports 17 because it
  also matches a doc comment in `tests/http_integration.rs`; the attribute count
  is 16. Recount at every release freeze before quoting externally.

## Totals

| | Count |
|---|---:|
| Test functions in crate `src/` and `crates/*/tests/` | 233 |
| Test functions in top-level `tests/` | 89 |
| **Total** | **322** |
| Runnable by default (`cargo test --workspace`) | 306 |
| `#[ignore]`d — need a live PostgreSQL/MySQL DSN | 16 |

## Coverage matrix (per area)

| Area | Files | Tests | Runs where |
|---|---|---:|---|
| Guard & classification | `guard/classify.rs` (12), `guard/lib.rs` (38), `guard/params.rs` (15), `guard/patterns.rs` (4), `guard/capabilities.rs` (2) | 71 | Default |
| Core config, policy, redaction, registry | `core/auth.rs` (13), `core/config.rs` (25), `core/redact.rs` (6), `core/registry.rs` (3) | 47 | Default |
| Tool handlers | `tools/bind.rs` (8), `ddl.rs` (9), `exec.rs` (10), `explain.rs` (6), `graph.rs` (4), `history.rs` (2), `introspect.rs` (11) | 50 | Default |
| Driver API | `driver-api/src/lib.rs` | 3 | Default |
| Driver crates | MySQL (12), PostgreSQL (12), SQLite (7), Driver Conformance Test Kit per engine (1 each = 3; MySQL/PostgreSQL cases `#[ignore]`d) | 34 | Default; ignored conformance cases need DSNs |
| Server, auth, OAuth | `server/src/lib.rs` (1), `server/src/oauth.rs` (4), `server/tests/oauth_server.rs` (11) | 16 | Default |
| Result formatting | `format/src/columnar.rs` | 12 | Default |
| F3 schema intelligence | `tests/f3_intelligence.rs` | 10 | Default |
| SQLite integration | `tests/sqlite_integration.rs` | 9 | Default |
| HTTP / handler integration | `tests/http_integration.rs` | 47 (5 ignored) | Default + Docker CI |
| MySQL integration | `tests/mysql_integration.rs` | 9 (all ignored) | Only with `MYSQL_DATABASE_URL` |
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
