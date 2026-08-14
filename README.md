# cmdash

`cmdash` is a Linux terminal application that combines a configurable dashboard with terminal-multiplexer capabilities. It is being designed as a modular compositor: a user can assemble a workspace from terminal sessions, dashboards, and other widgets without requiring terminal sessions at all.

The project is intentionally starting with architecture and behavior contracts before implementation. The most important rendering requirement is that every terminal session owns an independent terminal-emulation and graphics state. Kitty graphics, sixel content, cursor state, scrollback, and other visual state must remain isolated to the session/tab that produced them and be restored when that session becomes visible again.

## Project documents

- [Architecture](docs/ARCHITECTURE.md) — components, render pipeline, state ownership, and proposed Rust boundaries.
- [Roadmap](docs/ROADMAP.md) — staged implementation plan and acceptance criteria.
- [External library candidates](docs/DEPENDENCIES.md) — categorized crate list, evaluation criteria, and selection risks.

## License

cmdash is licensed under the [MIT License](LICENSE).

## Initial principles

1. **Modularity first:** widgets are optional, composable, and not coupled to terminal sessions.
2. **Session isolation:** each terminal tab has its own PTY, emulator, render state, and graphics resource namespace.
3. **Retained rendering:** widgets produce renderable scene data; the backend owns terminal I/O and frame submission.
4. **External crates where practical:** parsing, PTY management, async execution, layout, and terminal backends should use mature Rust libraries rather than bespoke implementations.
5. **Capability-aware behavior:** terminal features are detected and negotiated; unsupported graphics protocols degrade without corrupting layout or text.
6. **Testable core:** terminal state, layout, composition, and protocol handling should be testable without an attached interactive terminal.

## Status

Phase 2 complete. The project has a backend-neutral retained scene, a configuration-driven layout tree with columns, tabs, stacks, and overlays, and a widget runtime with lifecycle hooks, health reporting, clean shutdown, text, UTC clock, and system-information widgets. A versioned C-compatible plugin host contract is exercised by an external-widget fixture, while `--config <path>` / `-c <path>` loads user-provided TOML. Dynamic library loading and terminal sessions remain future work.
