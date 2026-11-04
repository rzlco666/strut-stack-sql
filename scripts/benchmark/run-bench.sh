#!/usr/bin/env bash
# One-door benchmark harness (PRD-F2-12/F2-13).
# Builds the release binary if missing, then measures stdio + HTTP and writes
# scripts/benchmark/results/<release>.json with machine specs.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
RELEASE="${RELEASE:-2.0.0}"
BINARY="${BINARY:-${ROOT}/target/release/strut-stack-sql}"
ITERATIONS="${ITERATIONS:-120}"
WARMUP="${WARMUP:-20}"
COLD_RUNS="${COLD_RUNS:-20}"

if [ ! -x "$BINARY" ]; then
  echo "Building release binary..." >&2
  (cd "$ROOT" && CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}" cargo build --release --workspace)
fi

exec python3 "${ROOT}/scripts/benchmark/bench.py" \
  --binary "$BINARY" \
  --release "$RELEASE" \
  --out "${ROOT}/scripts/benchmark/results/${RELEASE}.json" \
  --iterations "$ITERATIONS" \
  --warmup "$WARMUP" \
  --cold-runs "$COLD_RUNS"
