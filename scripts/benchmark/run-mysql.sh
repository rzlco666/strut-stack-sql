#!/usr/bin/env bash
# Benchmark strut-stack-sql on MySQL (localhost docker compose).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DIR="$(cd "$(dirname "$0")" && pwd)"
CLIENT="${DIR}/mcp-client.mjs"

DATABASE_URL="${DATABASE_URL:-mysql://demo:demo@127.0.0.1:3307/demo}"
BINARY="${BINARY:-${ROOT}/target/release/strut-stack-sql}"
ITERATIONS="${ITERATIONS:-50}"
WARMUP="${WARMUP:-5}"

export DATABASE_URL

require() {
  command -v "$1" >/dev/null 2>&1 || { echo "Missing dependency: $1" >&2; exit 1; }
}

require node

if [ ! -f "$BINARY" ]; then
  echo "Building strut-stack-sql release binary..."
  (cd "$ROOT" && cargo build --release)
fi

if ! docker compose -f "${ROOT}/docker-compose.yml" ps mysql 2>/dev/null | grep -q healthy; then
  echo "Starting MySQL via docker compose..."
  docker compose -f "${ROOT}/docker-compose.yml" up -d mysql
  for _ in $(seq 1 60); do
    if docker compose -f "${ROOT}/docker-compose.yml" ps mysql 2>/dev/null | grep -q healthy; then
      break
    fi
    sleep 2
  done
fi

ENV_JSON="$(node -e 'console.log(JSON.stringify({ DATABASE_URL: process.env.DATABASE_URL }))' DATABASE_URL="$DATABASE_URL")"
TOOL_INPUT='{"sql":"SELECT id, email, name FROM demo.users WHERE id = 42"}'

echo "==> strut-stack-sql MySQL single SELECT" >&2
node "$CLIENT" \
  --command "$BINARY" \
  --args "" \
  --env "$ENV_JSON" \
  --tool execute_sql \
  --input "$TOOL_INPUT" \
  --warmup "$WARMUP" \
  --iterations "$ITERATIONS"

echo "Done. Set ITERATIONS/WARMUP to tune. See docs/BENCHMARKS.md for reference numbers." >&2
