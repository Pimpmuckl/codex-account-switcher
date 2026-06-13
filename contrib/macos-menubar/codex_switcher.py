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

CLI = os.path.expanduser("~/.cargo/bin/codex-account-switcher")


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
        self._refresh_menu()

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
            plan_str = f"  ({plan})" if plan else ""
            self.title = f"⚡ {current_email.split('@')[0]}"
        else:
            self.title = "⚡ Codex"

        header = rumps.MenuItem("Codex Account Switcher")
        header.set_callback(None)
        self.menu.add(header)
        self.menu.add(None)

        # Active account display
        if current_email:
            active_label = f"✅  {current_email}{plan_str}"
            current_saved_id = status.get("current_account_saved_id")
            if not current_saved_id:
                active_label += "  [not saved]"
            active_item = rumps.MenuItem(active_label)
            active_item.set_callback(None)
            self.menu.add(active_item)
        else:
            ni = rumps.MenuItem("  ⚠️  Not logged in")
            ni.set_callback(None)
            self.menu.add(ni)

        self.menu.add(None)

        # Saved accounts
        accounts = account_list.get("accounts", []) if account_list else []
        saved_header = rumps.MenuItem("Switch to:")
        saved_header.set_callback(None)
        self.menu.add(saved_header)

        if accounts:
            for acc in accounts:
                aid = acc["id"]
                email = acc["email"]
                is_active = acc.get("is_active", False)
                plan = acc.get("plan_label", "")
                plan_s = f" ({plan})" if plan else ""

                # Usage info
                usage_str = ""
                usage = acc.get("usage")
                if usage and usage.get("weekly"):
                    w = usage["weekly"]
                    remaining = w.get("remaining_percent", "?")
                    usage_str = f"  [{remaining}% left]"

                err = acc.get("usage_error", "")
                if err and "login required" in err.lower():
                    usage_str = "  [login required]"

                if is_active:
                    label = f"  ● {email}{plan_s}{usage_str}"
                    item = rumps.MenuItem(label)
                    item.set_callback(None)
                else:
                    label = f"  ○ {email}{plan_s}{usage_str}"
                    item = rumps.MenuItem(
                        label, callback=self._mk(self._switch, aid, email)
                    )
                self.menu.add(item)
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
        rumps.notification("Codex Switcher", "⏳ Switching…", f"→ {email}")

        # Kill Codex first for reliable swap
        subprocess.run(["pkill", "-f", "Codex"], capture_output=True)
        time.sleep(0.5)

        ok, msg = cli_run("activate", account_id, "--force")
        if ok:
            # Relaunch Codex
            subprocess.Popen(["open", "-a", "Codex"])
            rumps.notification("Codex Switcher", "✅ Switched", f"Now: {email}")
        else:
            rumps.notification("Codex Switcher", "❌ Error", msg[:100])

        self._refresh_menu()

    def _on_save(self, _):
        ok, msg = cli_run("save")
        if ok:
            rumps.notification("Codex Switcher", "💾 Saved", msg)
        else:
            rumps.notification("Codex Switcher", "❌ Error", msg[:100])
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
            rumps.notification("Codex Switcher", "Error", "Not logged in")
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

        rumps.notification(
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
            rumps.notification("Codex Switcher", "Error", "Not logged in yet")
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
            rumps.notification("Codex Switcher", "✅ Account Added", msg)
        else:
            rumps.notification("Codex Switcher", "❌ Error", msg[:100])

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
        rumps.notification("Codex Switcher", "", "Cancelled. Original restored.")
        self._refresh_menu()

    def _delete(self, account_id, email):
        if not ask_confirm("Delete Account", f"Delete saved snapshot for {email}?"):
            return
        ok, msg = cli_run("delete", account_id)
        if ok:
            rumps.notification("Codex Switcher", "🗑 Deleted", email)
        else:
            rumps.notification("Codex Switcher", "❌ Error", msg[:100])
        self._refresh_menu()

    def _on_refresh(self, _):
        self._refresh_menu()

    def _on_quit(self, _):
        rumps.quit_application()


if __name__ == "__main__":
    CodexSwitcher().run()
