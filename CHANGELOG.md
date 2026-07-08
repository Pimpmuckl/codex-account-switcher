# Changelog

## v0.1.9 - 2026-07-08

### Fixed

- Render the persistent TUI from cached usage data instead of blocking startup on saved-account usage refreshes.
- Refresh saved usage metadata in the background and skip auto-start checks for active cached weekly windows that cannot need a ping yet.
- Guard background usage refresh writes so stale refresh results cannot overwrite a newer account switch.

## v0.1.8 - 2026-07-08

### Fixed

- Run auto-start usage-window pings through a hidden native Codex launcher on Windows so tray mode does not flash a console window.
- Preserve direct `codex.exe` installs on `PATH` while bypassing npm `.cmd` shims when a bundled native executable is available.
- Report successful auto-start pings as `pinged` in CLI and TUI feedback.
- Keep moved-forward `100%` weekly reset timestamps eligible for auto-start pinging.

## v0.1.7 - 2026-06-29

### Fixed

- Detect floating weekly usage reset times before auto-starting saved Codex weekly windows.
- Preserve auto-start retry state until the weekly-window ping succeeds.
- Avoid refreshing saved weekly usage from the persistent TUI before the auto-start worker can inspect due windows.

## v0.1.6 - 2026-06-13

### Fixed

- Validate saved auth snapshots before activation so incomplete or corrupt snapshot files cannot be restored into the live Codex home.
- Recover interrupted auth restores more reliably by tracking restore transactions and cleaning abandoned restore artifacts before future reads.
- Sanitize persisted usage-refresh errors so local metadata does not retain raw API response text.
- Recover settings from interrupted writes by falling back to the last valid backup when the primary settings file is incomplete.

## v0.1.5 - 2026-05-22

### Added

- Added saved-account stale-login detection for expired or reused refresh tokens. Failed saved-account usage refreshes now persist a `Login required` marker so the CLI, TUI, and Windows tray can show which account needs a fresh sign-in.

### Fixed

- Prefer the stale-login marker over older cached weekly usage so expired accounts do not look healthy just because a previous usage snapshot still exists.
- Refresh the Windows tray menu after background auto-start usage checks so tray rows reflect refreshed usage and login-required state without reopening the app.

## v0.1.4 - 2026-05-11

### Fixed

- Restored the Windows tray account table so active and saved accounts show plan, weekly remaining percentage, and weekly reset time again.
- Improved tray table alignment in native Windows menus by using the menu detail column plus fixed-width percentage formatting.
- Kept active-account display informational while saved-account rows remain clickable switch targets.

## v0.1.3 - 2026-05-10

### Added

- Added optional auto-start usage-window refreshes. When enabled, the app checks saved accounts every five minutes and starts overdue weekly Codex usage windows with a minimal isolated `codex exec` ping.
- Added controls for the new refresh behavior in the persistent TUI, the Windows tray menu, and the `auto-start-usage-windows` CLI command.
- Added a tray checkmark for `Auto-start usage windows`; enabling it from tray immediately kicks off a background check without blocking the tray menu.
- Added `AGENTS.md` with brief repo instructions and commit-message examples for future agent work.

### Changed

- TUI account rows now show weekly reset dates with 24-hour time, for example `Reset: 2026-05-12 13:56`.
- Auto-start pings now run in a temporary `CODEX_HOME` seeded from the saved snapshot, so active app/CLI sessions keep their cached auth and the live Codex auth home is not swapped.
- Auto-start refreshes now keep using the active app environment, serialize concurrent manual/tray/background checks, preserve cached usage metadata on write-back, and scrub temporary auth material after cleanup edge cases.
- The tray menu now uses simple `Active:` and `Saved:` sections with email-only account rows instead of dense plan/status/usage columns.
- Startup/status rendering is faster because process detection now refreshes only the process fields the app actually displays.

### Fixed

- Removed the awkward `Which account do you want to activate?` prompt from activation flows.
- Hardened `auto-start-usage-windows --disable --run` so disabling remains disable-only.
- Fixed newer Clippy warnings on Rust 1.95 across Windows, macOS, and Linux CI.
