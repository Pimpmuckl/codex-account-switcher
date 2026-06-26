# Agent Notes

- Keep changes tight and minimal.
- Do not assume backwards compatibility; ask when unsure.
- Use concise conventional commits, scoped to the touched surface:
  - `fix(TUI) - fixed typo in xyz`
  - `fix(tray) - aligned xyz`
- Before finalizing, confirm the requested task and any follow-up work are complete.

## Architecture

- `src/main.rs` → `cli::run()` routes subcommands or interactive/tray mode.
- `app/service.rs` holds core business logic; `App<S: SecretStore>` is generic over storage.
- `repository/` persists snapshot metadata + gzip blobs; `secrets.rs` handles local/keyring migration.
- `codex.rs` reads/restores live `~/.codex/auth.json` + `cap_sid`.
- `usage.rs` fetches quota from OpenAI APIs; `app/auto_start.rs` runs background workers.
- Tray (`tray.rs`) and TUI (`app/tui.rs`) are Windows/macOS-only for tray; Linux/WSL is TUI-only.
- `contrib/macos-menubar/` is an optional Python wrapper; the Rust binary has its own native tray.

## Validation

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```
