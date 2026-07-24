#!/usr/bin/env bash
# Build release binary and install into PREFIX (default: ~/.local) and CARGO_HOME/bin.
# Repo `cargo build --release` alone does NOT update ~/.local/bin — run this after local changes
# so Cursor MCP (STRUT_STACK_SQL_BIN / MCP_SQL_RUST_BIN) and manual CLI stay aligned.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="${PREFIX}/bin"
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
NAME="strut-stack-sql"
SRC="${ROOT}/target/release/${NAME}"

install_pair() {
  local dest="$1"
  mkdir -p "$dest"
  install -m 755 "$SRC" "${dest}/${NAME}"
  ln -sfn "${dest}/${NAME}" "${dest}/mcp-sql-rust"
  echo "OK → ${dest}/${NAME}"
}

cd "$ROOT"
cargo build --release

install_pair "$BIN_DIR"
if [[ -d "$(dirname "$CARGO_BIN")" ]]; then
  install_pair "$CARGO_BIN"
fi

"${BIN_DIR}/${NAME}" --version
if command -v md5sum >/dev/null 2>&1; then
  md5sum "${BIN_DIR}/${NAME}"
elif command -v md5 >/dev/null 2>&1; then
  md5 -q "${BIN_DIR}/${NAME}"
fi
