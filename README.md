# codex-account-switcher

Cross-platform CLI for saving and restoring Codex login snapshots.

## Status

- Works as a Rust single-binary CLI.
- Targets Windows, WSL, Linux, and macOS.
- Uses the OS keychain for saved snapshots.
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
codex-account-switcher activate [ACCOUNT_ID] [--json]
codex-account-switcher delete [ACCOUNT_ID] [--json]
```

## Behavior

- `save` snapshots the currently logged-in Codex account.
- `list` shows saved accounts for the current environment.
- `status` shows the live account, Codex root, saved-account count, and process warnings.
- `activate` restores a saved snapshot and warns if Codex appears to be running.
- `delete` removes the saved snapshot from the switcher store only.

Account labels come from the `id_token` payload:

- email
- optional name
- best-effort plan label

## Build

```text
cargo build --release
```

## Validation

```text
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## Current Limits

- v1 supports only the existing live-login capture flow. It does not drive Codex login itself.
- Saved snapshots are keyed by the current environment and current account identity.
- Plan metadata is best effort and may be blank if the token does not expose a recognizable value.
