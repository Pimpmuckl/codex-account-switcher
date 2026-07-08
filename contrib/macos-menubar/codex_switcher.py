#!/usr/bin/env python3
"""
Codex Account Switcher — macOS Menu Bar App
Wraps codex-account-switcher CLI with a native menu bar UI.
One-click account switching, no TUI needed.
"""

import rumps
import subprocess
import json
import os
import time
import datetime

CLI_PATHS = [
    os.path.expanduser("~/.codex-account-switcher/bin/codex-account-switcher"),
    os.path.expanduser("~/.cargo/bin/codex-account-switcher"),
]
CLI = next((p for p in CLI_PATHS if os.path.exists(p)), CLI_PATHS[0])
ICON_PATHS = [
    os.path.join(os.path.dirname(__file__), "..", "..", "assets", "codex-account-switcher-transparent.png"),
    os.path.join(os.path.dirname(__file__), "icon.png"),
]


def load_menu_bar_icon():
    """Load template-friendly menu bar icon from bundled assets."""
    try:
        from AppKit import NSImage

        for path in ICON_PATHS:
            resolved = os.path.abspath(path)
            if not os.path.exists(resolved):
                continue
            image = NSImage.alloc().initByContentsOfFile_(resolved)
            if image is None:
                continue
            image.setSize_((18, 18))
            image.setTemplate_(True)
            return image
    except Exception:
        pass
    return None


def format_reset_time(reset_at_arr, is_weekly=False):
    """Parse 0-indexed day-of-year array to a readable time string."""
    if not reset_at_arr or len(reset_at_arr) < 5:
        return ""
    try:
        year, yday, hour, minute, second = reset_at_arr[0], reset_at_arr[1], reset_at_arr[2], reset_at_arr[3], reset_at_arr[4]
        dt = datetime.datetime(year, 1, 1) + datetime.timedelta(days=yday, hours=hour, minutes=minute, seconds=second)
        if is_weekly:
            # Show both date and clock time for weekly reset as well.
            # Example: "Jul 7, 7:27 PM"
            return dt.strftime("%b %-d, %-I:%M %p")
        else:
            return dt.strftime("%-I:%M %p")
    except Exception:
        return ""


def format_quota_bar(percent):
    """Build a compact quota bar: [██████░░] 75%"""
    bar_width = 6
    filled = round(percent * bar_width / 100)
    empty = bar_width - filled
    return f"[{'█' * filled}{'░' * empty}] {percent}%"


def _get_bottleneck_window(usage):
    """Return the window with lowest remaining_percent that hasn't reset yet."""
    if not usage:
        return None
    import time as _time
    now = _time.time()
    candidates = []
    for key in ("five_hour", "weekly"):
        w = usage.get(key)
        if not w:
            continue
        rem = w.get("remaining_percent")
        if rem is None:
            continue
        # reset_at is array [year, day_of_year, hour, minute, second]
        reset_at = w.get("reset_at")
        if reset_at and len(reset_at) >= 5:
            try:
                dt = datetime.datetime(reset_at[0], 1, 1) + datetime.timedelta(
                    days=reset_at[1], hours=reset_at[2], minutes=reset_at[3], seconds=reset_at[4]
                )
                if dt.timestamp() <= now:
                    continue  # past reset
            except Exception:
                pass
        candidates.append((rem, w, key))
    if not candidates:
        return None
    candidates.sort(key=lambda x: x[0])
    return candidates[0]  # (remaining_percent, window_dict, key)


def _format_account_details(plan_label, acc):
    """Build compact details line: Plan  •  [████░░░░] 52%  •  ↻ 12/05"""
    parts = []
    if plan_label:
        parts.append(plan_label)

    if acc:
        err = acc.get("usage_error", "")
        if err and "login required" in err.lower():
            parts.append("Login required")
            return " • ".join(parts)

        usage = acc.get("usage")
        bottleneck = _get_bottleneck_window(usage)
        if bottleneck:
            rem, window, key = bottleneck
            parts.append(format_quota_bar(rem))
            reset_str = format_reset_time(window.get("reset_at"), is_weekly=(key == "weekly"))
            if reset_str:
                parts.append(f"↻ {reset_str}")

    return " • ".join(parts) if parts else ""


def _status_tag(acc):
    """Compact status tag: Ready / Low / Depleted / Login / Stale / —"""
    err = acc.get("usage_error", "")
    if err and "login required" in err.lower():
        return "Login"
    usage = acc.get("usage")
    if usage:
        bottleneck = _get_bottleneck_window(usage)
        if bottleneck:
            rem = bottleneck[0]
            if rem == 0:
                return "Depleted"
            if rem <= 10:
                return "Low"
            return "Ready"
        return "Ready"
    if err:
        return "Stale"
    return "—"


def notify(title, subtitle, message):
    """Safe notification — falls back to osascript if rumps fails."""
    try:
        rumps.notification(title, subtitle, message)
    except Exception:
        try:
            subprocess.run(
                ["osascript", "-e",
                 f'display notification "{message}" with title "{title}" subtitle "{subtitle}"'],
                capture_output=True, timeout=5,
            )
        except Exception:
            pass


# ── Helpers ──────────────────────────────────────────────────

def cli_json(*args):
    """Run CLI with --json and return parsed output."""
    try:
        r = subprocess.run(
            [CLI, *args, "--json"],
            capture_output=True, text=True, timeout=10,
        )
        if r.returncode == 0 and r.stdout.strip():
            return json.loads(r.stdout)
    except Exception:
        pass
    return None


def cli_run(*args):
    """Run CLI and return (success, stdout)."""
    try:
        r = subprocess.run(
            [CLI, *args],
            capture_output=True, text=True, timeout=15,
        )
        return r.returncode == 0, r.stdout.strip()
    except Exception as e:
        return False, str(e)


def ask_text(title, message, default=""):
    """Show input dialog via osascript."""
    script = f'''
    tell application "System Events"
        activate
        set userInput to display dialog "{message}" ¬
            default answer "{default}" ¬
            with title "{title}" ¬
            buttons {{"Cancel", "OK"}} default button "OK"
        return text returned of userInput
    end tell
    '''
    try:
        r = subprocess.run(
            ["osascript", "-e", script],
            capture_output=True, text=True, timeout=60,
        )
        return r.stdout.strip() if r.returncode == 0 else None
    except Exception:
        return None


def ask_confirm(title, message):
    """Show confirmation dialog via osascript."""
    script = f'''
    tell application "System Events"
        activate
        display dialog "{message}" ¬
            with title "{title}" ¬
            buttons {{"Cancel", "OK"}} default button "OK"
    end tell
    '''
    try:
        r = subprocess.run(
            ["osascript", "-e", script],
            capture_output=True, text=True, timeout=30,
        )
        return r.returncode == 0
    except Exception:
        return False


# ── App ──────────────────────────────────────────────────────

class CodexSwitcher(rumps.App):
    def __init__(self):
        icon = load_menu_bar_icon()
        title = "" if icon is not None else "⚡"
        super().__init__(title, icon=icon, template=True, quit_button=None)
        self._register_login_item()
        self._refresh_menu()

    def _register_login_item(self):
        try:
            from pathlib import Path
            py_path = os.path.abspath(__file__)
            script_dir = os.path.dirname(py_path)
            cmd_path = os.path.join(script_dir, "run.command")
            venv_path = os.path.join(script_dir, ".venv/bin/python")

            if not os.path.exists(cmd_path):
                return
            
            # Create local launcher script to wait for mount
            local_bin = Path.home() / ".local" / "bin"
            local_bin.mkdir(parents=True, exist_ok=True)
            launcher_path = local_bin / "codex-switcher-launcher.sh"
            
            launcher_content = f"""#!/bin/bash
# Wait for external volume to mount (max 30 seconds)
PY_PATH="{py_path}"
VENV_PATH="{venv_path}"
for i in {{1..30}}; do
    if [ -f "$PY_PATH" ]; then
        pkill -f codex_switcher.py 2>/dev/null
        sleep 0.5
        exec "$VENV_PATH" "$PY_PATH"
    fi
    sleep 1
done
exit 1
"""
            if not launcher_path.exists() or launcher_path.read_text() != launcher_content:
                launcher_path.write_text(launcher_content)
                launcher_path.chmod(0o755)

            plist_dir = Path.home() / "Library" / "LaunchAgents"
            plist_dir.mkdir(parents=True, exist_ok=True)
            plist_path = plist_dir / "com.codex-switcher.plist"
            plist_content = f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.codex-switcher</string>
    <key>ProgramArguments</key>
    <array>
        <string>{launcher_path}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"""
            if not plist_path.exists() or plist_path.read_text() != plist_content:
                plist_path.write_text(plist_content)
                # Load agent immediately
                uid_out = subprocess.run(["id", "-u"], capture_output=True, text=True)
                uid = uid_out.stdout.strip()
                if uid:
                    subprocess.run(["launchctl", "bootout", f"gui/{uid}", str(plist_path)], capture_output=True)
                    subprocess.run(["launchctl", "bootstrap", f"gui/{uid}", str(plist_path)], capture_output=True)
        except Exception:
            pass


    # ── Menu ─────────────────────────────────────────────────

    def _refresh_menu(self):
        self.menu.clear()
        status = cli_json("status")
        account_list = cli_json("list")

        # Current account
        current_email = None
        plan = ""
        if status and status.get("current_account"):
            current_email = status["current_account"].get("email", "?")
            plan = status["current_account"].get("plan_label", "")
            if self.icon is None:
                self.title = f"⚡ {current_email.split('@')[0]}"
            else:
                self.title = ""
        else:
            self.title = "" if self.icon is not None else "⚡ Codex"

        accounts = account_list.get("accounts", []) if account_list else []
        current_saved_id = status.get("current_account_saved_id") if status else None

        # Find active account and build other_accounts list
        active_acc = None
        other_accounts = []
        for acc in accounts:
            if acc["email"] == current_email or acc.get("is_active", False) or (current_saved_id and acc["id"] == current_saved_id):
                active_acc = acc
            else:
                other_accounts.append(acc)

        # ── Active account ──────────────────────────────────
        if current_email:
            not_saved = " [not saved]" if not current_saved_id else ""
            active_label = f"\u2713  {current_email}{not_saved}"
            active_item = rumps.MenuItem(active_label)
            active_item.set_callback(None)
            self.menu.add(active_item)

            # Details: plan + quota bar + reset
            details = _format_account_details(plan, active_acc)
            if details:
                sub_item = rumps.MenuItem(f"      {details}")
                sub_item.set_callback(None)
                self.menu.add(sub_item)
        else:
            ni = rumps.MenuItem("Not logged in")
            ni.set_callback(None)
            self.menu.add(ni)

        self.menu.add(None)

        # ── Saved accounts (switch targets) ─────────────────
        if other_accounts:
            active_group = []
            depleted_group = []
            login_group = []

            for acc in other_accounts:
                tag = _status_tag(acc)
                if tag == "Login":
                    login_group.append(acc)
                elif tag == "Depleted":
                    depleted_group.append(acc)
                else:
                    active_group.append(acc)

            first_group = True

            def add_acc(acc):
                aid = acc["id"]
                email = acc["email"]
                short_email = email.removesuffix("@gmail.com") if email.endswith("@gmail.com") else email
                tag = _status_tag(acc)
                label = f"  {short_email} — {tag}"
                item = rumps.MenuItem(
                    label, callback=self._mk(self._switch, aid, email)
                )
                self.menu.add(item)

                # Details line
                acc_plan = acc.get("plan_label", "")
                details = _format_account_details(acc_plan, acc)
                if details:
                    sub_item = rumps.MenuItem(f"      {details}")
                    sub_item.set_callback(None)
                    self.menu.add(sub_item)

            if active_group:
                if not first_group:
                    self.menu.add(None)
                first_group = False
                header = rumps.MenuItem("🟢 Active (Còn token)", callback=lambda _: None)
                self.menu.add(header)
                for acc in active_group:
                    add_acc(acc)

            if depleted_group:
                if not first_group:
                    self.menu.add(None)
                first_group = False
                header = rumps.MenuItem("🔴 Depleted (Hết token)", callback=lambda _: None)
                self.menu.add(header)
                for acc in depleted_group:
                    add_acc(acc)

            if login_group:
                if not first_group:
                    self.menu.add(None)
                first_group = False
                header = rumps.MenuItem("⚠️ Login Required (Cần login lại)", callback=lambda _: None)
                self.menu.add(header)
                for acc in login_group:
                    add_acc(acc)
        else:
            empty = rumps.MenuItem("  (no saved accounts)")
            empty.set_callback(None)
            self.menu.add(empty)

        self.menu.add(None)

        # ── Actions ─────────────────────────────────────────
        self.menu.add(rumps.MenuItem(
            "⚡ Best Quota", callback=self._on_pick_best
        ))
        self.menu.add(rumps.MenuItem(
            "💾 Save Current", callback=self._on_save
        ))
        self.menu.add(rumps.MenuItem(
            "➕ Add Account…", callback=self._on_add
        ))

        if accounts:
            del_sub = rumps.MenuItem("🗑 Delete Account")
            for acc in accounts:
                del_sub.add(rumps.MenuItem(
                    acc["email"],
                    callback=self._mk(self._delete, acc["id"], acc["email"]),
                ))
            self.menu.add(del_sub)

        self.menu.add(None)

        # ── System ──────────────────────────────────────────
        self.menu.add(rumps.MenuItem("🔄 Refresh", callback=self._on_refresh))
        self.menu.add(rumps.MenuItem("Quit", callback=self._on_quit))

    def _mk(self, fn, *args):
        return lambda _: fn(*args)

    # ── Actions ──────────────────────────────────────────────

    def _switch(self, account_id, email):
        notify("Codex Switcher", "⏳ Switching…", f"→ {email}")

        # Kill Codex first for reliable swap
        subprocess.run(["pkill", "-x", "Codex"], capture_output=True)
        time.sleep(0.5)

        ok, msg = cli_run("activate", account_id, "--force")
        if ok:
            # Relaunch Codex
            subprocess.Popen(["open", "-a", "Codex"])
            notify("Codex Switcher", "✅ Switched", f"Now: {email}")
        else:
            notify("Codex Switcher", "❌ Error", msg[:100])

        self._refresh_menu()

    def _on_save(self, _):
        ok, msg = cli_run("save")
        if ok:
            notify("Codex Switcher", "💾 Saved", msg)
        else:
            notify("Codex Switcher", "❌ Error", msg[:100])
        self._refresh_menu()

    def _on_pick_best(self, _):
        notify("Codex Switcher", "⏳ Finding best quota…", "")
        ok, msg = cli_run("pick-best", "--relaunch")
        if ok:
            notify("Codex Switcher", "⚡ Best Quota", msg[:100])
        else:
            notify("Codex Switcher", "❌ Error", msg[:100])
        self._refresh_menu()

    def _on_add(self, _):
        """Temporarily remove auth → let user login → save → restore."""
        import shutil
        from pathlib import Path

        codex_dir = Path.home() / ".codex"
        auth = codex_dir / "auth.json"
        cap_sid = codex_dir / "cap_sid"
        backup_auth = codex_dir / "auth.json.switcher-bak"
        backup_sid = codex_dir / "cap_sid.switcher-bak"

        if not auth.exists():
            notify("Codex Switcher", "Error", "Not logged in")
            return

        # Backup
        shutil.copy2(str(auth), str(backup_auth))
        if cap_sid.exists():
            shutil.copy2(str(cap_sid), str(backup_sid))

        # Remove auth to trigger login screen
        auth.unlink()
        if cap_sid.exists():
            cap_sid.unlink()

        # Restart Codex
        subprocess.run(["pkill", "-x", "Codex"], capture_output=True)
        time.sleep(1)
        subprocess.Popen(["open", "-a", "Codex"])

        notify(
            "Codex Switcher",
            "🔑 Login with new account",
            "Login in Codex, then click ⚡ → Finish Adding Account",
        )

        # Swap menu to show "Finish" button
        self.menu.clear()
        header = rumps.MenuItem("Codex Account Switcher")
        header.set_callback(None)
        self.menu.add(header)
        self.menu.add(None)
        self.menu.add(rumps.MenuItem(
            "⏳  Waiting for login…"
        ))
        self.menu.add(None)
        self.menu.add(rumps.MenuItem(
            "✅  Finish Adding Account",
            callback=lambda _: self._finish_add(backup_auth, backup_sid),
        ))
        self.menu.add(rumps.MenuItem(
            "❌  Cancel",
            callback=lambda _: self._cancel_add(backup_auth, backup_sid),
        ))
        self.menu.add(None)
        self.menu.add(rumps.MenuItem("Quit", callback=self._on_quit))

    def _finish_add(self, backup_auth, backup_sid):
        from pathlib import Path
        import shutil

        codex_dir = Path.home() / ".codex"
        auth = codex_dir / "auth.json"

        if not auth.exists():
            notify("Codex Switcher", "Error", "Not logged in yet")
            return

        # Ensure cap_sid exists
        cap_sid = codex_dir / "cap_sid"
        if not cap_sid.exists():
            cap_sid.touch()

        # Save new account
        ok, msg = cli_run("save")

        # Kill Codex, restore original
        subprocess.run(["pkill", "-x", "Codex"], capture_output=True)
        time.sleep(0.5)

        if backup_auth.exists():
            shutil.copy2(str(backup_auth), str(auth))
            backup_auth.unlink()
        if backup_sid.exists():
            shutil.copy2(str(backup_sid), str(cap_sid))
            backup_sid.unlink()

        # Relaunch with original
        subprocess.Popen(["open", "-a", "Codex"])

        if ok:
            notify("Codex Switcher", "✅ Account Added", msg)
        else:
            notify("Codex Switcher", "❌ Error", msg[:100])

        self._refresh_menu()

    def _cancel_add(self, backup_auth, backup_sid):
        from pathlib import Path
        import shutil

        codex_dir = Path.home() / ".codex"
        auth = codex_dir / "auth.json"
        cap_sid = codex_dir / "cap_sid"

        subprocess.run(["pkill", "-x", "Codex"], capture_output=True)
        time.sleep(0.5)

        if backup_auth.exists():
            shutil.copy2(str(backup_auth), str(auth))
            backup_auth.unlink()
        if backup_sid.exists():
            shutil.copy2(str(backup_sid), str(cap_sid))
            backup_sid.unlink()

        subprocess.Popen(["open", "-a", "Codex"])
        notify("Codex Switcher", "", "Cancelled. Original restored.")
        self._refresh_menu()

    def _delete(self, account_id, email):
        if not ask_confirm("Delete Account", f"Delete saved snapshot for {email}?"):
            return
        ok, msg = cli_run("delete", account_id)
        if ok:
            notify("Codex Switcher", "🗑 Deleted", email)
        else:
            notify("Codex Switcher", "❌ Error", msg[:100])
        self._refresh_menu()

    def _on_refresh(self, _):
        self._refresh_menu()

    def _on_quit(self, _):
        rumps.quit_application()


if __name__ == "__main__":
    CodexSwitcher().run()
