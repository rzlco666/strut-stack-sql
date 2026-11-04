#!/usr/bin/env bash
# Credential-leak check (R1-06): no secret may appear in tool output, error
# text, or logs. Exercises the two paths that carry secrets — a DSN password
# and an HTTP bearer token — against a refused connection so the failure text
# is produced by the real redaction code.
#
# Usage: scripts/security/credential_leak_check.sh [BINARY]
# Exit 0 = no leak found.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
binary="${1:-${STRUT_STACK_SQL_BIN:-$root/target/release/strut-stack-sql}}"
if [ ! -x "$binary" ]; then
  echo "error: binary not executable: $binary" >&2
  exit 2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

password="sup3r-s3cret-p@ss"
token="tok3n-s3cret-value"
leaks=0

check() {
  local label="$1" file="$2"
  if grep -qF -e "$password" -e "$token" "$file"; then
    echo "LEAK: $label contains a secret" >&2
    grep -nF -e "$password" -e "$token" "$file" >&2 || true
    leaks=$((leaks + 1))
  else
    echo "ok: $label"
  fi
}

# --- 1. DSN password in a connection error --------------------------------
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"leak-check","version":"1"}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"execute_sql","arguments":{"sql":"SELECT 1"}}}' \
  | "$binary" --url "postgresql://user:${password}@127.0.0.1:1/db" --query-timeout 2 \
      >"$work/dsn.out" 2>"$work/dsn.err" || true
check "DSN stdout" "$work/dsn.out"
check "DSN stderr" "$work/dsn.err"

# --- 2. HTTP token in logs and in an auth failure -------------------------
port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
"$binary" --url "sqlite://$work/db.sqlite" --http "127.0.0.1:$port" --http-token "$token" \
  >"$work/http.out" 2>"$work/http.err" &
pid=$!
for _ in $(seq 1 100); do
  if python3 -c "import socket,sys; s=socket.socket(); s.settimeout(0.1); sys.exit(0 if s.connect_ex(('127.0.0.1',$port))==0 else 1)"; then
    break
  fi
  sleep 0.05
done
curl -sS -o "$work/http.body" -H "authorization: Bearer wrong-token" \
  -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  "http://127.0.0.1:$port/mcp" >/dev/null 2>&1 || true
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
check "HTTP stdout" "$work/http.out"
check "HTTP stderr" "$work/http.err"
check "HTTP auth-failure body" "$work/http.body"

if [ "$leaks" -ne 0 ]; then
  echo "FAIL: $leaks credential leak(s)" >&2
  exit 1
fi
echo "PASS: no credential in tool output, error text, or logs"
