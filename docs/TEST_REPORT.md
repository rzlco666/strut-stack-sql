# Test Report — strut-stack-sql v1.1.0

What the automated tests in this repository actually cover, counted from the
code — no scorecards, no self-assessment. If a number here ever disagrees
with a fresh count from the tree, the tree wins and this file is the bug.

- **Version:** 1.1.0
- **Counted on:** 2026-11-02
- **Counting method:** `grep -rn "#\[test\]\|#\[tokio::test\]" src tests`
  (test attributes), cross-checked per file below. Snapshot taken while the
  1.1.0 hardening was landing (count verified stable across two consecutive
  counts); **recount at release freeze** before quoting externally.

## Totals

| | Count |
|---|---:|
| Test functions in `src/` (inline unit tests) | 139 |
| Test functions in `tests/` (integration/unit files) | 62 |
| **Total** | **201** |
| Runnable by default (`cargo test`) | 187 |
| `#[ignore]`d — need a live engine DSN | 14 |

## Coverage matrix (per area)

| Area | Files | Tests | Runs where |
|---|---|---:|---|
| SQL guard & classification | `src/guard/classify.rs` (12), `src/guard/mod.rs` (38), `src/guard/params.rs` (15) | 65 | Default |
| Configuration & HTTP tokens | `src/config.rs` | 12 | Default |
| DB layer (bind, exec, pool, schema, value) | `src/db/bind.rs` (6), `src/db/exec.rs` (1), `src/db/pool.rs` (3), `src/db/schema.rs` (18), `src/db/value.rs` (12) | 40 | Default |
| Result formatting | `src/format/columnar.rs` | 7 | Default |
| URL redaction | `src/redact.rs` | 6 | Default |
| DDL tools | `src/tools/ddl.rs` | 9 | Default |
| Standalone unit tests | `tests/unit.rs` | 14 | Default |
| SQLite integration | `tests/sqlite_integration.rs` | 6 | Default |
| HTTP / handler integration | `tests/http_integration.rs` | 34 (28 default on SQLite + 6 ignored) | Default + CI docker job |
| MySQL integration | `tests/mysql_integration.rs` | 8 (all ignored) | Only with `MYSQL_DATABASE_URL` |

## What runs in CI

| Job | What it runs |
|---|---|
| `test` | `cargo test --all-targets` (the 187 default tests), then `cargo build --release` |
| `lint` | `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo audit` (RustSec) |
| `integration-http-docker` | Seeds PostgreSQL 16 + MySQL 8.4 service containers, runs the 6 ignored HTTP integration tests against both engines |
| `integration-mysql` (main pushes only) | The 8 MySQL integration tests when the `MYSQL_DATABASE_URL` secret is set; skips loudly otherwise |

## 1.1.0 regression tests (named, by fix)

Each 1.1.0 audit fix ships with a test that fails on v1.0.4 behavior:

| Fix | Tests |
|---|---|
| CTE writes classified as DML | `cte_delete_is_dml_not_read`, `cte_update_and_insert_are_dml`, `nested_cte_write_is_found`, `plain_cte_select_is_read` |
| `SELECT INTO` / `INTO OUTFILE` not reads | `select_into_is_not_read`, `mysql_into_outfile_is_not_read` |
| GRANT/REVOKE are Admin class, need `--allow-grants` | `grant_and_revoke_are_admin_not_ddl`, `grant_blocked_below_ddl_mode`, `grant_allowed_at_guard_level_in_ddl_mode` |
| Parameterized LIMIT clamp | `parameterized_limit_clamp_preserves_placeholder`, `clamps_explicit_limit_above_max_rows`, `clamps_user_limit_to_max_rows` |
| HTTP token parsing & modes | `parse_http_tokens_full_format`, `parse_http_tokens_rejects_garbage` |
| Deterministic, strict TOML config | `toml_default_source_is_deterministic`, `toml_unknown_fields_rejected` |
| URL redaction hardening | `redacts_password_containing_at_sign`, `redacts_password_containing_colon`, `redacts_sensitive_query_params` |
| Value fidelity | `blob_hex_json_is_tagged_object`, `bytes_to_hex_encodes_ascii`, `bigdecimal_high_precision_falls_back_to_string`, `bigdecimal_exact_binary_friendly_stays_number` |
| Param binding strictness | `param_count_mismatch_rejected`, `params_extra_rejected`, `params_missing_placeholder_rejected`, `unexpected_params_rejected` |

## Known gaps (stated, not hidden)

- There is no coverage/line-percentage gate in CI yet.
- Property-based tests for the placeholder scanner do not exist yet (the
  scanner is covered by example-based tests in `src/guard/params.rs`).
- MySQL integration tests depend on a repository secret and do not run on
  pull requests from forks.
- Performance is not asserted by tests; see [BENCHMARKS.md](BENCHMARKS.md)
  for the (historical) measurements and their stated limits.

## History note

The v0.4.0 edition of this report claimed 69 tests and a 9.9/10 self
scorecard. Both were stale by the time of the audit that produced 1.1.0
(the code already contained more tests than the report claimed). The
scorecard format is retired: this report now only states counts, coverage
areas, and gaps.
