#!/usr/bin/env python3
"""Token benchmark Phase 1 harness for strut-stack-sql.

Implements the design in token-benchmark-design-2026-11-05.md (Luka):
spawn the server binary via stdio, send MCP initialize + tools/list, and count
tokens over ONLY name + description + inputSchema per tool (octomind method).
Encodings: o200k_base (primary), cl100k_base (secondary).

Surface guarantees (Kawhi review, HARNESS-REVIEW-CHANGES):
- lite MUST list exactly the 3 CORE_TOOLS and full MUST list exactly the 20
  FULL_TOOLS defined in scripts/conformance/runner.py on main — the harness
  asserts this and fails hard on mismatch, so a smaller surface can never be
  measured silently. Full/admin spawns add --allow-ddl, matching the CI
  runner (["--url", sqlite, "--full-tools", "--allow-ddl"]).
- Byte cross-check: the CI byte budgets (lite <= 3072 B, full <= 12000 B)
  apply to the WHOLE tools/list result payload, i.e. raw_tools_list_result_bytes
  (identical to CI payload_bytes: compact separators, ensure_ascii=True; key
  order does not change length). The canonical per-tool bytes are a secondary
  metric only and are NOT compared to the CI caps: for ASCII payloads
  canonical = CI bytes - 14 B (lite, 3 tools) / - 31 B (full, 20 tools)
  (12 B for the {"tools":[...]} wrapper + 1 B comma per extra tool).
  Predicted validation values on the same binary: canonical lite ~2,629 B,
  full ~10,822 B (Kawhi). Non-ASCII descriptions break the offset (CI escapes
  them with ensure_ascii=True) — the raw-bytes comparison stays exact.
- $schema fields are stripped from inputSchema before counting (design trap #2)
  and the stripped count is reported; a non-zero count is a loud warning.
- tiktoken version is recorded automatically (importlib.metadata) and pinned in
  token-benchmark-requirements.txt (tiktoken==0.14.0, verified latest 2026-11-06).
  --tiktoken-version, if given, must match the installed version or the run fails.
- Protocol versions are tried in order 2025-06-18 (the server's own
  conformance suite version), 2025-03-26, 2024-11-05.
- First tiktoken run may download BPE data if the cache is cold; warm the
  cache (run once, or pre-fetch the encodings) before a fully offline run.

No repo files are touched; raw payloads + results land in the out dir.

LOCAL COPY (2026-11-06, LeBron): the WSL copy lives at
~/projects/strut-stack-sql-benchmark/scripts/token-benchmark/measure.py
(synced by Melo 2026-11-06 00:54, SHA verified). After ANY edit to this
file, re-sync with scp and re-verify SHA-256 matches on both sides.
Spawn-form fix 2026-11-06: --profile is ignored by the real v2.0.1 binary
(all values -> lite surface); list_tools() now maps profiles to the CI
spawn forms (--full-tools [+ --allow-ddl], full via --url sqlite::memory:).
"""
import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

# Exact surfaces from scripts/conformance/runner.py on main
# (runner.py:40 CORE_TOOLS; FULL_TOOLS set; downloaded 2026-11-06,
#  file SHA 13955106cebad6e76c500945257af681e62a898d).
CORE_TOOLS = {"search_objects", "execute_sql", "analyze_query_performance"}
FULL_TOOLS = {
    "add_column",
    "alter_column",
    "analyze_query_performance",
    "create_table",
    "describe_table",
    "drop_column",
    "drop_table",
    "execute_sql",
    "get_relationship_graph",
    "get_schema_context",
    "list_foreign_keys",
    "list_indexes",
    "list_schemas",
    "list_sources",
    "list_tables",
    "schema_mutate",
    "schema_overview",
    "search_objects",
    "suggest_joins",
    "truncate_table",
}
# CI byte caps from runner.py: CORE_TOOLS_LIST_BYTES_CAP = 3072,
# FULL_TOOLS_LIST_BYTES_CAP = 12000. No CI cap exists for standard/admin.
CI_BYTE_CAPS = {"lite": 3072, "full": 12000}
# Server's own conformance suite negotiates 2025-06-18 (SUPPORTED_VERSION).
PROTOCOL_VERSIONS = ("2025-06-18", "2025-03-26", "2024-11-05")


def send_recv(proc, payload, expect_id, timeout=30.0):
    proc.stdin.write((json.dumps(payload) + "\n").encode())
    proc.stdin.flush()
    deadline = time.time() + timeout
    while time.time() < deadline:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("server closed stdout while waiting for id=%s" % expect_id)
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            print("non-JSON stdout line skipped: %r" % line[:200], file=sys.stderr)
            continue
        if msg.get("id") == expect_id:
            if "error" in msg:
                raise RuntimeError("JSON-RPC error for id=%s: %s" % (expect_id, msg["error"]))
            return msg
    raise RuntimeError("timeout waiting for id=%s" % expect_id)


def list_tools(binary, profile, stderr_path, timeout=30.0):
    # Spawn forms verified against the REAL v2.0.1 release binary
    # (2026-11-06, LeBron, live): the --profile flag is IGNORED by the
    # server — lite/standard/full/admin ALL yield the 3-tool lite surface,
    # in both "--profile X" and "--profile=X" spellings, with or without
    # --url. The operative surface selector is --full-tools (13 tools =
    # standard) + --allow-ddl (7 DDL tools -> 20 = full), exactly as the CI
    # conformance runner spawns it ([--url, sqlite, --full-tools,
    # --allow-ddl]). The surface assert in main() is the backstop: if a
    # flag combination ever stops producing the CI surface, the run fails
    # loudly instead of measuring a smaller surface silently.
    # (Before this fix the harness spawned --profile <p> --ephemeral, which
    # the fake server honored but the real binary ignores — the backstop
    # caught it: full measured 3 tools and the run failed exit 2.)
    if profile == "lite":
        cmd = [str(binary), "--ephemeral"]
    elif profile == "standard":
        cmd = [str(binary), "--full-tools", "--ephemeral"]
    elif profile in ("full", "admin"):
        cmd = [str(binary), "--url", "sqlite::memory:", "--full-tools", "--allow-ddl"]
    else:  # pragma: no cover - argparse choices already restrict this
        raise ValueError("unknown profile %r" % profile)
    err = open(stderr_path, "wb")
    proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=err)
    try:
        negotiated = None
        last_exc = None
        for version in PROTOCOL_VERSIONS:
            init = {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": version, "capabilities": {},
                               "clientInfo": {"name": "token-benchmark", "version": "1.0.0"}}}
            try:
                resp = send_recv(proc, init, 1, timeout)
            except RuntimeError as exc:
                print("initialize %s failed (%s)" % (version, exc), file=sys.stderr)
                last_exc = exc
                continue
            negotiated = resp.get("result", {}).get("protocolVersion")
            break
        if negotiated is None:
            raise RuntimeError("initialize failed for all protocol versions %s: %s"
                               % (PROTOCOL_VERSIONS, last_exc))
        proc.stdin.write((json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n").encode())
        proc.stdin.flush()
        tools_resp = send_recv(proc, {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}, 2, timeout)
        return tools_resp.get("result", {}), negotiated
    finally:
        try:
            proc.stdin.close()
        except Exception:
            pass
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=10)
        err.close()


def installed_tiktoken_version():
    from importlib.metadata import PackageNotFoundError, version
    try:
        return version("tiktoken")
    except PackageNotFoundError:
        return ""


def measure(tools, enc_names):
    import tiktoken
    encs = {name: tiktoken.get_encoding(name) for name in enc_names}
    rows = []
    totals = {name: 0 for name in enc_names}
    total_bytes = 0
    schema_stripped = 0
    for tool in tools:
        schema = tool.get("inputSchema", {})
        if isinstance(schema, dict) and "$schema" in schema:
            # Design trap #2: $schema is never counted. The server removed it
            # in list_tools, so any occurrence is a regression — strip it
            # defensively and report the count loudly instead of counting it.
            schema = {k: v for k, v in schema.items() if k != "$schema"}
            schema_stripped += 1
        canon = json.dumps(
            {"name": tool.get("name", ""), "description": tool.get("description", ""),
             "inputSchema": schema},
            sort_keys=True, separators=(",", ":"), ensure_ascii=False)
        nbytes = len(canon.encode("utf-8"))
        counts = {name: len(enc.encode(canon)) for name, enc in encs.items()}
        for name in enc_names:
            totals[name] += counts[name]
        total_bytes += nbytes
        row = {"name": tool.get("name"), "bytes": nbytes}
        for name in enc_names:
            row["tokens_" + name] = counts[name]
        rows.append(row)
    return rows, totals, total_bytes, schema_stripped


def main():
    ap = argparse.ArgumentParser(description="Phase 1 schema token measurement")
    ap.add_argument("--binary", required=True, help="path to strut-stack-sql binary")
    ap.add_argument("--profile", required=True, choices=["lite", "standard", "full", "admin"])
    ap.add_argument("--commit", default="", help="commit SHA measured")
    ap.add_argument("--out-dir", required=True, help="results directory")
    ap.add_argument("--tiktoken-version", default="",
                    help="expected installed tiktoken version; must match the "
                         "installed version or the run fails (recorded "
                         "automatically when omitted)")
    args = ap.parse_args()

    tk_version = installed_tiktoken_version()
    if not tk_version:
        print("ERROR: tiktoken is not installed; run "
              "`pip install -r token-benchmark-requirements.txt` first", file=sys.stderr)
        return 2
    if args.tiktoken_version and args.tiktoken_version != tk_version:
        print("ERROR: --tiktoken-version %s does not match installed tiktoken %s"
              % (args.tiktoken_version, tk_version), file=sys.stderr)
        return 2

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    stderr_path = out_dir / ("server-stderr-" + args.profile + ".log")

    result, negotiated = list_tools(args.binary, args.profile, stderr_path)
    tools = result.get("tools", [])
    # Same value as CI payload_bytes: compact separators, ensure_ascii=True
    # (json.dumps default); key order does not change the length.
    raw_bytes = len(json.dumps(result, sort_keys=True, separators=(",", ":")).encode("utf-8"))

    # Surface assert: the measured surface MUST equal the CI surface, or the
    # run fails here — never publish numbers from a smaller surface.
    names = {t.get("name") for t in tools}
    surface_assertion = None
    if args.profile == "lite":
        if names != CORE_TOOLS:
            print("ERROR: lite surface mismatch: got %s, CI CORE_TOOLS is %s"
                  % (sorted(names), sorted(CORE_TOOLS)), file=sys.stderr)
            return 2
        surface_assertion = "lite==CORE_TOOLS(3)"
    elif args.profile == "full":
        if names != FULL_TOOLS or len(tools) != 20:
            print("ERROR: full surface mismatch: got %d tools %s, CI FULL_TOOLS is 20 tools %s"
                  % (len(tools), sorted(names), sorted(FULL_TOOLS)), file=sys.stderr)
            return 2
        surface_assertion = "full==FULL_TOOLS(20)"

    enc_names = ("o200k_base", "cl100k_base")
    rows, totals, total_bytes, schema_stripped = measure(tools, enc_names)
    if schema_stripped:
        print("WARNING: stripped $schema from %d tool inputSchema(s); "
              "the server is expected to omit it (see list_tools handler)"
              % schema_stripped, file=sys.stderr)

    cap = CI_BYTE_CAPS.get(args.profile)
    within_cap = None if cap is None else raw_bytes <= cap

    payload = {
        "commit": args.commit,
        "profile": args.profile,
        "binary": str(args.binary),
        "protocolVersion_negotiated": negotiated,
        "tiktoken_version": tk_version,
        "tiktoken_version_expected": args.tiktoken_version or None,
        "measured_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "tool_count": len(tools),
        "tool_names": [t.get("name") for t in tools],
        "surface_assertion": surface_assertion,
        "raw_tools_list_result_bytes": raw_bytes,
        "ci_byte_cap": cap,
        "raw_bytes_within_ci_cap": within_cap,
        "canonical_name_desc_schema_bytes": total_bytes,
        "tokens_total": totals,
        "dollar_schema_fields_stripped": schema_stripped,
        "per_tool": rows,
    }
    raw_path = out_dir / ("tools-list-" + args.profile + ".json")
    raw_path.write_text(json.dumps(result, indent=2, sort_keys=True))
    tokens_path = out_dir / ("tokens-" + args.profile + ".json")
    tokens_path.write_text(json.dumps(payload, indent=2, sort_keys=True))

    print("## profile %s (commit %s, protocol %s, tiktoken %s)"
          % (args.profile, args.commit or "?", negotiated, tk_version))
    print("tools: %d | canonical bytes: %d | raw result bytes: %d | $schema stripped: %d"
          % (len(tools), total_bytes, raw_bytes, schema_stripped))
    if surface_assertion:
        print("surface: %s (asserted vs CI runner)" % surface_assertion)
    if cap is None:
        print("CI byte cap: none defined for profile %s (raw %d B reported only)"
              % (args.profile, raw_bytes))
    else:
        print("CI byte cap: raw %d B vs cap %d B — %s"
              % (raw_bytes, cap, "PASS" if within_cap else "FAIL"))
    print("| tool | bytes | tokens o200k_base | tokens cl100k_base |")
    print("|---|---|---|---|")
    for r in rows:
        print("| %s | %d | %d | %d |" % (r["name"], r["bytes"], r["tokens_o200k_base"], r["tokens_cl100k_base"]))
    print("| **TOTAL** | **%d** | **%d** | **%d** |" % (total_bytes, totals["o200k_base"], totals["cl100k_base"]))
    print("raw: %s" % raw_path)
    print("tokens: %s" % tokens_path)
    if within_cap is False:
        print("ERROR: raw tools/list bytes %d exceed CI cap %d for profile %s"
              % (raw_bytes, cap, args.profile), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
