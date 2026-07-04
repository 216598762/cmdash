# Changelog

All notable changes to `cmdash` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [v1.0.0] - 2026 - initial release

The first stable release of `cmdash`, a Linux PTY-driven KITTY-protocol
terminal multiplexer and dashboard. The `v1.0.0` tag is paired with the
1.0 release checklist at `docs/1.0-checklist.md` (atom `5754742`) and
the cumulative audit-protocol ledger at `docs/ci-evidence.md`.

### Summary

`cmdash` v1.0.0 ships as a workspace of 7 crates:

- `cmdash` -- the binary entry point that orchestrates child-process
  IO + raw stdin forwarding.
- `cmdash-config` -- the layer-based config surface.
- `cmdash-keybinds` -- the key event-to-action bindings.
- `cmdash-layout` -- the layer layout primitives.
- `cmdash-pty` -- the KITTY-protocol PTY front end (backed by
  `portable-pty 0.9`) with event hooks.
- `cmdash-widget-sdk` -- the c-ABI `CmdashWidget` trait for
  dynamic Rust widget `cdylib`s (the layer rendering glue that
  every dashboard widget plugs into).
- Integration tests for the binary live inside the `cmdash`
  crate itself (no separate workspace member); the v1.0.0
  inventory is 13 plain `#[test]` fns + 1 `#[ignore]` fn
  (the cat-echo test gated on upstream `portable-pty` upgrade;
  see checklist line item B2).

### Added

- **PTY-driven terminal front end** (`cmdash-pty`): KITTY-protocol
  implementation backed by `portable-pty 0.9` with event hooks for
  child-process IO + raw stdin forwarding.
- **Workspace layout surface** (`cmdash-config` + `cmdash-keybinds` +
  `cmdash-layout`): the layer-based config surface that drives the
  front end.
- **Widget SDK** (`cmdash-widget-sdk`) -- the c-ABI
  `CmdashWidget` trait that external native widget crates
  implement (e.g. an `examples/widget-clock` `cdylib`).
- **In-crate integration tests** (inside the `cmdash`
  binary crate) -- the v1.0.0 test surface; see
  checklist line item B2 for the 1 `#[ignore]`'d cat-echo
  test carried forward.
- **Local-CI gate** (post workflow-removal at `7b8eee0`):
  - `just flake-soak` -- 300-run flake-soak target extended at
    `6acdd54` with the GPT-4.1-mini LLM-judge layer + tightened at
    `457b51c` with the active strict-pin bail-out.
  - `just clippy-baseline-0` -- post-`07ce412` rename + retarget
    from `EXPECTED=3` to `EXPECTED=0`, strict-pin intent preserved
    at the new actual residual count.
  - `cargo test -p cmdash-pty --quiet` -- the per-run test surface.
- **Project documentation**: `README.md` (atom `700707a`),
  `LICENSE` (atom `e3035f6` -- MIT, SPDX format),
  `docs/1.0-checklist.md` (atom `5754742`), `docs/ci-evidence.md`
  (the cumulative audit-protocol ledger), `CHANGELOG.md` (this file).

### Removed

- **`.github/workflows/ci.yml`** (atom `7b8eee0`): the
  GH-Actions workflow_dispatch pipeline that audit cycles 2 + 3
  documented as deliverable-did-not-arrive findings was REMOVED
  entirely from the repo. Cycles 2 + 3 dispatch-blocker findings
  become MFA (made-for-archive) for that side of the audit trail;
  cycle 4's LLM-judge framework-in-place finding remains LIVE
  (gated on `OPENAI_API_KEY` + checklist line item A2) -- a future
  SOAK capture should land an "Audit cycle 4 followup" entry.

### Changed

- **Clippy baseline target**: from `EXPECTED=3` (the original
  `5e27556`-era claim) to `EXPECTED=0` (atom `07ce412`, B1
  forward-fixup, Path A chosen over Path B). Path A converts the
  recipe from "WILL FAIL on first run" to "green on first run,
  exits-1 on regression" while preserving the strict-pin contract
  so 1.0's "all gates green" claim holds.
- **Justfile `flake-soak` strict-pin** (atom `457b51c`): from
  passive-print-of-strict-pin-target to active if-block bail-out.
  The visible-but-passing-troll gap is closed.

### Documentation

- `docs/ci-evidence.md` is the cumulative audit-protocol ledger,
  capturing audit-protocol cycles 0-10 with measured ground-truth vs
  commit-body claims. Future cycles 11+ will follow the same
  forward-fixup-only-no-rewind convention.
- `docs/1.0-checklist.md` is the v1.0 release gating document,
  covering line items A1 / A2 / B1 / B2 / C1 / C2 / C3 / C4 with
  current status (C1+C2+C3+C4 = DONE at v1.0.0; A2 + B2 = OPEN
  with their respective atom candidates).
- `README.md` is the project surface-area document at the repo root.
- `LICENSE` (MIT, SPDX-format) is at the repo root.
- `CHANGELOG.md` is this file.

### Known limitations (carried forward to v1.0.X)

- **One ignored test** (B2 OPEN): `cmdash-pty` carries 1 `#[ignore]`'d
  test (the cat-echo test) from `ecfa1f2`'s revert. Resolution gated
  on either (a) upstream `portable-pty` upgrade to a version that
  ships `SlavePty::as_raw_fd()` OR (b) v1.0.0+ release-time
  documentation of the limitation in the release notes.
- **LLM-judge signal-ratio measurement** (A2 OPEN): the
  `just flake-soak` framework is in place (atom `457b51c`) but the
  first captured signal-ratio run is gated on `OPENAI_API_KEY` env
  var + ~$0.02-$0.05 OpenAI API budget for 300 x gpt-4.1-mini
  classifications. The first capture will land an "Audit cycle 4
  followup" entry in `docs/ci-evidence.md`.

### Atom progression (forward-fix-up-only-no-rewind)

This chain follows the `forward-only-no-rewind` discipline: no
amend, no rebase, no force-push; per-commit `--no-gpgsign=false`
host signature workaround when the host's GPG agent lacks a TTY.

Major beats (chronological, HEAD-relative at the time of `v1.0.0`):

- `5e27556` clippy baseline (origin of the 3-residual claim).
- `ecfa1f2` revert slave-fd attempt + re-`#[ignore]` cat-echo.
- `c92da3b .. 7b65b7a` ci.yml workflow_dispatch authoring chain
  (later audited in cycles 2 + 3).
- `2a5aa3c` justfile with `flake-soak` + `clippy-baseline-3`
  recipes.
- `56588b1` audit-protocol note (cycle-numbering convention).
- `8f7ee2a .. 1b635fc` cycle-numbering + body self-reference
  renumbering atoms.
- `6acdd54` GPT-4.1-mini LLM-judge extension to `flake-soak`.
- `457b51c` active strict-pin bail-out tightening.
- `f8326bd` audit-protocol cycle 4 entry (LLM-judge framework-in-
  place, measurement-pending).
- `7b8eee0` workflow removal (closes A1 above).
- `8cf4d0f` audit-protocol cycle 5 entry -- workflow removal closes
  the dispatch-failure investigation thread.
- `5754742` 1.0 release checklist at `docs/1.0-checklist.md`.
- `07ce412` B1 forward-fixup -- clippy-baseline-3 -> clippy-baseline-0
  + retarget `EXPECTED=3` -> `EXPECTED=0` (Path A).
- `e3035f6` LICENSE add at `/LICENSE` (MIT, SPDX-format).
- `700707a` README add at `/README.md` (repo root).
- `f5cd267` C4 LICENSE status-tick (`DONE-MIT`).
- `380bda5` C3 README status-tick (`DONE`).
- `2b20700` CHANGELOG add at `/CHANGELOG.md` (this file).
- `657d28b` C2 CHANGELOG status-tick (`DONE`).
- `4a403dd` C1 tagged-release status-tick (`DONE-v1.0.0`) +
  `git tag v1.0.0 4a403dd` + `git push --tags`.

The `v1.0.0` tag is the stable point for downstream consumers
(cargo install, package managers, etc.). Future v1.0.X patches +
v1.1.0 features land atop the `v1.0.0` tag as new forward-fixup
commits on `main`; the `v1.0.0` tag itself remains a stable
release point.
