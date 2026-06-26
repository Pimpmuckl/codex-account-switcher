# macOS Menu Bar Wrapper

> **Note:** The Rust binary already includes a native macOS menu bar tray. This Python wrapper is optional and maintained for users who prefer its workflow. For new installs, use the Rust app directly (`codex-account-switcher` with no args on a non-TTY session).

A lightweight Python menu bar app that wraps the `codex-account-switcher` CLI, providing a native macOS menu bar experience for account switching.

## Features

- **⚡ Menu bar icon** — always visible, shows current account
- **One-click switching** — click any saved account to switch instantly
- **Auto Codex restart** — kills and relaunches Codex after switching
- **Add Account flow** — login with a new account without losing the current one
- **Delete accounts** — remove saved snapshots from the menu
- **Auto-start** — runs on login via LaunchAgent

## Requirements

- macOS 13+
- Python 3.9+
- `codex-account-switcher` CLI installed (`cargo install` or prebuilt binary)

## Install

```bash
# 1. Install the CLI first (if not already)
cargo install --git https://github.com/Pimpmuckl/codex-account-switcher

# 2. Run setup (creates venv, installs deps, configures LaunchAgent)
chmod +x setup.sh
./setup.sh
```

The setup script will:
1. Create a Python virtual environment
2. Install `rumps` (macOS menu bar framework)
3. Register a LaunchAgent for auto-start on login
4. Launch the app immediately

## Usage

After setup, look for **⚡** in your macOS menu bar.

| Menu Item | Description |
|---|---|
| ✅ email@example.com | Currently active account |
| ○ other@example.com | Click to switch to this account |
| 💾 Save Current Account | Snapshot the current Codex auth |
| 🔑 Add New Account… | Login with a new account (non-destructive) |
| 🗑 Delete Account | Remove a saved snapshot |
| 🔄 Refresh | Reload account list |

### Adding a new account

1. Click **🔑 Add New Account…**
2. Codex will relaunch with a login screen
3. Login with the new account
4. Click **⚡ → ✅ Finish Adding Account**
5. Original account is restored automatically

## Uninstall

```bash
launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/com.codex-switcher.plist
rm ~/Library/LaunchAgents/com.codex-switcher.plist
rm -f ~/.local/bin/codex-switcher-launcher.sh
```

## Logs

- stdout: `/tmp/codex-switcher.log`
- stderr: `/tmp/codex-switcher.err`
