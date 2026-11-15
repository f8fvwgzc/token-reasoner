#!/usr/bin/env bash
# Install lge on macOS: build it, put it on PATH, start the daemon, and
# optionally connect Codex and start lge at login. Safe to run again.
#
#   scripts/install-macos.sh                      build, install, start
#   scripts/install-macos.sh --connect-codex      also point Codex at lge
#   scripts/install-macos.sh --autostart          also start lge at login
#   scripts/install-macos.sh --uninstall          undo everything (keeps ~/.lge)
#
# Options:
#   --port N          port of the daemon (default 8787)
#   --workspace DIR   only folders under DIR may be indexed (default: any folder)
#   --force           replace an openai_base_url that points somewhere else
#   --purge           with --uninstall: also delete ~/.lge (graphs, usage log)
set -euo pipefail

PORT=8787
WORKSPACE=""
CONNECT=0
AUTOSTART=0
UNINSTALL=0
FORCE=0
PURGE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="$2"; shift 2 ;;
    --workspace) WORKSPACE="$2"; shift 2 ;;
    --connect-codex) CONNECT=1; shift ;;
    --autostart) AUTOSTART=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --force) FORCE=1; shift ;;
    --purge) PURGE=1; shift ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
  esac
done

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/lge"
DATA="$HOME/.lge"
CODEX_CONFIG="${CODEX_HOME:-$HOME/.codex}/config.toml"
BACKUP="$CODEX_CONFIG.before-lge"
PLIST="$HOME/Library/LaunchAgents/dev.lge.daemon.plist"
LABEL="dev.lge.daemon"
URL="http://127.0.0.1:$PORT"
LINE="openai_base_url = \"$URL/v1\""
MARK="# Model requests go through the local lge proxy. Remove this line to stop."

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
fail() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

stop_daemon() {
  if [ -f "$PLIST" ]; then
    launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  fi
  pkill -f "$BIN serve" 2>/dev/null || true
  pkill -f "target/release/lge serve" 2>/dev/null || true
}

remove_codex_line() {
  [ -f "$CODEX_CONFIG" ] || return 0
  # Remove only the two lines this script adds.
  /usr/bin/python3 - "$CODEX_CONFIG" "$MARK" <<'PY'
import sys, pathlib
path, mark = pathlib.Path(sys.argv[1]), sys.argv[2]
lines = path.read_text().splitlines(keepends=True)
out, skip_blank = [], False
for i, line in enumerate(lines):
    if line.rstrip("\n") == mark:
        continue
    if line.startswith("openai_base_url") and "127.0.0.1" in line and i > 0 and lines[i - 1].rstrip("\n") == mark:
        skip_blank = True
        continue
    if skip_blank and line.strip() == "":
        skip_blank = False
        continue
    skip_blank = False
    out.append(line)
path.write_text("".join(out))
PY
}

# ---- uninstall ----------------------------------------------------------------
if [ "$UNINSTALL" = 1 ]; then
  say "Stopping lge"
  stop_daemon
  rm -f "$PLIST"
  say "Removing the lge line from $CODEX_CONFIG"
  remove_codex_line
  if command -v codex >/dev/null 2>&1; then
    codex mcp remove local_graph >/dev/null 2>&1 || true
  fi
  rm -f "$BIN"
  if [ "$PURGE" = 1 ]; then
    say "Deleting $DATA"
    rm -rf "$DATA"
  fi
  say "Done. Codex talks to OpenAI directly again."
  [ -f "$BACKUP" ] && echo "    Your config from before lge is still at $BACKUP"
  exit 0
fi

# ---- prerequisites --------------------------------------------------------------
[ "$(uname -s)" = "Darwin" ] || fail "this script is for macOS; on Linux use Docker (see README)"

if ! xcode-select -p >/dev/null 2>&1; then
  fail "the Xcode Command Line Tools are missing. Run:  xcode-select --install  and run this script again"
fi

if ! command -v cargo >/dev/null 2>&1 && [ -x "$HOME/.cargo/bin/cargo" ]; then
  export PATH="$HOME/.cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null 2>&1; then
  say "Installing Rust with rustup (https://rustup.rs)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  export PATH="$HOME/.cargo/bin:$PATH"
fi
RUST_VERSION="$(rustc --version | awk '{print $2}')"
if [ "$(printf '%s\n1.88.0\n' "$RUST_VERSION" | sort -V | head -1)" != "1.88.0" ]; then
  say "Rust $RUST_VERSION is older than 1.88; updating"
  rustup update stable
fi

# LadybugDB links against OpenSSL. Homebrew keeps OpenSSL 3 outside the default
# search paths, so the build is told where it is.
if [ -z "${OPENSSL_DIR:-}" ]; then
  if command -v brew >/dev/null 2>&1; then
    if ! brew --prefix --installed openssl@3 >/dev/null 2>&1; then
      say "Installing OpenSSL 3 with Homebrew"
      brew install openssl@3
    fi
    OPENSSL_DIR="$(brew --prefix openssl@3)"
    export OPENSSL_DIR
  else
    fail "OpenSSL 3 is needed. Install Homebrew (https://brew.sh) and run this script again, or set OPENSSL_DIR"
  fi
fi

# ---- build and install ------------------------------------------------------------
say "Building lge (the first build takes a few minutes)"
(cd "$REPO" && cargo build --release -p lge-cli)
mkdir -p "$BIN_DIR" "$DATA"
stop_daemon
cp "$REPO/target/release/lge" "$BIN"
say "Installed $BIN"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "    $BIN_DIR is not on your PATH. Add this to ~/.zshrc:  export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac

say "Installing the full-text search extension (needs network once)"
"$BIN" --db "$DATA/setup/graph.lbug" setup || echo "    skipped: search falls back to substring matching"

# ---- start the daemon ---------------------------------------------------------------
ARGS=(serve --port "$PORT")
[ -n "$WORKSPACE" ] && ARGS+=(--workspace "$WORKSPACE")

if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  fail "port $PORT is in use by another program (Headroom?). Stop it, or run again with --port 8788"
fi

if [ "$AUTOSTART" = 1 ]; then
  say "Starting lge at login ($PLIST)"
  mkdir -p "$(dirname "$PLIST")"
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    echo '<plist version="1.0"><dict>'
    echo "  <key>Label</key><string>$LABEL</string>"
    echo '  <key>ProgramArguments</key><array>'
    echo "    <string>$BIN</string>"
    for a in "${ARGS[@]}"; do echo "    <string>$a</string>"; done
    echo '  </array>'
    echo "  <key>WorkingDirectory</key><string>$HOME</string>"
    echo '  <key>RunAtLoad</key><true/>'
    echo '  <key>KeepAlive</key><true/>'
    echo "  <key>StandardOutPath</key><string>$DATA/serve.log</string>"
    echo "  <key>StandardErrorPath</key><string>$DATA/serve.log</string>"
    echo '</dict></plist>'
  } > "$PLIST"
  launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true
  launchctl bootstrap "gui/$(id -u)" "$PLIST"
else
  say "Starting lge in the background (it stops at logout; use --autostart to keep it)"
  (cd "$HOME" && nohup "$BIN" "${ARGS[@]}" >> "$DATA/serve.log" 2>&1 &)
fi

for _ in $(seq 1 40); do
  curl -s -m 1 "$URL/health" >/dev/null 2>&1 && break
  sleep 0.25
done
curl -s -m 2 "$URL/health" | grep -q '"ok":true' || fail "lge did not start; see $DATA/serve.log"
say "lge is running: $URL"

# ---- connect Codex --------------------------------------------------------------------
if [ "$CONNECT" = 1 ]; then
  mkdir -p "$(dirname "$CODEX_CONFIG")"
  touch "$CODEX_CONFIG"
  if grep -q "^$LINE\$" "$CODEX_CONFIG"; then
    say "Codex already points at lge"
  else
    if grep -q '^openai_base_url' "$CODEX_CONFIG" && [ "$FORCE" != 1 ]; then
      fail "$CODEX_CONFIG already sets openai_base_url to something else (another proxy?). Remove it, or run again with --force"
    fi
    [ -f "$BACKUP" ] || cp -p "$CODEX_CONFIG" "$BACKUP"
    if [ "$FORCE" = 1 ]; then
      /usr/bin/sed -i '' '/^openai_base_url/d' "$CODEX_CONFIG"
    fi
    # A top-level key must come before the first [section] of the file.
    { printf '%s\n%s\n\n' "$MARK" "$LINE"; cat "$CODEX_CONFIG"; } > "$CODEX_CONFIG.lge-tmp"
    mv "$CODEX_CONFIG.lge-tmp" "$CODEX_CONFIG"
    say "Codex now sends model requests through lge (backup: $BACKUP)"
  fi
  if command -v codex >/dev/null 2>&1; then
    codex mcp add local_graph --url "$URL/mcp" >/dev/null 2>&1 && say "Registered the graph tools with Codex (local_graph)" || true
  fi
fi

cat <<EOF

Next:
  1. Open $URL  ->  add the folders of your projects (or just start Codex in a git repository).
  2. Use Codex as usual. Each session's input, cached, output and reasoning tokens show on the page.
  3. Log of every model request: $DATA/serve.log

Undo everything:  $REPO/scripts/install-macos.sh --uninstall
EOF
