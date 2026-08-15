# External library candidates

This is an evaluation list and initial direction, not a dependency lockfile. The decisions recorded below are the starting point for the package skeleton; each crate still needs to be pinned and verified when it is added. Before adding a library, verify its current API, maintenance activity, license compatibility with MIT, transitive dependencies, Linux behavior, and fit with cmdash's retained per-session scene model.

## Stage 1 decisions

| Concern | Initial direction | Boundary or gate |
| --- | --- | --- |
| Terminal backend | `crossterm` | Owns raw mode, input, resize, and basic controls; cmdash owns retained scenes and frame composition. |
| Terminal emulator | `alacritty_terminal` | One emulator per session; Kitty APC sequences are intercepted by a cmdash-owned session adapter because graphics resources are not global emulator state. |
| PTY and async runtime | `portable-pty` + `tokio` | Per-session I/O tasks communicate with the UI/coordinator through bounded messages. |
| Layout primitives | `ratatui` + `unicode-width` | Use Ratatui layout/text primitives behind the backend-neutral scene boundary and track narrow/wide cell occupancy explicitly. |
| Plugin boundary | Versioned native ABI | Active v1 host descriptor, manifest schema, widget settings map, and capability negotiation; dynamic loading remains a later isolation gate. |
| Workspace scope | One active workspace | Add saved/multiple workspace behavior only after the core runtime contracts are stable. |
| Graphics fallback | Capability-aware Kitty/sixel adapters | Unsupported or over-limit graphics are omitted with in-app degraded diagnostics; Kitty layers are replayed only when supported and sixel is an opt-in feature. |


## Selection priorities

1. Preserve session and graphics isolation.
2. Prefer mature crates with focused ownership boundaries over all-in-one frameworks that force the rendering model.
3. Keep the core testable without an interactive terminal.
4. Minimize unsafe code and avoid exposing Rust's unstable ABI to dynamic plugins.
5. Feature-gate optional terminal protocols and heavyweight plugin runtimes.
6. Pin compatible versions in the eventual Cargo manifest and review updates deliberately.

## Likely initial dependencies

These are the strongest candidates for the first executable once the package skeleton is created.

| Area | Candidate | Intended use | Status / risk |
| --- | --- | --- | --- |
| Terminal I/O | [`crossterm`](https://crates.io/crates/crossterm) | Raw mode, keyboard/mouse input, resize events, cursor and basic terminal control | Selected initial backend; graphics submission and retained scene output remain behind a cmdash-owned boundary. |
| PTYs | [`portable-pty`](https://crates.io/crates/portable-pty) | Spawn shells/processes, read/write PTY streams, resize sessions | Active v0.9 integration; continue validating Linux process lifecycle, signal handling, and shutdown semantics. |
| Async runtime | [`tokio`](https://crates.io/crates/tokio) | Event coordination, PTY I/O tasks, timers, bounded channels, cancellation | Selected initial direction; keep frame composition on one coordinator/UI owner. |
| Serialization | [`serde`](https://crates.io/crates/serde) | Versioned widget/application configuration types | Active in the initial widget configuration model; avoid serializing live PTY/emulator state in the first release. |
| Configuration format | [`toml`](https://crates.io/crates/toml) | Initial hand-authored workspace and widget configuration | Active version-1 parser with safe metadata-polled reload; keep schema/version migration explicit. |
| Diagnostics | [`tracing`](https://crates.io/crates/tracing) + [`tracing-subscriber`](https://crates.io/crates/tracing-subscriber) | Structured logs for sessions, plugins, frame timing, and protocol failures | Future structured logger; current recovery diagnostics are bounded and rendered in-app. |
| Error types | [`thiserror`](https://crates.io/crates/thiserror) + [`anyhow`](https://crates.io/crates/anyhow) | Typed library errors and application-level context | Strong candidates; use typed errors at plugin/session boundaries. |
| User paths | [`directories`](https://crates.io/crates/directories) | XDG-compatible config, cache, data, and plugin discovery paths | Strong candidate; confirm exact Linux/XDG behavior needed by the app. |
| CLI | [`clap`](https://crates.io/crates/clap) | Startup flags, config path, diagnostics mode, and version output | Still a future option; current `--config` / `-c` parsing remains dependency-free. |

## Terminal backend and input

### `crossterm`

Provides cross-platform terminal control, raw mode, input events, colors, cursor movement, and resize handling. It is the leading backend candidate because it integrates naturally with Ratatui and is broadly used in Rust TUIs.

**cmdash boundary:** use it for terminal I/O and capability-neutral controls, but keep the frame/scene model independent. Kitty graphics output should be emitted by a graphics-aware backend adapter, not directly by widgets.

### `termion`

A smaller Unix-oriented alternative for raw mode and terminal control. It may be attractive for a Linux-only implementation, but it should be compared against Crossterm for input coverage, maintenance, and graphics integration before choosing it.

### `terminfo`

A possible capability-detection source for terminals that expose terminfo behavior. It may complement, rather than replace, environment-variable and active-query detection. Avoid assuming terminfo alone describes Kitty graphics support.

### `termwiz`

A terminal handling and escape-sequence ecosystem associated with WezTerm. It is worth evaluating if its terminal capability and rendering abstractions cover requirements better than a Crossterm-based backend, but it may bring a larger conceptual surface.

## Layout, text, and scene primitives

### `ratatui`

Provides terminal layout, text/style primitives, widgets, and backend integration. It is a strong candidate for layout calculations and ordinary dashboard widgets.

**Important constraint:** Ratatui is primarily frame/widget oriented. cmdash should not allow its default `Terminal`/`Frame` lifecycle to become the owner of terminal-emulator or Kitty graphics state. Use compatible primitives where helpful, or wrap it behind the cmdash scene boundary and retain ownership of composition ourselves.

Useful adjacent crates to evaluate only if needed:

- [`unicode-width`](https://crates.io/crates/unicode-width) — selected for Unicode display-width calculation and wide-cell continuation tracking;
- [`unicode-segmentation`](https://crates.io/crates/unicode-segmentation) — grapheme-aware text handling;
- [`unicode-truncate`](https://crates.io/crates/unicode-truncate) — width-aware truncation;
- [`compact_str`](https://crates.io/crates/compact_str) — compact short-string storage if profiling shows text allocation pressure.

Do not add all of these up front. The selected terminal emulator may already provide the required Unicode behavior.

## PTY and terminal emulation

### `portable-pty`

**Active at v0.9.** It owns OS-specific PTY setup while cmdash owns session identity, output polling, input routing, resize ordering, and lifecycle policy.

### `alacritty_terminal`

**Active at v0.26.** This is a full terminal-emulation implementation extracted from Alacritty and provides the grid state, alternate screen, modes, cursor, scrollback, and parsing that cmdash needs. Cmdash uses its parser/grid behind the backend-neutral scene boundary and intercepts Kitty APC sequences before they reach the text emulator; session graphics remain owned by cmdash.

**Phase 5 result:** the selected emulator exposes the parser boundary needed to intercept APC sequences, but does not expose a session-owned Kitty graphics store. Cmdash therefore strips Kitty APC commands before feeding text to the emulator and retains resources/placements in a `SessionGraphicsStore`; no global graphics cache is used.

### `vte`

A low-level ANSI/VT escape parser. It can be useful when the project owns the terminal state machine or needs a narrow parser boundary around a selected emulator. It is not, by itself, a complete terminal emulator or graphics-state model.

### `vt100`

A self-contained terminal-screen parser/model that may simplify early text-only session tests. Evaluate it as a fast path for Phase 3, but do not adopt it if its state model prevents Kitty graphics, scrollback, or required terminal modes from being represented faithfully.

### `wezterm-term`

A possible alternative full emulator from the WezTerm ecosystem. Evaluate only if its packaging/API and protocol coverage make it a better fit; its integration surface may be broader than a focused crate dependency.

### `nix`

Useful for Linux-specific process, signal, file-descriptor, and PTY operations that the selected PTY crate does not expose. Keep this behind a small Unix adapter and avoid duplicating `portable-pty` behavior.

## Graphics and image handling

### `ratatui-image`

Provides image widgets and protocol backends for Sixel, Kitty, iTerm2, and Unicode fallback rendering. It may help dashboard widgets display ordinary images and can serve as a reference implementation.

**Boundary:** it should not automatically become the source of truth for terminal-originated Kitty images. Session graphics remain owned by `SessionGraphicsStore` and are exposed to the retained scene as session-qualified image layers.

### `little-kitty`

A low-level Kitty graphics protocol interface candidate. The current Phase 5 slice uses a narrow local encoder because it only needs replay of captured, session-owned APC payloads; evaluate `little-kitty` later if broader upload/format support is required.

### `kitty-graphics-protocol`

Another focused Kitty protocol candidate found on crates.io. Compare its API, maintenance, protocol completeness, and license against `little-kitty` before choosing one; do not depend on two encoders for the same backend.

### `image`

General-purpose image decoding and pixel conversion. It may be useful for Kitty payloads, dashboard image widgets, thumbnails, and format normalization. Gate or isolate it if the final application does not need broad image format support.

### Graphics protocol implementation decision

The first graphics milestone should explicitly choose one of these approaches:

1. reuse graphics support exposed by the selected full terminal emulator;
2. use a focused Kitty protocol crate for backend encoding plus a cmdash-owned session store;
3. implement a narrowly scoped protocol adapter with conformance tests if existing crates do not expose the required state semantics.

A widget-level image crate alone does not satisfy the requirement that terminal-originated images persist independently per tab.

## Dynamic plugins

Dynamic plugins are an early product requirement. These candidates represent different ABI and isolation strategies:

### `abi_stable`

Provides tools for defining Rust libraries that can be loaded across compiler/crate-version boundaries. It is the leading native Rust plugin candidate to investigate.

**Risks:** plugin ABI design, versioning, panic/error containment, unsafe loading, platform packaging, and the fact that ABI stability does not automatically provide security isolation. Keep the host-facing data model small and versioned.

### `libloading`

Low-level cross-platform dynamic-library loading. It can support a deliberately designed C-compatible plugin ABI, but it leaves symbol safety, version negotiation, memory ownership, and lifecycle rules to cmdash.

### `wasmtime`

A WebAssembly runtime candidate for stronger isolation and a portable plugin format. It can be attractive for untrusted or third-party widgets, but adds runtime size, host-function design, serialization/copying concerns, and a separate rendering ABI.

### `wasmer`

Another WebAssembly runtime alternative. Compare it with Wasmtime only if WASM plugins are selected; do not carry both runtimes into the initial product.

### `wit-bindgen` / WebAssembly Component Model tooling

Worth evaluating if the plugin contract is designed around a language-neutral, versionable interface. This may be more future-proof than exposing Rust types, but the component model must represent widget lifecycle, input, messages, scene data, and resource ownership without excessive copying.

### Initial plugin direction

Use a versioned native ABI for the first external-widget prototype. The host-facing contract must use C-compatible data, explicit ABI and capability versions, owned message/scene buffers, and documented lifecycle and failure behavior. `abi_stable` and a hand-defined C ABI remain implementation candidates, but neither may expose Rust's unstable ABI or raw terminal/backend handles.

A WASM runtime remains a later option if stronger isolation or language neutrality becomes a product requirement; it is not part of the initial dependency set.

## Configuration, discovery, and live reload

- [`notify`](https://crates.io/crates/notify) — watch configuration/plugin directories; test event coalescing and editor-save patterns on Linux.
- [`directories`](https://crates.io/crates/directories) — platform-aware config, cache, and plugin roots.
- [`serde`](https://crates.io/crates/serde) and [`toml`](https://crates.io/crates/toml) — typed configuration and explicit schema evolution.
- [`serde_json`](https://crates.io/crates/serde_json) — useful for diagnostics, plugin manifests, IPC, or machine-readable state even if TOML remains the user format.
- [`url`](https://crates.io/crates/url) — only if widgets support URL-aware links/actions.

Plugin discovery uses a validated manifest shape with ABI/API version, widget types, capabilities, required permissions, and human-readable metadata. The current host validates this metadata without loading dynamic code; loading a plugin must not execute arbitrary code merely because an unrelated file appears in the config directory.

## Testing and quality

- [`proptest`](https://crates.io/crates/proptest) — a future property-test dependency for layout invariants, clipping, resource namespaces, and parser state transitions; deterministic parser stress coverage is active now.
- [`insta`](https://crates.io/crates/insta) — snapshot tests for scenes, layout trees, diagnostics, and serialized config.
- [`assert_cmd`](https://crates.io/crates/assert_cmd) — executable-level tests for CLI behavior and failure modes.
- [`tempfile`](https://crates.io/crates/tempfile) — isolated config, plugin, and PTY fixture directories.
- [`criterion`](https://crates.io/crates/criterion) — benchmark frame composition, high-volume PTY output, and graphics-store operations.
- `cargo-fuzz` — fuzz escape/protocol parsing, config migration, and plugin-manifest handling; this is a development tool rather than a runtime dependency.
- The optional `sixel` feature currently uses a local, dependency-free encoder; add an image/quantization dependency only after profiling demonstrates the need.

The most valuable first regression test remains: two tabs each use graphics image ID `1`, produce different images, switch A → B → A, and verify that the retained scenes and submitted placements remain isolated.

## Suggested implementation evaluation order

1. Add and pin `crossterm`, `ratatui`, `portable-pty`, `tokio`, `serde`, `toml`, `tracing`, and `directories` as the package skeleton requires them.
2. Integrate `alacritty_terminal` behind the per-session emulator boundary and verify the APIs needed for grid, alternate screen, cursor, scrollback, and resize behavior.
3. Verify Kitty support and choose the smallest suitable cmdash-owned adapter using `little-kitty`, `kitty-graphics-protocol`, or a local implementation; the current Phase 5 slice uses a local adapter with session-qualified IDs and visible-placement replay.
4. Prototype the versioned native plugin ABI with `abi_stable` or a hand-defined C ABI; evaluate Wasmtime only if isolation or language neutrality justifies it.
5. Add `notify`, `proptest`, `insta`, benchmarks, and other support crates as actual features require them.
6. Run the checked-in fuzz targets before releases and attach the reproducible Linux archive plus checksum generated by the release workflow.

The initial directions are now recorded, but dependency versions and protocol/API compatibility must still be verified in the Cargo package before they become locked implementation choices.
