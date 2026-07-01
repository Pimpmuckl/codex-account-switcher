#!/bin/bash
# Launch Codex Account Switcher as a menu-bar agent (no Dock icon).
# Double-click in Finder, or: open contrib/macos-launch/open-menubar.command

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="$HOME/Library/Application Support/com.nextide.codex-account-switcher"
LOG_FILE="$LOG_DIR/tray.log"
LOCK_FILE="$LOG_DIR/tray.lock"
INSTALL_BIN="$HOME/.local/bin/codex-account-switcher-menubar"
APP_BUNDLE="$PROJECT_DIR/target/release/Codex Account Switcher.app"
SOURCE_BIN=""

mkdir -p "$LOG_DIR" "$(dirname "$INSTALL_BIN")"

# Prefer a freshly built cargo binary over an older packaged .app bundle.
if [[ -x "$PROJECT_DIR/target/release/codex-account-switcher" ]]; then
  SOURCE_BIN="$PROJECT_DIR/target/release/codex-account-switcher"
elif [[ -x "$APP_BUNDLE/Contents/MacOS/codex-account-switcher" ]]; then
  SOURCE_BIN="$APP_BUNDLE/Contents/MacOS/codex-account-switcher"
else
  echo "Build first: cargo build --release && ./scripts/build_macos_app.sh" >&2
  exit 1
fi

install -m 755 "$SOURCE_BIN" "$INSTALL_BIN"
echo "Installed menubar binary from: $SOURCE_BIN"

OLD_PID=""
if [[ -f "$LOCK_FILE" ]]; then
  OLD_PID="$(tr -d '[:space:]' <"$LOCK_FILE" 2>/dev/null || true)"
fi

if [[ -n "$OLD_PID" ]] && kill -0 "$OLD_PID" 2>/dev/null; then
  echo "Restarting tray (pid=$OLD_PID) to load updated binary..."
  kill "$OLD_PID" 2>/dev/null || true
  for _ in {1..10}; do
    if ! kill -0 "$OLD_PID" 2>/dev/null; then
      break
    fi
    sleep 0.2
  done
fi

echo "Launching installed menubar binary: $INSTALL_BIN"
nohup "$INSTALL_BIN" </dev/null >>"$LOG_FILE" 2>&1 &

sleep 2
if pgrep -f 'codex-account-switcher-menubar' >/dev/null || pgrep -f 'codex-account-switcher' >/dev/null; then
  echo "Running. Check menu bar (Control Center → Menu Bar if hidden)."
  echo "Log: $LOG_FILE"
  tail -n 5 "$LOG_FILE" 2>/dev/null || true
else
  echo "Process did not stay running. See log: $LOG_FILE" >&2
  tail -n 20 "$LOG_FILE" 2>/dev/null || true
  exit 1
fi
