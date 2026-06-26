#!/bin/bash
# Re-run Codex Switcher in background and detach it from the terminal
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
pkill -f codex_switcher.py 2>/dev/null
sleep 0.5
osascript -e "do shell script \"${SCRIPT_DIR}/.venv/bin/python ${SCRIPT_DIR}/codex_switcher.py >/dev/null 2>&1 &\""
osascript -e 'tell application "Terminal" to close first window' &
exit
