#!/usr/bin/env bash
# Update the Scoop and winget manifests to a released version, filling the
# Windows zip sha256 from that release's SHA256SUMS.
#
# Usage: scripts/bump-windows-manifests.sh VERSION     (e.g. 2.0.0)
#
# Artifact name follows .github/workflows/release.yml:
#   strut-stack-sql-windows-x64.zip
# In the manifests the hash placeholder is the literal UPDATE_AFTER_RELEASE;
# this script replaces it (or a previous release's hash) with the real digest.
set -euo pipefail

VERSION="${1:?usage: $0 VERSION (e.g. 2.0.0)}"
REPO="${REPO:-rzlco666/strut-stack-sql}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SUMS_URL="https://github.com/${REPO}/releases/download/v${VERSION}/SHA256SUMS"
ARTIFACT="strut-stack-sql-windows-x64.zip"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

curl -fsSL "$SUMS_URL" -o "$TMP"
HASH="$(awk -v f="$ARTIFACT" '$2 == f { print $1; exit }' "$TMP")"
if [ -z "$HASH" ]; then
  echo "Hash not found for ${ARTIFACT} in ${SUMS_URL}" >&2
  exit 1
fi

SCOOPS=("${ROOT}/packaging/scoop/strut-stack-sql.json")
WINGETS=("${ROOT}/packaging/winget/rzlco666.strut-stack-sql.yaml")

for SCOOP in "${SCOOPS[@]}"; do
  # The URL rewrite must not touch the autoupdate block: its URL template
  # uses Scoop's $version placeholder on purpose. So: rewrite download URLs
  # only in the part of the file before "autoupdate".
  sed -i \
    -e "s/\"version\": \"[^\"]*\"/\"version\": \"${VERSION}\"/" \
    -e "s/strut-stack-sql-x86_64-pc-windows-msvc\.zip/${ARTIFACT}/g" \
    -e "s/\"hash\": \"[^\"]*\"/\"hash\": \"${HASH}\"/" \
    "$SCOOP"
  sed -i "/\"autoupdate\"/,\$!s|releases/download/v[^/]*/|releases/download/v${VERSION}/|g" "$SCOOP"
  echo "Updated ${SCOOP} for v${VERSION}"
done

for WINGET in "${WINGETS[@]}"; do
  sed -i \
    -e "s/^PackageVersion: .*/PackageVersion: ${VERSION}/" \
    -e "s|releases/download/v[^/]*/|releases/download/v${VERSION}/|g" \
    -e "s/strut-stack-sql-x86_64-pc-windows-msvc\.zip/${ARTIFACT}/g" \
    -e "s/^    InstallerSha256: .*/    InstallerSha256: ${HASH}/" \
    "$WINGET"
  echo "Updated ${WINGET} for v${VERSION}"
done

for f in "${SCOOPS[@]}" "${WINGETS[@]}"; do
  if grep -q "UPDATE_AFTER_RELEASE" "$f"; then
    echo "WARNING: UPDATE_AFTER_RELEASE placeholder remains in $f" >&2
  fi
done
