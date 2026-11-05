# Contributing to strut-stack-sql

Thank you for considering a contribution. There are opportunities to
contribute at any level — no contribution is too small and all contributions
are valued. Do not let this guide intimidate you; it should be considered a
map.

By participating, you agree to follow our
[Code of Conduct](CODE_OF_CONDUCT.md).

## How to contribute

There are three ways to help:

1. **Open an issue** to discuss a bug, a feature idea, or a documentation
   gap — especially before writing a large change.
2. **Comment and help triage** — confirm a report, add a reproduction, or
   answer a question.
3. **Work on an issue** — pick one up, or bring your own fix.

Small fixes (a typo, a small bug) can go straight to a pull request. A large
change — a new feature, a new configuration option, a new lint rule — starts
with an issue that describes the proposed change, so the design can be
discussed before code is written.

Issues suitable for newcomers are labeled [`good first issue`](https://github.com/rzlco666/strut-stack-sql/labels/good%20first%20issue)
and [`help wanted`](https://github.com/rzlco666/strut-stack-sql/labels/help%20wanted).

## Development setup

The full development setup — Rust version, Docker databases for integration
tests, benchmarks, agent workflow files — lives in
[docs/CONTRIBUTING.md](docs/CONTRIBUTING.md). In short:

```bash
git clone https://github.com/rzlco666/strut-stack-sql
cd strut-stack-sql
cargo test
```

Rust 1.88+ is required. Integration tests that need a live database are
ignored by default; see `docs/CONTRIBUTING.md` for the compose setup and the
per-engine commands.

## Gates — run these before opening a PR

CI runs the same four gates on every pull request:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo audit
```

All four must pass. A pull request that fails a gate will not be merged.

## Code style and commits

- Format with `rustfmt`. Do not reformat files you did not touch.
- Write clear commit messages, following the repository pattern:
  `fix(...)`, `feat(...)`, `docs(...)`, `chore(...)`, `test(...)`.
- Add tests for every behavior change. Name a bug-fix test after the issue
  it fixes where that helps.
- Document user-facing features and fixes in `CHANGELOG.md` under the
  unreleased section, and update the relevant page under `docs/` when a
  feature raises a common question.

## Reporting a bug

Use the bug report template (`.github/ISSUE_TEMPLATE/bug_report.yml`) and
include:

- The server version (`--version` or release tag).
- The database engine.
- The tool profile (`lite`, `standard`, `full`, `admin`).
- Steps to reproduce, the expected behavior, and the actual behavior.
- Relevant logs or output (as text, not screenshots) and the exact command
  you ran.
- A minimal, complete, and verifiable example where possible.

Do **not** report a security vulnerability through a public issue — see
[SECURITY.md](SECURITY.md).

## Proposing a feature

Open a feature-request issue first: describe the problem you want solved
before the solution, the solution you propose, and alternatives you
considered. Say whether you are willing to work on it.

## Review

Any community member is welcome to review any pull request. The maintainer
reviews every pull request before merging; please be patient and responsive
to feedback. A pull request may ask for changes, and that is a normal part
of the process.

## License

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in `strut-stack-sql` by you shall be licensed as
MIT, without any additional terms or conditions. This project does not use
a Contributor License Agreement (CLA) or a Developer Certificate of Origin
(DCO) sign-off — the inbound-MIT sentence above is the whole agreement.
