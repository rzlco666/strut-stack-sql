#!/usr/bin/env bash
# Benchmark strut-stack-sql vs rival MCP SQL servers (PostgreSQL).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DIR="$(cd "$(dirname "$0")" && pwd)"
CLIENT="${DIR}/mcp-client.mjs"
LOCK="${DIR}/versions.lock"
OUT="${DIR}/benchmark-results.json"

# shellcheck source=/dev/null
source "$LOCK"
export SERVER_POSTGRES_NPM POSTGRES_MYSQL_MCP_NPM MYSQL_MCP_PYPI STRUT_STACK_SQL_VERSION

DATABASE_URL="${DATABASE_URL:-postgresql://demo:demo@localhost:5433/demo}"
BINARY="${BINARY:-${ROOT}/target/release/strut-stack-sql}"
ITERATIONS="${ITERATIONS:-100}"
WARMUP="${WARMUP:-5}"

export DATABASE_URL

require() {
  command -v "$1" >/dev/null 2>&1 || { echo "Missing dependency: $1" >&2; exit 1; }
}

require node
require npx

if [ ! -f "$BINARY" ]; then
  echo "Building strut-stack-sql release binary..."
  (cd "$ROOT" && cargo build --release)
fi

if ! docker compose -f "${ROOT}/docker-compose.yml" ps postgres 2>/dev/null | grep -q healthy; then
  echo "Starting docker compose..."
  docker compose -f "${ROOT}/docker-compose.yml" up -d postgres
  for _ in $(seq 1 60); do
    if docker compose -f "${ROOT}/docker-compose.yml" ps postgres 2>/dev/null | grep -q healthy; then
      break
    fi
    sleep 2
  done
fi

ENV_JSON="$(node -e 'console.log(JSON.stringify({ DATABASE_URL: process.env.DATABASE_URL }))' DATABASE_URL="$DATABASE_URL")"
TOOL_INPUT='{"sql":"SELECT id, email, name FROM demo.users WHERE id = 42"}'

run_case() {
  local name="$1"
  local command="$2"
  local args="$3"
  local tool="$4"
  local input="${5:-$TOOL_INPUT}"
  echo "==> $name" >&2
  node "$CLIENT" \
    --command "$command" \
    --args "$args" \
    --env "$ENV_JSON" \
    --tool "$tool" \
    --input "$input" \
    --warmup "$WARMUP" \
    --iterations "$ITERATIONS" 2>/dev/null \
    | node -e '
      const name = process.argv[1];
      let data = "";
      process.stdin.on("data", c => data += c);
      process.stdin.on("end", () => {
        const row = JSON.parse(data);
        row.name = name;
        console.log(JSON.stringify(row));
      });
    ' "$name"
}

RESULTS='[]'

add_result() {
  local row="$1"
  RESULTS="$(node -e 'const r=JSON.parse(process.argv[1]); const row=JSON.parse(process.argv[2]); console.log(JSON.stringify(r.concat([row])));' "$RESULTS" "$row")"
}

add_result "$(run_case "strut-stack-sql" "$BINARY" '[]' "execute_sql")"

SP_ARGS="$(node -e "console.log(JSON.stringify(['-y', process.env.SERVER_POSTGRES_NPM, process.env.DATABASE_URL]))" SERVER_POSTGRES_NPM="$SERVER_POSTGRES_NPM" DATABASE_URL="$DATABASE_URL")"
if ROW="$(run_case "server-postgres" "npx" "$SP_ARGS" "query" 2>/dev/null)"; then
  add_result "$ROW"
else
  add_result '{"name":"server-postgres","skipped":true,"reason":"benchmark failed"}'
fi

if PM_ROW="$(run_case "postgres-mysql-mcp-server" "npx" "[\"-y\",\"${POSTGRES_MYSQL_MCP_NPM}\"]" "execute_sql" 2>/dev/null)"; then
  add_result "$PM_ROW"
else
  add_result '{"name":"postgres-mysql-mcp-server","skipped":true,"reason":"tool or package unavailable"}'
fi

META="$(node -e "
console.log(JSON.stringify({
  ran_at: new Date().toISOString(),
  database_url: process.env.DATABASE_URL,
  iterations: Number(process.env.ITERATIONS),
  warmup: Number(process.env.WARMUP),
  versions: {
    server_postgres: process.env.SERVER_POSTGRES_NPM,
    postgres_mysql_mcp: process.env.POSTGRES_MYSQL_MCP_NPM
  },
  hostname: require('os').hostname(),
  disclaimer: 'Results vary by hardware and DB latency. RSS measured via /proc on Linux only.'
}));
" DATABASE_URL="$DATABASE_URL" ITERATIONS="$ITERATIONS" WARMUP="$WARMUP" SERVER_POSTGRES_NPM="$SERVER_POSTGRES_NPM" POSTGRES_MYSQL_MCP_NPM="$POSTGRES_MYSQL_MCP_NPM")"

node -e "
const fs = require('fs');
const out = process.argv[1];
const meta = JSON.parse(process.argv[2]);
const results = JSON.parse(process.argv[3]);
fs.writeFileSync(out, JSON.stringify({ meta, results }, null, 2));
" "$OUT" "$META" "$RESULTS"

echo "Wrote ${OUT}" >&2
cat "$OUT"
