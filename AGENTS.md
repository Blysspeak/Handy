# AGENTS.md

Diktor: a headless Rust daemon + CLI for real-time voice input on Hyprland
(Omarchy). Derived from the Handy fork; no Tauri, no webview, no frontend.

## Commands

```bash
export TRANSCRIBE_CMAKE_ARGS=-DBLA_VENDOR=OpenBLAS CARGO_BUILD_JOBS=2 CMAKE_BUILD_PARALLEL_LEVEL=2
cargo build --release
cargo test --release
cargo clippy --release --all-targets -- -D warnings   # unwrap/expect denied outside tests
(cd src/voice_session && python3 -m unittest)         # desktop helper tests
scripts/install.sh                                    # ~/.local/opt/diktor/<date>-<commit>/
```

Release is used for tests too: a debug build compiles transcribe-cpp a second time.

## Layout

- `src/main.rs` — CLI (`daemon`, `toggle`, `off`, `status`, `copy [N]`, `dismiss`, `devices`) and the daemon wiring.
- `src/config.rs` — `~/.config/diktor/config.toml`, XDG paths, `DIKTOR_*` overrides.
- `src/ipc.rs` — Unix socket in the runtime dir, one line in, one JSON line out.
- `src/engine.rs` — transcribe-cpp model slot and the fork's streaming voice loop.
- `src/audio.rs`, `src/audio_toolkit/` — microphone (or `--input-wav`), resampler, Silero VAD.
- `src/voice_session/` — the fork's phrase state machine (`state.rs`), manager (`mod.rs`),
  desktop helpers (`hyprland_bridge.py` on top of `kde_bridge.py`), bar state file (`bar_state.rs`).
- `src/journal.rs` — `diktor.db`: every phrase pending before its paste, then delivered/failed.
- `contrib/` — example config, systemd user unit, Omarchy bar widget.

## Rules

- Voice behaviour in `voice_session/state.rs` and `mod.rs` is the owner's decision; do not change it casually.
- Live checks never use the real microphone or play audio: `diktor daemon --input-wav`,
  a private scratch window, `DIKTOR_ONLY_WINDOW` set to it.
- Commits: conventional prefixes, message says why.
