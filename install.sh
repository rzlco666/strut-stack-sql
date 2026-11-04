#!/usr/bin/env bash
# Install strut-stack-sql from GitHub Releases.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash
#   curl -fsSL ... | bash -s -- --version 1.0.0 --prefix ~/.local
set -euo pipefail

REPO="${REPO:-rzlco666/strut-stack-sql}"
PREFIX="${PREFIX:-${HOME}/.local}"
VERSION=""
VERIFY=1

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Options:
  --version VER   Install specific release (default: latest)
  --prefix DIR    Install prefix (default: ~/.local) → $PREFIX/bin
  --no-verify     Skip SHA256SUMS verification
  -h, --help      Show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    --no-verify) VERIFY=0; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  FAMILY="linux" ;;
  Darwin) FAMILY="macos" ;;
  MINGW*|MSYS*|CYGWIN*)
    echo "On Windows, use Scoop or winget. See docs/INSTALL.md" >&2
    exit 1
    ;;
  *)
    echo "Unsupported OS: $OS" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64)  TARGET_ARCH="x64" ;;
  aarch64|arm64) TARGET_ARCH="arm64" ;;
  *)
    echo "Unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

ARTIFACT="strut-stack-sql-${FAMILY}-${TARGET_ARCH}.tar.gz"

if [ -z "$VERSION" ]; then
  VERSION="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | sed -n 's/.*"tag_name": *"v\([^"]*\)".*/\1/p' | head -1)"
  if [ -z "$VERSION" ]; then
    echo "Failed to resolve latest release version" >&2
    exit 1
  fi
fi

BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "Installing strut-stack-sql v${VERSION} (${FAMILY}-${TARGET_ARCH}) to ${PREFIX}/bin"

curl -fsSL "${BASE_URL}/${ARTIFACT}" -o "${TMPDIR}/${ARTIFACT}"

if [ "$VERIFY" = "1" ]; then
  curl -fsSL "${BASE_URL}/SHA256SUMS" -o "${TMPDIR}/SHA256SUMS"
  (
    cd "$TMPDIR"
    if command -v sha256sum >/dev/null 2>&1; then
      sha256sum -c --ignore-missing SHA256SUMS
    elif command -v shasum >/dev/null 2>&1; then
      grep " ${ARTIFACT}\$" SHA256SUMS | shasum -a 256 -c -
    else
      echo "warning: no sha256 tool; skipping verify" >&2
    fi
  )
fi

tar -xzf "${TMPDIR}/${ARTIFACT}" -C "$TMPDIR"
mkdir -p "${PREFIX}/bin"
install -m 755 "${TMPDIR}/strut-stack-sql" "${PREFIX}/bin/strut-stack-sql"

if ! echo ":$PATH:" | grep -q ":${PREFIX}/bin:"; then
  echo ""
  echo "Add to your shell profile:"
  echo "  export PATH=\"${PREFIX}/bin:\$PATH\""
fi

echo "OK → $(${PREFIX}/bin/strut-stack-sql --version)"
