#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
VENV="$SCRIPT_DIR/.venv"
PLIST=~/Library/LaunchAgents/com.codex-switcher.agent.plist

echo "=== Codex Account Switcher Setup ==="

# 1. venv
if [ ! -d "$VENV" ]; then
    echo "→ Creating venv..."
    python3 -m venv "$VENV"
fi

echo "→ Installing deps..."
"$VENV/bin/pip" install -q -r "$SCRIPT_DIR/requirements.txt"

# 2. LaunchAgent (auto-start on login, run in menu bar)
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
 "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.codex-switcher.agent</string>
    <key>ProgramArguments</key>
    <array>
        <string>${VENV}/bin/python</string>
        <string>${SCRIPT_DIR}/codex_switcher.py</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>StandardOutPath</key>
    <string>/tmp/codex-switcher.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/codex-switcher.err</string>
</dict>
</plist>
EOF

# 3. Load agent
launchctl bootout gui/$(id -u) "$PLIST" 2>/dev/null || true
launchctl bootstrap gui/$(id -u) "$PLIST"

echo ""
echo "✅ Done! Look for ⚡ in your menu bar."
echo "   Logs: /tmp/codex-switcher.log"
