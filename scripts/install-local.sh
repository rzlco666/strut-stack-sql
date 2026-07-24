#!/usr/bin/env bash
# Build release binary and install into PREFIX (default: ~/.local).
# Repo `cargo build --release` alone does NOT update ~/.local/bin — run this after local changes
# so Cursor MCP (STRUT_STACK_SQL_BIN / MCP_SQL_RUST_BIN) picks up the new binary.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="${PREFIX}/bin"
NAME="strut-stack-sql"

cd "$ROOT"
cargo build --release

mkdir -p "$BIN_DIR"
install -m 755 "${ROOT}/target/release/${NAME}" "${BIN_DIR}/${NAME}"
ln -sfn "${BIN_DIR}/${NAME}" "${BIN_DIR}/mcp-sql-rust"

echo "OK → ${BIN_DIR}/${NAME}"
"${BIN_DIR}/${NAME}" --version
if command -v md5sum >/dev/null 2>&1; then
  md5sum "${BIN_DIR}/${NAME}"
elif command -v md5 >/dev/null 2>&1; then
  md5 -q "${BIN_DIR}/${NAME}"
fi
