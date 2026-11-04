#!/usr/bin/env bash
# Update the Homebrew formulas to a released version, filling sha256 values
# from that release's SHA256SUMS.
#
# Usage: scripts/bump-homebrew-formula.sh VERSION     (e.g. 2.0.0)
#
# Artifact names follow .github/workflows/release.yml:
#   strut-stack-sql-{macos-arm64,macos-x64,linux-arm64,linux-x64}.tar.gz
# In the manifest the sha256 placeholder is the literal
# UPDATE_AFTER_RELEASE; this script replaces it (or a previous release's
# hash) with the real digest, per artifact.
set -euo pipefail

VERSION="${1:?usage: $0 VERSION (e.g. 2.0.0)}"
REPO="${REPO:-rzlco666/strut-stack-sql}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FORMULAS=("${ROOT}/packaging/homebrew/strut-stack-sql.rb")
SUMS_URL="https://github.com/${REPO}/releases/download/v${VERSION}/SHA256SUMS"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

curl -fsSL "$SUMS_URL" -o "$TMP"

hash_for() {
  awk -v f="$1" '$2 == f { print $1; exit }' "$TMP"
}

MACOS_ARM="strut-stack-sql-macos-arm64.tar.gz"
MACOS_X64="strut-stack-sql-macos-x64.tar.gz"
LINUX_ARM="strut-stack-sql-linux-arm64.tar.gz"
LINUX_X64="strut-stack-sql-linux-x64.tar.gz"

for artifact in "$MACOS_ARM" "$MACOS_X64" "$LINUX_ARM" "$LINUX_X64"; do
  if [ -z "$(hash_for "$artifact")" ]; then
    echo "Missing hash for ${artifact} in ${SUMS_URL}" >&2
    exit 1
  fi
done

# Replace the sha256 on the first sha256 line following the URL of $artifact.
set_hash_after_url() {
  local file="$1" artifact="$2" hash="$3"
  local url_line sha_line
  url_line="$(grep -n "$artifact" "$file" | head -1 | cut -d: -f1)"
  if [ -z "$url_line" ]; then
    echo "Artifact URL not found in $file: $artifact" >&2
    exit 1
  fi
  sha_line="$(awk -v start="$url_line" 'NR > start && /sha256/ { print NR; exit }' "$file")"
  sed -i "${sha_line}s/sha256 \"[^\"]*\"/sha256 \"${hash}\"/" "$file"
}

for FORMULA in "${FORMULAS[@]}"; do
  # Version + download path, and normalize any legacy (pre-1.0) artifact
  # names to the current release scheme so old manifests self-heal.
  sed -i \
    -e "s/version \"[^\"]*\"/version \"${VERSION}\"/" \
    -e "s|releases/download/v[^/]*/|releases/download/v${VERSION}/|g" \
    -e "s/strut-stack-sql-aarch64-apple-darwin\.tar\.gz/${MACOS_ARM}/g" \
    -e "s/strut-stack-sql-x86_64-apple-darwin\.tar\.gz/${MACOS_X64}/g" \
    -e "s/strut-stack-sql-aarch64-unknown-linux-gnu\.tar\.gz/${LINUX_ARM}/g" \
    -e "s/strut-stack-sql-x86_64-unknown-linux-gnu\.tar\.gz/${LINUX_X64}/g" \
    "$FORMULA"

  set_hash_after_url "$FORMULA" "$MACOS_ARM" "$(hash_for "$MACOS_ARM")"
  set_hash_after_url "$FORMULA" "$MACOS_X64" "$(hash_for "$MACOS_X64")"
  set_hash_after_url "$FORMULA" "$LINUX_ARM" "$(hash_for "$LINUX_ARM")"
  set_hash_after_url "$FORMULA" "$LINUX_X64" "$(hash_for "$LINUX_X64")"

  if grep -q "UPDATE_AFTER_RELEASE" "$FORMULA"; then
    echo "WARNING: UPDATE_AFTER_RELEASE placeholder remains in $FORMULA" >&2
  fi
  echo "Updated ${FORMULA} for v${VERSION}"
done
