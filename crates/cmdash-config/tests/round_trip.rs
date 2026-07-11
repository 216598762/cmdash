use std::path::PathBuf;

use cmdash_config::{
    parse, Config, KeyAction, KeyToken, LayoutNode, Modifiers, Pane, PaneKind,
    Ratio, SplitAxis,
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

#[test]
fn parses_theme_example_round_trip() {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("..");
    p.push("..");
    p.push("examples");
    p.push("09-theme.kdl");
    let src = std::fs::read_to_string(&p).expect("09-theme.kdl is readable");
    // The entire file must parse as valid KDL v2.
    let cfg = parse(&src).expect("09-theme.kdl must parse cleanly");
    let theme = cfg.theme.expect("theme block must be parsed from 09-theme.kdl");

    // All 15 theme keys should be populated (the light theme is active)
    assert!(theme.default_fg.is_some(), "default-fg should be set");
    assert!(theme.default_bg.is_some(), "default-bg should be set");
    assert!(theme.cursor_style.is_some(), "cursor-style should be set");
    assert!(theme.tab_bar_bg.is_some(), "tab-bar-bg should be set");
    assert!(theme.tab_active_bg.is_some(), "tab-active-bg should be set");
    assert!(theme.tab_active_fg.is_some(), "tab-active-fg should be set");
    assert!(theme.tab_inactive_bg.is_some(), "tab-inactive-bg should be set");
    assert!(theme.tab_inactive_fg.is_some(), "tab-inactive-fg should be set");
    assert!(theme.status_bar_bg.is_some(), "status-bar-bg should be set");
    assert!(theme.status_mode_fg.is_some(), "status-mode-fg should be set");
    assert!(theme.status_mode_bg.is_some(), "status-mode-bg should be set");
    assert!(theme.status_clock_fg.is_some(), "status-clock-fg should be set");
    assert!(theme.status_pane_title_fg.is_some(), "status-pane-title-fg should be set");
    assert!(theme.border_color.is_some(), "border-color should be set");
    assert!(theme.error_color.is_some(), "error-color should be set");
}

/// KDL round-trip test: a theme block with all 15 recognized keys
/// parses cleanly via the strict `parse()` path.
#[test]
fn parses_theme_all_keys_round_trip() {
    let src = r#"theme {
    default-fg      "black"
    default-bg      "white"
    cursor-style    "bar"
    tab-bar-bg         "gray"
    tab-active-bg      "blue"
    tab-active-fg      "white"
    tab-inactive-bg    "gray"
    tab-inactive-fg    "black"
    status-bar-bg        "gray"
    status-mode-fg       "white"
    status-mode-bg       "blue"
    status-clock-fg      "black"
    status-pane-title-fg "black"
    border-color    "gray"
    error-color     "red"
}
"#;
    let cfg = parse(src).expect("theme with all 15 keys must parse");
    let theme = cfg.theme.expect("theme block present");

    assert!(theme.default_fg.is_some());
    assert!(theme.default_bg.is_some());
    assert!(theme.cursor_style.is_some());
    assert!(theme.tab_bar_bg.is_some());
    assert!(theme.tab_active_bg.is_some());
    assert!(theme.tab_active_fg.is_some());
    assert!(theme.tab_inactive_bg.is_some());
    assert!(theme.tab_inactive_fg.is_some());
    assert!(theme.status_bar_bg.is_some());
    assert!(theme.status_mode_fg.is_some());
    assert!(theme.status_mode_bg.is_some());
    assert!(theme.status_clock_fg.is_some());
    assert!(theme.status_pane_title_fg.is_some());
    assert!(theme.border_color.is_some());
    assert!(theme.error_color.is_some());
}

/// Theme block with hex color values and cursor-style aliases.
#[test]
fn parses_theme_hex_and_aliases() {
    let src = r##"theme {
    default-fg      "#ff8800"
    default-bg      "#f80"
    cursor-style    "under"
    tab-active-bg   "rgb(68,136,255)"
    error-color     "i196"
    border-color    "reset"
}
"##;
    let cfg = parse(src).expect("theme with hex/rgb/indexed/reset must parse");
    let theme = cfg.theme.expect("theme block present");
    assert!(theme.default_fg.is_some());
    assert!(theme.default_bg.is_some());
    assert!(theme.cursor_style.is_some());
    assert!(theme.tab_active_bg.is_some());
    assert!(theme.error_color.is_some());
    assert!(theme.border_color.is_some());
}

/// Unknown key in theme block returns an error.
#[test]
fn theme_unknown_key_errors() {
    let src = r#"theme {
    bogus-key "red"
}
"#;
    let err = parse(src).expect_err("unknown theme key must error");
    let msg = format!("{}", err);
    assert!(msg.contains("bogus-key"), "error should mention the bad key: {msg}");
}
