# codex-account-switcher

Cross-platform CLI for saving and restoring Codex login snapshots.

## Status

- Works as a Rust single-binary CLI.
- Targets Windows, WSL, Linux, and macOS.
- Stores saved snapshots locally under the switcher app-data directory.
- Treats each environment separately. Windows and WSL do not share state.

## What It Switches

v1 manages only these Codex auth files:

- `~/.codex/auth.json`
- `~/.codex/cap_sid`

It does not modify other Codex config, history, sessions, or sqlite state.

## Commands

Run without arguments for interactive mode.

```text
codex-account-switcher
codex-account-switcher status [--json]
codex-account-switcher list [--json]
codex-account-switcher save [--json]
codex-account-switcher usage [ACCOUNT_ID] [--json]
codex-account-switcher activate [ACCOUNT_ID] [--force] [--json]
codex-account-switcher delete [ACCOUNT_ID] [--json]
codex-account-switcher auto-start-usage-windows [--enable|--disable] [--run] [--json]
```

## Behavior

- `save` snapshots the currently logged-in Codex account.
- `list` shows saved accounts for the current environment and includes weekly usage/reset data when it can read it from the saved snapshot.
- `status` shows the live account, Codex root, saved-account count, and process warnings.
- `usage` fetches current usage for the live account or for a saved account by id.
- `activate` restores a saved snapshot. Reliable swaps require all Codex processes to be closed first; `--force` lets the command attempt activation anyway, but it still fails if the restored files do not stay stable.
- `delete` removes the saved snapshot from the switcher store only.
- `auto-start-usage-windows` is opt-in from the CLI, TUI, or tray checkmark. When enabled, the interactive app refreshes saved weekly windows every 5 minutes and starts due windows with a minimal `codex exec` ping when Codex is on `PATH`.

Saved snapshot data lives in the app-data directory for the current environment:

- Windows: `%LOCALAPPDATA%\\nextide\\codex-account-switcher`
- macOS: `~/Library/Application Support/nextide/codex-account-switcher`
- Linux / WSL: `${XDG_DATA_HOME:-~/.local/share}/nextide/codex-account-switcher`

Account labels come from the `id_token` payload:

- email
- account identity
- optional name
- best-effort plan label

## Build

```text
cargo build --release
```

## Install

Tagged releases publish prebuilt archives plus installer scripts on GitHub Releases.

- Windows: Install prebuilt binaries via PowerShell script

```text
powershell -ExecutionPolicy Bypass -c "irm https://github.com/Pimpmuckl/codex-account-switcher/releases/download/v0.1.6/codex-account-switcher-installer.ps1 | iex"
```

- macOS / Linux / WSL: Install prebuilt binaries via shell script

```text
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/Pimpmuckl/codex-account-switcher/releases/download/v0.1.6/codex-account-switcher-installer.sh | sh
```

Default installer location:

- `~/.codex-account-switcher/bin`

Override install location by setting:

- `CODEX_ACCOUNT_SWITCHER_INSTALL_DIR`

The release workflow builds these targets:

- `x86_64-pc-windows-msvc`
- `x86_64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Release automation lives in:

- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`

To cut a release, bump `Cargo.toml` to the target version and push a matching tag like `v0.1.6`.

## Validation

```text
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

## Current Limits

- v1 supports only the existing live-login capture flow. It does not drive Codex login itself.
- Saved snapshots are keyed by the current account identity in the current app-data directory.
- Plan metadata is best effort and may be blank if the token does not expose a recognizable value.
- Usage enrichment depends on saved `auth.json` tokens still being present and accepted by the Codex/OpenAI usage endpoints.
- Auto-starting usage windows can ping while Codex processes are running; active Codex sessions keep their auth cached, and the switcher restores the previous live account afterward.
