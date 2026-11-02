#!/usr/bin/env bash
# Patch server.json with release .mcpb SHA-256 for MCP Registry publish.
set -euo pipefail

VERSION="${1:?usage: $0 VERSION (e.g. 1.0.4)}"
REPO="${REPO:-rzlco666/strut-stack-sql}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SERVER_JSON="${ROOT}/server.json"
TMP="$(mktemp -d)"

cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

if ! command -v jq >/dev/null 2>&1; then
  echo "jq is required" >&2
  exit 1
fi

gh release download "v${VERSION}" --repo "$REPO" -p "SHA256SUMS" -D "$TMP"
hash_for() {
  awk -v f="$1" '$2 == f { print $1; exit }' "${TMP}/SHA256SUMS"
}

BASE="https://github.com/${REPO}/releases/download/v${VERSION}"
jq \
  --arg version "$VERSION" \
  --arg repo_url "https://github.com/${REPO}" \
  --arg linux_x64 "${BASE}/strut-stack-sql-linux-x64.mcpb" \
  --arg linux_x64_h "$(hash_for strut-stack-sql-linux-x64.mcpb)" \
  --arg macos_arm "${BASE}/strut-stack-sql-macos-arm64.mcpb" \
  --arg macos_arm_h "$(hash_for strut-stack-sql-macos-arm64.mcpb)" \
  --arg macos_x64 "${BASE}/strut-stack-sql-macos-x64.mcpb" \
  --arg macos_x64_h "$(hash_for strut-stack-sql-macos-x64.mcpb)" \
  --arg linux_arm "${BASE}/strut-stack-sql-linux-arm64.mcpb" \
  --arg linux_arm_h "$(hash_for strut-stack-sql-linux-arm64.mcpb)" \
  --arg win_x64 "${BASE}/strut-stack-sql-windows-x64.mcpb" \
  --arg win_x64_h "$(hash_for strut-stack-sql-windows-x64.mcpb)" \
  '
  .version = $version
  | .name = "io.github.rzlco666/strut-stack-sql"
  | .repository.url = $repo_url
  | .packages[0].identifier = $linux_x64
  | .packages[0].fileSha256 = $linux_x64_h
  | .packages[1].identifier = $macos_arm
  | .packages[1].fileSha256 = $macos_arm_h
  | .packages[2].identifier = $macos_x64
  | .packages[2].fileSha256 = $macos_x64_h
  | .packages[3].identifier = $linux_arm
  | .packages[3].fileSha256 = $linux_arm_h
  | .packages[4].identifier = $win_x64
  | .packages[4].fileSha256 = $win_x64_h
  ' \
  "$SERVER_JSON" > "${SERVER_JSON}.tmp"

mv "${SERVER_JSON}.tmp" "$SERVER_JSON"
echo "Updated server.json → v${VERSION}"
jq '{name, version, n_packages: (.packages|length)}' "$SERVER_JSON"
