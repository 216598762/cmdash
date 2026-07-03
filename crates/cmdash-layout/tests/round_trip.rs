use std::path::PathBuf;

use cmdash_config::{parse, PaneKind};
use cmdash_layout::{ComputedLayout, LayoutError, PaneId, Rect};

fn fixture() -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("config.kdl");
    std::fs::read_to_string(p).expect("fixture readable")
}

#[test]
fn layout_zero_area_errors() {
    let root = parse("layout { pane kind=shell }").unwrap().layout.unwrap();
    let err = ComputedLayout::compute(
        &root,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 0,
        },
    )
    .unwrap_err();
    assert!(matches!(err, LayoutError::ZeroArea { w: 80, h: 0, .. }));
}

#[test]
fn layout_single_pane_fills_area() {
    let root = parse(r#"layout { pane kind=shell label="only" }"#)
        .unwrap()
        .layout
        .unwrap();
    let lyt = ComputedLayout::compute(
        &root,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap();
    assert_eq!(lyt.panes.len(), 1);
    assert_eq!(
        lyt.panes[0].rect,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24
        }
    );
    assert_eq!(lyt.panes[0].label.as_deref(), Some("only"));
    assert_eq!(lyt.panes[0].kind, PaneKind::Shell);
}

#[test]
fn layout_horizontal_split_60() {
    let cfg = parse(
        r#"layout {
        split axis=horizontal ratio=0.6 {
            pane kind=shell label="left"
            pane kind=shell label="right"
        }
    }"#,
    )
    .unwrap();
    let lyt = ComputedLayout::compute(
        &cfg.layout.unwrap(),
        Rect {
            x: 0,
            y: 0,
            w: 100,
            h: 10,
        },
    )
    .unwrap();
    assert_eq!(lyt.panes.len(), 2);
    assert_eq!(
        lyt.panes[0].rect,
        Rect {
            x: 0,
            y: 0,
            w: 60,
            h: 10
        }
    );
    assert_eq!(
        lyt.panes[1].rect,
        Rect {
            x: 60,
            y: 0,
            w: 40,
            h: 10
        }
    );
}

#[test]
fn layout_vertical_split() {
    let cfg = parse(
        r#"layout {
        split axis=vertical ratio=0.3 {
            pane kind=shell label="top"
            pane kind=shell label="bot"
        }
    }"#,
    )
    .unwrap();
    let lyt = ComputedLayout::compute(
        &cfg.layout.unwrap(),
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 10,
        },
    )
    .unwrap();
    assert_eq!(lyt.panes.len(), 2);
    assert_eq!(
        lyt.panes[0].rect,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 3
        }
    );
    assert_eq!(
        lyt.panes[1].rect,
        Rect {
            x: 0,
            y: 3,
            w: 80,
            h: 7
        }
    );
}

#[test]
fn layout_stack_divides_height_equally() {
    let cfg = parse(
        r#"layout {
        stack {
            pane kind=shell label="a"
            pane kind=shell label="b"
        }
    }"#,
    )
    .unwrap();
    let lyt = ComputedLayout::compute(
        &cfg.layout.unwrap(),
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap();
    assert_eq!(lyt.panes.len(), 2);
    // Stack divides the area into N equal-height vertical strips,
    // top to bottom; the last strip absorbs any leftover rows so
    // the slices tile the area exactly.
    assert_eq!(
        lyt.panes[0].rect,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 12
        }
    );
    assert_eq!(
        lyt.panes[1].rect,
        Rect {
            x: 0,
            y: 12,
            w: 80,
            h: 12
        }
    );
}

#[test]
fn layout_stack_divides_height_three_panes() {
    let cfg = parse(
        r#"layout {
        stack {
            pane kind=shell label="a"
            pane kind=shell label="b"
            pane kind=shell label="c"
        }
    }"#,
    )
    .unwrap();
    let lyt = ComputedLayout::compute(
        &cfg.layout.unwrap(),
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap();
    assert_eq!(lyt.panes.len(), 3);
    let h0 = lyt.panes[0].rect.h;
    let h1 = lyt.panes[1].rect.h;
    let h2 = lyt.panes[2].rect.h;
    // Equal-height vertical strips tile the area exactly; even
    // distribution means every strip is 8 rows.
    assert_eq!(h0, 8);
    assert_eq!(h1, 8);
    assert_eq!(h2, 8);
    assert_eq!(lyt.panes[0].rect.y, 0);
    assert_eq!(lyt.panes[1].rect.y, 8);
    assert_eq!(lyt.panes[2].rect.y, 16);
}

/// `stack { ... }` is described by AGENTS.md as a "tabbed viewer"
/// but cmdash-layout v1 implements it as an equal-height
/// VERTICAL STRIP-STACK: one pane per child, each occupying a
/// slice of the parent's height. This test pins that v1
/// contract so a future refactor that *does* collapse multiple
/// stack children into one tabbed pane is an intentional change,
/// not a silent regression. (Existing tests in this file
/// describe how stacks divide height; this one adds the
/// "NOT collapsed" framing -- the explicit guard against the
/// tabbed-viewer reading of AGENTS.md.)
#[test]
fn stack_emits_one_pane_per_child_in_v1() {
    let cfg = parse(
        r#"layout {
            stack {
                pane kind=shell label="a"
                pane kind=shell label="b"
            }
        }"#,
    )
    .unwrap();
    let root = cfg.layout.unwrap();
    let lyt = ComputedLayout::compute(
        &root,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap();
    // Two children -> two panes (NOT one "tabbed viewer" pane).
    assert_eq!(
        lyt.panes.len(),
        2,
        "v1 stack emits one pane per child (strip-stack), NOT a single tabbed viewer pane; v2 may genuinely collapse siblings into one tab"
    );
    assert_eq!(lyt.panes[0].label.as_deref(), Some("a"));
    assert_eq!(lyt.panes[1].label.as_deref(), Some("b"));
    // Vertical strips tile the area exactly.
    assert_eq!(
        lyt.panes[0].rect,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 12
        }
    );
    assert_eq!(
        lyt.panes[1].rect,
        Rect {
            x: 0,
            y: 12,
            w: 80,
            h: 12
        }
    );
}

#[test]
fn layout_paneid_stable_across_resizes() {
    let cfg = parse(
        r#"layout {
        split axis=horizontal ratio=0.5 {
            pane kind=shell label="a"
            pane kind=shell label="b"
        }
    }"#,
    )
    .unwrap();
    let root = cfg.layout.unwrap();
    let lyt1 = ComputedLayout::compute(
        &root,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap();
    let lyt2 = ComputedLayout::compute(
        &root,
        Rect {
            x: 0,
            y: 0,
            w: 200,
            h: 60,
        },
    )
    .unwrap();
    assert_eq!(lyt1.panes[0].id, lyt2.panes[0].id);
    assert_eq!(lyt1.panes[1].id, lyt2.panes[1].id);
}

#[test]
fn layout_paneid_distinct_for_distinct_leaves() {
    let root = parse(
        r#"layout {
        split axis=horizontal ratio=0.5 {
            pane kind=shell label="a"
            pane kind=shell label="b"
        }
    }"#,
    )
    .unwrap()
    .layout
    .unwrap();
    let lyt = ComputedLayout::compute(
        &root,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap();
    let (id0, id1) = (lyt.panes[0].id, lyt.panes[1].id);
    assert_ne!(id0, id1);
    assert_eq!(id0.pre_order(), 0);
    assert_eq!(id1.pre_order(), 1);
}

#[test]
fn layout_paneid_path_reflects_layout_tree() {
    let root = parse(
        r#"layout {
        split axis=horizontal ratio=0.5 {
            pane kind=shell label="root.left"
            pane kind=shell label="root.right"
        }
    }"#,
    )
    .unwrap()
    .layout
    .unwrap();
    let lyt = ComputedLayout::compute(
        &root,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap();
    assert_eq!(lyt.panes[0].id.path(), &[0, 0]);
    assert_eq!(lyt.panes[1].id.path(), &[0, 1]);
}

#[test]
fn layout_canonical_fixture() {
    let cfg = parse(&fixture()).expect("fixture parses");
    let root = cfg.layout.expect("layout block present");
    let lyt = ComputedLayout::compute(
        &root,
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap();
    assert_eq!(lyt.panes.len(), 3);
    let mut ids: Vec<PaneId> = lyt.panes.iter().map(|p| p.id).collect();
    ids.sort();
    assert_eq!(ids[0].pre_order(), 0);
    assert_eq!(ids[1].pre_order(), 1);
    assert_eq!(ids[2].pre_order(), 2);
    assert_eq!(
        lyt.panes[0].rect,
        Rect {
            x: 0,
            y: 0,
            w: 48,
            h: 12
        }
    );
    assert_eq!(lyt.panes[0].label.as_deref(), Some("dash 1"));
    assert_eq!(
        lyt.panes[1].rect,
        Rect {
            x: 0,
            y: 12,
            w: 48,
            h: 12
        }
    );
    assert_eq!(lyt.panes[1].label.as_deref(), Some("dash 2"));
    assert_eq!(
        lyt.panes[2].rect,
        Rect {
            x: 48,
            y: 0,
            w: 32,
            h: 24
        }
    );
    assert_eq!(lyt.panes[2].label.as_deref(), Some("main"));
    for p in &lyt.panes {
        assert_eq!(p.kind, PaneKind::Shell);
    }
}

#[test]
fn layout_preset_root_errors() {
    let cfg = parse(r#"layout { preset "only" }"#).unwrap();
    let err = ComputedLayout::compute(
        &cfg.layout.unwrap(),
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap_err();
    assert!(matches!(err, LayoutError::PresetAtRoot));
}

#[test]
fn layout_preset_inside_stack_is_ignored() {
    let cfg = parse(
        r#"layout {
        stack {
            preset "x"
            pane kind=shell label="real"
        }
    }"#,
    )
    .unwrap();
    let lyt = ComputedLayout::compute(
        &cfg.layout.unwrap(),
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap();
    assert_eq!(lyt.panes.len(), 1);
    assert_eq!(lyt.panes[0].label.as_deref(), Some("real"));
}

#[test]
fn layout_empty_split_errors() {
    let cfg = parse(
        r#"layout {
        split axis=horizontal {
            pane kind=shell
        }
    }"#,
    )
    .unwrap();
    let err = ComputedLayout::compute(
        &cfg.layout.unwrap(),
        Rect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        },
    )
    .unwrap_err();
    assert!(matches!(err, LayoutError::SplitChildCount { got: 1 }));
}
