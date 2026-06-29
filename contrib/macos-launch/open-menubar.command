#!/bin/bash
# Launch Codex Account Switcher as a menu-bar agent (no Dock icon).
# Double-click in Finder, or: open contrib/macos-launch/open-menubar.command

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="$HOME/Library/Application Support/com.nextide.codex-account-switcher"
LOG_FILE="$LOG_DIR/tray.log"
APP_BUNDLE="$PROJECT_DIR/target/release/Codex Account Switcher.app"
BINARY="$PROJECT_DIR/target/release/codex-account-switcher"

mkdir -p "$LOG_DIR"

pkill -f 'codex-account-switcher' 2>/dev/null || true
sleep 0.5

if [[ -x "$APP_BUNDLE/Contents/MacOS/codex-account-switcher" ]]; then
  echo "Launching app bundle: $APP_BUNDLE"
  nohup "$APP_BUNDLE/Contents/MacOS/codex-account-switcher" </dev/null >>"$LOG_FILE" 2>&1 &
elif [[ -x "$BINARY" ]]; then
  echo "Launching binary: $BINARY"
  nohup "$BINARY" </dev/null >>"$LOG_FILE" 2>&1 &
else
  echo "Build first: cargo build --release && ./scripts/build_macos_app.sh" >&2
  exit 1
fi

sleep 2
if pgrep -f 'codex-account-switcher' >/dev/null; then
  echo "Running. Check menu bar (Control Center → Menu Bar if hidden)."
  echo "Log: $LOG_FILE"
  tail -n 5 "$LOG_FILE" 2>/dev/null || true
else
  echo "Process did not stay running. See log: $LOG_FILE" >&2
  tail -n 20 "$LOG_FILE" 2>/dev/null || true
  exit 1
fi
