# Dependencies

This is the authoritative record of cmdash's dependency decisions, reconciled
with `Cargo.toml`. It is not a lockfile — version pins live in `Cargo.toml` /
`Cargo.lock`. Phase 20 in [ROADMAP.md](ROADMAP.md) documents the full
"reinvention review" that produced the keep-bespoke verdicts below.

## Adopted dependencies (in `Cargo.toml`)

| Crate | Role |
| --- | --- |
| `alacritty_terminal` 0.26 | Per-session terminal emulator (grid, modes, cursor, scrollback, parsing). Kitty APC is intercepted by cmdash, which owns the graphics store. |
| `crossterm` 0.29 | Raw mode, keyboard/mouse input, resize, cursor, basic terminal control. |
| `portable-pty` 0.9 | PTY open, spawn, resize, child lifecycle. |
| `ratatui` 0.29 | Layout primitives (`Rect`) only; scene/compositor are cmdash-owned. |
| `unicode-width` 0.2 | Display-width and wide-cell continuation tracking. |
| `serde` + `toml` 0.9 | Versioned TOML configuration parsing. |
| `serde_json` | The compositor API envelopes. |
| `flate2` | RFC 1950 zlib decode for Kitty `o=z` payloads. |
| `png` + `gif` | Kitty `f=100` (PNG) decode and GIF auto-animation decode. |
| `base64` 0.22 | Kitty payload encoding/decoding (replaced a hand-rolled encoder). |
| `clap` 4 | CLI parsing (replaced the hand-rolled `--config`/`-c` match). |
| `directories` 5 | XDG config/cache/crash roots (replaced hand-rolled discovery). |
| `thiserror` 2 | Typed error definitions (replaced 26 hand-rolled `Display` impls). |
| `libc` | Pixel-size ioctl and narrow Unix syscall access. |
| `wasmtime` (optional) | Opt-in `wasm-plugins` isolation host (dormant). |
| `notify` 7 (optional) | Event-driven config reload-on-save; compiled only with the `watch` feature. |
| `image` 0.25 (optional) | JPEG/BMP decode for the script-widget/dashboard image path; compiled only with the `image` feature. |

## Async model

There is no `tokio`. Per-session PTY reads run on standard-library reader
threads that push bytes through bounded channels and notify the coordinator
with a coalescing wakeup; frame composition stays on one UI/coordinator owner.
Blocking-on-PTY readers do not need an async runtime.

## Feature-gated dependencies

| Feature | Dependency | Status |
| --- | --- | --- |
| `sixel` | none (local 220-line 16-color encoder in `src/sixel.rs`) | Opt-in; the default build is dependency-free here. |
| `watch` | `notify` 7 | Event-driven config reload-on-save (watches the config file's parent directory); the default build keeps metadata-polled `Ctrl+R` reload. |
| `image` | `image` 0.25 (jpeg + bmp only) | JPEG/BMP `decode_image` for the script-widget `@@CMDASH_IMAGE` directive; WebP is skipped because its decoder is not vendored for offline builds. |
| `wasm-plugins` | `wasmtime` 47 | Import-free module validation and per-instance isolation. A dormant foundation: the host-function ABI is not yet exposed, so this is not the product's extension model. |

The Kitty protocol slice stays on the narrower `png`+`gif` crates because
in-band `f=100` is PNG-only; the `image` feature is a separate decode path for
dashboard-owned images.

## Deliberately bespoke

Phase 20 confirmed these stay in-house — they are the novel core, not
reinventions of something available elsewhere:

- **Kitty graphics store + VT scroll observer** (`src/graphics.rs`,
  `src/virtual_buffer.rs`). `little-kitty`, `kitty-graphics-protocol`, and
  `ratatui-image` are *client-side* encoders for an app drawing its own images
  to its own terminal; they cannot parse a child process's APC stream or model
  per-session retained state, stable `p=` re-placement, ack-gated GC,
  relative/virtual placements, or the Unicode-placeholder/tmux-passthrough
  re-emission modes cmdash needs as a multiplexer. No emulator-side crate
  exists; tmux does not re-emit Kitty at all.
- **Scene/compositor/frame-diff** (`src/scene.rs`, `src/compositor.rs`).
  ratatui is immediate-mode: its `Buffer`/`Frame` are rebuilt per draw and do
  not carry retained diffs, session-qualified graphics, occlusion, or cursor
  ownership. cmdash's retained model is what makes tab restoration and
  protocol-faithful image lifetime work.
- **Widget runtime/layout/coordinator** (`src/widget.rs`, `src/layout.rs`,
  `src/state.rs`). Owns session/graphics isolation and persistence that no
  framework models.
- **Animation scheduler** and **keymap grammar**. Coordinator-owned motion and
  crossterm-typed key tokens have no off-the-shelf equivalent.
- **Sixel encoder**. Deliberately bounded and dependency-free; adopt
  `sixel-rs`/`tty-sixel`/`libsixel` only if truecolor sixel fidelity is ever
  required.

## Considered but not adopted

These were evaluated and set aside. Revisit only if a concrete need reappears.

- `termion`, `termwiz` — terminal backends; `crossterm` is the selected one.
- `terminfo` — capability detection; env-var hints + active probing are used
  instead.
- `vte`, `vt100`, `wezterm-term` — parser/emulator alternatives;
  `alacritty_terminal` is the selected emulator.
- `nix` — Linux syscall/process helpers; `portable-pty` + narrow `libc`
  adapters cover the need.
- `ratatui-image`, `little-kitty`, `kitty-graphics-protocol` — client-side
  image encoders; direction is inverted for a multiplexer (see above).
- `abi_stable`, `libloading`, `wasmer`, `wit-bindgen`/Component Model — native
  plugin ABI strategies; the plugin path is dormant and script widgets are the
  extension model.
- `tracing`/`tracing-subscriber` — structured logging; current recovery
  diagnostics are bounded and rendered in-app.
- `proptest`, `insta`, `criterion`, `assert_cmd`, `tempfile` — testing crates;
  added only on a concrete profile/test need.
- `compact_str`, `unicode-segmentation`, `unicode-truncate`, `url` — small
  helpers; added only if profiling or a feature justifies them.

## Selection policy

1. Preserve session and graphics isolation.
2. Prefer mature crates with focused ownership boundaries over all-in-one
   frameworks that force the rendering model.
3. Keep the core testable without an interactive terminal.
4. Minimize unsafe code and avoid exposing Rust's unstable ABI to plugins.
5. Feature-gate optional terminal protocols and heavyweight runtimes.
6. Review dependency updates deliberately.

## Testing and quality

- `cargo-fuzz` fuzzes escape/protocol parsing, config migration, plugin-manifest
  handling, and sixel encoding; minimized seed corpora are checked in under
  `fuzz/corpus/`. This is a development tool rather than a runtime dependency.
- `wasmtime` is optional and should not be enabled for the default binary or
  fuzz harness unless a plugin-runtime test requires it.
- A key regression test: write a Kitty image with ID `1` in tab A, write a
  different image with ID `1` in tab B, switch A → B → A, and verify that each
  tab restores its own image without cross-contamination.
