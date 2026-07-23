# Agent Notes

- Keep changes tight and minimal.
- Do not assume backwards compatibility; ask when unsure.
- Use concise conventional commits, scoped to the touched surface:
  - `fix(TUI) - fixed typo in xyz`
  - `fix(tray) - aligned xyz`
- Before finalizing, confirm the requested task and any follow-up work are complete.

## Architecture

- `src/main.rs` → `cli::run()` routes clap subcommands, or launches the menubar tray when no subcommand is given (macOS/Windows).
- `app/service.rs` holds core business logic; `App<S: SecretStore>` is generic over storage.
- `repository/` persists snapshot metadata + gzip blobs; `secrets.rs` handles local/keyring migration.
- `codex.rs` / `cursor.rs` / `claude.rs` read/restore live auth for each provider.
- `usage.rs` fetches quota from OpenAI APIs; `app/auto_start.rs` runs background workers.
- Tray (`tray.rs`) is the primary macOS/Windows UX: left-click opens a CodexBar-style meter-card popover (`/menu`); right-click keeps the NSMenu; Overview is a secondary wry window (`/`).
- Linux/WSL: CLI subcommands only (no tray / no TUI).

## Validation

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```
