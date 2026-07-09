use std::path::PathBuf;

use cmdash_config::{
    parse, Config, KeyAction, KeyToken, LayoutNode, Modifiers, Pane, PaneKind, Ratio, SplitAxis,
};

fn fixture() -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("config.kdl");
    std::fs::read_to_string(p).expect("fixture is readable")
}

#[test]
fn empty_parses_to_default() {
    let cfg = parse("").expect("empty is a valid cmdash config");
    assert_eq!(cfg, Config::default());
}

#[test]
fn parses_canonical_fixture() {
    let cfg = parse(&fixture()).expect("fixture parses");

    let split = match cfg.layout.expect("layout block present") {
        LayoutNode::Split {
            axis,
            ratio,
            children,
        } => {
            assert_eq!(axis, SplitAxis::Horizontal);
            assert_eq!(ratio, Ratio(60));
            children
        }
        other => panic!("expected Split at top, got {other:?}"),
    };
    assert_eq!(split.len(), 2);

    let panes = match &split[0] {
        LayoutNode::Stack { panes } => panes,
        other => panic!("expected Stack, got {other:?}"),
    };
    assert_eq!(panes.len(), 2);
    for p in panes {
        match p {
            LayoutNode::Pane(Pane {
                kind: PaneKind::Shell,
                label: Some(_),
                ..
            }) => {}
            other => panic!("expected labeled shell pane, got {other:?}"),
        }
    }
    assert!(matches!(&split[1], LayoutNode::Pane(_)));

    assert_eq!(cfg.keybinds.len(), 8);

    let ctrl_q = cfg
        .keybinds
        .iter()
        .find(|b| matches!(b.action, KeyAction::AppClose))
        .expect("ctrl-q binds app.close");
    assert_eq!(
        ctrl_q.mods,
        Modifiers {
            ctrl: true,
            shift: false,
            alt: false,
            super_: false
        }
    );
    assert_eq!(ctrl_q.key, KeyToken::Char('q'));

    let ctrl_shift_c = cfg
        .keybinds
        .iter()
        .find(|b| matches!(b.action, KeyAction::PaneClose))
        .expect("ctrl-shift-c binds pane.close");
    assert!(ctrl_shift_c.mods.shift);

    let f1 = cfg
        .keybinds
        .iter()
        .find(|b| matches!(b.key, KeyToken::F(1)))
        .expect("f1 binds something");
    assert_eq!(f1.mods, Modifiers::default());

    let preset = cfg
        .keybinds
        .iter()
        .find(|b| matches!(b.action, KeyAction::PanePreset(_)))
        .expect("pane.preset.dev binds something");
    assert!(matches!(preset.action, KeyAction::PanePreset(ref s) if s == "dev"));
}

#[test]
fn parse_is_idempotent() {
    let a = parse(&fixture()).unwrap();
    let b = parse(&fixture()).unwrap();
    assert_eq!(a, b);
}

#[test]
fn unknown_top_level_errors() {
    let r = parse("flag { }");
    let msg = format!("{}", r.unwrap_err());
    assert!(msg.contains("flag"), "got: {msg}");
}

#[test]
fn unknown_layout_node_errors() {
    let r = parse(r#"layout { mystery { } }"#);
    let msg = format!("{}", r.unwrap_err());
    assert!(msg.contains("mystery"), "got: {msg}");
}
