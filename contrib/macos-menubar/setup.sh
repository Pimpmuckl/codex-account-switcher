#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
VENV="$SCRIPT_DIR/.venv"
PLIST=~/Library/LaunchAgents/com.codex-switcher.plist

echo "=== Codex Account Switcher Setup ==="

# 1. venv
if [ ! -d "$VENV" ]; then
    echo "→ Creating venv..."
    python3 -m venv "$VENV"
fi

echo "→ Installing deps..."
"$VENV/bin/pip" install -q -r "$SCRIPT_DIR/requirements.txt"

# 2. Create local launcher script to wait for mount
LAUNCHER_PATH=~/.local/bin/codex-switcher-launcher.sh
mkdir -p ~/.local/bin
cat > "$LAUNCHER_PATH" <<EOF
#!/bin/bash
# Wait for external volume to mount (max 30 seconds)
PY_PATH="${SCRIPT_DIR}/codex_switcher.py"
VENV_PATH="${VENV}/bin/python"
for i in {1..30}; do
    if [ -f "\$PY_PATH" ]; then
        pkill -f codex_switcher.py 2>/dev/null
        sleep 0.5
        exec "\$VENV_PATH" "\$PY_PATH"
    fi
    sleep 1
done
exit 1
EOF
chmod +x "$LAUNCHER_PATH"

# 3. LaunchAgent (auto-start on login, run in menu bar)
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
 "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.codex-switcher</string>
    <key>ProgramArguments</key>
    <array>
        <string>${LAUNCHER_PATH}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
EOF

# 4. Load agent
launchctl bootout gui/$(id -u) "$PLIST" 2>/dev/null || true
launchctl bootstrap gui/$(id -u) "$PLIST"

echo ""
echo "✅ Done! Look for ⚡ in your menu bar."
