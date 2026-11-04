#!/usr/bin/env bash
# Run the self-contained MCP conformance gate against a built release binary.
#
# Usage:
#   scripts/conformance/run.sh [BINARY] [runner options...]
#
# BINARY defaults to target/release/strut-stack-sql (or $STRUT_STACK_SQL_BIN).
# Extra arguments are forwarded to runner.py (e.g. --json-out /tmp/report.json).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary="${STRUT_STACK_SQL_BIN:-$root/target/release/strut-stack-sql}"
if [ $# -gt 0 ] && [ "${1#-}" = "$1" ]; then
  binary="$1"
  shift
fi

if [ ! -x "$binary" ]; then
  echo "error: release binary not executable: $binary" >&2
  echo "build it with: cargo build --release --workspace" >&2
  exit 2
fi

exec python3 "$root/scripts/conformance/runner.py" --binary "$binary" "$@"
