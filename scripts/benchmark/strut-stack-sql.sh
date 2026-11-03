#!/usr/bin/env bash
# Benchmark mcp-sql-rust only (helper for run.sh).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BINARY="${BINARY:-${ROOT}/target/release/mcp-sql-rust}"
DATABASE_URL="${DATABASE_URL:-postgresql://demo:demo@localhost:5433/demo}"
CLIENT="${ROOT}/scripts/benchmark/mcp-client.mjs"

if [ ! -x "$BINARY" ] && [ ! -f "$BINARY" ]; then
  echo "Build release binary first: cargo build --release" >&2
  exit 1
fi

ENV_JSON="$(node -e 'console.log(JSON.stringify({ DATABASE_URL: process.env.DATABASE_URL }))' DATABASE_URL="$DATABASE_URL")"

node "$CLIENT" \
  --command "$BINARY" \
  --args '[]' \
  --env "$ENV_JSON" \
  --tool execute_sql \
  --input '{"sql":"SELECT id, email, name FROM demo.users WHERE id = 42"}' \
  --warmup 5 \
  --iterations 100
