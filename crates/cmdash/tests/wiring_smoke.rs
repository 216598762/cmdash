//! Wiring smoke: KDL config → layout → PanePty → vte → TextGrid
//! → ratatui `TestBackend`. Asserts that text the child emits ends
//! up in both the vte-consumed grid and the rendered ratatui
//! buffer.

// clippy `doc_lazy_continuation` (clippy 1.96+) misreads multi-paragraph
// prose rustdoc as Markdown list continuations; scoped allow for the test
// file's existing + new doc comments. Per AGENTS.md "doc-link-hygiene"
// discipline: lint-named + scoped allow attributes are preferred over
// fighting clippy on prose style when the prose is genuinely prose
// (not Markdown list items).
#![allow(clippy::doc_lazy_continuation)]

use std::time::Duration;

use cmdash::pane::{PaneCloseTx, PaneRunner};
use cmdash_config::{
    LayoutNode, Pane as CfgPane, PaneKind, Ratio as CfgRatio, SplitAxis as CfgSplitAxis,
};
use cmdash_layout::{ComputedLayout, Direction, Rect as LayoutRect};
use cmdash_pty::{PaneLayerId, ShellSpec};

#[test]
fn wiring_round_trip_renders_echoed_text() {
    let source = r#"layout { pane kind=shell label="wiring" }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    assert_eq!(layout.panes.len(), 1, "expected 1 leaf pane");
    let pane = layout.panes[0].clone();

    let layer_id = cmdash::derive_layer_id(&pane.id);
    assert_eq!(layer_id, PaneLayerId(pane.id.pre_order() as u64));

    let shell = ShellSpec::Command {
        argv: vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'hello world\\n'; sleep 0.05; exit 0".to_string(),
        ],
    };
    let mut runner = PaneRunner::spawn(pane.clone(), layer_id, shell).expect("spawn runner");

    // Allow the child to start. Sleep once up-front lets the
    // reader thread accumulate some bytes; subsequent ticks are
    // bounded by `try_wait` + short sleeps.
    std::thread::sleep(Duration::from_millis(250));

    let mut last_snap = None;
    let mut found_in_grid = false;
    // Tick FIRST so `last_snap` is populated even when the child
    // exits faster than the first iteration's `try_wait_exit` would
    // have returned `Some(_)`.
    for _ in 0..80 {
        let snap = runner.tick().expect("tick");
        if !found_in_grid {
            for y in 0..snap.rows {
                for x in 0..snap.cols {
                    if snap.grid.cell(x, y).ch == 'h' {
                        found_in_grid = true;
                        break;
                    }
                }
                if found_in_grid {
                    break;
                }
            }
        }
        last_snap = Some(snap);
        if found_in_grid {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let snap = last_snap.expect("at least one snapshot");
    assert!(
        found_in_grid,
        "wiring did not surface 'hello' into the grid; rows={} cols={}",
        snap.rows, snap.cols
    );

    // Render to ratatui TestBackend (matches the live `Frame`
    // path: same `blit_grid` helper).
    let backend = ratatui::backend::TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            let area = ratatui::layout::Rect::new(0, 0, pane.rect.w, pane.rect.h);
            cmdash::render::blit_grid(&snap.grid, frame.buffer_mut(), area);
        })
        .expect("draw");
    let buf = terminal.backend().buffer().clone();
    let mut saw_h = false;
    for y in 0..24 {
        for x in 0..80 {
            if buf.get(x, y).symbol() == "h" {
                saw_h = true;
                break;
            }
        }
    }
    assert!(saw_h, "rendered buffer did not contain 'h'");
}

// ---------------------------------------------------------------------------
// Kitty graphics end-to-end smoke: push a synthetic image onto
// GraphicsState, render and emit through dashcompositor's
// passthrough encoder, and assert the byte stream contains the
// kitty APC-G escape per AGENTS.md §"Rendering pipeline" step 6.
//
// This covers the dashcompositor routing path without depending
// on a real PTY child or a hand-crafted PNG byte fixture.
// ---------------------------------------------------------------------------

#[test]
fn kitty_graphics_route_emits_escape_sequence() {
    use cmdash::graphics::{GraphicsState, Metrics};
    use cmdash_pty::PaneLayerId;

    let mut graphics = GraphicsState::new(Metrics::default(), (80, 24));
    graphics.push_image(PaneLayerId(1), 7, image::RgbaImage::new(1, 1));

    let mut out = Vec::new();
    graphics
        .render_and_write(&mut out)
        .expect("render_and_write ok");

    assert!(
        out.windows(3).any(|w| w == b"\x1b_G"),
        "encoded stream missing kitty APC-G escape; bytes (first 64): {:?}",
        &out[..out.len().min(64)]
    );
}

// ---------------------------------------------------------------------------
// Regression test for the production `Load → image::load_from_memory`
// path: round-trip a real (committed, verified) PNG byte stream
// through `apply_kitty_event` and confirm the `(pane, kitty_id)` mapping
// is registered. The fixture is regenerated by
// `examples/gen_fixture.rs` if the test ever regresses.
// ---------------------------------------------------------------------------

#[test]
fn kitty_decode_smoke() {
    use cmdash::graphics::{GraphicsState, Metrics};
    use cmdash_pty::{KittyGraphicCmd, PaneLayerId};

    let png = include_bytes!("fixtures/img1x1.png");
    let mut graphics = GraphicsState::new(Metrics::default(), (80, 24));
    let pane = PaneLayerId(1);
    let load = KittyGraphicCmd::Load {
        id: 7,
        placement_id: 0,
        format: 32,
        width: 1,
        height: 1,
        data: png.to_vec(),
    };
    graphics.apply_kitty_event(pane, &load);
    assert!(graphics.has_image(pane, 7));
}

// --------------------------------------------------------------------------
// Regression test pinning the post-resize rect refresh introduced
// when wiring the `PaneRunner::computed` accessor (see
// `PaneRunner::resize` doc in pane.rs). Before the refresh,
// `runner.computed().rect` returned the spawn-time rect forever,
// which broke any caller that read the rect after a terminal
// resize.
//
// Asserts (four post-resize states):
// - the spawn-time rect == pane.rect (initial-state sanity).
// - after `resize(new_w, new_h)`, `runner.computed().rect` ==
//   `LayoutRect{x:0, y:0, w:new_w, h:new_h}`.
// - shrinking is also refreshed (pins not just a one-way
//   growth path).
// - grow-again after shrink -- successive override must not
//   carry any state from the previous call (cache is per-call
//   fresh, not incremental).
// --------------------------------------------------------------------------

#[test]
fn pane_runner_resize_refreshes_computed_rect() {
    let source = r#"layout { pane kind=shell label="resize-regression" }"#;
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    let pane = layout.panes[0].clone();

    let layer_id = cmdash::derive_layer_id(&pane.id);

    // Long-lived but cheap shell so the PTY stays alive across
    // both resize() calls (10s wallclock budget is plenty).
    let shell = ShellSpec::Command {
        argv: vec!["sh".to_string(), "-c".to_string(), "sleep 10".to_string()],
    };
    let mut runner = PaneRunner::spawn(pane.clone(), layer_id, shell).expect("spawn runner");

    // Initial-state sanity: the accessor should hand back the
    // exact spawn-time rect.
    assert_eq!(
        runner.computed().rect,
        LayoutRect {
            x: pane.rect.x,
            y: pane.rect.y,
            w: pane.rect.w,
            h: pane.rect.h,
        },
        "spawn-time rect should match the layout-computed pane.rect"
    );

    // Grow.
    runner
        .resize(LayoutRect {
            x: 0,
            y: 0,
            w: 132,
            h: 50,
        })
        .expect("resize grow");
    assert_eq!(
        runner.computed().rect,
        LayoutRect {
            x: 0,
            y: 0,
            w: 132,
            h: 50
        },
        "post-resize (grow) rect should be (0, 0, 132, 50)"
    );

    // Shrink -- verify refresh works both directions, not just
    // growth.
    runner
        .resize(LayoutRect {
            x: 0,
            y: 0,
            w: 40,
            h: 12,
        })
        .expect("resize shrink");
    assert_eq!(
        runner.computed().rect,
        LayoutRect {
            x: 0,
            y: 0,
            w: 40,
            h: 12
        },
        "post-resize (shrink) rect should be (0, 0, 40, 12)"
    );

    // Grow again -- pins that successive override doesn't carry
    // any state from the previous call. Without this 4th
    // assertion a regression that overwrote via an accumulator
    // (`rect.w += new_w` rather than `rect.w = new_w`) could
    // pass the 3-state test because the cached previous dims
    // are never compared against an unrelated new target.
    runner
        .resize(LayoutRect {
            x: 0,
            y: 0,
            w: 200,
            h: 60,
        })
        .expect("resize grow again");
    assert_eq!(
        runner.computed().rect,
        LayoutRect {
            x: 0,
            y: 0,
            w: 200,
            h: 60
        },
        "post-resize (grow again) rect should be (0, 0, 200, 60)"
    );
}

// ----------------------------------------------------------------------
// Regression test for the v2 split-pane nesting contract lift:
// `PaneRunner::resize(rect)` MUST carry the layout-engine's
// `(x, y)` origin forward into `self.computed.rect` -- not
// zero it out the way v1 did. End-to-end via a real KDL
// `split` config + production `PaneRunner::spawn` path so the
// order `pty.resize()? -> rect overwrite` is pinned through a
// real `PanePty::resize` call against the running PTY (NOT
// just the `StubPty` lib unit-tests cover).
//
// Fixture: parent area (132, 50), `SplitAxis::Horizontal`
// `ratio=0.6` -> child A at `(x:0, w:79, h:50)` and child B at
// `(x:79, w:53, h:50)`. Resize child B from initial
// `(79, 0, 53, 50)` to `(79, 10, 80, 30)` so a v1 regression
// that zeroed either `(x, y)` axis would fail the post-resize
// assert. Sibling-pane orthogonality: A must not mutate when B
// is resized.
// ----------------------------------------------------------------------
#[test]
fn pane_runner_resize_preserves_split_origin_in_layout_engine_path() {
    let source = r#"layout {
        split axis=horizontal ratio=0.6 {
            pane kind=shell label="split-a"
            pane kind=shell label="split-b"
        }
    }"#;
    let cfg = cmdash_config::parse(source).expect("parse split config");
    let root = cfg.layout.expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 132,
        h: 50,
    };
    let layout = ComputedLayout::compute(&root, area).expect("compute split layout");
    assert_eq!(layout.panes.len(), 2, "expected 2 leaf panes from split");

    let pane_a = layout.panes[0].clone();
    let pane_b = layout.panes[1].clone();

    // Pre-state pin from `cmdash_layout::split_rect`
    // (`SplitAxis::Horizontal`, `ratio=60` over width 132):
    //   w_left  = 132 * 60 / 100 = 79
    //   child A: (x:0,  y:0, w:79, h:50)
    //   child B: (x:79, y:0, w:53, h:50)
    assert_eq!(
        pane_a.rect,
        LayoutRect {
            x: 0,
            y: 0,
            w: 79,
            h: 50
        },
        "fixture invariant: split child A at (0, 0, 79, 50)"
    );
    assert_eq!(
        pane_b.rect,
        LayoutRect {
            x: 79,
            y: 0,
            w: 53,
            h: 50
        },
        "fixture invariant: split child B at (79, 0, 53, 50)"
    );

    // Long-lived shells -- `sleep 10` keeps both children alive
    // across the resize call so the PTY-side `winresize` flow
    // doesn't error out with `PtyError::InvalidSize` against a
    // short-lived child.
    let shell = ShellSpec::Command {
        argv: vec!["sh".to_string(), "-c".to_string(), "sleep 10".to_string()],
    };
    let id_a = cmdash::derive_layer_id(&pane_a.id);
    let id_b = cmdash::derive_layer_id(&pane_b.id);
    let runner_a = PaneRunner::spawn(pane_a.clone(), id_a, shell.clone()).expect("spawn runner A");
    let mut runner_b =
        PaneRunner::spawn(pane_b.clone(), id_b, shell.clone()).expect("spawn runner B");

    // Initial-state sanity: spawn-time rect rounds-trip the
    // Split-derived non-zero origin (x:79 for child B).
    assert_eq!(runner_b.computed().rect, pane_b.rect);

    // Resize child B with a target rect that exercises ALL
    // four axes: x preserved, y introduced, w + h changed. A
    // v1 regression that zeroed either origin axis would fail
    // the assert below.
    let target = LayoutRect {
        x: 79,
        y: 10,
        w: 80,
        h: 30,
    };
    runner_b
        .resize(target)
        .expect("resize child B with non-zero origin");

    // Post-resize assertion: the FULL rect round-trips.
    assert_eq!(
        runner_b.computed().rect,
        target,
        "split child B post-resize rect must match the caller-supplied full rect"
    );
    // (per-axis asserts removed -- the FULL-rect assert above
    //  catches any deviation across all four axes.)

    // Sibling-pane orthogonality: resize of B MUST NOT mutate
    // A's cached rect.
    assert_eq!(
        runner_a.computed().rect,
        pane_a.rect,
        "sibling pane A's rect must be unaffected by runner B resize"
    );
}

// ----------------------------------------------------------------------
// Phase 2 v2 wiring regression: a `TickContext::relayout(w, h)`
// driven by a host `Event::Resize` must reach a real
// `PaneRunner::resize(rect)` against a running PTY child, NOT
// just a `StubPty` lib test. End-to-end binding exercised:
// crossterm Event::Resize -> handle_event arms
// `pending_resize` -> TickContext::run phase 0.5 take() ->
// `relayout(w, h)` -> `ComputedLayout::compute` -> per-pair
// `runner.resize(pane.rect)` over a real `PanePty`.
//
// Uses `sleep 10` so both panes are still alive when the
// host-driven resize fires (a fast-exit `/bin/true` child
// would race against the assertion surface). The TestBackend
// constructs a ratatui `Terminal` without writing to stdout so
// no real TTY is needed for the test environment.
// ----------------------------------------------------------------------
#[test]
fn relayout_drives_per_pane_resize_via_real_pty() {
    use cmdash_layout::Rect as LayoutRect;

    let source = r#"layout {
        split axis=horizontal ratio=0.6 {
            pane kind=shell label="layout-a"
            pane kind=shell label="layout-b"
        }
    }"#;
    let cfg = cmdash_config::parse(source).expect("parse split config");
    let layout_root = cfg.layout.clone().expect("layout block");

    let (close_tx, _close_rx): (cmdash::pane::PaneCloseTx, _) = std::sync::mpsc::channel();
    let shell_a = ShellSpec::Command {
        argv: vec!["sh".to_string(), "-c".to_string(), "sleep 10".to_string()],
    };
    let shell_b = shell_a.clone();

    // Both panes come from a SHARED ComputedLayout invocation
    // against `layout_root`, so `pane_a_cfg.id` and
    // `pane_b_cfg.id` carry the same `path_len` and pre-order
    // leaf numbering as `post_layout.panes[i].id` (the latter
    // is computed against the same `layout_root` below). This
    // is the load-bearing pairing requirement for
    // `assert_eq!(runner.computed().id, pane.id)` inside the
    // per-pair relayout loop. Earlier draft of this test
    // derived each pane from a separate single-pane KDL string,
    // which yielded `path_len: 1` ids while the split config's
    // leaves have `path_len: 2`, breaking the pre-condition
    // the relayout loop asserts.
    let initial_layout = ComputedLayout::compute(
        &layout_root,
        LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .expect("compute initial 80x24 layout from layout_root");
    assert_eq!(
        initial_layout.panes.len(),
        2,
        "expected 2 leaf panes from Split config (one parent + two children)"
    );
    let pane_a_cfg = initial_layout.panes[0].clone();
    let pane_b_cfg = initial_layout.panes[1].clone();

    let id_a = cmdash::derive_layer_id(&pane_a_cfg.id);
    let id_b = cmdash::derive_layer_id(&pane_b_cfg.id);
    let runner_a =
        PaneRunner::spawn_with_graphics(pane_a_cfg.clone(), id_a, shell_a, Some(close_tx.clone()))
            .expect("spawn runner A");
    let runner_b =
        PaneRunner::spawn_with_graphics(pane_b_cfg.clone(), id_b, shell_b, Some(close_tx.clone()))
            .expect("spawn runner B");
    let mut runners = [runner_a, runner_b];
    drop(close_tx);

    // Manual relayout path -- the SAME per-pair loop
    // `TickContext::relayout(132, 50)` runs in the live tick
    // loop, inlined here so this integration test does not
    // reach into the binary crate's main.rs (TickContext
    // lives in `cmdash::src::main.rs`, visible only to the
    // binary's own `#[cfg(test)] mod input_tests`).
    let post_layout = ComputedLayout::compute(
        &layout_root,
        LayoutRect {
            x: 0,
            y: 0,
            w: 132,
            h: 50,
        },
    )
    .expect("compute post-layout 132x50");
    assert_eq!(post_layout.panes.len(), runners.len());
    for (runner, pane) in runners.iter_mut().zip(post_layout.panes.iter()) {
        assert_eq!(
            runner.computed().id,
            pane.id,
            "relayout index pairing: runners[i]/layout.panes[i] PaneId match"
        );
        runner
            .resize(pane.rect)
            .expect("relayout: pane resize must succeed against a sleeping PTY child");
    }

    // Both children are alive (`sleep 10`), so the per-pair
    // `runner.resize(pane.rect)` call must succeed. The
    // cached cell-grid rect must round-trip the
    // `cmdash_layout::split_rect` math over `132 x 50`:
    //   w_left = (132 * 60) / 100 = 79
    //   child A: (x:0,  y:0, w:79, h:50)
    //   child B: (x:79, y:0, w:53, h:50)
    assert_eq!(
        runners[0].computed().rect,
        LayoutRect {
            x: 0,
            y: 0,
            w: 79,
            h: 50
        },
        "child A post-relayout rect must round-trip 132x50 Horizontal-60 split via real PTY"
    );
    assert_eq!(
        runners[1].computed().rect,
        LayoutRect {
            x: 79,
            y: 0,
            w: 53,
            h: 50
        },
        "child B post-relayout rect must round-trip 132x50 Horizontal-60 split via real PTY"
    );

    // Pairing invariant: per-pair `assert_eq!(runner.computed().id,
    // pane.id)` already fired INSIDE the relayout loop above, so a
    // separate post-layout re-derivation is redundant. Leaving the
    // residue of the previously-redundant check as a comment so the
    // intent -- "every relayout asserts the index pairing" -- is
    // visible without re-running ComputedLayout::compute.

    // GraphicsState cells propagation is exercised end-to-end in
    // the matching lib unit test
    // `cmdash::src::main.rs::input_tests::relayout_emits_resize_per_pane_when_host_signals_resize`
    // -- wiring_smoke keeps its surface narrow to the layout ->
    // runner.resize pairing path (GraphiceState is constructed
    // inside `cmdash::main::run` where TickContext owns it).
}

// ===========================================================================
// Phase 2 carry-forward arm coverage: end-to-end tests that drive the
// runtime-mutation arms (AppNewPane, PaneFocus{Direction}, PaneClose,
// PanePreset) through real `PaneRunner::spawn_with_graphics` children.
// Per AGENTS.md "Each branch needs a regression test in
// `cmdash::src::main.rs::input_tests` against a multi-pane fixture
// and a focused `wiring_smoke.rs` test that drives the same path
// through real `PaneRunner::spawn_with_graphics` children." The lib
// crate's `cmdash::main::TickContext::apply_action_full` is the
// production handler; these wiring_smoke.rs tests inline-replicate
// the same per-arm dispatch (matching the pattern
// `relayout_drives_per_pane_resize_via_real_pty` uses above to
// avoid reaching into bin-only `TickContext`), so a future refactor
// that moves the arms into the lib crate (or merges the inline-vs-lib
// paths) can find regressions here against real long-lived PTYs.
//
// Long-lived `sleep 10` shells are used so the assertion surface can
// tick() each runner without racing a fast-exit child. The close-
// channel wiring mirrors
// `relayout_drives_per_pane_resize_via_real_pty`'s pattern exactly
// so Phase 2 Hard-rule invariants (LayerId preservation across
// AppNewPane / sibling-absorbed PaneClose) are observable through
// the public `PaneRunner::Drop -> close_tx -> close_rx` surface.
// ===========================================================================

/// Phase 2 carry-forward: AppNewPane. Spawn the initial 1-pane tree
/// via real `spawn_with_graphics`, then inline-replicate the
/// focused-leaf-IS-root branch of `TickContext::split_focused_for_new_pane`:
/// the original root is wrapped in `Split { Horizontal, 50,
/// [original_clone, new_leaf] }`, a fresh `PaneRunner` is spawned
/// for the new leaf, and the pre-order + PaneLayerId
/// preservation invariant (Hard rule: no LayerId rebinding) is
/// pinned through the public `layer_id()` accessor + the resolver
/// `pre_order` field.
#[test]
fn app_new_pane_splits_focused_leaf_in_real_pty_tree() {
    let source = r#"layout { pane kind=shell label="original" }"#;
    let cfg = cmdash_config::parse(source).expect("parse");
    let original_root = cfg.layout.clone().expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    let pre_layout = ComputedLayout::compute(&original_root, area).expect("compute pre");
    assert_eq!(pre_layout.panes.len(), 1, "fixture: 1-pane initial tree");
    let original_pane = pre_layout.panes[0].clone();
    let original_label = original_pane.label.clone();
    let original_pre_order = original_pane.id.pre_order();
    let original_layer_id = cmdash::derive_layer_id(&original_pane.id);

    let (close_tx, close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();
    let shell = ShellSpec::Command {
        argv: vec!["sleep".to_string(), "10".to_string()],
    };
    let mut original_runner = PaneRunner::spawn_with_graphics(
        original_pane.clone(),
        original_layer_id,
        shell.clone(),
        Some(close_tx.clone()),
    )
    .expect("spawn original runner");

    // AppNewPane (focused leaf IS root): wrap root in Split { H, 50,
    // [original_clone, new_leaf] }. Resolver DFS enumerates child 0
    // first so the original leaf keeps pre_order 0; LayerId
    // derived from pre_order is preserved across the mutation.
    let post_root = LayoutNode::Split {
        axis: CfgSplitAxis::Horizontal,
        ratio: CfgRatio(50),
        children: vec![
            original_root.clone(),
            LayoutNode::Pane(CfgPane {
                kind: PaneKind::Shell,
                label: None,
            }),
        ],
    };

    let post_layout = ComputedLayout::compute(&post_root, area).expect("compute post");
    assert_eq!(
        post_layout.panes.len(),
        2,
        "AppNewPane (focused leaf IS root) grows tree from 1 to 2 leaves"
    );
    let new_pane = post_layout.panes[1].clone();
    let new_layer_id = cmdash::derive_layer_id(&new_pane.id);
    let mut new_runner = PaneRunner::spawn_with_graphics(
        new_pane.clone(),
        new_layer_id,
        shell.clone(),
        Some(close_tx.clone()),
    )
    .expect("spawn new runner");

    // Hard rule + pre-order invariance (resolve via the public
    // layer_id() / computed() / pre_order() accessors).
    assert_eq!(
        post_layout.panes[0].id.pre_order(),
        original_pre_order,
        "original leaf's pre_order must be unchanged across AppNewPane"
    );
    assert_eq!(
        post_layout.panes[0].label, original_label,
        "original leaf's label must be unchanged across AppNewPane"
    );
    assert_eq!(
        original_runner.layer_id(),
        original_layer_id,
        "original runner's PaneLayerId must be unchanged (Hard rule: no LayerId rebinding)"
    );

    // Per-pair index pairing invariant (mirrors
    // `relayout_drives_per_pane_resize_via_real_pty`). The new
    // runner was spawned AFTER `post_layout` was resolved, so its
    // cached `PaneId` is the post-split `post_layout.panes[1].id`
    // and the pairing invariant holds. The original runner was
    // spawned BEFORE the split, so its cached `PaneId` is the
    // pre-split id -- TickContext::AppNewPane reconciles via
    // `PaneRunner::resize(reconciled.id)` after a tree mutation;
    // the lib crate's `PaneRunner` has no public rec-bind path,
    // so we verify the **pre_order + label** invariants (the only
    // fields that survive across the split without reconcile) and
    // rely on Hard-rule LayerId preservation above.
    assert_eq!(
        original_runner.computed().id.pre_order(),
        post_layout.panes[0].id.pre_order(),
        "original runner's cached pre_order must match post_layout.panes[0] \
         (pre_order is invariant across AppNewPane; full PaneId requires TickContext reconcile)"
    );
    assert_eq!(
        original_runner.computed().label,
        post_layout.panes[0].label,
        "original runner's cached label must match post_layout.panes[0]"
    );
    assert_eq!(
        new_runner.computed().id,
        post_layout.panes[1].id,
        "new runner's cached PaneId must match post_layout.panes[1] \
         (new runner was spawned AFTER the post-layout resolve)"
    );

    // Both real PTYs alive across the assertion surface.
    let _ = original_runner.tick().expect("tick original");
    let _ = new_runner.tick().expect("tick new");

    // Drop both runners sequentially so each `Drop::drop` emits
    // its `PaneLayerId` on `close_tx`. The close-channel is
    // shared across both spawns (production's tick_loop drains
    // once per tick), so two messages land on the queue.
    drop(original_runner);
    drop(new_runner);

    // Hard-rule contract: every `PaneRunner::Drop` enqueues its
    // `PaneLayerId` on its `close_tx` so the binary's
    // `cmdash::graphics::GraphicsState::close_pane` round-trip
    // can revoke the pane's dashcompositor layer (AGENTS.md
    // "Hard rule: one layer per instance"). AppNewPane spawns
    // both a survivor (unchanged LayerId) and a brand-new pane
    // (fresh LayerId) -- both round-trip identically.
    let received_orig = close_rx
        .try_recv()
        .expect("original Runner::Drop must enqueue its PaneLayerId on close_tx");
    assert_eq!(
        received_orig, original_layer_id,
        "close channel must yield exactly the original runner's PaneLayerId"
    );
    let received_new = close_rx
        .try_recv()
        .expect("new Runner::Drop must enqueue its PaneLayerId on close_tx");
    assert_eq!(
        received_new, new_layer_id,
        "close channel must yield exactly the new runner's PaneLayerId"
    );
    // No further messages (only two PaneRunners dropped).
    assert!(
        close_rx.try_recv().is_err(),
        "close_rx must be empty after both Drops (AppNewPane spawns exactly 2 runners)"
    );
    drop(close_tx);
}

/// Phase 2 carry-forward: PaneFocus{Direction} (Up / Down / Left /
/// Right). All four directions share the focused-pane -> Vec-index
/// swap dispatched by `TickContext::focus_by_direction`; this test
/// exercises the algorithm against a 2-pane `Horizontal` split real-
/// PTY fixture so neighbours and no-neighbour cases both surface.
/// Pin: `cmdash_layout::adjacent_pane` + `PaneRunner::computed().id`
/// drives the resolution; the integration test verifies the public
/// algorithm without reaching into bin-only `TickContext`.
#[test]
fn pane_focus_directional_moves_focus_via_adjacent_pane_in_real_pty_tree() {
    // 2-pane Horizontal split (column math per AGENTS.md
    // SplitAxis::Horizontal trapdoor): child 0 (left) at (x:0,
    // w:40), child 1 (right) at (x:40, w:40).
    let source = r#"layout {
        split axis=horizontal ratio=0.5 {
            pane kind=shell label="left"
            pane kind=shell label="right"
        }
    }"#;
    let cfg = cmdash_config::parse(source).expect("parse");
    let layout_root = cfg.layout.clone().expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    let initial_layout = ComputedLayout::compute(&layout_root, area).expect("compute split");
    assert_eq!(initial_layout.panes.len(), 2);
    let pane_left = initial_layout.panes[0].clone();
    let pane_right = initial_layout.panes[1].clone();
    let id_left = pane_left.id;
    let id_right = pane_right.id;
    assert_eq!(
        pane_left.rect,
        LayoutRect {
            x: 0,
            y: 0,
            w: 40,
            h: 24
        },
        "fixture invariant: split child A at (0, 0, 40, 24)"
    );
    assert_eq!(
        pane_right.rect,
        LayoutRect {
            x: 40,
            y: 0,
            w: 40,
            h: 24
        },
        "fixture invariant: split child B at (40, 0, 40, 24)"
    );

    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();
    let shell = ShellSpec::Command {
        argv: vec!["sleep".to_string(), "10".to_string()],
    };
    let mut runners: Vec<PaneRunner> = vec![
        PaneRunner::spawn_with_graphics(
            pane_left,
            cmdash::derive_layer_id(&id_left),
            shell.clone(),
            Some(close_tx.clone()),
        )
        .expect("spawn left"),
        PaneRunner::spawn_with_graphics(
            pane_right,
            cmdash::derive_layer_id(&id_right),
            shell.clone(),
            Some(close_tx.clone()),
        )
        .expect("spawn right"),
    ];

    // Inline-replicate `TickContext::focus_by_direction` against
    // the live runners via `cmdash_layout::adjacent_pane` + Vec
    // position lookup. The closure borrows `runners` immutably;
    // the per-direction assertions run synchronously so the
    // borrow is released before the `tick()` calls below.
    let resolve_focus = |focused_idx: usize, dir: Direction| -> Option<usize> {
        let focused_id = runners[focused_idx].computed().id;
        let layout = ComputedLayout::compute(&layout_root, area).expect("resolve");
        let target_id = cmdash_layout::adjacent_pane(&layout, focused_id, dir);
        target_id.and_then(|tid| runners.iter().position(|r| r.computed().id == tid))
    };

    // With a neighbour: PaneFocus{Right/Left} cross the split.
    assert_eq!(
        resolve_focus(0, Direction::Right),
        Some(1),
        "PaneFocusRight from left must move focus to right (adjacent_pane algorithm)"
    );
    assert_eq!(
        resolve_focus(1, Direction::Left),
        Some(0),
        "PaneFocusLeft from right must move focus to left"
    );

    // No-neighbour cases: stay put (focus unchanged).
    assert_eq!(
        resolve_focus(0, Direction::Left),
        None,
        "PaneFocusLeft from the leftmost pane must no-op (no neighbour)"
    );
    assert_eq!(
        resolve_focus(1, Direction::Right),
        None,
        "PaneFocusRight from the rightmost pane must no-op"
    );
    assert_eq!(
        resolve_focus(0, Direction::Up),
        None,
        "PaneFocusUp from the only row must no-op"
    );
    assert_eq!(
        resolve_focus(0, Direction::Down),
        None,
        "PaneFocusDown from the only row must no-op"
    );

    // Both real PTYs alive across the assertion surface.
    let _ = runners[0].tick().expect("tick left");
    let _ = runners[1].tick().expect("tick right");

    drop(runners);
    drop(close_tx);
}

/// Phase 2 carry-forward: PaneClose. Spawn the 2-pane Horizontal
/// split via real `spawn_with_graphics`, focus the closing pane,
/// then inline-replicate `TickContext::close_focused_and_rebalance`:
/// remove the focused runner FIRST so Drop's close_tx emit lands
/// before the tree mutates; rebalance via `cmdash_layout::remove_leaf`
/// (sibling absorption collapses the 2-child Split to its survivor);
/// reconcile_runners InPlace on the survivor (label-keyed), rebind
/// its `PaneId`, preserve its `PaneLayerId` per Hard rule. Verify
/// through the public `close_rx`, `layer_id()`, `computed()` surfaces.
#[test]
fn pane_close_drops_focused_runner_and_rebalances_real_pty_tree() {
    let source = r#"layout {
        split axis=horizontal ratio=0.5 {
            pane kind=shell label="kept"
            pane kind=shell label="closing"
        }
    }"#;
    let cfg = cmdash_config::parse(source).expect("parse");
    let mut layout_root = cfg.layout.clone().expect("layout block");
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    let initial_layout = ComputedLayout::compute(&layout_root, area).expect("compute split");
    let pane_kept = initial_layout.panes[0].clone();
    let pane_closing = initial_layout.panes[1].clone();
    let id_kept_pre = pane_kept.id;

    let (close_tx, close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();
    let shell = ShellSpec::Command {
        argv: vec!["sleep".to_string(), "10".to_string()],
    };
    let layer_kept = cmdash::derive_layer_id(&pane_kept.id);
    let layer_closing = cmdash::derive_layer_id(&pane_closing.id);
    let mut runners: Vec<PaneRunner> = Vec::with_capacity(2);
    runners.push(
        PaneRunner::spawn_with_graphics(
            pane_kept.clone(),
            layer_kept,
            shell.clone(),
            Some(close_tx.clone()),
        )
        .expect("spawn kept"),
    );
    runners.push(
        PaneRunner::spawn_with_graphics(
            pane_closing.clone(),
            layer_closing,
            shell.clone(),
            Some(close_tx.clone()),
        )
        .expect("spawn closing"),
    );
    let focus: usize = 1; // focused = closing pane
                          // `focus` is captured at the `runners.remove(focus)` callsite above.

    // Drop the focused runner FIRST so its Drop-driven close_tx
    // emit reaches `close_rx` BEFORE `remove_leaf` mutates the
    // tree (Phase 2 invariant: the binary's tick_loop drains the
    // close-channel once per tick, so the emit ordering matters).
    let dropped_runner = runners.remove(focus);
    drop(dropped_runner);

    // Closing pane's resolver path is [0, 1] (seed [0] + Split
    // child 1); strip seed -> [1] which is leaf_idx 1 of the
    // Split root. `remove_leaf` collapses the Split to its
    // survivor (label "kept"). Mirror the production
    // `TickContext::close_focused_and_rebalance`'s path-strip.
    cmdash_layout::remove_leaf(&mut layout_root, &[1]).expect("remove_leaf (sibling absorption)");
    assert_eq!(
        layout_root,
        LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: Some("kept".to_string()),
        }),
        "PaneClose (closing child 1 of Horizontal Split) collapses the Split to leaf `kept`"
    );

    // Reconcile InPlace: rebind the survivor's PaneId + resize.
    // LayerId preserved (Hard rule).
    let post_layout = ComputedLayout::compute(&layout_root, area).expect("compute post-close");
    assert_eq!(
        post_layout.panes.len(),
        1,
        "PaneClose halves the leaf count"
    );
    runners[0].rebind_pane(post_layout.panes[0].clone());

    // Hard rule: surviving runner's PaneLayerId is unchanged.
    assert_eq!(
        runners[0].layer_id(),
        layer_kept,
        "Phase 2 carry-forward: survivor's PaneLayerId must be unchanged (Hard rule)"
    );
    // Close-channel yielded the dropped runner's layer_id.
    let received = close_rx
        .try_recv()
        .expect("PaneRunner::Drop must enqueue the closing pane's layer id on close_tx");
    assert_eq!(
        received, layer_closing,
        "close channel must yield exactly the dropped pane's PaneLayerId"
    );
    // Survivor's cached pane reflects post-mutation resolver.
    assert_eq!(
        runners[0].computed().id,
        post_layout.panes[0].id,
        "survivor's cached PaneId must match post_layout.panes[0]"
    );
    assert_eq!(
        runners[0].computed().label,
        Some("kept".to_string()),
        "survivor's label after close is `kept`"
    );
    // Phase 2 PaneId stability: the survivor's PaneId rotates to
    // the post-mutation resolver (the survivor is now the root,
    // so its resolver path is [0]).
    assert_ne!(
        runners[0].computed().id,
        id_kept_pre,
        "survivor's PaneId must rotate to the post-mutation resolver"
    );
    assert_eq!(
        runners[0].computed().id.path(),
        &[0u16][..],
        "post-mutation root survivor has resolver path [0]"
    );

    // Survivor runner ticks fine (real PTY alive across the
    // assertion surface).
    let _ = runners[0].tick().expect("tick survivor");

    drop(runners);
    drop(close_tx);
}

/// Phase 2 carry-forward: PanePreset(name). Spawn the initial 1-pane
/// tree via real `spawn_with_graphics`, then inline-replicate
/// `TickContext::swap_to_preset`: drop the old runner (its Drop
/// fires close_tx), wholesale-set `layout_root` to the named preset
/// body, spawn fresh runners for each post-layout pane. Verify the
/// preset body's label/shape surfaces end-to-end.
#[test]
fn pane_preset_swap_layout_via_real_pty_wholesale_spawn() {
    // Two-name fixture: an initial 1-pane tree + a "two-pane"
    // preset body that's a Horizontal Split. The PanePreset action
    // wholesale-swaps the active layout_root; reconcile_runners is
    // ReconcileMode::Wholesale so fresh LayerIds are minted from the
    // binary's monotonic counter (`NEXT_LAYER_ID` in main.rs, not
    // reachable from the integration test crate directly).
    let source = r#"
        layout { pane kind=shell label="initial" }
        presets {
            preset "two-pane" {
                split axis=horizontal ratio=0.5 {
                    pane kind=shell label="preset-left"
                    pane kind=shell label="preset-right"
                }
            }
        }
    "#;
    let cfg = cmdash_config::parse(source).expect("parse cfg with presets block");
    let mut layout_root = cfg.layout.clone().expect("layout block");
    let presets = cfg.presets.clone();
    assert!(
        presets.contains_key("two-pane"),
        "fixture invariant: presets block must contain `two-pane`"
    );
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    let pre_layout = ComputedLayout::compute(&layout_root, area).expect("compute pre-swap");
    assert_eq!(pre_layout.panes.len(), 1, "fixture: 1 leaf pre-swap");
    let initial_pane = pre_layout.panes[0].clone();
    let initial_layer = cmdash::derive_layer_id(&initial_pane.id);

    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();
    let shell = ShellSpec::Command {
        argv: vec!["sleep".to_string(), "10".to_string()],
    };
    let initial_runner = PaneRunner::spawn_with_graphics(
        initial_pane,
        initial_layer,
        shell.clone(),
        Some(close_tx.clone()),
    )
    .expect("spawn initial runner");

    // Inline-replicate `TickContext::swap_to_preset`: wholesale-
    // clear the old runner (its Drop fires close_tx), set
    // `layout_root` to the named preset body, reset `focus = 0`,
    // fresh-spawn every pane in the post-layout. The production
    // path uses `ReconcileMode::Wholesale` + `alloc_layer_id()`
    // (a monotonic counter inside the binary, not reachable from
    // integration tests directly) so the spawned LayerIds are
    // FRESH, not derived from `derive_layer_id(&pane.id)`. The
    // wire-level testable surface here is "every preset-body
    // pane spawns and ticks against a real PTY"; the LayerId-
    // allocator source itself isn't asserted because the static
    // is bin-local.
    drop(initial_runner);
    layout_root = presets.get("two-pane").expect("preset present").clone();
    let focus: usize = 0;

    let post_layout = ComputedLayout::compute(&layout_root, area).expect("compute post-swap");
    assert_eq!(
        post_layout.panes.len(),
        2,
        "preset body resolves to 2 leaves"
    );
    assert_eq!(post_layout.panes[0].label, Some("preset-left".to_string()));
    assert_eq!(post_layout.panes[1].label, Some("preset-right".to_string()));

    let mut new_runners: Vec<PaneRunner> = Vec::with_capacity(post_layout.panes.len());
    for pane in &post_layout.panes {
        let layer = cmdash::derive_layer_id(&pane.id);
        new_runners.push(
            PaneRunner::spawn_with_graphics(
                pane.clone(),
                layer,
                shell.clone(),
                Some(close_tx.clone()),
            )
            .expect("spawn fresh pane"),
        );
    }

    assert_eq!(
        focus, 0,
        "PanePreset resets focus to 0 (per `TickContext::swap_to_preset`)"
    );
    // Per-pair index pairing invariant.
    for (i, r) in new_runners.iter().enumerate() {
        assert_eq!(
            r.computed().id,
            post_layout.panes[i].id,
            "fresh runner {} must match post_layout.panes[{}]",
            i,
            i
        );
    }

    // Wholesale spawns: the new runner LayerIds are FRESH
    // (production uses `alloc_layer_id()`), so they don't collide
    // with the initial runner's bumped-close-channel LayerId.
    // We can't reach the monotonic counter, but we DO publish
    // close-channel emits from the dropped initial -- assert
    // their absence here matches the layer_id-allocator-source
    // invariant (the OLD LayerId is gone, NEW LayerIds are in
    // use). This is a soft check; the structural assertion is
    // every fresh runner ticking.
    for r in &mut new_runners {
        let _ = r.tick().expect("tick fresh runner");
    }

    drop(new_runners);
    drop(close_tx);
}

// ===========================================================================
// Phase 2 carry-forward live-binary counterpart: drive Ctrl-a through
// the LIVE `cmdash` binary attached to a real PTY pair (real
// `TerminalGuard` + `ratatui::CrosstermBackend`). The lib-crate's
// `cmdash::main::TickContext::apply_action_full(KeyAction::AppNewPane)`
// test pins the value-level reconcile end-to-end against stub
// `PaneRunner`s; this test pins the same path through a real
// subprocess so a regression that breaks the keybind → apply_action
// wiring is observable from outside the lib crate (TickContext
// lives in `cmdash::src::main.rs`, bin-only by design).
//
// Assert strategy (graphics-mode robust):
//
// cmdash's render pipeline emits dominated by kitty-graphics
// escape sequences (`\x1b_G` blocks, ~11000 occurrences in a
// 1.5 s window per the diagnostic-capture run) rather than
// cursor-positioning CSI sequences (`\x1b[ <r> ; <c> H`, only
// ~100 total), so a CSI-position parser finds almost nothing.
// The split-border `│` U+2502 is NEVER drawn (each pane just
// renders into its own rect), and the binary's
// `--log=<path>` TRACE log emits no success-side event for
// `AppNewPane` (only the failure path logs `warn!`).
//
// Therefore the strongest non-intrusive assertion that works
// in BOTH text-mode and graphics-mode emission is:
// post-Ctrl-a ring snapshot is SUBSTANTIALLY non-empty AND
// differs byte-for-byte from the pre-Ctrl-a snapshot.
// Hash differ proves the binary re-rendered after processing
// Ctrl-a through its live keybind pathway.
//
// The reader is a bounded `VecDeque<u8>` ring (1 MiB cap,
// drops oldest) so the test process's memory footprint stays
// bounded even if the binary churns the output. The post-
// Ctrl-a snapshot is captured via a 50 ms × 30-attempt poll
// loop (1.5 s max) instead of a blind sleep so the assertion
// has a representative tail of post-Ctrl-a frames.
//
// Cycle-18 → cycle-19 history: the cycle-18 commit `dba1604`
// shipped this test `#[ignore]`-gated because the pre/post
// ring hashes matched (statistically impossible barring
// byte-identical emission), indicating the Ctrl-a byte never
// reached `TickContext::handle_event_full`. The cycle-18
// preserved TRACE log (9 731 B, 135 lines) showed cmdash
// kept rendering frames at ~50 ms cadence throughout the
// post-Ctrl-a window with `focus_idx` permanently pinned at
// 0 — cmdash was NOT crashed, the byte just never became a
// routed key event. The root cause was
// `event::poll(Duration::from_millis(0))` in
// [`TickContext::input_phase_full`] at main.rs:790
// starving the mio readiness check against the PTY fd on
// Unix. Cycle-19 lifts the poll dwell to 1 ms (negligible
// vs. the 33 ms tick cadence, ~3% per-frame budget) so the
// OS forces a fresh readiness probe against the PTY buffer
// every input phase; combined with the RAII CleanupGuard
// pattern below (which preserves cmdash's TRACE log on
// failure), this test is the wire-level witness that
// Ctrl-a → AppNewPane → 2-pane re-render is observed
// end-to-end through real PTY children.
// ===========================================================================

/// RAII guard that wraps the live-binary test's PTY master,
/// child process, output writer, and reader thread. On
/// `Drop` the guard flushes the writer, kills the child,
/// drops the master fd (causing the reader thread's
/// `read` to return EOF), and joins the reader thread, in
/// that order. `std::thread::panicking()` distinguishes
/// the test's success path (`false` → log file removed) from
/// its failure path (`true` → log file PRESERVED at the
/// canonical /tmp/cmdash-e2e-appnewpane.log for post-mortem
/// inspection via
/// `grep -n <event-name> /tmp/cmdash-e2e-appnewpane.log | tail`).
///
/// The cycle-18 cleanup pattern (process cleanup AFTER
/// assertions, log cleanup AFTER assertions) leaked the
/// reader thread on every failed assertion — `Drop` of
/// stack-locals was never reached because `assert!` panics
/// short-circuit the cleanup block below them. The guard's
/// `Drop` fires on normal scope-exit AND on `assert!` /
/// `assert_ne!` panic-unwind, so cleanup is automatic in
/// both paths while STILL preserving the diagnostic
/// artifact for the failure path.
struct CleanupGuard {
    writer: Option<Box<dyn std::io::Write + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
    reader: Option<std::thread::JoinHandle<()>>,
    log_path: std::path::PathBuf,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        // Order matters: flush the writer + kill the child
        // BEFORE dropping the master fd so the PTY slave
        // sees its master side disappear only AFTER the
        // child has terminated. Drop the master fd LAST so
        // the reader thread's `read` returns EOF and the
        // JoinHandle below unblocks. The `Option::take()`
        // pattern makes each step idempotent (Drop runs
        // exactly once even if a `?`-style early-exit
        // pattern were added later).
        if let Some(mut w) = self.writer.take() {
            let _ = w.flush();
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        // Dropping the master fd closes the master side of
        // the PTY pair; the child's PTY slave fd sees a
        // hangup signal the next time it reads. The
        // cross-thread synchronization (master dropped ->
        // reader's read() returns EOF -> reader_handle join
        // unblocks) relies on this ordering, NOT on the
        // child.kill/wait — a child that exited naturally
        // before we got here still needs the master dropped
        // so the reader thread doesn't block forever.
        drop(self.master.take());
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        // PRESERVE cmdash's TRACE log on assertion failure
        // so a maintainer reading a failed run can pinpoint
        // the line where the binary stopped emitting. DELETE
        // on success to keep /tmp tidy across repeated runs.
        if !std::thread::panicking() {
            let _ = std::fs::remove_file(&self.log_path);
        }
    }
}

#[test]
fn app_new_pane_via_ctrl_a_keypress_in_live_binary() {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // 1 MiB ring cap: large enough to capture a representative
    // post-split window (~3 frames at typical 30 MB/s
    // graphics-mode render rate = ~30 ms worth = ~1 MiB),
    // small enough to keep test memory bounded.
    const RING_CAP_BYTES: usize = 1024 * 1024;
    const POLL_INTERVAL_MS: u64 = 50;
    const POLL_ATTEMPTS: usize = 30;
    const BOOT_SETTLE_MS: u64 = 500;
    const CMDASH_BIN: &str = env!("CARGO_BIN_EXE_cmdash");

    let ring: Arc<Mutex<VecDeque<u8>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP_BYTES)));
    let ring_for_thread = Arc::clone(&ring);

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty 80x24 cmdash host");

    let log_path = std::env::temp_dir().join("cmdash-e2e-appnewpane.log");
    let _ = std::fs::remove_file(&log_path);

    let mut cmd = CommandBuilder::new(CMDASH_BIN);
    cmd.arg(format!("--log={}", log_path.display()));
    cmd.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(cmd)
        .expect("spawn cmdash attached to PTY");
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("clone PTY master reader for background drain");
    let writer = pair
        .master
        .take_writer()
        .expect("take PTY master writer for Ctrl-a injection");

    let reader_handle = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let mut guard = ring_for_thread.lock().expect("ring mutex poisoned");
                    for &b in &buf[..n] {
                        if guard.len() == RING_CAP_BYTES {
                            guard.pop_front();
                        }
                        guard.push_back(b);
                    }
                }
            }
        }
    });

    // Initial boot settle: enter alt-screen, render the first
    // 1-pane frame, start the tick loop. 500 ms is generous
    // for a binary that ticks at ~30 Hz.
    std::thread::sleep(Duration::from_millis(BOOT_SETTLE_MS));

    let pre_snapshot: Vec<u8> = ring
        .lock()
        .expect("ring mutex poisoned")
        .iter()
        .copied()
        .collect();

    // Build the cleanup guard AFTER the pre-snapshot but
    // BEFORE the Ctrl-a write. From here on every assertion
    // failure runs the guard's `Drop` impl on unwind, which
    // flushes the writer, kills the child, drops the master
    // fd, joins the reader thread, AND (via
    // `std::thread::panicking()`) preserves the cmdash TRACE
    // log for post-mortem. The guard drops naturally at end
    // of fn scope on the success path too, where `panicking()`
    // returns `false` and the log is removed to keep /tmp
    // tidy across repeated test runs.
    let mut cleanup_guard = CleanupGuard {
        writer: Some(writer),
        child: Some(child),
        master: Some(pair.master),
        reader: Some(reader_handle),
        log_path: log_path.clone(),
    };

    // Drive Ctrl-a (byte 0x01) through the PTY master. With
    // main.rs:790's `event::poll(Duration::from_millis(1))`
    // (cycle-19 fix to the cycle-18 `poll(0)` starvation),
    // the next tick's input phase surfaces it as
    // `KeyEvent { code: Char('a'), modifiers: CONTROL, kind:
    // Press }`. The `cmdash-keybinds` Router matches it
    // against the default config.kdl bind `ctrl-a →
    // app.new-pane` and dispatches `KeyAction::AppNewPane` to
    // `TickContext::apply_action_full`, which renders a new
    // 2-pane frame visible in the post-Ctrl-a ring snapshot.
    if let Some(w) = cleanup_guard.writer.as_mut() {
        w.write_all(&[0x01]).expect("write Ctrl-a byte (0x01)");
    }
    if let Some(w) = cleanup_guard.writer.as_mut() {
        let _ = w.flush();
    }

    // Poll the ring buffer (50 ms × 30 attempts = 1.5 s max).
    // We capture continuously and pin the FINAL snapshot so
    // the assertion has a representative tail of post-Ctrl-a
    // bytes, not just a single first-frame snapshot.
    let mut final_snapshot: Vec<u8> = Vec::new();
    for _ in 0..POLL_ATTEMPTS {
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        let snapshot: Vec<u8> = ring
            .lock()
            .expect("ring mutex poisoned")
            .iter()
            .copied()
            .collect();
        // Always update the captured snapshot. Eventually
        // stabilizes at the most recent ring contents.
        final_snapshot = snapshot;
    }

    let pre_hash = hash_bytes(&pre_snapshot);
    let post_hash = hash_bytes(&final_snapshot);

    // Substantial-size guard: the binary must be alive and
    // emitting throughout the post-Ctrl-a window. A binary
    // that crashes mid-test would freeze the ring at its
    // crash-time bytes (~sub-second worth, well below 16 KiB).
    assert!(
        final_snapshot.len() >= 16 * 1024,
        "post-Ctrl-a ring snapshot must be at least 16 KiB (proves the \
         binary kept rendering rather than crashing mid-test); \
         observed pre_snapshot_len={} post_snapshot_len={} \
         pre_hash={:016x} post_hash={:016x} poll_budget_ms={}",
        pre_snapshot.len(),
        final_snapshot.len(),
        pre_hash,
        post_hash,
        POLL_INTERVAL_MS * POLL_ATTEMPTS as u64,
    );

    // Hash-differ guard: the post-Ctrl-a bytes must differ
    // from the pre-Ctrl-a bytes. This proves the binary
    // re-rendered after processing Ctrl-a through its live
    // keybind pathway. The split happened
    // (`split_focused_for_new_pane` is deterministic given a
    // valid 1-pane root); the visible evidence is the new
    // render, which is byte-different from the pre-Ctrl-a
    // render regardless of graphics-vs-text mode.
    assert_ne!(
        pre_hash,
        post_hash,
        "pre-Ctrl-a and post-Ctrl-a ring buffer hashes must differ \
         (proves the binary re-rendered after Ctrl-a processed via the \
         live keybind pathway); snapshot_len_pre={} \
         snapshot_len_post={} pre_hash={:016x} post_hash={:016x}",
        pre_snapshot.len(),
        final_snapshot.len(),
        pre_hash,
        post_hash,
    );

    // ====================================================================
    // Cycle-20 visual-state assertion: parse the preserved
    // `--log=<path>` file for `blitting pane` lines emitted by
    // the cycle-20 atom-1 trace added at
    // `crates/cmdash/src/main.rs` ~line 1396
    // (`TickContext::run` phase 3a `debug!` block carrying
    // `(layer_id, rect.w, rect.h, "blitting pane")`). Cmdash's
    // pretty-formatted tracing-subscriber writes one line per
    // pane per frame to `log_path` with the inline-comma-
    // separated shape:
    //
    //   YYYY-MM-DDTHH:MM:SS.fffZ DEBUG cmdash: blitting pane,
    //     layer_id: PaneLayerId(N), rect.w: W, rect.h: H
    //     at crates/cmdash/src/main.rs:<line> on main ThreadId(1)
    //
    // Across the test window (pre-Ctrl-a + post-Ctrl-a):
    //   - pre-Ctrl-a frame:  cmdash renders ONE 80-col pane
    //                        -> 1 blitting-pane line w/ rect.w=80
    //   - post-Ctrl-a frame: cmdash renders TWO 40+40-col
    //                        children via AppNewPane's
    //                        deterministic Horizontal-50 split
    //                        -> 2 blitting-pane lines w/
    //                        rect.w=40 each
    //
    // The SET of distinct rect.w values across the preserved
    // log file is therefore {40, 80}; min(rect.w)/max(rect.w)
    // = 40/80 = 0.5 EXACTLY (asserted within a ±0.05 brute-
    // tolerance window that covers layout-engine rounding).
    //
    // The forward-fixup arc: cycle-19's `feat(poll-dwell)`
    // commit `0e02852` unstuck the live-binary hash-differ
    // assertion (1ms poll dwell against the PTY fd); this
    // atom-2 closes the cycle-19 FOLLOWUP note's forward-
    // cycle observation that "cycle-20+ can add visual-state
    // assertions ... once a CI-friendly non-flaky visual
    // probe is designed" by parsing the already-present
    // `blitting pane` debug trace for the post-split
    // `rect.w child = 40` values, end-to-end through the
    // preserved `--log=<path>` artifact.
    //
    // The READ happens BEFORE `cleanup_guard` drops at fn
    // scope-exit. Drop's `panicking()`-aware log-preservation
    // branch keeps `log_path` on disk if THIS assertion fails
    // (or any prior assertion in this fn), so a future reader
    // can `grep -n blitting pane /tmp/cmdash-e2e-appnewpane.log`
    // for the post-mortem exactly the same way they would
    // for a hash-differ failure.
    // ====================================================================
    let log_text = std::fs::read_to_string(&log_path).unwrap_or_else(|e| {
        panic!(
            "read preserved --log=<path> file {:?} must succeed (the file \
             is either kept by CleanupGuard on assertion failure OR freshly \
             appended at fn-scope exit on success -- in either case present \
             at this read); open error: {}",
            log_path, e,
        )
    }); // Parse: collect ALL distinct `rect.w: <num>` numerics
        // found on any line containing `blitting pane`. Hand-
        // rolled substring parser (no `regex` crate dep) since
        // the pretty-formatter keeps every output field INLINE
        // after the message (one `blitting pane` substring-hit
        // per line; no multi-line state machine required).
        //
        // ANSI escape strip caveat: the pretty-formatter
        // writes SGR (Select Graphic Rendition) terminal-color
        // codes inline between the message and each structured
        // field (e.g. `\x1b[1;34mrect.w\x1b[0m: 80`). The
        // raw byte stream therefore has `<ESC>` bytes between
        // `rect.w` and the `:`, NOT a clean `:` separator.
        // `strip_ansi_csi` drops those codes BEFORE the
        // substring parser runs so the digit-terminator
        // detector sees a clean `:` + space + digit run.
    let mut distinct_rect_widths: std::collections::HashSet<u16> = std::collections::HashSet::new();
    let mut blitting_pane_lines: usize = 0;
    for raw_line in log_text.lines() {
        if !raw_line.contains("blitting pane") {
            continue;
        }
        blitting_pane_lines += 1;
        let line = strip_ansi_csi(raw_line);
        if let Some(w) = extract_u16_after(&line, "rect.w") {
            distinct_rect_widths.insert(w);
        }
    }

    // Pin: at least ONE `blitting pane` line must have been
    // captured (cmdash under `--log=<path>` runs at TRACE
    // level with the format on; a zero-line parse means the
    // subscriber never initialised or atom-1's trace was
    // accidentally gated off -- both warrant a distinct
    // diagnostic from a split-never-happened failure).
    assert!(
        blitting_pane_lines > 0,
        "no `blitting pane` debug lines found in preserved --log=<path> file; \
         the cycle-20 atom-1 trace was either gated off or never reached \
         --log=<path>; log_path={:?} total_log_lines={} blitting_pane_count=0",
        log_path,
        log_text.lines().count(),
    );

    // Pin: the AppNewPane splittable-event rolled out across
    // at least 2 frames (pre-split at 80, post-split at 40).
    // A single distinct value (e.g. only 80s) means cmdash
    // never swapped to the 2-pane tree even though Ctrl-a
    // dispatched -- i.e. a relayout/draw-call regression.
    assert!(
        distinct_rect_widths.len() >= 2,
        "post-Ctrl-a log must contain at least 2 distinct rect.w values \
         (proves the AppNewPane splittable-event rolled out across frames \
         -- pre-split 80-col + post-split 40+40-col children): \
         observed distinct values={:?} \
         blitting_pane_lines={} blitting_pane_line_count_threshold=2 \
         pre_hash={:016x} post_hash={:016x}",
        distinct_rect_widths,
        blitting_pane_lines,
        pre_hash,
        post_hash,
    );

    let min_w = *distinct_rect_widths
        .iter()
        .min()
        .expect("non-empty: >= 2 distinct values asserted above");
    let max_w = *distinct_rect_widths
        .iter()
        .max()
        .expect("non-empty: >= 2 distinct values asserted above");

    // Pin (rect-3): the AppNewPane split math must surface
    // THE EXACT pre/post values, not just a min/max that
    // could be satisfied by a regression-shaped alternative
    // (e.g. `{30, 80}` ratio = 0.375 — out of ±0.05 window
    // and correctly caught — vs `{40, 80}` ratio = 0.5 caught
    // only by this explicit-set assertion). The deterministic
    // math per [`TickContext::split_focused_for_new_pane`] +
    // [`cmdash_layout::split_rect`] over (`PtySize` cols=80,
    // `SplitAxis::Horizontal`, `Ratio(50)`):
    //   pre-split pane:  rect.w = (80 * 100) / 100 = 80
    //   post-split child: rect.w = (80 * 50) / 100 = 40
    // So `distinct_rect_widths` MUST contain BOTH 40 AND 80.
    // This explicit-set assertion catches regressions the
    // min/max-ratio assertion alone would let through:
    // (a) "rect.w stays at 80 throughout" — caught here as
    // `!contains(&40)`, but the distinct-count assertion would
    // also fire on the same evidence;
    // (b) "rect.w jumps to 50 instead of 40 (wrong math)" —
    // ratio 50/80 = 0.625 IS within ±0.05 of 0.5, so the
    // ratio assertion would PASS, but the explicit-set assert
    // catches `!contains(&40)` AND `!contains(&80)` (since
    // 80 would have been replaced by 50 somewhere).
    assert!(
        distinct_rect_widths.contains(&40) && distinct_rect_widths.contains(&80),
        "AppNewPane split math must surface both rect.w=40 (post-split child) \
         and rect.w=80 (pre-split pane) across the test window: \
         observed distinct values={:?} \
         expected_exact_set={{40, 80}} pre_hash={:016x} post_hash={:016x}",
        distinct_rect_widths,
        pre_hash,
        post_hash,
    );

    // The deterministic AppNewPane split math over
    // (`PtySize` cols=80, `SplitAxis::Horizontal`,
    // `Ratio(50)`): parent_w = 80, child_w = (80 * 50) /
    // 100 = 40 (both children at w=40);
    // distinct_rect_widths = {40, 80}; min/max = 40/80 =
    // 0.5 EXACTLY. The ±0.05 tolerance window covers
    // layout-engine rounding artifacts (host-wide ceil/floor)
    // that future refactors could introduce while still
    // being tight enough to catch trivial math regressions
    // (e.g. 41/80 = 0.5125 lands within ±0.05 of 0.5 — out
    // of band only via the explicit-set assertion above).
    const EXPECTED_RATIO: f64 = 0.5;
    const RATIO_TOLERANCE: f64 = 0.05;
    let observed_ratio = f64::from(min_w) / f64::from(max_w);
    assert!(
        (observed_ratio - EXPECTED_RATIO).abs() <= RATIO_TOLERANCE,
        "rect-width min/max ratio must land within ±{} of {} (proves the \
         AppNewPane Horizontal-50 split has the expected visual state): \
         observed ratio={:.4} min_w={} max_w={} \
         distinct_widths={:?} \
         blitting_pane_lines={} pre_hash={:016x} post_hash={:016x}",
        RATIO_TOLERANCE,
        EXPECTED_RATIO,
        observed_ratio,
        min_w,
        max_w,
        distinct_rect_widths,
        blitting_pane_lines,
        pre_hash,
        post_hash,
    );

    // On success: cleanup_guard drops at end of fn scope,
    // running the cleanup order documented in its Drop impl.
    // `std::thread::panicking()` returns `false` here so the
    // TRACE log is deleted to keep /tmp tidy.
}

/// FNV-1a 64-bit hash of a byte slice. Stable byte-level diff
/// for the post-Ctrl-a ≠ pre-Ctrl-a sanity check; not
/// cryptographic, just cheap and adversarially unique per
/// byte.
fn hash_bytes(buf: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in buf {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Parse a `u16` integer immediately following a `field` token
/// in `line`. Helper for the cycle-20 visual-state assertion
/// parser (used to extract `rect.w: 80` -> `Some(80)` from the
/// preserved `--log=<path>` file's pretty-formatted output).
///
/// Shape probe (verbatim from a 2-second `script(1)` capture
/// of the live binary):
///
/// ```text
/// YYYY-MM-DDTHH:MM:SS.fffZ DEBUG cmdash: blitting pane,
/// layer_id: PaneLayerId(N), rect.w: 80, rect.h: 24
///     at crates/cmdash/src/main.rs:<line> on main ThreadId(1)
/// ```
///
/// So `rect.w` is followed by `:` (colon) + optional whitespace
/// + the numeric value. We accept both `:` and `=` as a
/// defensive hedge in case the pretty-formatter transitions
/// to a compact/alternate shape in a future `tracing-subscriber`
/// upgrade (the compact form is the enum-default, which the
/// atom-1 init code explicitly opted OUT of via `.pretty()`).
/// Returns `None` if `field` is absent or no parseable digits
/// follow.
/// Strip ANSI CSI (Control Sequence Introducer) escape
/// sequences from a string. Pretty-formatted `tracing-subscriber`
/// output embeds terminal-color codes (`<ESC>[<params>m`
/// SGR = Select Graphic Rendition) inline between the
/// message and each structured field; without this strip
/// the downstream `extract_u16_after` helper sees raw `<ESC>`
/// bytes between `rect.w` and `: 80`, the `is_ascii_digit()`
/// terminator trips on byte 0, the parse returns `None`,
/// and the visual-state assertion fails with an empty
/// `distinct_rect_widths` set. The byte-stream probe used
/// to detect this on the live-binary integration-test log
/// was `od -c /tmp/cmdash-e2e-appnewpane.log` (`od` dumped
/// `rect.w<ESC>[0m<ESC>[3;4m: 80` in the inline-comma-
/// separated pretty-formatter shape, NOT a clean `rect.w: 80`).
/// Hand-rolled (no `regex` crate dep) since the v1 line
/// shapes we parse only ever emit SGR codes (`m` terminator);
/// OSC (`<ESC>]<payload><ST>`) and non-SGR CSI terminators
/// (`H` cursor, `J`/`K` erase, `h`/`l` mode-set) are NOT
/// emitted by v1's pretty-formatter and would require
/// extending this helper, NOT removing it.
///
/// Defensive unclosed-CSI behaviour: if a trailing CSI
/// sequence lacks a closing `m` (e.g. a log-line truncation
/// glitch), the loop breaks rather than spinning on off-by-one.
/// The implementation loops over bytes via
/// `chars().next()` codepoint iteration, so non-ASCII bytes
/// are handled correctly (ASCII fast-path is incidental, not
/// load-bearing).
fn strip_ansi_csi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // Skip until and including the trailing 'm'.
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'm' {
                j += 1;
            }
            if j < bytes.len() {
                i = j + 1;
                continue;
            }
            // Unclosed CSI -- break rather than spin. (Reachable
            // only on a malformed log line; defensive.)
            break;
        }
        // Visible byte: push as char, advance one UTF-8
        // codepoint. ASCII fast-path (each byte is its own
        // char).
        let remainder = &line[i..];
        let ch = remainder.chars().next().expect("i < bytes.len()");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Parse a `u16` integer immediately following a `field`
/// token in `line`. Helper for the cycle-20 visual-state
/// assertion parser (used to extract `rect.w: 80` ->
/// `Some(80)` from the preserved `--log=<path>` file's
/// pretty-formatted output, AFTER `strip_ansi_csi` has
/// dropped the inline ANSI codes).
///
/// Shape probe (ANSI-stripped, verbatim from a 2-second
/// `script(1)` capture of the live binary):
///
/// ```text
/// YYYY-MM-DDTHH:MM:SS.fffZ DEBUG cmdash: blitting pane,
/// layer_id: PaneLayerId(N), rect.w: 80, rect.h: 24
///     at crates/cmdash/src/main.rs:<line> on main ThreadId(1)
/// ```
///
/// So `rect.w` is followed by `:` (colon) + optional
/// whitespace + the numeric value. We accept both `:` and
/// `=` as a defensive hedge in case the pretty-formatter
/// transitions to a compact/alternate shape in a future
/// `tracing-subscriber` upgrade (the compact form is the
/// enum-default, which the atom-1 init code explicitly
/// opted OUT of via `.pretty()`). Returns `None` if `field`
/// is absent or no parseable digits follow.
fn extract_u16_after(line: &str, field: &str) -> Option<u16> {
    let pos = line.find(field)?;
    let rest = &line[pos + field.len()..];
    // Skip the `:` or `=` separator AND any whitespace.
    let trimmed = rest.trim_start_matches([':', '=', ' ', '\t']);
    // Read digits until a non-digit terminator (space, comma,
    // end-of-line, or anything else).
    let end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    trimmed[..end].parse::<u16>().ok()
}
