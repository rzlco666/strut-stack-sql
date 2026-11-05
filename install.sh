#!/usr/bin/env bash
# Install strut-stack-sql from GitHub Releases, and optionally wire it into
# your AI tools with a single command.
#
# Install only:
#   curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash
#
# Install + set up every AI tool found on this machine:
#   curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash -s -- --setup
#
# Install + set up, with your database URL baked in:
#   curl -fsSL https://raw.githubusercontent.com/rzlco666/strut-stack-sql/main/install.sh | bash -s -- --setup --database-url 'postgresql://user:pass@localhost:5432/mydb'
#
# Set up only (binary already installed):
#   install.sh --setup --skip-install
#
# Project-local configs instead of global ones (writes .cursor/mcp.json,
# .vscode/mcp.json, .mcp.json, opencode.json, .codex/config.toml in $PWD):
#   install.sh --setup --project
set -euo pipefail

REPO="${REPO:-rzlco666/strut-stack-sql}"
PREFIX="${PREFIX:-${HOME}/.local}"
VERSION=""
VERIFY=1
SETUP=0
SETUP_ALL=0
PROJECT=0
SKIP_INSTALL=0
BIN_OVERRIDE=""
DB_URL="${DATABASE_URL:-}"
TOOLS_ARG=""

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Install options:
  --version VER     Install specific release (default: latest)
  --prefix DIR      Install prefix (default: ~/.local) → $PREFIX/bin
  --no-verify       Skip SHA256SUMS verification
  --bin PATH        Use this existing binary instead of downloading one
  --skip-install    Do not download; only run --setup against an installed binary

Setup options (configure AI tools to use the installed server):
  --setup           Configure every supported AI tool detected on this machine
  --setup-all       Configure every supported AI tool, detected or not
  --tools LIST      Comma list to configure (default: detected):
                      claude-code,cursor,vscode,claude-desktop,codex,
                      opencode,windsurf,zed
  --database-url U  DATABASE_URL to store in tool configs (else $DATABASE_URL,
                    else an interactive prompt, else ${env:DATABASE_URL} where
                    the tool supports it)
  --db U            Alias for --database-url
  --project         Write project-local configs in the current directory
                    instead of global ones
  -h, --help        Show this help

Supported tools: Claude Code, Cursor, VS Code, Claude Desktop, Codex,
OpenCode, Windsurf, Zed. Configs are backed up before they are changed,
existing entries for other servers are kept, and nothing is ever sent
anywhere: everything happens on your machine.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    --no-verify) VERIFY=0; shift ;;
    --bin) BIN_OVERRIDE="$2"; shift 2 ;;
    --skip-install) SKIP_INSTALL=1; shift ;;
    --setup) SETUP=1; shift ;;
    --setup-all) SETUP=1; SETUP_ALL=1; shift ;;
    --tools) TOOLS_ARG="$2"; shift 2 ;;
    --database-url|--db) DB_URL="$2"; shift 2 ;;
    --project) PROJECT=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

if [ "$SKIP_INSTALL" = "1" ] && [ "$SETUP" = "0" ]; then
  echo "--skip-install only makes sense together with --setup" >&2
  exit 1
fi

# ---------------------------------------------------------------- install ---

BIN=""
if [ -n "$BIN_OVERRIDE" ]; then
  BIN="$BIN_OVERRIDE"
elif [ "$SKIP_INSTALL" = "1" ]; then
  BIN="$(command -v strut-stack-sql || true)"
  if [ -z "$BIN" ]; then
    echo "strut-stack-sql not found on PATH; install it first or pass --bin PATH" >&2
    exit 1
  fi
else
  OS="$(uname -s)"
  ARCH="$(uname -m)"

  case "$OS" in
    Linux)  FAMILY="linux" ;;
    Darwin) FAMILY="macos" ;;
    MINGW*|MSYS*|CYGWIN*)
      echo "On Windows, use Scoop or winget (see docs/INSTALL.md), then run this script with --setup --skip-install from Git Bash." >&2
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
  BIN="${PREFIX}/bin/strut-stack-sql"

  if ! echo ":$PATH:" | grep -q ":${PREFIX}/bin:"; then
    echo ""
    echo "Add to your shell profile:"
    echo "  export PATH=\"${PREFIX}/bin:\$PATH\""
  fi

  echo "OK → $("$BIN" --version)"
fi

# ------------------------------------------------------------------ setup ---
# Everything below only runs with --setup. It writes config files for AI
# tools; it never uploads anything.

if [ "$SETUP" = "0" ]; then
  exit 0
fi

# Resolve the database URL: flag > environment > interactive prompt (only
# when a real terminal is attached; curl|bash has no usable stdin) > empty.
# NOTE: /dev/tty can pass a readability test yet fail to open (no controlling
# terminal), so the prompt must never be fatal under `set -e`.
if [ -z "$DB_URL" ]; then
  if printf "DATABASE_URL (Enter to keep it as \${env:DATABASE_URL} where supported): " > /dev/tty 2>/dev/null && read -r DB_URL < /dev/tty 2>/dev/null; then
    :
  else
    DB_URL=""
  fi
fi

HAVE_PY=0
command -v python3 >/dev/null 2>&1 && HAVE_PY=1

# Absolute binary path: tool configs must not depend on the user's PATH.
case "$BIN" in
  /*) ;;
  *) BIN="$(command -v "$BIN" || echo "$BIN")" ;;
esac

SERVER_ARGS='["--url-env", "DATABASE_URL", "--full-tools"]'

# --- config file locations (global unless --project) -----------------------
F_DESKTOP=""
F_WINDSURF_NEW=""
F_WINDSURF_OLD=""
F_ZED=""
if [ "$PROJECT" = "1" ]; then
  F_CURSOR="$PWD/.cursor/mcp.json"
  F_VSCODE="$PWD/.vscode/mcp.json"
  F_CLAUDE="$PWD/.mcp.json"
  F_OPENCODE="$PWD/opencode.json"
  F_CODEX="$PWD/.codex/config.toml"
else
  F_CURSOR="${HOME}/.cursor/mcp.json"
  case "$(uname -s)" in
    Darwin)
      F_VSCODE="${HOME}/Library/Application Support/Code/User/mcp.json"
      F_DESKTOP="${HOME}/Library/Application Support/Claude/claude_desktop_config.json"
      ;;
    *)  F_VSCODE="${HOME}/.config/Code/User/mcp.json"
      F_DESKTOP="${HOME}/.config/Claude/claude_desktop_config.json"
      ;;
  esac
  F_CLAUDE="${HOME}/.claude.json"
  F_OPENCODE="${HOME}/.config/opencode/opencode.json"
  F_CODEX="${HOME}/.codex/config.toml"
  F_WINDSURF_NEW="${HOME}/.config/devin/mcp_config.json"
  F_WINDSURF_OLD="${HOME}/.codeium/windsurf/mcp_config.json"
  F_ZED="${HOME}/.config/zed/settings.json"
fi

detect() { # tool -> 0 if detected
  case "$1" in
    claude-code) command -v claude >/dev/null 2>&1 || [ -f "$F_CLAUDE" ] ;;
    cursor) command -v cursor >/dev/null 2>&1 || [ -f "$F_CURSOR" ] || [ -d "${HOME}/.cursor" ] ;;
    vscode) command -v code >/dev/null 2>&1 || [ -f "$F_VSCODE" ] ;;
    claude-desktop) [ -n "$F_DESKTOP" ] && [ -f "$F_DESKTOP" ] || [ -d "${HOME}/.config/Claude" ] || [ -d "${HOME}/Library/Application Support/Claude" ] ;;
    codex) command -v codex >/dev/null 2>&1 || [ -f "$F_CODEX" ] || [ -d "${HOME}/.codex" ] ;;
    opencode) command -v opencode >/dev/null 2>&1 || [ -f "$F_OPENCODE" ] ;;
    windsurf) [ -n "$F_WINDSURF_NEW" ] && { [ -f "$F_WINDSURF_NEW" ] || [ -f "$F_WINDSURF_OLD" ]; } ;;
    zed) [ -n "$F_ZED" ] && [ -f "$F_ZED" ] ;;
  esac
}

ALL_TOOLS="claude-code cursor vscode claude-desktop codex opencode windsurf zed"

# Pick the tool list: explicit --tools > --setup-all > detected.
if [ -n "$TOOLS_ARG" ]; then
  WANT="$(echo "$TOOLS_ARG" | tr ',' ' ')"
elif [ "$SETUP_ALL" = "1" ]; then
  WANT="$ALL_TOOLS"
else
  WANT=""
  for t in $ALL_TOOLS; do
    if detect "$t"; then WANT="$WANT $t"; fi
  done
  WANT="$(echo "$WANT" | sed 's/^ //')"
fi

if [ -z "$WANT" ]; then
  echo ""
  echo "No supported AI tool detected. Install one, or re-run with --setup-all"
  echo "to write configs for every tool, or --tools <list> for specific ones."
  exit 0
fi

echo ""
echo "Setting up strut-stack-sql ($BIN) for: $WANT"

# --- JSON/TOML writer (python3) --------------------------------------------
# Takes a JSON spec as $1: {"writes": [{"kind": "json"|"toml", ...}]}.
# JSON writes merge one server entry under a root key, preserving everything
# else; TOML writes replace-or-append one [mcp_servers.<name>] section.
# Every touched file is backed up first (file.bak-YYYYMMDD-HHMMSS).
write_configs() {
  python3 - "$BIN" "$SERVER_ARGS" "$1" <<'PYEOF'
import json, os, shutil, sys, time

bin_path, server_args, spec = sys.argv[1], json.loads(sys.argv[2]), json.loads(sys.argv[3])

def backup(path):
    if os.path.exists(path):
        stamp = time.strftime("%Y%m%d-%H%M%S")
        shutil.copy2(path, path + ".bak-" + stamp)

def load_json(path):
    if os.path.exists(path):
        try:
            with open(path) as f:
                data = json.load(f)
            return data if isinstance(data, dict) else {}
        except (ValueError, OSError):
            return {}
    return {}

def dump_json(path, data):
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    backup(path)
    with open(path, "w") as f:
        json.dump(data, f, indent=2)
        f.write("\n")

for w in spec["writes"]:
    path = os.path.expandvars(w["file"])
    if w["kind"] == "json":
        data = load_json(path)
        container = data.setdefault(w["root"], {})
        container["strut-stack-sql"] = w["entry"]
        dump_json(path, data)
        print("  wrote " + path)
    elif w["kind"] == "toml":
        section = "[mcp_servers.strut-stack-sql]"
        lines = []
        if os.path.exists(path):
            with open(path) as f:
                lines = f.read().splitlines()
        out, i, replaced = [], 0, False
        while i < len(lines):
            if lines[i].strip() == section:
                out.extend(w["body"].splitlines())
                replaced = True
                i += 1
                while i < len(lines) and not lines[i].startswith("["):
                    i += 1
            else:
                out.append(lines[i])
                i += 1
        if not replaced:
            if out and out[-1].strip():
                out.append("")
            out.extend(w["body"].splitlines())
        parent = os.path.dirname(path)
        if parent:
            os.makedirs(parent, exist_ok=True)
        backup(path)
        with open(path, "w") as f:
            f.write("\n".join(out) + "\n")
        print("  wrote " + path)
PYEOF
}

# Build the per-tool entries. When no literal DATABASE_URL is available,
# each client gets its own documented fallback (env interpolation or
# forwarding) instead of a fake URL, and the summary warns about it.
json_entry() { # mcpServers-style stdio entry; $1 = env JSON object or ""
  python3 -c 'import json,sys; print(json.dumps({"type":"stdio","command":sys.argv[1],"args":json.loads(sys.argv[2]),"env":json.loads(sys.argv[3])} if sys.argv[3] else {"type":"stdio","command":sys.argv[1],"args":json.loads(sys.argv[2])}))' "$BIN" "$SERVER_ARGS" "$1"
}

WRITES='{"writes": []}'
add_write() { # json: kind file root entry-json | toml: kind file body
  WRITES="$(python3 -c '
import json, sys
spec = json.loads(sys.argv[1])
kind, path = sys.argv[2], sys.argv[3]
if kind == "json":
    spec["writes"].append({"kind": "json", "file": path, "root": sys.argv[4], "entry": json.loads(sys.argv[5])})
else:
    spec["writes"].append({"kind": "toml", "file": path, "body": sys.argv[4]})
print(json.dumps(spec))
' "$WRITES" "$@")"
}

NO_URL_NOTE=0
env_literal() { # -> env JSON object, or "" when no literal URL
  if [ -n "$DB_URL" ]; then python3 -c 'import json,sys; print(json.dumps({"DATABASE_URL": sys.argv[1]}))' "$DB_URL"; fi
}

for tool in $WANT; do
  case "$tool" in
    claude-code)
      # Prefer the official CLI; fall back to ~/.claude.json / .mcp.json.
      if [ "$PROJECT" = "0" ] && command -v claude >/dev/null 2>&1 && [ -n "$DB_URL" ]; then
        if claude mcp add --scope user --transport stdio --env "DATABASE_URL=$DB_URL" strut-stack-sql -- "$BIN" --url-env DATABASE_URL --full-tools 2>/dev/null; then
          echo "  claude-code: added via 'claude mcp add'"
          continue
        fi
        echo "  claude-code: CLI add failed, falling back to config file"
      fi
      if [ "$HAVE_PY" = "1" ]; then
        if [ -n "$DB_URL" ]; then
          add_write json "$F_CLAUDE" "mcpServers" "$(json_entry "$(env_literal)")"
        else
          # Claude Code expands ${VAR} in .mcp.json / user config.
          add_write json "$F_CLAUDE" "mcpServers" "$(python3 -c 'import json,sys; print(json.dumps({"type":"stdio","command":sys.argv[1],"args":json.loads(sys.argv[2]),"env":{"DATABASE_URL":"${DATABASE_URL}"}}))' "$BIN" "$SERVER_ARGS")"
          NO_URL_NOTE=1
        fi
      fi
      ;;
    cursor)
      if [ -n "$DB_URL" ]; then ENV_J="$(env_literal)"; else ENV_J='{"DATABASE_URL": "${env:DATABASE_URL}"}'; NO_URL_NOTE=1; fi
      [ "$HAVE_PY" = "1" ] && add_write json "$F_CURSOR" "mcpServers" "$(json_entry "$ENV_J")"
      ;;
    vscode)
      if [ -n "$DB_URL" ]; then ENV_J="$(env_literal)"; else ENV_J='{"DATABASE_URL": "${env:DATABASE_URL}"}'; NO_URL_NOTE=1; fi
      [ "$HAVE_PY" = "1" ] && add_write json "$F_VSCODE" "servers" "$(json_entry "$ENV_J")"
      ;;
    claude-desktop)
      if [ "$PROJECT" = "1" ]; then
        echo "  claude-desktop: no project-local config; skipped (use global setup)"
        continue
      fi
      if [ "$HAVE_PY" = "1" ]; then
        if [ -n "$DB_URL" ]; then
          add_write json "$F_DESKTOP" "mcpServers" "$(python3 -c 'import json,sys; print(json.dumps({"command":sys.argv[1],"args":json.loads(sys.argv[2]),"env":json.loads(sys.argv[3])}))' "$BIN" "$SERVER_ARGS" "$(env_literal)")"
        else
          add_write json "$F_DESKTOP" "mcpServers" "$(python3 -c 'import json,sys; print(json.dumps({"command":sys.argv[1],"args":json.loads(sys.argv[2])}))' "$BIN" "$SERVER_ARGS")"
          NO_URL_NOTE=1
        fi
      fi
      ;;
    codex)
      if [ "$HAVE_PY" = "1" ]; then
        if [ -n "$DB_URL" ]; then
          BODY="$(python3 -c '
import json, sys
db = sys.argv[1]; b = sys.argv[2]; args = json.loads(sys.argv[3])
print("[mcp_servers.strut-stack-sql]")
print("command = " + json.dumps(b))
print("args = " + json.dumps(args))
print("env = { DATABASE_URL = " + json.dumps(db) + " }")
print("startup_timeout_sec = 20")
' "$DB_URL" "$BIN" "$SERVER_ARGS")"
        else
          BODY="$(python3 -c '
import json, sys
b = sys.argv[1]; args = json.loads(sys.argv[2])
print("[mcp_servers.strut-stack-sql]")
print("command = " + json.dumps(b))
print("args = " + json.dumps(args))
print("env_vars = [\"DATABASE_URL\"]")
print("startup_timeout_sec = 20")
' "$BIN" "$SERVER_ARGS")"
          NO_URL_NOTE=1
        fi
        add_write toml "$F_CODEX" "$BODY"
      fi
      ;;
    opencode)
      # Prefer the official CLI; fall back to opencode.json.
      if command -v opencode >/dev/null 2>&1 && [ -n "$DB_URL" ] && [ "$PROJECT" = "0" ]; then
        if opencode mcp add strut-stack-sql --env "DATABASE_URL=$DB_URL" -- "$BIN" --url-env DATABASE_URL --full-tools 2>/dev/null; then
          echo "  opencode: added via 'opencode mcp add'"
          continue
        fi
        echo "  opencode: CLI add failed, falling back to config file"
      fi
      if [ -n "$DB_URL" ]; then ENV_J="$(env_literal)"; else ENV_J='{"DATABASE_URL": "{env:DATABASE_URL}"}'; NO_URL_NOTE=1; fi
      if [ "$HAVE_PY" = "1" ]; then
        # Repo-checked shape: mcp.<name> with type local + command array.
        # (OpenCode v2 also accepts mcp.servers.<name>; verify with
        # `opencode mcp list` if the server does not appear.)
        ENTRY="$(python3 -c 'import json,sys; print(json.dumps({"type":"local","command":[sys.argv[1]]+json.loads(sys.argv[2]),"enabled":True,"environment":json.loads(sys.argv[3]),"timeout":30000}))' "$BIN" "$SERVER_ARGS" "$ENV_J")"
        add_write json "$F_OPENCODE" "mcp" "$ENTRY"
      fi
      ;;
    windsurf)
      if [ "$PROJECT" = "1" ]; then
        echo "  windsurf: no project-local config; skipped (use global setup)"
        continue
      fi
      if [ -n "$DB_URL" ]; then ENV_J="$(env_literal)"; else ENV_J='{"DATABASE_URL": "${env:DATABASE_URL}"}'; NO_URL_NOTE=1; fi
      if [ -f "$F_WINDSURF_NEW" ]; then F_W="$F_WINDSURF_NEW";
      elif [ -f "$F_WINDSURF_OLD" ]; then F_W="$F_WINDSURF_OLD";
      else F_W="$F_WINDSURF_NEW"; fi
      if [ "$HAVE_PY" = "1" ]; then
        add_write json "$F_W" "mcpServers" "$(python3 -c 'import json,sys; print(json.dumps({"command":sys.argv[1],"args":json.loads(sys.argv[2]),"env":json.loads(sys.argv[3])} if sys.argv[3] else {"command":sys.argv[1],"args":json.loads(sys.argv[2])}))' "$BIN" "$SERVER_ARGS" "$ENV_J")"
      fi
      ;;
    zed)
      if [ "$PROJECT" = "1" ]; then
        echo "  zed: no project-local config; skipped (use global setup)"
        continue
      fi
      if [ "$HAVE_PY" = "1" ]; then
        if [ -n "$DB_URL" ]; then
          add_write json "$F_ZED" "context_servers" "$(python3 -c 'import json,sys; print(json.dumps({"command":sys.argv[1],"args":json.loads(sys.argv[2]),"env":json.loads(sys.argv[3])}))' "$BIN" "$SERVER_ARGS" "$(env_literal)")"
        else
          add_write json "$F_ZED" "context_servers" "$(python3 -c 'import json,sys; print(json.dumps({"command":sys.argv[1],"args":json.loads(sys.argv[2])}))' "$BIN" "$SERVER_ARGS")"
          NO_URL_NOTE=1
        fi
      fi
      ;;
    *)
      echo "  unknown tool: $tool (skipped)" >&2
      ;;
  esac
done

if [ "$HAVE_PY" = "1" ]; then
  write_configs "$WRITES"
else
  echo ""
  echo "python3 was not found, so config files were NOT written automatically."
  echo "Install python3 and re-run with --setup, or paste this block manually"
  echo "(replace DATABASE_URL with your real URL):"
  echo ""
  echo "{"
  echo "  \"mcpServers\": {"
  echo "    \"strut-stack-sql\": {"
  echo "      \"command\": \"$BIN\","
  echo "      \"args\": [\"--url-env\", \"DATABASE_URL\", \"--full-tools\"],"
  echo "      \"env\": { \"DATABASE_URL\": \"${DB_URL:-postgresql://user:pass@localhost:5432/mydb}\" }"
  echo "    }"
  echo "  }"
  echo "}"
  echo ""
  echo "Per-tool file locations and formats: docs/CLIENTS.md"
fi

echo ""
echo "Setup done. Restart your AI tool, then try:"
echo "  \"List the tables in my database, then show me the 5 most recent orders.\""
if [ "$NO_URL_NOTE" = "1" ]; then
  echo ""
  echo "Note: no DATABASE_URL was given, so some configs reference"
  echo "\${env:DATABASE_URL} (or forward it, for Codex). Export it before"
  echo "starting your AI tool, or re-run with --database-url '<url>'."
fi
echo ""
echo "Verify the server itself with: $BIN --help"
