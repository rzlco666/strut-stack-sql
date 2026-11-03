# MCP conformance report

Release gate for the MCP specification version this server pins. Two suites
run:

1. the **official** `@modelcontextprotocol/conformance` server scenarios;
2. the **repo-owned** runner (`scripts/conformance/`) that covers stdio and
   Streamable HTTP, including auth, sessions, error shapes, and batching.

Both are wired into CI; the release workflow makes the repo runner a required
dependency of publishing (PRD-CONF-01).

## Result summary

| Item | Value |
|---|---|
| Date | 2026-11-04 |
| Binary | `strut-stack-sql 2.0.0` (`cargo build --release --workspace`) |
| Source | `feat/v2` at `7df8f31` plus the R1-05 surface caps |
| Supported MCP spec version | **2025-06-18** (pinned, PRD-CONF-02) |
| Official suite | `@modelcontextprotocol/conformance@0.1.16`, run via `bunx` |
| Official scenarios | 6 passed, 20 not applicable (rerun against 2.0.0; details below) |
| Repo runner | **51 passed, 0 failed, 3 optional, 0 not run** (54 checks) |
| Machine report | `target/conformance/report.json` |
| Client matrix check | `scripts/conformance/check_clients.py` — 16 marked snippets, smoke passed |
| `tools/list` size caps | core 2,920 B / 3,072 B; full 20-tool 11,757 B / 12,000 B (stdio and HTTP) |

`node`/`npx` are not installed on the reference machine (`which node npx` →
not found). `bunx` is installed and ran the official suite; this is recorded
here rather than silently skipped.

## Pinned spec version and why

The server advertises **2025-06-18** only. The pin is stated in this document,
in the public README, and in `server.json` under
`_meta["io.modelcontextprotocol.registry/publisher-provided"].mcpSpecVersion`
(the registry schema has no first-class protocol-version field).

- 2025-06-18 removed JSON-RPC batching. 2025-03-26 and earlier require
  servers to **receive** batches (`MCP implementations … MUST support
  receiving JSON-RPC batches`). The underlying rmcp transport does not
  implement batch receive, so claiming 2025-03-26 would be a false claim.
- Clients that request an older or unknown version are answered with
  2025-06-18, which is the lifecycle spec's required fallback. The stdio
  transport rewrites the initialize request's version; the HTTP middleware
  rewrites the body and rejects an unsupported `MCP-Protocol-Version` header
  with 400.
- JSON-RPC batch input is therefore rejected on both transports, which is the
  2025-06-18 behavior.

## Official suite

Command (server on a loopback HTTP bind, empty SQLite fixture):

```bash
bunx @modelcontextprotocol/conformance server \
  --url http://127.0.0.1:18938/mcp \
  --spec-version 2025-06-18 \
  -o target/conformance/official
```

The official suite tests servers over HTTP only (`server --url`; there is no
stdio server mode), so stdio is covered by the repo runner below.

### Passed (6)

| Scenario | Notes |
|---|---|
| `server-initialize` | initialize + initialized handshake |
| `ping` | empty result |
| `tools-list` | every listed tool has name/description/inputSchema |
| `completion-complete` | server returns an empty completion result |
| `resources-list` | server returns an empty resources list (capability is not advertised) |
| `prompts-list` | server returns an empty prompts list (capability is not advertised) |

### Not applicable (20)

These scenarios require capabilities or fixture objects this product does not
expose. They are failures in the official summary but are **not** conformance
defects for a tools-only SQL server; no check was weakened.

| Scenario group | Count | Concrete reason |
|---|---|---|
| `tools-call-simple-text`, `-image`, `-audio`, `-embedded-resource`, `-mixed-content`, `-error`, `-with-progress`, `-sampling`, `-elicitation` | 9 | The suite calls fixture tools named `test_simple_text`, `test_image_content`, etc. This server intentionally exposes only its real SQL tools; unknown names are rejected. |
| `tools-call-with-logging`, `logging-set-level` | 2 | The server does not implement the MCP logging capability (`logging/setLevel` → `-32601`). |
| `resources-read-text`, `-read-binary`, `-templates-read`, `-subscribe`, `-unsubscribe` | 5 | No resources capability; `resources/list` is empty, so reads/subscriptions return `-32601`. |
| `prompts-get-simple`, `-with-args`, `-embedded-resource`, `-with-image` | 4 | No prompts capability; `prompts/list` is empty, so gets return `-32601`. |

The official suite is explicitly marked unstable by its maintainers; results
are recorded verbatim rather than treated as a stable score.

## Repo runner

Command:

```bash
scripts/conformance/run.sh --json-out target/conformance/report.json
```

It builds nothing; pass a release binary (default
`target/release/strut-stack-sql`, override with `$STRUT_STACK_SQL_BIN` or the
first argument). It uses Python 3 stdlib only and starts its own SQLite
fixture, HTTP servers, and fake OAuth authorization server.

### Coverage

| Group | Pass | Optional | What it proves |
|---|---|---|---|
| `stdio.lifecycle` | 3 | — | initialize, pinned version, unsupported-version fallback, ping |
| `stdio.tools` | 19 | — | 3-tool default surface, 20-tool full+DDL surface, schema validity of every tool, happy-path calls for core/full/DDL groups, ordered batch |
| `stdio.errors` | 2 | 2 | unknown tool error shape, invalid JSON-RPC envelope → `-32600`; optional: canonical `-32602`, parse-error reply |
| `stdio.batch` | 1 | — | batch rejected under 2025-06-18 |
| `http.lifecycle` | 4 | — | initialize, session header, `notifications/initialized` → 202, ping, unsupported-version fallback |
| `http.tools` | 2 | — | `tools/list` 20 tools + schemas, `execute_sql` call |
| `http.errors` | 2 | — | unknown tool shape, malformed body → 4xx |
| `http.batch` | 1 | — | batch rejected |
| `http.session` | 4 | 1 | unknown session → 404, missing session → 4xx, DELETE, post-DELETE 404; optional: exact 400 |
| `http.protocol` | 1 | — | unsupported `MCP-Protocol-Version` → 400 |
| `http.auth` | 2 | — | missing/invalid API key → 401 + `WWW-Authenticate` |
| `http.oauth` | 5 | — | RFC 9728 metadata (root + resource path), 404 when unconfigured, challenge `resource_metadata`, invalid-token challenge |
| `http.origin` | 2 | — | non-loopback Host/Origin → 4xx; loopback Host/Origin → 2xx |
| **Total** | **48** | **3** | 51 checks |

### Optional findings (recorded, not hidden)

1. **Unknown tool code.** The server returns a typed JSON-RPC error
   `-32010 authorization_denied` with `data.code = "authorization_denied"`.
   The tools spec *recommends* `-32602` (`SHOULD`). The typed error is part of
   the product's error contract (PRD-F3-10) and an unexposed tool is an
   authorization decision, so the code was not changed. The check asserts a
   valid JSON-RPC error object and records the code as optional.
2. **Parse error over stdio.** Syntactically invalid JSON receives no
   `-32700` response; rmcp deliberately ignores unparsable input (its upstream
   issue #938). The stdio transport contract says the client **MUST NOT** write
   anything that is not a valid MCP message, so the behavior is recorded as
   optional/informational rather than failing the gate.
3. **Missing session header status.** A non-initialize request without
   `Mcp-Session-Id` returns **422**, while the spec says servers that require a
   session **SHOULD** respond 400. 422 is still a 4xx rejection; the runner
   accepts any 4xx and records the exact status.

No other deviations were observed. All required checks passed.

## Server fixes made for this gate

These were genuine defects exposed by the probes/suite and are covered by
regression tests:

1. **Server identity.** `initialize` advertised `serverInfo.name = "rmcp"` and
   `version = "2.2.0"` (the SDK's build environment). It now advertises
   `strut-stack-sql` + the crate version. Test:
   `initialize_advertises_this_server_identity`.
2. **Protocol negotiation.** rmcp echoed every known version, including
   2025-03-26, whose batch requirement is not met. The server now pins
   2025-06-18: a stdio transport wrapper rewrites the initialize version; an
   HTTP middleware rewrites the body and rejects unsupported
   `MCP-Protocol-Version` headers with 400. Tests:
   `initialize_negotiates_pinned_protocol_version`,
   `http_rejects_unsupported_protocol_version_header`.
3. **DNS rebinding.** The HTTP transport did not validate `Origin`. A
   non-loopback `Origin` (the browser attack path) is now rejected with 403;
   loopback origins must agree with the `Host` header and port; non-loopback
   deployments can allow exact origins through
   `STRUT_STACK_SQL_ALLOWED_ORIGINS`. Test:
   `http_rejects_non_loopback_origin`, `http_accepts_loopback_origin`.

## CI wiring

- `.github/workflows/ci.yml` → job **MCP conformance gate**: builds the
  workspace release binary, runs the repo runner and the client-matrix
  checker, uploads `target/conformance/`.
- `.github/workflows/release.yml` → job **Release gate (tests + MCP
  conformance)**: runs `cargo test --workspace`, the repo runner, and the
  client-matrix checker. The `release` job declares
  `needs: [build, conformance]`, so a red conformance gate blocks publishing
  (PRD-CONF-01).

## Not run / limits

- Official suite **stdio** mode: the package has no stdio server mode; the repo
  runner covers stdio.
- Official suite scenarios that require resources, prompts, logging, or the
  suite's fixture tools: not applicable (see table above).
- PostgreSQL/MySQL conformance: this runner uses SQLite. The three official
  drivers are covered separately by the Driver Conformance Test Kit; the Docker
  integration suite remains the engine-level gate.
- Real OAuth token issuance: the runner uses a fake authorization server for
  discovery/metadata and invalid-token rejection; it does not mint a valid RS256
  access token.
- Per-client end-to-end smoke through the six client applications: those
  binaries are not installed on the reference machine. Their config blocks are
  parsed and shape-checked, and the documented generic stdio command line is
  spawned for `initialize` + `tools/list`; the per-client application smoke
  from PRD-CONF-03 remains a release-environment step.
