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


def format_reset_time(reset_at_arr, is_weekly=False):
    """Parse 0-indexed day-of-year array to a readable time string."""
    if not reset_at_arr or len(reset_at_arr) < 5:
        return ""
    try:
        year, yday, hour, minute, second = reset_at_arr[0], reset_at_arr[1], reset_at_arr[2], reset_at_arr[3], reset_at_arr[4]
        dt = datetime.datetime(year, 1, 1) + datetime.timedelta(days=yday, hours=hour, minutes=minute, seconds=second)
        if is_weekly:
            return dt.strftime("%d/%m")
        else:
            return dt.strftime("%H:%M")
    except Exception:
        return ""



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
        super().__init__("⚡", quit_button=None)
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
        if status and status.get("current_account"):
            current_email = status["current_account"].get("email", "?")
            plan = status["current_account"].get("plan_label", "")
            self.title = f"⚡ {current_email.split('@')[0]}"
        else:
            self.title = "⚡ Codex"

        # Helpers for compact labels
        def format_email(email):
            if not email:
                return ""
            if email.endswith("@gmail.com"):
                return email[:-10]
            return email

        def get_clean_usage_str(acc):
            usage = acc.get("usage")
            if not usage:
                err = acc.get("usage_error", "")
                if err and "login required" in err.lower():
                    return " • login required"
                return ""
            parts = []
            five_hour = usage.get("five_hour")
            if five_hour:
                fh_rem = five_hour.get("remaining_percent", "?")
                fh_str = ""
                if fh_rem != "?":
                    if fh_rem < 100:
                        fh_str = f"5h {fh_rem}%"
                        if fh_rem <= 20:
                            fh_reset = format_reset_time(five_hour.get("reset_at"), is_weekly=False)
                            if fh_reset:
                                fh_str += f"@{fh_reset}"
                else:
                    fh_str = "5h ?"
                if fh_str:
                    parts.append(fh_str)

            weekly = usage.get("weekly")
            if weekly:
                w_rem = weekly.get("remaining_percent", "?")
                w_str = ""
                if w_rem != "?":
                    if w_rem < 100:
                        w_str = f"W {w_rem}%"
                        if w_rem <= 20:
                            w_reset = format_reset_time(weekly.get("reset_at"), is_weekly=True)
                            if w_reset:
                                w_str += f"@{w_reset}"
                else:
                    w_str = "W ?"
                if w_str:
                    parts.append(w_str)

            if parts:
                return " • " + " • ".join(parts)
            return ""

        # Active account display
        accounts = account_list.get("accounts", []) if account_list else []
        active_usage_str = ""
        current_saved_id = status.get("current_account_saved_id") if status else None

        # Extract active account's usage and exclude it from the inactive switcher list
        other_accounts = []
        for acc in accounts:
            if acc["email"] == current_email or acc.get("is_active", False) or (current_saved_id and acc["id"] == current_saved_id):
                active_usage_str = get_clean_usage_str(acc)
            else:
                other_accounts.append(acc)

        if current_email:
            display_email = format_email(current_email)
            plan_str = f" ({plan})" if plan else ""
            active_label = f"✅  {display_email}{plan_str}"
            if not current_saved_id:
                active_label += " (not saved)"
            active_item = rumps.MenuItem(active_label)
            active_item.set_callback(None)
            self.menu.add(active_item)
            
            # Sub-label for active usage if exists
            if active_usage_str:
                sub_label = "      " + active_usage_str.replace(" • ", "", 1).strip()
                sub_item = rumps.MenuItem(sub_label)
                sub_item.set_callback(None)
                self.menu.add(sub_item)
        else:
            ni = rumps.MenuItem("  ⚠️  Not logged in")
            ni.set_callback(None)
            self.menu.add(ni)

        self.menu.add(None)

        # Saved accounts header
        saved_header = rumps.MenuItem("Switch to:")
        saved_header.set_callback(None)
        self.menu.add(saved_header)

        if other_accounts:
            for acc in other_accounts:
                aid = acc["id"]
                email = acc["email"]
                plan_lbl = acc.get("plan_label", "")
                
                display_email = format_email(email)
                plan_str = f" ({plan_lbl})" if plan_lbl else ""
                
                label = f"  ○  {display_email}{plan_str}"
                item = rumps.MenuItem(
                    label, callback=self._mk(self._switch, aid, email)
                )
                self.menu.add(item)
                
                # Sub-label for inactive usage if exists
                usage_str = get_clean_usage_str(acc)
                if usage_str:
                    sub_label = "      " + usage_str.replace(" • ", "", 1).strip()
                    sub_item = rumps.MenuItem(sub_label)
                    sub_item.set_callback(None)
                    self.menu.add(sub_item)
        else:
            empty = rumps.MenuItem("  (no saved accounts)")
            empty.set_callback(None)
            self.menu.add(empty)

        self.menu.add(None)

        # Actions
        self.menu.add(rumps.MenuItem(
            "💾  Save Current Account", callback=self._on_save
        ))
        self.menu.add(rumps.MenuItem(
            "🔑  Add New Account…", callback=self._on_add
        ))

        if accounts:
            del_sub = rumps.MenuItem("🗑  Delete Account")
            for acc in accounts:
                del_sub.add(rumps.MenuItem(
                    acc["email"],
                    callback=self._mk(self._delete, acc["id"], acc["email"]),
                ))
            self.menu.add(del_sub)

        self.menu.add(None)
        self.menu.add(rumps.MenuItem("🔄  Refresh", callback=self._on_refresh))
        self.menu.add(rumps.MenuItem("Quit", callback=self._on_quit))

    def _mk(self, fn, *args):
        return lambda _: fn(*args)

    # ── Actions ──────────────────────────────────────────────

    def _switch(self, account_id, email):
        notify("Codex Switcher", "⏳ Switching…", f"→ {email}")

        # Kill Codex first for reliable swap
        subprocess.run(["pkill", "-f", "Codex"], capture_output=True)
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
        subprocess.run(["pkill", "-f", "Codex"], capture_output=True)
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
        wait = rumps.MenuItem("⏳  Waiting for login…")
        wait.set_callback(None)
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
        subprocess.run(["pkill", "-f", "Codex"], capture_output=True)
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

        subprocess.run(["pkill", "-f", "Codex"], capture_output=True)
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
    rumps.debug_mode(True)
    CodexSwitcher().run()
