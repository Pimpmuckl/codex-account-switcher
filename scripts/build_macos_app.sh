#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
ASSETS_DIR="$PROJECT_DIR/assets"
PNG_ICON="$ASSETS_DIR/codex-account-switcher-dock.png"
ICNS_ICON="$ASSETS_DIR/codex-account-switcher.icns"
APP_NAME="Codex Account Switcher"
APP_DIR="$PROJECT_DIR/target/release/$APP_NAME.app"
APP_VERSION="$(awk -F '\"' '/^version = / {print $2; exit}' "$PROJECT_DIR/Cargo.toml")"

echo "=== Creating macOS Icon ==="
python3 "$SCRIPT_DIR/generate_icons.py"

echo "=== Building Rust Application ==="
cargo build --release

echo "=== Packaging App Bundle ==="
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

# Copy binary
cp "$PROJECT_DIR/target/release/codex-account-switcher" "$APP_DIR/Contents/MacOS/"

# Copy icon
cp "$ICNS_ICON" "$APP_DIR/Contents/Resources/icon.icns"

# Create Info.plist
cat > "$APP_DIR/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>codex-account-switcher</string>
    <key>CFBundleIconFile</key>
    <string>icon.icns</string>
    <key>CFBundleIdentifier</key>
    <string>com.pimpmuckl.codex-account-switcher</string>
    <key>CFBundleName</key>
    <string>Codex Account Switcher</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${APP_VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${APP_VERSION}</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
EOF

echo "✅ App bundle created at $APP_DIR"
