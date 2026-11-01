# rdreh

Crate `rdreh`, command `rdr` ("herdr" backwards). A keyboard-first terminal multiplexer for running and tracking AI coding agents. Rust 2024, dual licensed MIT/Apache-2.0. This is the flagship. The older tmux+bash version lives on as the GitHub repo JohnGF/agent-swarm.

## Build and test

- `cargo clippy --all-targets -- -D warnings`, `cargo test` and `cargo build --release`, the same three steps CI runs (`.github/workflows/ci.yml`, ubuntu-latest).
- There is no lib target. To run unit tests only, use `cargo test --bin rdr`. End-to-end tests are in `tests/e2e.rs`.
- The e2e fake agent is a shell script named `claude` (`sleep "$1"`). Ubuntu's uutils coreutils rejects a renamed or symlinked `sleep`, even with `exec -a`, so don't go back to that.
- Tests set `RDR_TOKENS_CACHE=off` so the token disk cache doesn't leak between runs.

## Layout

- `server.rs` / `client.rs`: tmux-like persistent sessions.
  - The server owns the PTY panes and renders frames through a remote backend.
  - The client sends crossterm events as JSON lines over a Unix socket (`ipc.rs`).
  - The server runs in a systemd scope when available, otherwise it detaches with setsid. `RDR_NO_SCOPE` turns the scope off.
- `pane.rs`: portable-pty plus vt100. `ui.rs`, `layout.rs`, `keys.rs`: ratatui UI.
- `procinfo.rs`: detects agents and their state from `/proc`, which is Linux only.
- `desktop.rs`: KDE/freedesktop D-Bus badge and notifications. Fails silently when D-Bus isn't there.
- `tokens.rs`: token telemetry.
  - Generic `JsonlSpec` sources (built in: claude, pi). Users add more with `[[tokens.jsonl]]` in the config.
  - OpenCode (sqlite) and agy (protobuf) have their own readers.
  - The scanner is incremental and cached at `$XDG_CACHE_HOME/rdr/tokens.json`. When a file vanishes, shrinks or changes inode, its whole source is rescanned so dedup stays exact.
  - `RDR_TOKENS_TIMING` prints per-source timings.
- `config.rs`: TOML config (`config.example.toml`).
- `packaging/`: desktop file and icon.

## Releasing

Bump `version` in `Cargo.toml` and commit. Wait for CI to pass, then `git tag vX.Y.Z && git push --tags`. Not published to crates.io yet; that needs the owner's login.

## Open work

- See `TODO.md`: native Windows (PowerShell/ConPTY) port, macOS support, and checks on non-KDE desktops.
- A GitHub Pages site under `docs/` was handed to Jules, session `17706326242193196556`.
  - Review it before merging. It should add only `docs/`, with image slots at `docs/assets/screens/*.png`, and no Rust, test or CI changes.
  - After merging, turn on Pages in the repo settings: master, `/docs`.
