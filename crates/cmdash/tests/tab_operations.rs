//! Tab operations integration tests: TabNew, TabClose, TabSwitch
//! driven through `cmdash::tabs::TabStack` with real PTY children.
//!
//! These tests exercise the tab-axis runtime primitives end-to-end:
//! each tab holds real `PaneRunner` instances backed by `sleep 10`
//! PTY children, and the `TabStack` API mirrors the
//! `TickContext::{create_new_tab, close_active_tab, switch_to_tab}`
//! dispatch paths without reaching into the bin-only `main.rs`.
//!
//! Per AGENTS.md §"Tabs": every tab's panes are independent
//! `PaneRunner` instances with their own `LayerId`s; switching
//! tabs calls `sync_v1_from_active_tab` + `reconcile_runners(Wholesale)`
//! in the production path. These tests verify the data-structure
//! contract (cursor movement, runner lifecycle, close-channel
//! cleanup) that the production code depends on.

use cmdash::pane::{PaneCloseTx, PaneRunner};
use cmdash::tabs::TabStack;
use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
use cmdash_pty::ShellSpec;

/// Long-lived shell so PTYs stay alive across assertions.
fn long_shell() -> ShellSpec {
    ShellSpec::Command {
        argv: vec!["sleep".to_string(), "10".to_string()],
    }
}

/// Spawn a single-pane runner from a KDL layout source.
fn spawn_runner(source: &str, area: LayoutRect, close_tx: &PaneCloseTx) -> PaneRunner {
    let cfg = cmdash_config::parse(source).expect("parse config");
    let root = cfg.layout.expect("layout block");
    let layout = ComputedLayout::compute(&root, area).expect("compute layout");
    let pane = layout.panes[0].clone();
    let layer_id = cmdash::derive_layer_id(&pane.id);
    PaneRunner::spawn_with_graphics(pane, layer_id, long_shell(), Some(close_tx.clone()))
        .expect("spawn runner")
}

// ===========================================================================
// TabNew: create a new tab with a fresh runner, verify TabStack state.
// ===========================================================================

/// TabNew: pushing a new tab appends it to the stack and switches
/// the active cursor to it. Both the original and new tab's runners
/// must remain alive (tickable) after the push.
#[test]
fn tab_new_appends_and_switches_with_live_runners() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    // Tab 1: initial single-pane tab.
    let runner1 = spawn_runner(
        r#"layout { pane kind=shell label="tab1" }"#,
        area,
        &close_tx,
    );
    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner1]);
    assert_eq!(tabs.len(), 1, "initial stack has 1 tab");
    assert_eq!(tabs.active_idx(), 0);

    // TabNew: create a fresh runner and push as a new tab.
    let runner2 = spawn_runner(
        r#"layout { pane kind=shell label="tab2" }"#,
        area,
        &close_tx,
    );
    let new_idx = tabs.push(vec![runner2]);
    assert_eq!(new_idx, 1, "push returns the new tab index");
    assert_eq!(tabs.len(), 2, "stack now has 2 tabs");
    assert_eq!(tabs.active_idx(), 1, "cursor moved to the new tab");

    // Tab 1 runners still alive (tick through get_mut).
    {
        let runners = tabs.get_mut(0).unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("tab1 runner must tick after TabNew");
        }
    }

    // Tab 2 runners alive (tick through active_mut).
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("tab2 runner must tick after creation");
        }
    }
}

/// TabNew: pushing multiple tabs sequentially creates a stack of
/// the expected size, and the cursor always lands on the last push.
#[test]
fn tab_new_sequential_pushes_grow_stack() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    let runner1 = spawn_runner(r#"layout { pane kind=shell label="t1" }"#, area, &close_tx);
    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner1]);

    for i in 1..5u32 {
        let runner = spawn_runner(
            &format!("layout {{ pane kind=shell label=\"t{}\" }}", i + 1),
            area,
            &close_tx,
        );
        let idx = tabs.push(vec![runner]);
        assert_eq!(idx, i as usize, "push #{i} returns index {i}");
        assert_eq!(tabs.len(), (i + 1) as usize);
        assert_eq!(
            tabs.active_idx(),
            i as usize,
            "cursor follows the most recent push"
        );
    }

    assert_eq!(tabs.len(), 5, "5 sequential pushes produce 5 tabs");
    drop(tabs);
    drop(close_tx);
}

// ===========================================================================
// TabClose: close the active tab, verify runner cleanup.
// ===========================================================================

/// TabClose: removing the active tab from a 2-tab stack leaves
/// the other tab's runner alive and the cursor clamped.
#[test]
fn tab_close_removes_active_tab_leaves_survivor_alive() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    let runner1 = spawn_runner(
        r#"layout { pane kind=shell label="survivor" }"#,
        area,
        &close_tx,
    );
    let runner2 = spawn_runner(
        r#"layout { pane kind=shell label="closing" }"#,
        area,
        &close_tx,
    );

    // Stack: [survivor, closing], active = 1 (closing).
    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner1]);
    tabs.push(vec![runner2]);
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs.active_idx(), 1);

    // TabClose: remove the active tab (closing).
    let removed = tabs.remove_active();
    assert!(removed.is_some(), "remove_active returns the removed tab");
    assert_eq!(tabs.len(), 1, "stack shrinks to 1 tab");
    assert_eq!(tabs.active_idx(), 0, "cursor clamped to len-1 (survivor)");

    // Survivor runner still ticks.
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("survivor runner must tick after TabClose");
        }
    }

    // Drop the removed tab so its runners fire Drop → close_tx.
    drop(removed);

    // The dropped runner's LayerId was sent on close_rx.
    let received = close_rx
        .try_recv()
        .expect("dropped runner must enqueue its LayerId on close_tx");
    // For a single-pane layout, derive_layer_id returns PaneLayerId(0)
    // (pre_order=0, tab_id=0), so we only assert the channel delivered
    // a message, not a specific non-zero value.
    let _ = received; // valid LayerId received on the close channel
}

/// TabClose: closing the active tab (at index 0) from a 3-tab
/// stack shifts the remaining tabs down and the cursor stays at 0.
#[test]
fn tab_close_first_tab_shifts_remaining_down() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    let runner1 = spawn_runner(
        r#"layout { pane kind=shell label="first" }"#,
        area,
        &close_tx,
    );
    let runner2 = spawn_runner(
        r#"layout { pane kind=shell label="second" }"#,
        area,
        &close_tx,
    );
    let runner3 = spawn_runner(
        r#"layout { pane kind=shell label="third" }"#,
        area,
        &close_tx,
    );

    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner1]);
    tabs.push(vec![runner2]);
    tabs.push(vec![runner3]);
    // Stack: [first, second, third], active = 2.

    // Switch to tab 0 (first), then close it.
    tabs.switch_to(0);
    assert_eq!(tabs.active_idx(), 0);
    let removed = tabs.remove_active();
    assert!(removed.is_some());
    assert_eq!(tabs.len(), 2, "3 → 2 after closing first");

    // Remaining tabs: [second, third], cursor at 0 (second).
    assert_eq!(tabs.active_idx(), 0);
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("second runner must tick");
        }
    }
    {
        let runners = tabs.get_mut(1).unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("third runner must tick");
        }
    }
}

/// TabClose: closing the LAST tab leaves an empty stack.
/// Mirrors the production `TickContext::close_active_tab` path
/// where `self.tabs.is_empty()` → `self.running = false`.
#[test]
fn tab_close_last_tab_leaves_empty_stack() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    let runner = spawn_runner(
        r#"layout { pane kind=shell label="only" }"#,
        area,
        &close_tx,
    );
    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner]);
    assert_eq!(tabs.len(), 1);

    let removed = tabs.remove_active();
    assert!(removed.is_some(), "removed tab is returned");
    assert!(
        tabs.is_empty(),
        "closing the last tab leaves the stack empty"
    );
    assert!(tabs.active().is_none(), "empty stack has no active tab");
    assert_eq!(tabs.len(), 0);
    drop(close_tx);
}

// ===========================================================================
// TabSwitch: switch between tabs, verify cursor movement.
// ===========================================================================

/// TabSwitch: switch_to(n) moves the cursor to the target tab.
/// All runners remain alive and tickable after switching.
#[test]
fn tab_switch_changes_active_cursor() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    let runner1 = spawn_runner(
        r#"layout { pane kind=shell label="alpha" }"#,
        area,
        &close_tx,
    );
    let runner2 = spawn_runner(
        r#"layout { pane kind=shell label="beta" }"#,
        area,
        &close_tx,
    );
    let runner3 = spawn_runner(
        r#"layout { pane kind=shell label="gamma" }"#,
        area,
        &close_tx,
    );

    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner1]);
    tabs.push(vec![runner2]);
    tabs.push(vec![runner3]);
    // Stack: [alpha, beta, gamma], active = 2.

    // Switch to tab 0 (alpha).
    assert!(tabs.switch_to(0), "switch_to(0) returns true");
    assert_eq!(tabs.active_idx(), 0);
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("alpha runner must tick");
        }
    }

    // Switch to tab 1 (beta).
    assert!(tabs.switch_to(1), "switch_to(1) returns true");
    assert_eq!(tabs.active_idx(), 1);
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("beta runner must tick");
        }
    }

    // Switch back to tab 2 (gamma).
    assert!(tabs.switch_to(2), "switch_to(2) returns true");
    assert_eq!(tabs.active_idx(), 2);
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("gamma runner must tick");
        }
    }
}

/// TabSwitch: out-of-range switch_to is a silent no-op (returns false).
/// Mirrors M-1..M-9 keybind semantics where out-of-range chords
/// are silently ignored.
#[test]
fn tab_switch_out_of_range_is_noop() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    let runner1 = spawn_runner(r#"layout { pane kind=shell label="a" }"#, area, &close_tx);
    let runner2 = spawn_runner(r#"layout { pane kind=shell label="b" }"#, area, &close_tx);

    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner1]);
    tabs.push(vec![runner2]);
    assert_eq!(tabs.active_idx(), 1);

    // Out-of-range: tab 5 on a 2-tab stack.
    assert!(
        !tabs.switch_to(5),
        "switch_to(5) on 2-tab stack must return false"
    );
    assert_eq!(
        tabs.active_idx(),
        1,
        "cursor unchanged after out-of-range switch"
    );

    // Out-of-range: usize::MAX.
    assert!(!tabs.switch_to(usize::MAX));
    assert_eq!(tabs.active_idx(), 1);
    drop(tabs);
    drop(close_tx);
}

// ===========================================================================
// Full lifecycle: TabNew → TabSwitch → TabClose with real PTYs.
// ===========================================================================

/// Full lifecycle: create 3 tabs, switch between them, close the
/// middle one, verify the remaining two runners survive.
#[test]
fn tab_lifecycle_new_switch_close_with_real_pty_runners() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    // Tab 0: initial.
    let runner0 = spawn_runner(
        r#"layout { pane kind=shell label="tab-0" }"#,
        area,
        &close_tx,
    );
    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner0]);

    // TabNew: tab 1.
    let runner1 = spawn_runner(
        r#"layout { pane kind=shell label="tab-1" }"#,
        area,
        &close_tx,
    );
    tabs.push(vec![runner1]);

    // TabNew: tab 2.
    let runner2 = spawn_runner(
        r#"layout { pane kind=shell label="tab-2" }"#,
        area,
        &close_tx,
    );
    tabs.push(vec![runner2]);

    assert_eq!(tabs.len(), 3);
    assert_eq!(tabs.active_idx(), 2);

    // TabSwitch to tab 1.
    tabs.switch_to(1);
    assert_eq!(tabs.active_idx(), 1);

    // TabClose: close tab 1 (active).
    let removed = tabs.remove_active();
    assert!(removed.is_some());
    assert_eq!(tabs.len(), 2, "3 → 2 after closing tab 1");
    // Cursor clamped to len-1 = 1 (tab 2).
    assert_eq!(tabs.active_idx(), 1);

    // Tab 0 runner still alive.
    {
        let runners = tabs.get_mut(0).unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("tab-0 runner must tick after lifecycle");
        }
    }

    // Tab 2 runner (now at index 1) still alive.
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r.tick().expect("tab-2 runner must tick after lifecycle");
        }
    }

    // Drop the removed tab so its runners fire Drop → close_tx.
    drop(removed);

    // The closed tab's LayerId was sent on close_rx.
    let received = close_rx
        .try_recv()
        .expect("closed tab runner must send LayerId on close_tx");
    // For a single-pane layout, derive_layer_id returns PaneLayerId(0)
    // (pre_order=0, tab_id=0), so we only assert the channel delivered
    // a message, not a specific non-zero value.
    let _ = received; // valid LayerId received on the close channel

    // Drop remaining tabs — their LayerIds also arrive on close_rx.
    drop(tabs);
    // Drain the close channel for remaining tabs.
    let mut remaining = 0;
    while close_rx.try_recv().is_ok() {
        remaining += 1;
    }
    assert_eq!(
        remaining, 2,
        "2 remaining tabs drop 2 LayerIds on close channel"
    );
}

/// Full lifecycle: TabNew → TabSwitch → TabNew → TabClose(last)
/// leaves an empty stack. Verifies the production
/// `close_active_tab` → `self.tabs.is_empty()` → `running = false`
/// contract.
#[test]
fn tab_lifecycle_new_switch_close_last_quits() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    let runner0 = spawn_runner(r#"layout { pane kind=shell label="a" }"#, area, &close_tx);
    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner0]);

    // TabNew.
    let runner1 = spawn_runner(r#"layout { pane kind=shell label="b" }"#, area, &close_tx);
    tabs.push(vec![runner1]);

    // TabSwitch back to tab 0.
    tabs.switch_to(0);
    assert_eq!(tabs.active_idx(), 0);

    // TabClose tab 0.
    tabs.remove_active();
    assert_eq!(tabs.len(), 1, "1 tab remaining");
    assert_eq!(tabs.active_idx(), 0);

    // TabClose last tab.
    tabs.remove_active();
    assert!(
        tabs.is_empty(),
        "closing the last tab leaves the stack empty"
    );
    drop(close_tx);
}

/// Multi-pane tab: a tab can hold multiple runners (e.g. a split
/// layout). TabNew creates a new tab; TabClose drops all runners
/// in the closed tab; TabSwitch moves focus.
#[test]
fn tab_operations_with_multi_pane_tabs() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    // Tab 0: single-pane.
    let runner0 = spawn_runner(
        r#"layout { pane kind=shell label="single" }"#,
        area,
        &close_tx,
    );
    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner0]);

    // Tab 1: dual-pane (split layout — two runners in one tab).
    let cfg = cmdash_config::parse(
        r#"layout {
            split axis=horizontal ratio=0.5 {
                pane kind=shell label="split-a"
                pane kind=shell label="split-b"
            }
        }"#,
    )
    .expect("parse split config");
    let root = cfg.layout.expect("layout block");
    let layout = ComputedLayout::compute(&root, area).expect("compute split");
    assert_eq!(layout.panes.len(), 2, "split resolves to 2 panes");

    let mut tab1_runners: Vec<PaneRunner> = Vec::new();
    for pane in &layout.panes {
        let layer = cmdash::derive_layer_id(&pane.id);
        tab1_runners.push(
            PaneRunner::spawn_with_graphics(
                pane.clone(),
                layer,
                long_shell(),
                Some(close_tx.clone()),
            )
            .expect("spawn split pane"),
        );
    }
    tabs.push(tab1_runners);
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs.active_idx(), 1);

    // Tab 1 has 2 runners — both must tick.
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        assert_eq!(runners.len(), 2, "tab 1 holds 2 runners from split");
        for r in runners.iter_mut() {
            let _ = r.tick().expect("split pane runner must tick");
        }
    }

    // TabSwitch to tab 0, verify it still ticks.
    tabs.switch_to(0);
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        assert_eq!(runners.len(), 1, "tab 0 holds 1 runner");
        for r in runners.iter_mut() {
            let _ = r.tick().expect("tab 0 runner must tick after switch");
        }
    }

    // TabClose tab 1 (the multi-pane tab) — both runners drop.
    tabs.switch_to(1);
    let removed = tabs.remove_active();
    assert!(removed.is_some());
    assert_eq!(tabs.len(), 1);

    // Drop the removed tab so its runners fire Drop → close_tx.
    drop(removed);

    // The 2 dropped runners each sent their LayerId on close_rx.
    let mut close_count = 0;
    while close_rx.try_recv().is_ok() {
        close_count += 1;
    }
    assert_eq!(
        close_count, 2,
        "closing a 2-runner tab sends 2 LayerIds on close channel"
    );

    // Tab 0 still alive.
    {
        let runners = tabs.active_mut().unwrap().state_mut();
        for r in runners.iter_mut() {
            let _ = r
                .tick()
                .expect("tab 0 runner must still tick after closing tab 1");
        }
    }
}

/// Switch-after-close: after closing a tab, switching to an
/// out-of-range index is a no-op. Verifies cursor clamping
/// interacts correctly with switch_to bounds checking.
#[test]
fn tab_switch_after_close_respects_bounds() {
    let area = LayoutRect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };
    let (close_tx, _close_rx): (PaneCloseTx, _) = std::sync::mpsc::channel();

    let runner0 = spawn_runner(r#"layout { pane kind=shell label="a" }"#, area, &close_tx);
    let runner1 = spawn_runner(r#"layout { pane kind=shell label="b" }"#, area, &close_tx);
    let runner2 = spawn_runner(r#"layout { pane kind=shell label="c" }"#, area, &close_tx);

    let mut tabs: TabStack<Vec<PaneRunner>> = TabStack::new(vec![runner0]);
    tabs.push(vec![runner1]);
    tabs.push(vec![runner2]);
    // Stack: [a, b, c], active = 2.

    // Close tab 2 (active) → stack shrinks to [a, b].
    tabs.remove_active();
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs.active_idx(), 1);

    // switch_to(2) is now out-of-range → no-op.
    assert!(!tabs.switch_to(2), "switch_to(2) on 2-tab stack must fail");
    assert_eq!(tabs.active_idx(), 1, "cursor unchanged");

    // switch_to(0) is in-range → succeeds.
    assert!(tabs.switch_to(0));
    assert_eq!(tabs.active_idx(), 0);
    drop(tabs);
    drop(close_tx);
}
