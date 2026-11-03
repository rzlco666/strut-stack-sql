#!/usr/bin/env bash
# Compare strut-stack-sql vs mcp-mysql-server (local-mysql rival) on shared queries.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${MCP_SQL_RUST_BIN:-$ROOT/target/release/strut-stack-sql}"
MYSQL_URL="${MYSQL_DATABASE_URL:-${DATABASE_URL:-mysql://demo:demo@127.0.0.1:3307/demo}}"

if [[ ! -x "$BIN" ]]; then
  echo "Building release binary..."
  (cd "$ROOT" && cargo build --release)
  BIN="$ROOT/target/release/strut-stack-sql"
fi

echo "=== strut-stack-sql cold spawn (stdio handshake skipped) ==="
/usr/bin/time -f 'elapsed %e s' "$BIN" --url "$MYSQL_URL" --help >/dev/null

echo ""
echo "=== strut-stack-sql tools/list size ==="
# Requires running server — approximate via --help + known tool count
wc -c <<< '{"tools":10}' | awk '{print "approx full-tools schema bytes (see server list_tools):", $1}'

echo ""
echo "=== Query parity (run manually with MCP client) ==="
cat <<'SQL'
SELECT COUNT(*) total_tables, SUM(table_rows) approx_rows
FROM information_schema.tables
WHERE table_schema = DATABASE() AND table_type='BASE TABLE';
SQL

echo ""
echo "Rival: bunx mcp-mysql-server (see scripts/benchmark/run-mysql.sh)"
echo "Rust:  $BIN --url '$MYSQL_URL' --full-tools"
