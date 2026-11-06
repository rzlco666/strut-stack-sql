#!/usr/bin/env python3
"""Enforce the adopted tools/list token budgets for strut-stack-sql.

Reads the `tokens-<profile>.json` files written by scripts/token-benchmark/measure.py
and fails when the `o200k_base` token total of a profiled surface exceeds its budget.

Budgets (adopted 2026-11-06, see docs/TOKEN-BENCHMARK.md section 4):
    lite <= 750 tokens (o200k_base)
    full <= 3000 tokens (o200k_base)

This script deliberately checks ONLY the token budget. Surface assertions
(lite == 3 tools, full == 20 tools) and the CI byte caps are already enforced
by measure.py, which exits non-zero before this script ever runs.

Exit codes: 0 = all budgets met, 1 = budget exceeded, 2 = missing/invalid input.
"""
import argparse
import json
import sys
from pathlib import Path

BUDGETS = {"lite": 750, "full": 3000}
ENCODING = "o200k_base"


def load_tokens(directory, profile):
    path = Path(directory) / ("tokens-" + profile + ".json")
    try:
        payload = json.loads(path.read_text())
    except FileNotFoundError:
        print("ERROR: missing %s (did measure.py run?)" % path, file=sys.stderr)
        raise SystemExit(2)
    except json.JSONDecodeError as exc:
        print("ERROR: %s is not valid JSON: %s" % (path, exc), file=sys.stderr)
        raise SystemExit(2)
    try:
        tokens = payload["tokens_total"][ENCODING]
    except (KeyError, TypeError):
        print("ERROR: %s has no tokens_total.%s" % (path, ENCODING), file=sys.stderr)
        raise SystemExit(2)
    if not isinstance(tokens, int):
        print("ERROR: %s tokens_total.%s is not an int" % (path, ENCODING), file=sys.stderr)
        raise SystemExit(2)
    return path, payload, tokens


def main():
    ap = argparse.ArgumentParser(description="Enforce tools/list token budgets")
    ap.add_argument("--dir", required=True, help="directory holding tokens-<profile>.json")
    ap.add_argument("--lite-cap", type=int, default=BUDGETS["lite"],
                    help="token cap for the lite surface (default: %(default)s)")
    ap.add_argument("--full-cap", type=int, default=BUDGETS["full"],
                    help="token cap for the full surface (default: %(default)s)")
    args = ap.parse_args()
    caps = {"lite": args.lite_cap, "full": args.full_cap}

    failures = 0
    for profile in ("lite", "full"):
        path, payload, tokens = load_tokens(args.dir, profile)
        cap = caps[profile]
        ok = tokens <= cap
        print("%-6s %-5s %5d tokens (%s) <= cap %5d  [%s]  tools=%d raw_bytes=%s  %s"
              % (profile,
                 "PASS" if ok else "FAIL",
                 tokens, ENCODING, cap,
                 "within" if ok else "over by %d" % (tokens - cap),
                 payload.get("tool_count", -1),
                 payload.get("raw_tools_list_result_bytes", "?"),
                 path))
        if not ok:
            failures += 1

    if failures:
        print("ERROR: %d token budget(s) exceeded" % failures, file=sys.stderr)
        return 1
    print("All tools/list token budgets met.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
