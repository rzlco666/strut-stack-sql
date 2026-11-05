# Security Policy

## Supported Versions

| Version | Supported |
| ------- | --------- |
| 2.x (latest release) | ✅ Security fixes |
| 1.x and earlier | ❌ Please upgrade to 2.x |

Security fixes ship as patch releases on the latest 2.x line. Releases are
tagged on GitHub — verify the `SHA256SUMS` file before running a release
binary.

## Reporting a Vulnerability

**Do not open a public issue, pull request, or Discussion for a security
vulnerability.**

- **Preferred:** [GitHub Private Vulnerability Reporting](https://github.com/rzlco666/strut-stack-sql/security/advisories/new)
  (repository Security tab → "Report a vulnerability"). The report stays
  private; the maintainer is notified automatically.
- **Fallback:** email [syahrizalhanif@gmail.com](mailto:syahrizalhanif@gmail.com)
  with the subject `[strut-stack-sql security]`.

Please include: a description of the issue, the affected version, steps to
reproduce, and the impact you see. A working exploit is **not** required and
should not be included in the first report.

> Note: Private Vulnerability Reporting must be enabled in the repository
> Settings — this file alone does not enable it.

## Response Timeline

This is a single-maintainer project, so the timeline below is a realistic
target, not an enterprise SLA:

| Step | Target |
| ---- | ------ |
| Acknowledgement (report received) | ≤ 3 working days |
| Triage (confirmed or not, severity) | ≤ 7 calendar days |
| Fix + advisory for high severity | Patch release, target ≤ 30 days |
| Advisory publication (coordinated disclosure) | When the patch is available; if no fix yet, ≤ 90 days after the report |

## Coordinated Disclosure

Please keep the report private until the advisory is published. The reporter
may request credit in the advisory. Do not publish details or a proof of
concept before the advisory is out.

## Safe Harbor

Good-faith security research is welcome. We will not pursue legal action
against researchers who follow this policy. Please do not test against
systems you do not own, exfiltrate data, or disrupt service availability.

## What This Server Does and Does Not Protect

Defense in depth, not a sandbox. The controls below are real, but none of
them replaces least-privilege database accounts, network restrictions, and
review of SQL privileges.

**What exists today (2.0.1):**

- **Read-only by default.** The `lite` profile exposes 3 read-only tools.
  Write and DDL tools are opt-in (`--full-tools` plus `--allow-ddl`), and
  destructive tools require `confirm: true` (and, with
  `--require-approval`, an accepted approval).
- **Per-database allowlist (V1).** A server can be restricted to named
  databases; catalog table arguments are checked against the allowlist
  before any driver call.
- **Column redaction.** `--deny-columns` replaces the cells of named columns
  with `"[redacted]"`. This is accidental-exposure protection, **not** an
  access-control boundary: the match is by exact column name
  (case-insensitive), so an alias (`SELECT password AS p`) evades it. Use a
  least-privilege database account and views for real isolation.
- **Token budget.** The `tools/list` payload size is budget-checked in CI
  (conformance gate), so the tool surface cannot silently bloat.

**What is not built yet (roadmap, not a promise):**

- A persistent audit log (JSONL) with agent identity.
- Column masking / row-filter policy.
- A health and diagnostics tool in MCP.

**Deployment notes:**

- Database privileges remain decisive. Run the server with a dedicated,
  least-privilege database account — never an administrative one — and
  isolate demonstration environments.
- Do not expose the HTTP transport publicly without a properly secured
  TLS / reverse-proxy and network policy. Bind locally for development.
- Treat credentials as secrets. Never commit `.env` files, real token
  hashes, raw tokens, or audit records containing sensitive SQL.
- Treat data returned through tools as untrusted content, not as
  instructions to the agent.

This project makes no claim of absolute security. Review and test it before
any production deployment.
