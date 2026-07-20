//! Server-side task for Milestone 1 of session persistence.
//!
//! `ServerTask` owns all long-lived session state: pane runners,
//! layout tree, tab stack, config, and per-pane mode flags. It
//! receives input/actions from the frontend over an mpsc channel,
//! advances the PTY state machines on a tick interval, and streams
//! `RenderFrame` payloads back to the frontend.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use cmdash_config::{
    KeyAction, LayoutNode, Pane as CfgPane, PaneKind, Ratio as CfgRatio, SplitAxis as CfgSplitAxis,
};
use cmdash_keybinds::Mode;
use cmdash_layout::{
    adjacent_pane, remove_leaf, replace_leaf_with_split, update_split_ratio, walk_imut,
    ComputedLayout, Direction, PaneId, Rect as LayoutRect,
};
use cmdash_pty::{PaneLayerId, ShellSpec, DEFAULT_SCROLLBACK_CAPACITY};
use tracing::{debug, info, warn};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::clipboard::copy_text_to_clipboard;
use crate::graphics::TermCapabilities;
use crate::pane::{PaneCloseTx, PaneRunner};
use crate::protocol::{
    ClientMessage, ConfigReload, CopyModeState, FrameData, HostModeFlags, ServerConfig,
    ServerMessage, TabBarDataOwned, WidgetFactories,
};
use crate::render::extract_selected_text;
use crate::tabs::TabStack;
use crate::tick_context::{
    alloc_layer_id, encode_kitty_key_event, event_to_bytes, redacted_event_debug,
    shell_spec_from_command, ReconcileMode, STATUS_BAR_HEIGHT, TAB_BAR_HEIGHT,
};

/// Per-tab payload carried by every [`Tab<T>`] in the
/// `TabStack<TabState>` stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TabState {
    pub runners: Vec<PaneRunner>,
    pub focus: usize,
    pub layout_root: LayoutNode,
    pub stack_focus: BTreeMap<PaneId, usize>,
}

/// Server-side task. Owns runners, layout, tabs, and config.
pub struct ServerTask {
    runners: Vec<PaneRunner>,
    focus: usize,
    running: bool,
    close_rx: UnboundedReceiver<PaneLayerId>,
    close_tx: UnboundedSender<PaneLayerId>,
    tick: Duration,
    layout_root: LayoutNode,
    pending_resize: Option<(u16, u16)>,
    last_area: LayoutRect,
    /// Last host-terminal dimensions seen by `relayout`. Kept
    /// separate from `last_area` (which is the pane layout area)
    /// so callers like `pane_resize_by_direction` can re-run
    /// `relayout` with the original host size.
    last_host_size: (u16, u16),
    presets: BTreeMap<String, LayoutNode>,
    stack_focus: BTreeMap<PaneId, usize>,
    shell: ShellSpec,
    drag_state: Option<DragState>,
    copy_mode: Option<crate::protocol::CopyModeState>,
    last_focused_snapshot: Option<cmdash_pty::PaneTerminalState>,
    tabs: TabStack<TabState>,
    config_reload_rx: Option<UnboundedReceiver<ConfigReload>>,
    status_bar: Option<cmdash_config::Bar>,
    theme: cmdash_config::Theme,
    widget_factories: WidgetFactories,
    pane_keyboard_flags: HashMap<PaneLayerId, u8>,
    pane_bracketed_paste: HashMap<PaneLayerId, bool>,
    pane_focus_reporting: HashMap<PaneLayerId, bool>,
    pane_alternate_screen: HashMap<PaneLayerId, bool>,
    client_rx: UnboundedReceiver<ClientMessage>,
    server_tx: UnboundedSender<ServerMessage>,
    /// Host terminal capabilities (kitty keyboard, bracketed paste,
    /// focus events). Detected once at startup and used by input
    /// handling to decide encoding strategies.
    caps: TermCapabilities,
    /// Whether the host terminal currently has focus. Drives
    /// forwarding of focus-change events to the focused pane.
    host_focused: bool,
    /// Current keybind mode. Synced to the frontend via frame
    /// messages so its `Router` matches the server's state.
    mode: Mode,
}

/// Bundle of mpsc channels shared between [`ServerTask`] and
/// [`FrontendTask`](crate::frontend_task::FrontendTask).
#[derive(Debug)]
pub struct ServerChannels {
    pub close_tx: UnboundedSender<PaneLayerId>,
    pub close_rx: UnboundedReceiver<PaneLayerId>,
    pub config_reload_rx: Option<UnboundedReceiver<ConfigReload>>,
    pub client_rx: UnboundedReceiver<ClientMessage>,
    pub server_tx: UnboundedSender<ServerMessage>,
}

/// Active drag-to-resize state for Alt+drag on split panes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DragState {
    split_path: [u16; 8],
    split_path_len: u8,
    start_pos: u16,
    initial_ratio: u8,
    axis: cmdash_config::SplitAxis,
    total_cells: u16,
}

impl ServerTask {
    /// Construct a new `ServerTask` from an initial config,
    /// pane runners, layout area, and a bundle of communication
    /// channels.
    pub fn new(
        config: ServerConfig,
        runners: Vec<PaneRunner>,
        focus: usize,
        last_area: LayoutRect,
        channels: ServerChannels,
    ) -> Self {
        let initial_tab = TabState {
            runners: runners.clone(),
            focus,
            layout_root: config.layout_root.clone(),
            stack_focus: BTreeMap::new(),
        };
        Self {
            runners,
            focus,
            running: true,
            close_rx: channels.close_rx,
            close_tx: channels.close_tx,
            tick: Duration::from_millis(33),
            layout_root: config.layout_root,
            pending_resize: None,
            last_area,
            last_host_size: (last_area.w, last_area.h),
            presets: config.presets,
            stack_focus: BTreeMap::new(),
            shell: config.shell,
            drag_state: None,
            copy_mode: None,
            last_focused_snapshot: None,
            tabs: TabStack::new(initial_tab),
            config_reload_rx: channels.config_reload_rx,
            status_bar: config.status_bar,
            theme: config.theme,
            widget_factories: config.widget_factories,
            pane_keyboard_flags: HashMap::new(),
            pane_bracketed_paste: HashMap::new(),
            pane_focus_reporting: HashMap::new(),
            pane_alternate_screen: HashMap::new(),
            client_rx: channels.client_rx,
            server_tx: channels.server_tx,
            caps: TermCapabilities::detect(),
            host_focused: true,
            mode: Mode::Normal,
        }
    }

    /// Drive the server-side event loop until `running` becomes
    /// `false` or every pane exits.
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut tick_interval = tokio::time::interval(self.tick);

        while self.running {
            tokio::select! {
                msg = self.client_rx.recv() => {
                    match msg {
                        Some(ClientMessage::Action(action)) => self.apply_action(action),
                        Some(ClientMessage::Input(evt)) => self.handle_input(&evt),
                        Some(ClientMessage::Resize(w, h)) => self.pending_resize = Some((w, h)),
                        Some(ClientMessage::Detach) => self.running = false,
                        None => {
                            warn!("client channel closed; exiting server loop");
                            break;
                        }
                    }
                }

                id = self.close_rx.recv() => {
                    if let Some(id) = id {
                        self.close_pane(id);
                    }
                }

                msg = async {
                    match self.config_reload_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(msg) = msg {
                        self.apply_config_reload(msg);
                    }
                }

                _ = tick_interval.tick() => {
                    self.tick_and_emit()?;
                }
            }
        }

        Ok(())
    }

    /// Apply a parsed key action.
    fn apply_action(&mut self, action: KeyAction) {
        match action {
            KeyAction::AppClose => self.running = false,
            KeyAction::PaneFocusNext => {
                if !self.runners.is_empty() {
                    self.set_focus((self.focus + 1) % self.runners.len());
                }
            }
            KeyAction::PaneFocusPrev => {
                if !self.runners.is_empty() {
                    self.set_focus((self.focus + self.runners.len() - 1) % self.runners.len());
                }
            }
            KeyAction::PaneFocusUp => self.focus_by_direction(Direction::Up),
            KeyAction::PaneFocusDown => self.focus_by_direction(Direction::Down),
            KeyAction::PaneFocusLeft => self.focus_by_direction(Direction::Left),
            KeyAction::PaneFocusRight => self.focus_by_direction(Direction::Right),
            KeyAction::PaneStackCycle => self.handle_stack_cycle(),
            KeyAction::PaneStackDown => self.crosstack_member(Direction::Down, true),
            KeyAction::PaneStackUp => self.crosstack_member(Direction::Up, false),
            KeyAction::PaneStackLeft => self.crosstack_member(Direction::Left, false),
            KeyAction::PaneStackRight => self.crosstack_member(Direction::Right, true),
            KeyAction::AppNewPane => self.split_focused_for_new_pane(),
            KeyAction::PaneClose => self.close_focused_and_rebalance(),
            KeyAction::PanePreset(name) => self.swap_to_preset(&name),
            KeyAction::TabNew => self.create_new_tab(),
            KeyAction::TabClose => self.close_active_tab(),
            KeyAction::TabSwitch(n) => self.switch_to_tab(n),
            KeyAction::EnterPaneResize => {
                self.mode = Mode::PaneResize;
            }
            KeyAction::EnterTabSwitch => {
                self.mode = Mode::TabSwitch;
            }
            KeyAction::EnterPresetPick => {
                self.mode = Mode::PresetPick;
            }
            KeyAction::EnterCopyMode => {
                self.enter_copy_mode();
            }
            KeyAction::ModeExit => {
                self.mode = Mode::Normal;
                self.copy_mode = None;
                self.last_focused_snapshot = None;
            }
            KeyAction::CopyModeMoveUp => self.copy_mode_move(0, -1),
            KeyAction::CopyModeMoveDown => self.copy_mode_move(0, 1),
            KeyAction::CopyModeMoveLeft => self.copy_mode_move(-1, 0),
            KeyAction::CopyModeMoveRight => self.copy_mode_move(1, 0),
            KeyAction::CopyModeStartSelection => self.copy_mode_start_selection(),
            KeyAction::CopyModeCopy => {
                if let Err(e) = self.copy_mode_copy() {
                    warn!(error = ?e, "copy-mode copy failed");
                }
            }
            KeyAction::PaneResizeUp => self.pane_resize_by_direction(Direction::Up),
            KeyAction::PaneResizeDown => self.pane_resize_by_direction(Direction::Down),
            KeyAction::PaneResizeLeft => self.pane_resize_by_direction(Direction::Left),
            KeyAction::PaneResizeRight => self.pane_resize_by_direction(Direction::Right),
        }
    }

    /// Handle a raw crossterm input event that the frontend's
    /// Router did not consume. Mouse, paste, focus, and
    /// unmatched key events are forwarded to the focused
    /// pane's PTY. The frontend already handled keybind
    /// dispatch and resize signals.
    fn handle_input(&mut self, evt: &Event) {
        debug!("server input = {}", redacted_event_debug(evt));
        // Mouse events: click-to-focus, Alt+drag resize, PTY forwarding.
        if let Event::Mouse(mouse) = evt {
            self.handle_mouse_event(mouse);
            return;
        }
        // Paste events: forward to the focused pane.
        if let Event::Paste(text) = evt {
            self.handle_paste(text);
            return;
        }
        // Host focus-change events.
        if let Event::FocusGained = evt {
            self.host_focused = true;
            self.forward_focus_event_to_focused_pane(true);
            return;
        }
        if let Event::FocusLost = evt {
            self.host_focused = false;
            self.forward_focus_event_to_focused_pane(false);
            return;
        }
        let Event::Key(KeyEvent {
            code,
            kind,
            modifiers,
            ..
        }) = evt
        else {
            return;
        };
        // Intercept PageUp/PageDown for scrollback navigation.
        let focused_in_alt = self
            .runners
            .get(self.focus)
            .map(|r| r.in_alternate_screen())
            .unwrap_or(false);
        let page_size = self
            .runners
            .get(self.focus)
            .map(|r| r.computed().rect.h as usize)
            .unwrap_or(24);
        match code {
            KeyCode::PageUp if !focused_in_alt => {
                if let Some(runner) = self.runners.get_mut(self.focus) {
                    runner.scrollback_up(page_size);
                }
                return;
            }
            KeyCode::PageDown if !focused_in_alt => {
                let in_sb = self
                    .runners
                    .get(self.focus)
                    .map(|r| r.in_scrollback())
                    .unwrap_or(false);
                if in_sb {
                    if let Some(runner) = self.runners.get_mut(self.focus) {
                        runner.scrollback_down(page_size);
                    }
                    return;
                }
            }
            _ => {
                if let Some(runner) = self.runners.get_mut(self.focus) {
                    if runner.in_scrollback() {
                        runner.scrollback_reset();
                    }
                }
            }
        }
        let focused_flags = self
            .runners
            .get(self.focus)
            .map(|r| r.keyboard_flags())
            .unwrap_or(0);
        let is_widget = self
            .runners
            .get(self.focus)
            .map(|r| r.is_widget())
            .unwrap_or(false);
        if is_widget {
            if !matches!(kind, KeyEventKind::Press) {
                return;
            }
            if let Some(runner) = self.runners.get_mut(self.focus) {
                if let Some(widget) = runner.widget.as_mut() {
                    let widget_code = match code {
                        KeyCode::Char(c) => cmdash_widget_sdk::KeyCode::Char(*c),
                        KeyCode::Enter => cmdash_widget_sdk::KeyCode::Enter,
                        KeyCode::Esc => cmdash_widget_sdk::KeyCode::Esc,
                        KeyCode::Backspace => cmdash_widget_sdk::KeyCode::Backspace,
                        KeyCode::Tab => cmdash_widget_sdk::KeyCode::Tab,
                        KeyCode::Up => cmdash_widget_sdk::KeyCode::Up,
                        KeyCode::Down => cmdash_widget_sdk::KeyCode::Down,
                        KeyCode::Left => cmdash_widget_sdk::KeyCode::Left,
                        KeyCode::Right => cmdash_widget_sdk::KeyCode::Right,
                        KeyCode::Home => cmdash_widget_sdk::KeyCode::Home,
                        KeyCode::End => cmdash_widget_sdk::KeyCode::End,
                        KeyCode::PageUp => cmdash_widget_sdk::KeyCode::PageUp,
                        KeyCode::PageDown => cmdash_widget_sdk::KeyCode::PageDown,
                        KeyCode::F(n) => cmdash_widget_sdk::KeyCode::F(*n),
                        _ => return,
                    };
                    let widget_evt = cmdash_widget_sdk::WidgetEvent::Key {
                        code: widget_code,
                        modifiers: cmdash_widget_sdk::KeyModifiers {
                            ctrl: modifiers.contains(KeyModifiers::CONTROL),
                            shift: modifiers.contains(KeyModifiers::SHIFT),
                            alt: modifiers.contains(KeyModifiers::ALT),
                            super_: false,
                        },
                    };
                    widget.on_event(&widget_evt);
                }
            }
            return;
        }
        let bytes = if focused_flags != 0 && self.caps.supports_kitty_keyboard() {
            encode_kitty_key_event(code, *modifiers, *kind)
        } else if matches!(kind, KeyEventKind::Press) {
            event_to_bytes(*code)
        } else {
            None
        };
        let Some(bytes) = bytes else {
            return;
        };
        if let Some(runner) = self.runners.get_mut(self.focus) {
            if let Err(e) = runner.write_input(&bytes) {
                debug!(
                    error = ?e,
                    layer_id = ?runner.layer_id(),
                    "write_input failed"
                );
            }
        }
    }

    /// Set the focused pane index.
    fn set_focus(&mut self, idx: usize) {
        if idx != self.focus {
            self.copy_mode = None;
        }
        self.focus = idx;
    }

    /// Move focus in the given direction via rect-proximity
    /// algorithm (`adjacent_pane`). No-op if no neighbour exists.
    fn focus_by_direction(&mut self, dir: Direction) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        let layout = match ComputedLayout::compute(&self.layout_root, self.last_area) {
            Ok(l) => l,
            Err(e) => {
                warn!(error = ?e, "focus_by_direction: compute failed");
                return;
            }
        };
        let Some(target_id) = adjacent_pane(&layout, focused_id, dir) else {
            return;
        };
        if let Some(idx) = self
            .runners
            .iter()
            .position(|r| r.computed().id == target_id)
        {
            self.set_focus(idx);
        }
    }

    /// Close a pane and revoke its layer.
    fn close_pane(&mut self, id: PaneLayerId) {
        self.pane_keyboard_flags.remove(&id);
        self.pane_bracketed_paste.remove(&id);
        self.pane_focus_reporting.remove(&id);
        self.pane_alternate_screen.remove(&id);
        // TODO: notify frontend of closed pane
    }

    /// Apply a config reload payload.
    fn apply_config_reload(&mut self, msg: ConfigReload) {
        self.presets = msg.presets;
        self.status_bar = msg.status_bar;
        self.theme = msg.theme.unwrap_or_default();
        if let Some(new_layout) = msg.layout_root {
            if new_layout != self.layout_root {
                info!("config hot-reload: layout changed; rebuilding panes");
                self.layout_root = new_layout;
                self.reconcile_runners(ReconcileMode::Wholesale);
            }
        }
        // TODO: forward `msg.keybinds` to the frontend via a
        // `ServerMessage::UpdateKeybinds` variant so the
        // frontend's Router can reload keybinds on config
        // hot-reload. The server doesn't own the Router.
    }

    /// Single tick: advance runners, collect snapshots, and emit a
    /// `ServerMessage::FrameIncremental` to the frontend.
    fn tick_and_emit(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.process_pending_resize();
        self.drain_close_channel();

        let mut all_exited = true;
        let mut snapshots: Vec<Option<cmdash_pty::PaneTerminalState>> =
            Vec::with_capacity(self.runners.len());
        for runner in self.runners.iter_mut() {
            if runner.try_wait_exit()?.is_none() {
                all_exited = false;
            }
            if runner.is_widget() {
                snapshots.push(None);
            } else {
                snapshots.push(Some(runner.tick()?));
            }
        }

        self.update_last_focused_snapshot(&snapshots);
        self.update_keyboard_flags_from_snapshots(&snapshots);
        self.update_bracketed_paste_from_snapshots(&snapshots);
        self.update_focus_reporting_from_snapshots(&snapshots);
        self.update_alternate_screen_from_snapshots(&snapshots);
        self.handle_device_attributes_queries(&snapshots);

        let mut graphics = Vec::new();
        for (runner, snap) in self.runners.iter().zip(snapshots.iter()) {
            if let Some(snap) = snap {
                for ev in &snap.pending_events {
                    if let cmdash_pty::PaneEvent::KittyGraphic { cmd } = ev {
                        graphics.push((runner.layer_id(), cmd.clone()));
                    }
                }
            }
        }

        let layout = ComputedLayout::compute(&self.layout_root, self.last_area)?;
        let mut grids = std::collections::HashMap::new();
        let mut cursors = std::collections::HashMap::new();
        for (runner, snap) in self.runners.iter().zip(snapshots.iter()) {
            if let Some(snap) = snap {
                let layer_id = runner.layer_id();
                grids.insert(layer_id, snap.grid.clone());
                let cursor = snap.grid.cursor();
                cursors.insert(layer_id, cursor);
            }
        }

        let mode_flags = HostModeFlags {
            kitty_keyboard: self.pane_keyboard_flags.values().fold(0, |acc, &f| acc | f),
            bracketed_paste: self.pane_bracketed_paste.values().any(|&v| v),
            focus_reporting: self.pane_focus_reporting.values().any(|&v| v),
        };

        let tabs = TabBarDataOwned {
            labels: self.tabs.iter().map(|t| t.label.clone()).collect(),
            active_idx: self.tabs.active_idx(),
            bar_width_cells: self.last_area.w,
        };

        let _ = self.server_tx.send(ServerMessage::FrameIncremental {
            layout,
            frame: FrameData {
                grids,
                graphics,
                cursors,
            },
            mode_flags,
            focus: self.focus,
            tabs,
            running: self.running,
            mode: self.mode,
            copy_mode: self.copy_mode,
        });

        if all_exited {
            self.running = false;
        }

        Ok(())
    }

    fn process_pending_resize(&mut self) {
        if let Some((w, h)) = self.pending_resize.take() {
            self.relayout(w, h);
        }
    }
    fn relayout(&mut self, w: u16, h: u16) {
        if w == 0 || h == 0 {
            warn!(w, h, "relayout: zero-area resize signal; skipping");
            return;
        }
        self.last_host_size = (w, h);
        let chrome_height = TAB_BAR_HEIGHT
            + if self.status_bar.as_ref().is_some_and(|sb| sb.enabled) {
                STATUS_BAR_HEIGHT
            } else {
                0
            };
        let layout_area = LayoutRect {
            x: 0,
            y: 0,
            w,
            h: h.saturating_sub(chrome_height),
        };
        let layout = match ComputedLayout::compute(&self.layout_root, layout_area) {
            Ok(l) => l,
            Err(e) => {
                warn!(error = ?e, w, h, "relayout: ComputedLayout::compute failed; skipping");
                return;
            }
        };
        if layout.panes.len() != self.runners.len() {
            warn!(
                live_runners = self.runners.len(),
                computed_panes = layout.panes.len(),
                "relayout: runner/pane count diverged; skipping per-pane resize"
            );
            return;
        }
        for (runner, pane) in self.runners.iter_mut().zip(layout.panes.iter()) {
            if runner.computed().id != pane.id {
                warn!("relayout: runners[i]/layout.panes[i] pairing violated");
                continue;
            }
            if let Err(e) = runner.resize(pane.rect) {
                warn!(error = ?e, layer_id = ?runner.layer_id(), "relayout: pane resize failed");
            }
        }
        self.last_area = layout_area;
    }

    fn drain_close_channel(&mut self) {
        while let Ok(id) = self.close_rx.try_recv() {
            self.close_pane(id);
        }
    }

    fn update_last_focused_snapshot(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        if self.copy_mode.is_some() {
            self.last_focused_snapshot = snapshots.get(self.focus).cloned().flatten();
        } else {
            self.last_focused_snapshot = None;
        }
    }

    fn update_keyboard_flags_from_snapshots(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        let _changed = crate::pane::collect_keyboard_enhancement_flags(
            &self.runners,
            snapshots,
            &mut self.pane_keyboard_flags,
        );
    }

    fn update_bracketed_paste_from_snapshots(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        let _changed = crate::pane::collect_bracketed_paste_flags(
            &self.runners,
            snapshots,
            &mut self.pane_bracketed_paste,
        );
    }

    fn update_focus_reporting_from_snapshots(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        let _changed = crate::pane::collect_focus_reporting_flags(
            &self.runners,
            snapshots,
            &mut self.pane_focus_reporting,
        );
    }

    fn update_alternate_screen_from_snapshots(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        let _changed = crate::pane::collect_alternate_screen_flags(
            &self.runners,
            snapshots,
            &mut self.pane_alternate_screen,
        );
    }

    fn handle_device_attributes_queries(
        &mut self,
        snapshots: &[Option<cmdash_pty::PaneTerminalState>],
    ) {
        use cmdash_pty::{DeviceAttributesKind, PaneEvent};
        for (runner, snapshot) in self.runners.iter_mut().zip(snapshots.iter()) {
            if runner.is_widget() {
                continue;
            }
            let Some(snapshot) = snapshot else { continue };
            for event in &snapshot.pending_events {
                let response = match event {
                    PaneEvent::DeviceAttributesQuery {
                        kind: DeviceAttributesKind::Primary,
                    } => Some(self.caps.da1_response()),
                    PaneEvent::DeviceAttributesQuery {
                        kind: DeviceAttributesKind::Secondary,
                    } => Some(self.caps.da2_response()),
                    _ => None,
                };
                if let Some(resp) = response {
                    if let Err(e) = runner.write_input(resp.as_bytes()) {
                        debug!(error = ?e, layer_id = ?runner.layer_id(), "device attributes response write failed");
                    }
                }
            }
        }
    }
    // ------------------------------------------------------------------
    // ZStack focus helpers
    // ------------------------------------------------------------------

    /// Locate the focused pane's parent `ZStack` + its member
    /// index. Returns `Some((parent_path, member_idx))` if the
    /// focused pane is a direct child of a `LayoutNode::ZStack`,
    /// otherwise `None`.
    fn focused_zstack_context(
        layout_root: &LayoutNode,
        focused_path: &[u16],
    ) -> Option<(Vec<u16>, usize)> {
        if focused_path.is_empty() {
            return None;
        }
        let last_idx = *focused_path.last()? as usize;
        let parent_path = focused_path.split_last()?.1.to_vec();
        let parent_node = walk_imut(layout_root, &parent_path).ok()?;
        match parent_node {
            LayoutNode::ZStack { panes } => {
                if last_idx < panes.len() {
                    Some((parent_path, last_idx))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// `PaneStackCycle`: advance focus to the next ZStack member,
    /// wrapping from the last member back to the first.
    fn handle_stack_cycle(&mut self) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        let seed_path = focused_id.path();
        let tree_path: &[u16] = if !seed_path.is_empty() {
            &seed_path[1..]
        } else {
            &[]
        };
        let Some((parent_path, member_idx)) =
            Self::focused_zstack_context(&self.layout_root, tree_path)
        else {
            return;
        };
        let Some(LayoutNode::ZStack { panes }) = walk_imut(&self.layout_root, &parent_path).ok()
        else {
            return;
        };
        let next_idx = (member_idx + 1) % panes.len();
        let mut next_path = parent_path.clone();
        next_path.push(next_idx as u16);
        if let Some(idx) = self.runners.iter().position(|r| {
            let rp = r.computed().id.path();
            let tp: &[u16] = if !rp.is_empty() { &rp[1..] } else { &[] };
            tp == next_path.as_slice()
        }) {
            self.set_focus(idx);
            let new_id = self.runners[idx].computed().id;
            self.stack_focus.insert(new_id, next_idx);
        }
    }

    /// Directed `ZStack` focus primitive. Advances or retreats
    /// through ZStack members, handing off to a geometric
    /// neighbour at the boundary via `focus_by_direction`.
    fn crosstack_member(&mut self, handoff_direction: Direction, advance: bool) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        let seed_path = focused_id.path();
        let tree_path: &[u16] = if !seed_path.is_empty() {
            &seed_path[1..]
        } else {
            &[]
        };
        let Some((parent_path, member_idx)) =
            Self::focused_zstack_context(&self.layout_root, tree_path)
        else {
            return;
        };
        let Some(LayoutNode::ZStack { panes }) = walk_imut(&self.layout_root, &parent_path).ok()
        else {
            return;
        };
        if advance {
            if member_idx + 1 == panes.len() {
                self.focus_by_direction(handoff_direction);
                return;
            }
            let next_idx = member_idx + 1;
            let mut next_path = parent_path.clone();
            next_path.push(next_idx as u16);
            if let Some(idx) = self.runners.iter().position(|r| {
                let rp = r.computed().id.path();
                let tp: &[u16] = if !rp.is_empty() { &rp[1..] } else { &[] };
                tp == next_path.as_slice()
            }) {
                self.set_focus(idx);
                let new_id = self.runners[idx].computed().id;
                self.stack_focus.insert(new_id, next_idx);
            }
        } else {
            if member_idx == 0 {
                self.focus_by_direction(handoff_direction);
                return;
            }
            let prev_idx = member_idx - 1;
            let mut next_path = parent_path.clone();
            next_path.push(prev_idx as u16);
            if let Some(idx) = self.runners.iter().position(|r| {
                let rp = r.computed().id.path();
                let tp: &[u16] = if !rp.is_empty() { &rp[1..] } else { &[] };
                tp == next_path.as_slice()
            }) {
                self.set_focus(idx);
                let new_id = self.runners[idx].computed().id;
                self.stack_focus.insert(new_id, prev_idx);
            }
        }
    }

    // ------------------------------------------------------------------
    // Layout mutations (AppNewPane, PaneClose, PanePreset)
    // ------------------------------------------------------------------

    /// `AppNewPane`: replace the focused leaf with a `Split`
    /// containing the original and a new shell pane.
    fn split_focused_for_new_pane(&mut self) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        let seed_path = focused_id.path();
        let tree_path: &[u16] = if !seed_path.is_empty() {
            &seed_path[1..]
        } else {
            &[]
        };
        if tree_path.is_empty() {
            let original_root = match &self.layout_root {
                LayoutNode::Pane(p) => LayoutNode::Pane(p.clone()),
                _ => {
                    warn!(
                        "AppNewPane: focused leaf IS the root but root is {:?}; no-op",
                        self.layout_root
                    );
                    return;
                }
            };
            self.layout_root = LayoutNode::Split {
                axis: CfgSplitAxis::Horizontal,
                ratio: CfgRatio(50),
                children: vec![
                    original_root,
                    LayoutNode::Pane(CfgPane {
                        kind: PaneKind::Shell,
                        label: None,
                        command: None,
                        scrollback_capacity: None,
                    }),
                ],
            };
            self.reconcile_runners(ReconcileMode::InPlace);
            return;
        }
        let new_leaf = LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: None,
            command: None,
            scrollback_capacity: None,
        });
        match replace_leaf_with_split(
            &mut self.layout_root,
            tree_path,
            new_leaf,
            CfgSplitAxis::Horizontal,
            CfgRatio(50),
        ) {
            Ok(_) => self.reconcile_runners(ReconcileMode::InPlace),
            Err(e) => {
                warn!(error = ?e, "AppNewPane: replace_leaf_with_split failed");
            }
        }
    }

    /// `PaneClose`: drop the focused runner, remove the leaf
    /// from the layout tree, and reconcile survivors.
    fn close_focused_and_rebalance(&mut self) {
        if self.runners.is_empty() {
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        let focused_id = self.runners[self.focus].computed().id;
        if focused_id.path().len() <= 1 {
            warn!("PaneClose: focused leaf IS the root; quitting");
            self.runners.clear();
            self.running = false;
            return;
        }
        let seed_path = focused_id.path();
        let tree_path: &[u16] = &seed_path[1..];
        self.runners.remove(self.focus);
        if let Err(e) = remove_leaf(&mut self.layout_root, tree_path) {
            warn!(error = ?e, "PaneClose: remove_leaf failed; treating as quit");
            self.running = false;
            return;
        }
        if self.runners.is_empty() {
            self.running = false;
            return;
        }
        if self.focus >= self.runners.len() {
            self.set_focus(self.runners.len() - 1);
        }
        self.reconcile_runners(ReconcileMode::InPlace);
    }

    /// `PanePreset`: wholesale-swap the layout tree for a named
    /// preset and reconcile all runners.
    fn swap_to_preset(&mut self, name: &str) {
        let Some(new_root) = self.presets.get(name).cloned() else {
            warn!(name, "PanePreset: unknown name; no-op");
            return;
        };
        self.runners.clear();
        self.layout_root = new_root;
        self.focus = 0;
        self.reconcile_runners(ReconcileMode::Wholesale);
        if self.runners.is_empty() {
            warn!(name, "PanePreset: new tree has zero leaves; quitting");
            self.running = false;
        }
    }

    // ------------------------------------------------------------------
    // Runner reconciliation
    // ------------------------------------------------------------------

    /// Reconcile live runners against the current layout tree.
    ///
    /// InPlace: survivors matched by label keep their `PaneLayerId`.
    /// Wholesale: every old runner is dropped; every new pane
    /// gets a freshly-allocated `PaneLayerId` from
    /// [`alloc_layer_id`].
    fn reconcile_runners(&mut self, mode: ReconcileMode) {
        let post_layout = match ComputedLayout::compute(&self.layout_root, self.last_area) {
            Ok(l) => l,
            Err(e) => {
                warn!(error = ?e, "reconcile: compute failed");
                return;
            }
        };
        let old_runners = std::mem::take(&mut self.runners);
        let mut survivors: HashMap<String, Vec<PaneRunner>> = HashMap::new();
        let mut to_drop: Vec<PaneRunner> = Vec::with_capacity(old_runners.len());
        match mode {
            ReconcileMode::Wholesale => {
                to_drop = old_runners;
                self.stack_focus.clear();
            }
            ReconcileMode::InPlace => {
                for r in old_runners {
                    if let Some(label) = r.computed().label.clone() {
                        let preserved = post_layout
                            .panes
                            .iter()
                            .any(|p| p.label.as_deref() == Some(label.as_str()));
                        if preserved {
                            survivors.entry(label).or_default().push(r);
                            continue;
                        }
                    }
                    to_drop.push(r);
                }
            }
        }
        drop(to_drop);
        let mut new_runners: Vec<PaneRunner> = Vec::with_capacity(post_layout.panes.len());
        let env_vars = self.caps.to_env_vars();
        for pane in &post_layout.panes {
            let survivor_opt: Option<PaneRunner> = if matches!(mode, ReconcileMode::InPlace) {
                pane.label
                    .as_deref()
                    .map(str::to_string)
                    .and_then(|l| survivors.get_mut(&l).and_then(|v| v.pop()))
            } else {
                None
            };
            if let Some(mut r) = survivor_opt {
                let mut updated = pane.clone();
                if r.resize(pane.rect).is_err() {
                    warn!(rect = ?pane.rect, "reconcile: resize failed; keeping previous rect");
                    updated.rect = r.computed().rect;
                }
                r.rebind_pane(updated);
                new_runners.push(r);
            } else {
                let layer_id = if matches!(mode, ReconcileMode::Wholesale) {
                    alloc_layer_id()
                } else {
                    crate::derive_layer_id_for_tab(&pane.id, self.active_tab_idx_u32())
                };
                let tx: PaneCloseTx = self.close_tx.clone();
                match &pane.kind {
                    PaneKind::Widget { ref_name } => {
                        if let Some(factory) = self.widget_factories.get(ref_name) {
                            let raw = unsafe {
                                (factory.create)(cmdash_widget_sdk::CMDASH_WIDGET_ABI_VERSION)
                            };
                            if raw.is_null() {
                                warn!(name = %ref_name, "reconcile: widget create returned null");
                            } else {
                                let widget = unsafe { cmdash_widget_sdk::widget_from_raw(raw) };
                                new_runners.push(PaneRunner::spawn_widget(
                                    pane.clone(),
                                    layer_id,
                                    widget,
                                    Some(tx),
                                ));
                            }
                        } else {
                            warn!(name = %ref_name, "reconcile: widget not found");
                        }
                    }
                    PaneKind::Script => {
                        let cmd = pane.command.as_deref().unwrap_or("");
                        match crate::script_widget::ScriptWidget::spawn(cmd, pane.label.as_deref())
                        {
                            Ok(mut widget) => {
                                widget.set_theme(self.theme.clone());
                                new_runners.push(PaneRunner::spawn_widget(
                                    pane.clone(),
                                    layer_id,
                                    Box::new(widget),
                                    Some(tx),
                                ));
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    command = %cmd,
                                    ?layer_id,
                                    "reconcile: script spawn failed"
                                );
                            }
                        }
                    }
                    PaneKind::Shell => {
                        let shell = shell_spec_from_command(&pane.command, &self.shell);
                        match PaneRunner::spawn_with_graphics_and_env(
                            pane.clone(),
                            layer_id,
                            shell,
                            Some(tx),
                            env_vars.clone(),
                            pane.scrollback_capacity
                                .unwrap_or(DEFAULT_SCROLLBACK_CAPACITY),
                        ) {
                            Ok(r) => new_runners.push(r),
                            Err(e) => {
                                warn!(error = %e, ?layer_id, "reconcile: spawn failed");
                            }
                        }
                    }
                }
            }
        }
        drop(survivors);
        self.runners = new_runners;
    }

    // ------------------------------------------------------------------
    // Tab operations
    // ------------------------------------------------------------------

    fn active_tab_idx_u32(&self) -> u32 {
        self.tabs.active_idx() as u32
    }

    fn sync_v1_from_active_tab(&mut self) {
        if let Some(active) = self.tabs.active() {
            self.focus = active.state.focus;
            self.layout_root = active.state.layout_root.clone();
            self.stack_focus = active.state.stack_focus.clone();
        }
    }

    fn create_new_tab(&mut self) {
        let new_state = TabState {
            runners: Vec::new(),
            focus: 0,
            layout_root: LayoutNode::Pane(CfgPane {
                kind: PaneKind::Shell,
                label: None,
                command: None,
                scrollback_capacity: None,
            }),
            stack_focus: BTreeMap::new(),
        };
        self.tabs.push(new_state);
        self.sync_v1_from_active_tab();
        self.reconcile_runners(ReconcileMode::Wholesale);
    }

    fn close_active_tab(&mut self) {
        let _removed = self.tabs.remove_active();
        if self.tabs.is_empty() {
            self.running = false;
        } else {
            self.sync_v1_from_active_tab();
            self.reconcile_runners(ReconcileMode::Wholesale);
        }
    }

    fn switch_to_tab(&mut self, n: usize) {
        if self.tabs.switch_to(n) {
            self.sync_v1_from_active_tab();
            self.reconcile_runners(ReconcileMode::Wholesale);
        }
    }

    // ------------------------------------------------------------------
    // Copy mode
    // ------------------------------------------------------------------

    /// Enter copy mode for the focused pane.
    fn enter_copy_mode(&mut self) {
        if self.runners.is_empty() {
            return;
        }
        if self.runners[self.focus].is_widget() {
            return;
        }
        self.copy_mode = Some(CopyModeState {
            cursor_x: 0,
            cursor_y: 0,
            selection_start: None,
        });
        self.mode = Mode::Copy;
    }

    /// Move the copy-mode cursor by `(dx, dy)` cells, clamping
    /// to the focused pane's rect.
    fn copy_mode_move(&mut self, dx: i16, dy: i16) {
        let Some(state) = self.copy_mode.as_mut() else {
            return;
        };
        let rect = self.runners.get(self.focus).map(|r| r.computed().rect);
        let Some(rect) = rect else { return };
        let new_x =
            (state.cursor_x as i32 + dx as i32).clamp(0, rect.w.saturating_sub(1) as i32) as u16;
        let new_y =
            (state.cursor_y as i32 + dy as i32).clamp(0, rect.h.saturating_sub(1) as i32) as u16;
        state.cursor_x = new_x;
        state.cursor_y = new_y;
    }

    /// Toggle the selection anchor at the current cursor position.
    fn copy_mode_start_selection(&mut self) {
        let Some(state) = self.copy_mode.as_mut() else {
            return;
        };
        if state.selection_start.is_some() {
            state.selection_start = None;
        } else {
            state.selection_start = Some((state.cursor_x, state.cursor_y));
        }
    }

    /// Copy the selected text to the system clipboard and exit
    /// copy mode.
    fn copy_mode_copy(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = self.copy_mode.as_ref().copied();
        let Some(state) = state else { return Ok(()) };
        let Some(snapshot) = self.last_focused_snapshot.as_ref() else {
            return Ok(());
        };
        let text = extract_selected_text(
            &snapshot.grid,
            state.cursor_x,
            state.cursor_y,
            state.selection_start,
        );
        let mut clipboard = crate::ArboardClipboard;
        copy_text_to_clipboard(&mut clipboard, &text)?;
        self.copy_mode = None;
        self.mode = Mode::Normal;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Pane resize
    // ------------------------------------------------------------------

    /// Find the parent Split of the focused pane and return
    /// `(split_path, axis, current_ratio, child_index)`.
    fn parent_split_of_focused(&self) -> Option<(Vec<u16>, CfgSplitAxis, u8, usize)> {
        if self.runners.is_empty() {
            return None;
        }
        let focused_id = self.runners[self.focus].computed().id;
        let tree_path = focused_id.path();
        if tree_path.len() <= 1 {
            return None;
        }
        let parent_path: Vec<u16> = tree_path[1..tree_path.len() - 1].to_vec();
        let child_idx = *tree_path.last().unwrap_or(&0) as usize;
        let mut node = &self.layout_root;
        for &idx in &parent_path {
            match node {
                LayoutNode::Split { children, .. } => {
                    node = children.get(idx as usize)?;
                }
                LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
                    node = panes.get(idx as usize)?;
                }
                _ => return None,
            }
        }
        match node {
            LayoutNode::Split { axis, ratio, .. } => Some((parent_path, *axis, ratio.0, child_idx)),
            _ => None,
        }
    }

    /// Resize the focused pane's parent split in the given
    /// direction (±2% per press). No-op if the direction
    /// doesn't match the Split's axis.
    fn pane_resize_by_direction(&mut self, dir: Direction) {
        let Some((parent_path, axis, current_ratio, child_idx)) = self.parent_split_of_focused()
        else {
            return;
        };
        if !matches!(
            (axis, dir),
            (CfgSplitAxis::Horizontal, Direction::Left | Direction::Right)
                | (CfgSplitAxis::Vertical, Direction::Up | Direction::Down)
        ) {
            return;
        }
        let delta: i16 = match (axis, dir) {
            (CfgSplitAxis::Horizontal, Direction::Right) => 2,
            (CfgSplitAxis::Horizontal, Direction::Left) => -2,
            (CfgSplitAxis::Vertical, Direction::Down) => 2,
            (CfgSplitAxis::Vertical, Direction::Up) => -2,
            _ => unreachable!(),
        };
        let adjusted_delta = if child_idx == 1 { -delta } else { delta };
        let new_ratio = (current_ratio as i16 + adjusted_delta).clamp(1, 99) as u8;
        let _ = update_split_ratio(&mut self.layout_root, &parent_path, CfgRatio(new_ratio));
        let (w, h) = self.last_host_size;
        self.relayout(w, h);
    }

    // ------------------------------------------------------------------
    // Mouse event handling
    // ------------------------------------------------------------------

    /// Dispatch a crossterm mouse event: click-to-focus, Alt+drag
    /// split resize, scroll, and SGR mouse forwarding.
    fn handle_mouse_event(&mut self, mouse: &MouseEvent) {
        let tab_bar_offset = TAB_BAR_HEIGHT;
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.modifiers.contains(KeyModifiers::ALT) {
                    self.start_drag_resize(mouse, tab_bar_offset);
                } else {
                    self.focus_by_click(mouse.column, mouse.row, tab_bar_offset);
                    self.forward_mouse_to_pty(mouse, tab_bar_offset);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.drag_state.is_some() {
                    self.update_drag_resize(mouse, tab_bar_offset);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag_state = None;
                self.forward_mouse_to_pty(mouse, tab_bar_offset);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.forward_mouse_to_pty(mouse, tab_bar_offset);
            }
            _ => {
                self.forward_mouse_to_pty(mouse, tab_bar_offset);
            }
        }
    }

    /// Click-to-focus: find the pane whose rect contains the
    /// click position and swap focus.
    fn focus_by_click(&mut self, col: u16, row: u16, tab_bar_offset: u16) {
        if self.runners.is_empty() {
            return;
        }
        let layout_area = LayoutRect {
            x: 0,
            y: 0,
            w: self.last_area.w,
            h: self.last_area.h,
        };
        let layout = match ComputedLayout::compute(&self.layout_root, layout_area) {
            Ok(l) => l,
            Err(_) => return,
        };
        let layout_row = row.saturating_sub(tab_bar_offset);
        for (idx, pane) in layout.panes.iter().enumerate() {
            if pane.rect.x <= col
                && col < pane.rect.x.saturating_add(pane.rect.w)
                && pane.rect.y <= layout_row
                && layout_row < pane.rect.y.saturating_add(pane.rect.h)
            {
                self.set_focus(idx);
                return;
            }
        }
    }

    /// Begin an Alt+drag resize: record the initial drag state.
    fn start_drag_resize(&mut self, mouse: &MouseEvent, tab_bar_offset: u16) {
        if self.runners.is_empty() {
            return;
        }
        let layout_area = LayoutRect {
            x: 0,
            y: 0,
            w: self.last_area.w,
            h: self.last_area.h,
        };
        let layout = match ComputedLayout::compute(&self.layout_root, layout_area) {
            Ok(l) => l,
            Err(_) => return,
        };
        let layout_row = mouse.row.saturating_sub(tab_bar_offset);
        let clicked_pane_idx = layout.panes.iter().position(|p| {
            p.rect.x <= mouse.column
                && mouse.column < p.rect.x.saturating_add(p.rect.w)
                && p.rect.y <= layout_row
                && layout_row < p.rect.y.saturating_add(p.rect.h)
        });
        let Some(pane_idx) = clicked_pane_idx else {
            return;
        };
        self.set_focus(pane_idx);
        let pane_id = layout.panes[pane_idx].id;
        let tree_path = pane_id.path();
        if tree_path.len() <= 1 {
            return;
        }
        let raw_path = &tree_path[1..tree_path.len() - 1];
        let mut node = &self.layout_root;
        for &idx in raw_path {
            match node {
                LayoutNode::Split { children, .. } => {
                    node = children.get(idx as usize).unwrap();
                }
                LayoutNode::Stack { panes } | LayoutNode::ZStack { panes } => {
                    node = panes.get(idx as usize).unwrap();
                }
                _ => return,
            }
        }
        if let LayoutNode::Split { axis, ratio, .. } = node {
            let total_cells = match axis {
                CfgSplitAxis::Horizontal => self.last_area.w,
                CfgSplitAxis::Vertical => self.last_area.h,
            };
            let start_pos = match axis {
                CfgSplitAxis::Horizontal => mouse.column,
                CfgSplitAxis::Vertical => layout_row,
            };
            let mut path_arr = [0u16; 8];
            let path_len = raw_path.len().min(8);
            path_arr[..path_len].copy_from_slice(&raw_path[..path_len]);
            self.drag_state = Some(DragState {
                split_path: path_arr,
                split_path_len: path_len as u8,
                start_pos,
                initial_ratio: ratio.0,
                axis: *axis,
                total_cells,
            });
        }
    }

    /// Continue an Alt+drag resize: compute the delta and
    /// update the Split's ratio.
    fn update_drag_resize(&mut self, mouse: &MouseEvent, tab_bar_offset: u16) {
        let (split_path, split_path_len, start_pos, initial_ratio, axis, total_cells) = {
            let Some(ref drag) = self.drag_state else {
                return;
            };
            (
                drag.split_path,
                drag.split_path_len,
                drag.start_pos,
                drag.initial_ratio,
                drag.axis,
                drag.total_cells,
            )
        };
        let layout_row = mouse.row.saturating_sub(tab_bar_offset);
        let current_pos = match axis {
            CfgSplitAxis::Horizontal => mouse.column,
            CfgSplitAxis::Vertical => layout_row,
        };
        let delta = current_pos as i32 - start_pos as i32;
        let pct_delta = if total_cells > 0 {
            (delta * 100 / total_cells as i32) as i16
        } else {
            0
        };
        let new_ratio = (initial_ratio as i16 + pct_delta).clamp(1, 99) as u8;
        let path_slice = &split_path[..split_path_len as usize];
        if let Err(e) = update_split_ratio(&mut self.layout_root, path_slice, CfgRatio(new_ratio)) {
            warn!(error = ?e, "drag resize: update_split_ratio failed");
        }
        let w = self.last_area.w;
        let h = self.last_area.h;
        self.relayout(w, h);
    }

    /// Forward a mouse event to the focused pane's PTY as an SGR
    /// extended mouse sequence.
    fn forward_mouse_to_pty(&mut self, mouse: &MouseEvent, tab_bar_offset: u16) {
        if self.focus >= self.runners.len() {
            return;
        }
        let layout_row = mouse.row.saturating_sub(tab_bar_offset);
        let button: u8 = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => 0,
            MouseEventKind::Down(MouseButton::Middle) => 1,
            MouseEventKind::Down(MouseButton::Right) => 2,
            MouseEventKind::Up(MouseButton::Left) => 0,
            MouseEventKind::Up(MouseButton::Middle) => 1,
            MouseEventKind::Up(MouseButton::Right) => 2,
            MouseEventKind::Drag(MouseButton::Left) => 32,
            MouseEventKind::Drag(MouseButton::Middle) => 33,
            MouseEventKind::Drag(MouseButton::Right) => 34,
            MouseEventKind::ScrollUp => 64,
            MouseEventKind::ScrollDown => 65,
            _ => return,
        };
        let mut modifiers: u8 = 0;
        if mouse.modifiers.contains(KeyModifiers::SHIFT) {
            modifiers |= 4;
        }
        if mouse.modifiers.contains(KeyModifiers::ALT) {
            modifiers |= 8;
        }
        if mouse.modifiers.contains(KeyModifiers::CONTROL) {
            modifiers |= 16;
        }
        let btn = button | modifiers;
        let suffix = if matches!(mouse.kind, MouseEventKind::Up(..)) {
            'm'
        } else {
            'M'
        };
        let seq = format!(
            "\x1b[<{};{};{}{suffix}",
            btn,
            mouse.column + 1,
            layout_row + 1
        );
        if let Some(runner) = self.runners.get_mut(self.focus) {
            let _ = runner.write_input(seq.as_bytes());
        }
    }

    // ------------------------------------------------------------------
    // Paste and focus event handling
    // ------------------------------------------------------------------

    fn focused_bracketed_paste_enabled(&self) -> bool {
        let Some(layer_id) = self.runners.get(self.focus).map(|r| r.layer_id()) else {
            return false;
        };
        self.pane_bracketed_paste
            .get(&layer_id)
            .copied()
            .unwrap_or(false)
    }

    fn prepare_paste_bytes(&self, text: &str) -> Vec<u8> {
        let bracketed =
            self.focused_bracketed_paste_enabled() && self.caps.supports_bracketed_paste();
        if bracketed {
            let mut out = Vec::with_capacity(text.len() + 12);
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
            out
        } else {
            text.as_bytes().to_vec()
        }
    }

    /// Forward a paste event to the focused pane, wrapping in
    /// bracketed-paste delimiters if the pane requested them.
    fn handle_paste(&mut self, text: &str) {
        if self.runners.is_empty() {
            return;
        }
        if self
            .runners
            .get(self.focus)
            .map(|r| r.is_widget())
            .unwrap_or(false)
        {
            return;
        }
        let bytes = self.prepare_paste_bytes(text);
        if let Some(runner) = self.runners.get_mut(self.focus) {
            if let Err(e) = runner.write_input(&bytes) {
                debug!(error = ?e, layer_id = ?runner.layer_id(), "write_input failed for paste");
            }
        }
    }

    /// Forward a host focus-change event to the focused pane's
    /// PTY as `CSI I` (gained) or `CSI O` (lost).
    fn forward_focus_event_to_focused_pane(&mut self, focused: bool) {
        if self.runners.is_empty() {
            return;
        }
        if !self.caps.supports_focus_events() {
            return;
        }
        let Some(runner) = self.runners.get_mut(self.focus) else {
            return;
        };
        if runner.is_widget() {
            return;
        }
        if !runner.focus_reporting_enabled() {
            return;
        }
        let bytes = if focused {
            b"\x1b[I".to_vec()
        } else {
            b"\x1b[O".to_vec()
        };
        if let Err(e) = runner.write_input(&bytes) {
            debug!(error = ?e, layer_id = ?runner.layer_id(), "write_input failed for focus event");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ServerConfig;
    use crate::test_helpers::{
        drain_frames, last_frame_focus, last_frame_mode, single_layout, split_h_layout,
        split_v_layout,
    };
    use cmdash_config::{LayoutNode, Pane as CfgPane, PaneKind, Ratio, SplitAxis};
    use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
    use cmdash_pty::{PaneLayerId, PanePtyOps, PaneTerminalState, PtyError, TextGrid};
    use std::collections::{BTreeMap, HashMap};
    use tokio::sync::mpsc::unbounded_channel;

    struct StubPty {
        layer_id: PaneLayerId,
    }

    impl PanePtyOps for StubPty {
        fn layer_id(&self) -> PaneLayerId {
            self.layer_id
        }
        fn resize(&mut self, _: u16, _: u16) -> Result<(), PtyError> {
            Ok(())
        }
        fn write(&mut self, _: &[u8]) -> Result<usize, PtyError> {
            Ok(0)
        }
        fn advance(&mut self, _: &[u8]) -> Result<(), PtyError> {
            Ok(())
        }
        fn snapshot(&mut self) -> PaneTerminalState {
            PaneTerminalState {
                grid: TextGrid::new(80, 24),
                cols: 80,
                rows: 24,
                pending_events: vec![],
            }
        }
        fn try_wait(&mut self) -> Result<Option<i32>, PtyError> {
            Ok(None)
        }
        fn kill(&mut self) -> Result<(), PtyError> {
            Ok(())
        }
        fn keyboard_flags(&self) -> u8 {
            0
        }
        fn focus_reporting_enabled(&self) -> bool {
            false
        }
        fn scrollback_up(&mut self, _: usize) {}
        fn scrollback_down(&mut self, _: usize) {}
        fn scrollback_reset(&mut self) {}
        fn in_scrollback(&self) -> bool {
            false
        }
        fn in_alternate_screen(&self) -> bool {
            false
        }
    }

    fn make_server(layout_root: LayoutNode, focus: usize) -> ServerTask {
        let (server, _client_tx, _server_rx, _close_tx) = crate::test_helpers::build_server(
            layout_root,
            focus,
            std::collections::BTreeMap::new(),
            |lid| Box::new(StubPty { layer_id: lid }),
        );
        server
    }

    // Type aliases so existing call sites (single_layout(), split_h_layout(), split_v_layout())
    // continue to work after migrating to the shared test_helpers module.

    #[test]
    fn new_defaults() {
        let server = make_server(single_layout(), 0);
        assert!(server.running);
        assert_eq!(server.focus, 0);
        assert!(server.copy_mode.is_none());
    }

    #[test]
    fn set_focus_within_bounds() {
        let mut server = make_server(split_h_layout(), 0);
        server.set_focus(1);
        assert_eq!(server.focus, 1);
    }

    #[test]
    fn apply_action_app_close() {
        let mut server = make_server(single_layout(), 0);
        server.apply_action(KeyAction::AppClose);
        assert!(!server.running);
    }

    #[test]
    fn apply_action_focus_next() {
        let mut server = make_server(split_h_layout(), 0);
        server.apply_action(KeyAction::PaneFocusNext);
        assert_eq!(server.focus, 1);
        server.apply_action(KeyAction::PaneFocusNext);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn apply_action_focus_prev() {
        let mut server = make_server(split_h_layout(), 0);
        server.apply_action(KeyAction::PaneFocusPrev);
        assert_eq!(server.focus, 1);
    }

    #[test]
    fn apply_action_mode_entry_exit() {
        let mut server = make_server(single_layout(), 0);
        server.apply_action(KeyAction::EnterPaneResize);
        assert_eq!(server.mode, cmdash_keybinds::Mode::PaneResize);
        server.apply_action(KeyAction::ModeExit);
        assert_eq!(server.mode, cmdash_keybinds::Mode::Normal);
    }

    #[tokio::test]
    async fn apply_action_tab_new() {
        let mut server = make_server(single_layout(), 0);
        let initial_tabs = server.tabs.len();
        server.apply_action(KeyAction::TabNew);
        assert_eq!(server.tabs.len(), initial_tabs + 1);
    }

    #[test]
    fn set_focus_clears_copy_mode_on_change() {
        let mut server = make_server(split_h_layout(), 0);
        // Set up copy mode.
        server.copy_mode = Some(crate::protocol::CopyModeState {
            cursor_x: 5,
            cursor_y: 3,
            selection_start: None,
        });
        // Changing to a different index clears copy mode.
        server.set_focus(1);
        assert!(
            server.copy_mode.is_none(),
            "set_focus to different idx should clear copy mode"
        );
        // Setting the same index preserves copy mode.
        server.copy_mode = Some(crate::protocol::CopyModeState {
            cursor_x: 1,
            cursor_y: 1,
            selection_start: None,
        });
        server.set_focus(1);
        assert!(
            server.copy_mode.is_some(),
            "set_focus to same idx should preserve copy mode"
        );
    }

    #[test]
    fn close_pane_clears_per_pane_flags() {
        // Note: close_pane() only clears per-pane flag maps.
        // Runner removal is handled by close_focused_and_rebalance()
        // or drain_close_channel() which calls close_pane() internally.
        let mut server = make_server(single_layout(), 0);
        let lid = server.runners[0].layer_id();
        server.pane_keyboard_flags.insert(lid, 0b111);
        server.pane_bracketed_paste.insert(lid, true);
        server.pane_focus_reporting.insert(lid, true);
        server.pane_alternate_screen.insert(lid, true);
        server.close_pane(lid);
        assert!(server.pane_keyboard_flags.is_empty());
        assert!(server.pane_bracketed_paste.is_empty());
        assert!(server.pane_focus_reporting.is_empty());
        assert!(server.pane_alternate_screen.is_empty());
    }

    #[test]
    fn apply_action_focus_next_single_pane_wraps() {
        let mut server = make_server(single_layout(), 0);
        server.apply_action(KeyAction::PaneFocusNext);
        assert_eq!(server.focus, 0, "single pane: focus should wrap to 0");
    }

    #[test]
    fn apply_action_close_last_tab_stops_running() {
        let mut server = make_server(single_layout(), 0);
        assert!(server.tabs.len() == 1, "should start with 1 tab");
        server.apply_action(KeyAction::TabClose);
        assert!(!server.running, "closing last tab should stop running");
    }

    #[test]
    fn enter_and_exit_copy_mode() {
        let mut server = make_server(single_layout(), 0);
        server.apply_action(KeyAction::EnterCopyMode);
        assert!(server.copy_mode.is_some());
        assert_eq!(server.mode, cmdash_keybinds::Mode::Copy);
        server.apply_action(KeyAction::ModeExit);
        assert!(server.copy_mode.is_none());
        assert_eq!(server.mode, cmdash_keybinds::Mode::Normal);
    }

    #[test]
    fn copy_mode_move_clamps_to_rect() {
        let mut server = make_server(single_layout(), 0);
        server.apply_action(KeyAction::EnterCopyMode);
        // Move far beyond the pane rect.
        for _ in 0..200 {
            server.apply_action(KeyAction::CopyModeMoveRight);
            server.apply_action(KeyAction::CopyModeMoveDown);
        }
        let state = server.copy_mode.as_ref().unwrap();
        let rect = server.runners[0].computed().rect;
        // copy_mode_move clamps to w-1, h-1.
        assert_eq!(state.cursor_x, rect.w.saturating_sub(1));
        assert_eq!(state.cursor_y, rect.h.saturating_sub(1));
    }

    #[test]
    fn copy_mode_toggle_selection() {
        let mut server = make_server(single_layout(), 0);
        server.apply_action(KeyAction::EnterCopyMode);
        assert!(server.copy_mode.as_ref().unwrap().selection_start.is_none());
        server.apply_action(KeyAction::CopyModeStartSelection);
        assert!(server.copy_mode.as_ref().unwrap().selection_start.is_some());
        server.apply_action(KeyAction::CopyModeStartSelection);
        assert!(server.copy_mode.as_ref().unwrap().selection_start.is_none());
    }

    #[test]
    fn relayout_zero_area_is_noop() {
        let mut server = make_server(single_layout(), 0);
        let old_area = server.last_area;
        server.relayout(0, 0);
        assert_eq!(
            server.last_area, old_area,
            "zero-area relayout should be a no-op"
        );
    }

    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Integration tests: async run() loop with short-lived channels
    // ------------------------------------------------------------------

    /// Build a ServerTask with explicit channel handles so the
    /// test can drive the run() loop from outside.
    fn make_server_with_channels(
        layout_root: LayoutNode,
    ) -> (
        ServerTask,
        tokio::sync::mpsc::UnboundedSender<ClientMessage>,
        tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
        tokio::sync::mpsc::UnboundedSender<PaneLayerId>,
    ) {
        crate::test_helpers::build_server(
            layout_root,
            0,
            std::collections::BTreeMap::new(),
            |lid| Box::new(StubPty { layer_id: lid }),
        )
    }

    // --- Pattern A: queue-and-run (for state-checking tests) ---

    #[tokio::test]
    async fn run_app_close_exits_loop() {
        let (mut server, client_tx, _server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), server.run()).await;
        assert!(result.is_ok(), "run() should not hang");
        assert!(result.unwrap().is_ok());
        assert!(!server.running);
    }

    #[tokio::test]
    async fn run_detach_stops_server() {
        let (mut server, client_tx, _server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        client_tx.send(ClientMessage::Detach).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), server.run()).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
        assert!(!server.running);
    }

    #[tokio::test]
    async fn run_client_channel_close_exits() {
        let (mut server, client_tx, _server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        drop(client_tx);
        let result = tokio::time::timeout(Duration::from_secs(2), server.run()).await;
        assert!(
            result.is_ok(),
            "run() should exit when client channel closes"
        );
        assert!(result.unwrap().is_ok());
    }

    // --- Pattern B: spawn-and-interact (for frame-observation tests) ---

    #[tokio::test]
    async fn run_focus_action_reflected_in_frames() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(split_h_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        // Wait for initial tick frame.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _initial = drain_frames(&mut server_rx, 5).await;

        // Send focus-next.
        client_tx
            .send(ClientMessage::Action(KeyAction::PaneFocusNext))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert!(
            last_frame_focus(&frames) == Some(1),
            "expected FrameIncremental with focus=1 after PaneFocusNext"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn run_resize_reflected_in_frames() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        // Wait for initial tick.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Send resize.
        client_tx.send(ClientMessage::Resize(120, 40)).unwrap();
        // Wait for the tick that processes the resize.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;

        // After resize, the layout should reflect the new area.
        // The total area in ComputedLayout should have w=120.
        let found_wide = frames.iter().any(|msg| match msg {
            ServerMessage::FrameIncremental { layout, .. } => layout.total.w == 120,
            _ => false,
        });
        assert!(
            found_wide,
            "expected FrameIncremental with layout.total.w=120 after resize"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn run_multiple_ticks_produce_frames() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        // Wait long enough for ~3 ticks (33ms each).
        tokio::time::sleep(Duration::from_millis(150)).await;
        let frames = drain_frames(&mut server_rx, 10).await;
        assert!(
            frames.len() >= 2,
            "expected at least 2 frames from ~150ms of ticks, got {}",
            frames.len()
        );
        // Every frame should be FrameIncremental with running=true.
        for msg in &frames {
            match msg {
                ServerMessage::FrameIncremental { running, .. } => {
                    assert!(*running, "server should still be running");
                }
                _ => panic!("expected only FrameIncremental messages"),
            }
        }

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn run_close_channel_triggers_flag_cleanup() {
        let (server, client_tx, mut server_rx, close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        // Wait for first tick.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Send a close notification via the close channel.
        // The server drains close_rx on each tick.
        let lid = PaneLayerId(1);
        close_tx.send(lid).unwrap();

        // Wait for the next tick to process the close.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        // The server should have produced at least one frame after
        // processing the close (flags cleared). We can't check
        // server.pane_keyboard_flags directly since it's moved,
        // but the fact that the tick completed without panic is
        // the main assertion.
        assert!(
            !frames.is_empty(),
            "expected frames after close channel notification"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn run_mode_action_reflected_in_frames() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Enter pane resize mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::EnterPaneResize))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(Mode::PaneResize),
            "expected FrameIncremental with mode=PaneResize"
        );

        // Exit mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::ModeExit))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(Mode::Normal),
            "expected FrameIncremental with mode=Normal after ModeExit"
        );
        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    // ------------------------------------------------------------------
    // handle_input tests: mouse click-to-focus, PTY forwarding,
    // PageUp/PageDown scrollback
    // ------------------------------------------------------------------

    use crate::test_pty::{TrackingPty, TrackingState};
    use std::sync::{Arc, Mutex};

    fn make_tracking_server(layout_root: LayoutNode) -> (ServerTask, Arc<Mutex<TrackingState>>) {
        let area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let layout = ComputedLayout::compute(&layout_root, area).unwrap();
        let state = Arc::new(Mutex::new(TrackingState::default()));
        let runners: Vec<PaneRunner> = layout
            .panes
            .iter()
            .enumerate()
            .map(|(i, pane)| {
                let lid = PaneLayerId(i as u64 + 1);
                let pty: Box<dyn PanePtyOps + Send> = Box::new(TrackingPty {
                    layer_id: lid,
                    state: Arc::clone(&state),
                });
                PaneRunner::with_pty_for_test(pane.clone(), lid, pty, None)
            })
            .collect();
        let config = ServerConfig {
            layout_root,
            presets: BTreeMap::new(),
            shell: cmdash_pty::ShellSpec::LoginShell,
            status_bar: None,
            theme: cmdash_config::Theme::default(),
            widget_factories: HashMap::new(),
        };
        let (_ctx, client_rx) = unbounded_channel();
        let (server_tx, _srx) = unbounded_channel();
        let (close_tx, close_rx) = unbounded_channel();
        let server = ServerTask::new(
            config,
            runners,
            0,
            area,
            super::ServerChannels {
                close_tx,
                close_rx,
                config_reload_rx: None,
                client_rx,
                server_tx,
            },
        );
        (server, state)
    }

    /// Helper: build a crossterm mouse event.
    fn mouse_event(kind: MouseEventKind, col: u16, row: u16, mods: KeyModifiers) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: mods,
        })
    }

    /// Helper: build a crossterm key event.
    fn key_event(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    // --- Mouse click-to-focus ---

    #[test]
    fn handle_input_click_focuses_pane_under_cursor() {
        // split_h: pane 0 at x=0..40, pane 1 at x=40..80.
        let (mut server, _state) = make_tracking_server(split_h_layout());
        assert_eq!(server.focus, 0);

        // Click in pane 1's area (col=50, row=2). TAB_BAR_OFFSET=1,
        // so layout_row = 2-1 = 1, which is inside pane 1's rect
        // (x=40, y=0, w=40, h=24).
        let click = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            50,
            2,
            KeyModifiers::NONE,
        );
        server.handle_input(&click);
        assert_eq!(server.focus, 1, "click at col=50 should focus pane 1");
    }

    #[test]
    fn handle_input_click_in_pane0_keeps_focus_0() {
        let (mut server, _state) = make_tracking_server(split_h_layout());
        assert_eq!(server.focus, 0);

        // Click in pane 0's area (col=10, row=2).
        let click = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            10,
            2,
            KeyModifiers::NONE,
        );
        server.handle_input(&click);
        assert_eq!(server.focus, 0, "click at col=10 should stay on pane 0");
    }
    #[test]
    fn handle_input_alt_click_starts_drag_resize() {
        // split_h: pane 0 at x=0..40, pane 1 at x=40..80.
        let (mut server, _state) = make_tracking_server(split_h_layout());
        assert_eq!(server.focus, 0);

        // Alt+click on pane 1 (col=50) triggers drag resize.
        // start_drag_resize internally calls set_focus first,
        // so focus DOES change to the clicked pane.
        let click = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            50,
            2,
            KeyModifiers::ALT,
        );
        server.handle_input(&click);
        assert_eq!(server.focus, 1, "Alt+click focuses the clicked pane");
        assert!(
            server.drag_state.is_some(),
            "Alt+click should start drag resize"
        );
        // Verify drag state captured the correct axis.
        let drag = server.drag_state.unwrap();
        assert_eq!(
            drag.axis,
            SplitAxis::Horizontal,
            "horizontal split should drag horizontally"
        );
    }

    // --- Mouse PTY forwarding ---

    #[test]
    fn handle_input_scroll_up_forwards_sgr_to_pty() {
        let (mut server, state) = make_tracking_server(single_layout());

        let scroll = mouse_event(MouseEventKind::ScrollUp, 10, 5, KeyModifiers::NONE);
        server.handle_input(&scroll);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert_eq!(calls.len(), 1, "expected one write_input call for scroll");
        // SGR: button=64 (ScrollUp), modifiers=0, col+1=11,
        // layout_row = 5-1=4, row+1=5. Suffix='M' (not Up).
        let expected = format!("\x1b[<{};{};{}{}", 64, 11, 5, 'M');
        assert_eq!(calls[0], expected.as_bytes());
    }

    #[test]
    fn handle_input_scroll_down_forwards_sgr_to_pty() {
        let (mut server, state) = make_tracking_server(single_layout());

        let scroll = mouse_event(MouseEventKind::ScrollDown, 10, 5, KeyModifiers::NONE);
        server.handle_input(&scroll);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert_eq!(calls.len(), 1);
        // button=65 (ScrollDown), modifiers=0.
        let expected = format!("\x1b[<{};{};{}{}", 65, 11, 5, 'M');
        assert_eq!(calls[0], expected.as_bytes());
    }

    #[test]
    fn handle_input_mouse_down_forwards_sgr_to_pty() {
        let (mut server, state) = make_tracking_server(single_layout());

        let down = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            10,
            5,
            KeyModifiers::NONE,
        );
        server.handle_input(&down);

        let calls = &state.lock().unwrap().write_input_bufs;
        // focus_by_click also runs (single pane, no-op), then
        // forward_mouse_to_pty writes the SGR sequence.
        assert!(!calls.is_empty(), "expected write_input for mouse down");
        // button=0 (Left Down), modifiers=0.
        let expected = format!("\x1b[<{};{};{}{}", 0, 11, 5, 'M');
        assert_eq!(calls.last().unwrap(), expected.as_bytes());
    }

    #[test]
    fn handle_input_mouse_up_forwards_sgr_with_m_suffix() {
        let (mut server, state) = make_tracking_server(single_layout());

        let up = mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            10,
            5,
            KeyModifiers::NONE,
        );
        server.handle_input(&up);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert!(!calls.is_empty());
        // Up events use suffix 'm'.
        let expected = format!("\x1b[<{};{};{}{}", 0, 11, 5, 'm');
        assert_eq!(calls.last().unwrap(), expected.as_bytes());
    }

    #[test]
    fn handle_input_mouse_with_shift_modifier() {
        let (mut server, state) = make_tracking_server(single_layout());

        let scroll = mouse_event(MouseEventKind::ScrollUp, 10, 5, KeyModifiers::SHIFT);
        server.handle_input(&scroll);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert_eq!(calls.len(), 1);
        // SHIFT modifier = 4, so button = 64 | 4 = 68.
        let expected = format!("\x1b[<{};{};{}{}", 68, 11, 5, 'M');
        assert_eq!(calls[0], expected.as_bytes());
    }

    // --- PageUp/PageDown scrollback ---

    #[test]
    fn handle_input_page_up_calls_scrollback_up() {
        let (mut server, state) = make_tracking_server(single_layout());

        server.handle_input(&key_event(KeyCode::PageUp));

        let calls = &state.lock().unwrap().scrollback_up_calls;
        assert_eq!(calls.len(), 1, "expected one scrollback_up call");
        // page_size = runner.computed().rect.h = 24 for 80x24 layout.
        assert_eq!(calls[0], 24, "page_size should equal pane height");
    }

    #[test]
    fn handle_input_page_down_in_scrollback_calls_scrollback_down() {
        let (mut server, state) = make_tracking_server(single_layout());
        // Set the stub to report "in scrollback".
        state.lock().unwrap().in_scrollback = true;

        server.handle_input(&key_event(KeyCode::PageDown));

        let calls = &state.lock().unwrap().scrollback_down_calls;
        assert_eq!(calls.len(), 1, "expected one scrollback_down call");
        assert_eq!(calls[0], 24, "page_size should equal pane height");
    }

    #[test]
    fn handle_input_page_down_not_in_scrollback_forwards_to_pty() {
        let (mut server, state) = make_tracking_server(single_layout());
        // NOT in scrollback (default). PageDown should fall through
        // to the PTY forwarding path.
        state.lock().unwrap().in_scrollback = false;

        server.handle_input(&key_event(KeyCode::PageDown));

        // scrollback_down should NOT have been called.
        assert!(
            state.lock().unwrap().scrollback_down_calls.is_empty(),
            "scrollback_down should not be called when not in scrollback"
        );
        // The key should be forwarded to the PTY as bytes.
        let bufs = &state.lock().unwrap().write_input_bufs;
        assert!(
            !bufs.is_empty(),
            "PageDown when not in scrollback should write to PTY"
        );
        // event_to_bytes(PageDown) = b"\x1b[6~"
        assert_eq!(bufs.last().unwrap(), b"\x1b[6~");
    }

    #[test]
    fn handle_input_page_up_in_alternate_screen_forwards_to_pty() {
        let (mut server, state) = make_tracking_server(single_layout());
        // In alternate screen: PageUp should NOT be intercepted.
        state.lock().unwrap().in_alternate_screen = true;

        server.handle_input(&key_event(KeyCode::PageUp));

        // scrollback_up should NOT have been called.
        assert!(
            state.lock().unwrap().scrollback_up_calls.is_empty(),
            "scrollback_up should not be called in alternate screen"
        );
        // Key should be forwarded to PTY.
        let bufs = &state.lock().unwrap().write_input_bufs;
        assert!(
            !bufs.is_empty(),
            "PageUp in alternate screen should write to PTY"
        );
        assert_eq!(bufs.last().unwrap(), b"\x1b[5~");
    }

    #[test]
    fn handle_input_regular_key_resets_scrollback() {
        let (mut server, state) = make_tracking_server(single_layout());
        state.lock().unwrap().in_scrollback = true;

        server.handle_input(&key_event(KeyCode::Char('a')));

        assert_eq!(
            state.lock().unwrap().scrollback_reset_count,
            1,
            "pressing a regular key while in scrollback should reset"
        );
    }

    #[test]
    fn handle_input_regular_key_not_in_scrollback_does_not_reset() {
        let (mut server, state) = make_tracking_server(single_layout());
        state.lock().unwrap().in_scrollback = false;

        server.handle_input(&key_event(KeyCode::Char('a')));

        assert_eq!(
            state.lock().unwrap().scrollback_reset_count,
            0,
            "pressing a regular key when not in scrollback should not reset"
        );
    }

    #[test]
    fn handle_input_focus_lost_event_updates_host_focused() {
        let (mut server, _state) = make_tracking_server(single_layout());
        assert!(server.host_focused, "should start focused");

        server.handle_input(&Event::FocusLost);
        assert!(!server.host_focused);

        server.handle_input(&Event::FocusGained);
        assert!(server.host_focused);
    }

    // ------------------------------------------------------------------
    // reconcile_runners tests: InPlace vs Wholesale, label matching
    // ------------------------------------------------------------------

    /// Build a labeled single-pane layout.
    fn labeled_single(label: &str) -> LayoutNode {
        LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: Some(label.to_string()),
            command: None,
            scrollback_capacity: None,
        })
    }

    /// Build a labeled horizontal split with two panes.
    fn labeled_split_h(left: &str, right: &str) -> LayoutNode {
        LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(50),
            children: vec![labeled_single(left), labeled_single(right)],
        }
    }

    /// Nested layout: a vertical split with a horizontal split on
    /// top and a single pane on bottom. The horizontal split is
    /// NOT at the root, so `update_split_ratio` receives a
    /// non-empty path and actually updates the ratio.
    ///
    /// ```text
    /// Split(V, 50%) ──── Split(H, 50%) ──── Pane(0)
    ///                   └────────────── Pane(1)
    /// └───────────────────────────────── Pane(2)
    /// ```
    fn nested_split_h() -> LayoutNode {
        LayoutNode::Split {
            axis: SplitAxis::Vertical,
            ratio: Ratio(50),
            children: vec![
                split_h_layout(), // inner horizontal split (panes 0, 1)
                single_layout(),  // bottom pane (pane 2)
            ],
        }
    }

    /// Build a server with labeled panes and return (server, initial_layer_ids).
    /// Panes are labeled left/right in a horizontal split.
    fn make_labeled_server(layout_root: LayoutNode) -> (ServerTask, Vec<PaneLayerId>) {
        let area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let layout = ComputedLayout::compute(&layout_root, area).unwrap();
        let runners: Vec<PaneRunner> = layout
            .panes
            .iter()
            .map(|pane| {
                let lid = alloc_layer_id();
                let pty: Box<dyn PanePtyOps + Send> = Box::new(StubPty { layer_id: lid });
                PaneRunner::with_pty_for_test(pane.clone(), lid, pty, None)
            })
            .collect();
        let layer_ids: Vec<PaneLayerId> = runners.iter().map(|r| r.layer_id()).collect();
        let config = ServerConfig {
            layout_root,
            presets: BTreeMap::new(),
            shell: cmdash_pty::ShellSpec::LoginShell,
            status_bar: None,
            theme: cmdash_config::Theme::default(),
            widget_factories: HashMap::new(),
        };
        let (_ctx, client_rx) = unbounded_channel();
        let (server_tx, _srx) = unbounded_channel();
        let (close_tx, close_rx) = unbounded_channel();
        let server = ServerTask::new(
            config,
            runners,
            0,
            area,
            super::ServerChannels {
                close_tx,
                close_rx,
                config_reload_rx: None,
                client_rx,
                server_tx,
            },
        );
        (server, layer_ids)
    }

    #[test]
    fn reconcile_inplace_preserves_matching_labels() {
        // Start with labeled split: left="a", right="b".
        let (mut server, initial_ids) = make_labeled_server(labeled_split_h("a", "b"));
        assert_eq!(server.runners.len(), 2);
        let id_a = initial_ids[0];
        let id_b = initial_ids[1];

        // Swap layout to a different ratio but same labels.
        server.layout_root = LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(70),
            children: vec![labeled_single("a"), labeled_single("b")],
        };
        server.reconcile_runners(ReconcileMode::InPlace);

        // Both runners should survive with the same layer IDs.
        assert_eq!(server.runners.len(), 2);
        assert_eq!(
            server.runners[0].layer_id(),
            id_a,
            "label 'a' runner should keep its layer ID"
        );
        assert_eq!(
            server.runners[1].layer_id(),
            id_b,
            "label 'b' runner should keep its layer ID"
        );
        // Labels should be updated on the computed pane.
        assert_eq!(server.runners[0].computed().label.as_deref(), Some("a"));
        assert_eq!(server.runners[1].computed().label.as_deref(), Some("b"));
    }

    #[tokio::test]
    async fn reconcile_inplace_drops_unmatched_runners() {
        // Start with labeled split: left="a", right="b".
        let (mut server, initial_ids) = make_labeled_server(labeled_split_h("a", "b"));
        let id_a = initial_ids[0];

        // Change layout to only have "a" ("b" is removed).
        server.layout_root = labeled_single("a");
        server.reconcile_runners(ReconcileMode::InPlace);

        // Only "a" should survive.
        assert_eq!(server.runners.len(), 1, "only one pane should remain");
        assert_eq!(
            server.runners[0].layer_id(),
            id_a,
            "survivor 'a' should keep its layer ID"
        );
        assert_eq!(server.runners[0].computed().label.as_deref(), Some("a"));
    }

    #[tokio::test]
    async fn reconcile_inplace_no_matching_labels_spawns_fresh() {
        // Start with labeled split: left="a", right="b".
        let (mut server, _initial_ids) = make_labeled_server(labeled_split_h("a", "b"));

        // Change layout to completely different labels.
        server.layout_root = labeled_split_h("x", "y");
        server.reconcile_runners(ReconcileMode::InPlace);

        // Old runners should be dropped, new ones spawned.
        assert_eq!(server.runners.len(), 2);
        // New runners should have different layer IDs (allocated fresh).
        // Labels should match the new layout.
        assert_eq!(server.runners[0].computed().label.as_deref(), Some("x"));
        assert_eq!(server.runners[1].computed().label.as_deref(), Some("y"));
    }

    #[tokio::test]
    async fn reconcile_inplace_label_swap_preserves_both() {
        // Start with labeled split: left="a", right="b".
        let (mut server, initial_ids) = make_labeled_server(labeled_split_h("a", "b"));
        let id_a = initial_ids[0];
        let id_b = initial_ids[1];

        // Swap labels: now left="b", right="a".
        server.layout_root = labeled_split_h("b", "a");
        server.reconcile_runners(ReconcileMode::InPlace);

        // Both should survive but swapped positions.
        assert_eq!(server.runners.len(), 2);
        // Runner "b" is now at position 0 (left).
        assert_eq!(server.runners[0].computed().label.as_deref(), Some("b"));
        assert_eq!(
            server.runners[0].layer_id(),
            id_b,
            "runner 'b' should keep its ID even after swap"
        );
        // Runner "a" is now at position 1 (right).
        assert_eq!(server.runners[1].computed().label.as_deref(), Some("a"));
        assert_eq!(
            server.runners[1].layer_id(),
            id_a,
            "runner 'a' should keep its ID even after swap"
        );
    }

    #[tokio::test]
    async fn reconcile_wholesale_drops_all_and_spawns_fresh() {
        // Start with labeled split.
        let (mut server, initial_ids) = make_labeled_server(labeled_split_h("a", "b"));
        let id_a = initial_ids[0];
        let id_b = initial_ids[1];

        // Wholesale reconcile with the SAME layout.
        // All old runners should be dropped and new ones created.
        server.reconcile_runners(ReconcileMode::Wholesale);

        assert_eq!(server.runners.len(), 2);
        // New runners should have DIFFERENT layer IDs than the originals.
        assert_ne!(
            server.runners[0].layer_id(),
            id_a,
            "wholesale should allocate new layer IDs"
        );
        assert_ne!(
            server.runners[1].layer_id(),
            id_b,
            "wholesale should allocate new layer IDs"
        );
        // stack_focus should be cleared in Wholesale mode.
        assert!(
            server.stack_focus.is_empty(),
            "wholesale should clear stack_focus"
        );
    }

    #[tokio::test]
    async fn reconcile_inplace_unlabeled_runners_are_not_preserved() {
        // Start with an unlabeled pane.
        let (mut server, initial_ids) = make_labeled_server(single_layout());
        assert_eq!(server.runners.len(), 1);
        let old_id = initial_ids[0];

        // Change to a split layout (also unlabeled).
        // The old runner has label=None → no match → dropped.
        // New panes are spawned (reconcile spawns real PTYs).
        server.layout_root = split_h_layout();
        server.reconcile_runners(ReconcileMode::InPlace);

        assert_eq!(server.runners.len(), 2, "split_h should produce 2 runners");
        // Neither new runner should have the old layer ID.
        assert_ne!(
            server.runners[0].layer_id(),
            old_id,
            "old unlabeled runner should be dropped"
        );
        assert_ne!(
            server.runners[1].layer_id(),
            old_id,
            "old unlabeled runner should be dropped"
        );
        // Unlabeled panes have label=None.
        assert!(server.runners[0].computed().label.is_none());
        assert!(server.runners[1].computed().label.is_none());
    }

    #[tokio::test]
    async fn reconcile_inplace_preserves_rect_update() {
        // Start with labeled panes at 50/50 split.
        let (mut server, initial_ids) = make_labeled_server(labeled_split_h("a", "b"));
        let id_a = initial_ids[0];

        // Change to 70/30 split (same labels).
        server.layout_root = LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(70),
            children: vec![labeled_single("a"), labeled_single("b")],
        };
        server.reconcile_runners(ReconcileMode::InPlace);

        // Runner "a" should have updated rect (wider).
        assert_eq!(server.runners[0].layer_id(), id_a);
        let rect_a = server.runners[0].computed().rect;
        assert_eq!(rect_a.w, 56, "70% of 80 = 56");
        let rect_b = server.runners[1].computed().rect;
        assert_eq!(rect_b.w, 24, "30% of 80 = 24");
    }

    // ------------------------------------------------------------------
    // Drag-to-resize tests: start_drag_resize, update_drag_resize,
    // pane_resize_by_direction
    // ------------------------------------------------------------------

    #[test]
    fn start_drag_sets_drag_state_with_correct_fields() {
        let (mut server, _state) = make_tracking_server(split_h_layout());
        assert_eq!(server.focus, 0);
        assert!(server.drag_state.is_none());

        // Alt+click on pane 1 (col=50, row=2). TAB_BAR_HEIGHT=1,
        // so layout_row = 2-1 = 1.
        let click = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            50,
            2,
            KeyModifiers::ALT,
        );
        server.handle_input(&click);

        let drag = server.drag_state.expect("drag_state should be set");
        assert_eq!(drag.axis, SplitAxis::Horizontal);
        assert_eq!(drag.initial_ratio, 50, "split_h starts at 50%%");
        assert_eq!(drag.total_cells, 80, "horizontal split uses width");
        assert_eq!(drag.start_pos, 50, "start_pos = mouse column");
    }

    #[test]
    fn start_drag_on_single_pane_is_noop() {
        let (mut server, _state) = make_tracking_server(single_layout());
        assert!(server.drag_state.is_none());

        // Alt+click on a single pane (no parent split).
        let click = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            40,
            5,
            KeyModifiers::ALT,
        );
        server.handle_input(&click);

        // Single pane has no parent split → drag_state stays None.
        assert!(server.drag_state.is_none(), "no parent split → no drag");
    }

    #[test]
    fn update_drag_resize_changes_ratio() {
        // Use nested layout so update_split_ratio gets a non-empty path.
        let (mut server, _state) = make_tracking_server(nested_split_h());

        // Manually set up drag_state for the inner horizontal split.
        // path [0] = child 0 of root = the inner Split(H).
        let mut path = [0u16; 8];
        path[0] = 0;
        server.drag_state = Some(DragState {
            split_path: path,
            split_path_len: 1,
            start_pos: 40,     // started at the split boundary
            initial_ratio: 50, // 50%% split
            axis: SplitAxis::Horizontal,
            total_cells: 80,
        });

        // Drag 10 cells to the right (col=50, from start_pos=40).
        // delta = 50-40 = 10, pct_delta = 10*100/80 = 12.
        // new_ratio = 50 + 12 = 62.
        let drag = mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            50,
            2,
            KeyModifiers::ALT,
        );
        server.handle_input(&drag);

        // Verify the ratio changed by checking parent_split_of_focused.
        let (_, axis, ratio, _) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(axis, SplitAxis::Horizontal);
        assert_eq!(ratio, 62, "ratio should be 62 after dragging right");
    }

    #[test]
    fn update_drag_resize_clamps_ratio() {
        let (mut server, _state) = make_tracking_server(nested_split_h());

        // Set up drag_state for the inner horizontal split.
        let mut path = [0u16; 8];
        path[0] = 0;
        server.drag_state = Some(DragState {
            split_path: path,
            split_path_len: 1,
            start_pos: 40,
            initial_ratio: 50,
            axis: SplitAxis::Horizontal,
            total_cells: 80,
        });

        // Drag far to the left (col=0, from start_pos=40).
        // delta = 0-40 = -40, pct_delta = -40*100/80 = -50.
        // new_ratio = 50 + (-50) = 0 → clamped to 1.
        let drag = mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            0,
            2,
            KeyModifiers::ALT,
        );
        server.handle_input(&drag);

        let (_, _, ratio, _) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(ratio, 1, "ratio should clamp to 1");
    }

    #[test]
    fn update_drag_resize_noop_without_drag_state() {
        let (mut server, _state) = make_tracking_server(split_h_layout());
        assert!(server.drag_state.is_none());

        // Drag event without prior Alt+click → no drag_state → no-op.
        let drag = mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            50,
            2,
            KeyModifiers::ALT,
        );
        server.handle_input(&drag);

        // Ratio should still be 50.
        let (_, _, ratio, _) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(ratio, 50, "no drag_state → ratio unchanged");
    }

    #[test]
    fn mouse_up_clears_drag_state() {
        let (mut server, _state) = make_tracking_server(split_h_layout());

        // Start drag.
        let click = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            50,
            2,
            KeyModifiers::ALT,
        );
        server.handle_input(&click);
        assert!(server.drag_state.is_some());

        // Release mouse → drag_state cleared.
        let up = mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            50,
            2,
            KeyModifiers::NONE,
        );
        server.handle_input(&up);
        assert!(server.drag_state.is_none(), "mouse up should clear drag");
    }

    // --- pane_resize_by_direction ---

    #[test]
    fn pane_resize_right_increases_ratio() {
        // Use nested layout so update_split_ratio gets a non-empty path.
        let (mut server, _state) = make_tracking_server(nested_split_h());
        assert_eq!(server.focus, 0);

        // Pane 0 focused, inner horizontal split, Right increases ratio.
        server.apply_action(KeyAction::PaneResizeRight);

        let (_, axis, ratio, _) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(axis, SplitAxis::Horizontal);
        assert_eq!(ratio, 52, "Right should increase ratio by 2");
    }

    #[test]
    fn pane_resize_left_decreases_ratio() {
        let (mut server, _state) = make_tracking_server(nested_split_h());

        server.apply_action(KeyAction::PaneResizeLeft);

        let (_, _, ratio, _) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(ratio, 48, "Left should decrease ratio by 2");
    }

    #[test]
    fn pane_resize_wrong_axis_is_noop() {
        let (mut server, _state) = make_tracking_server(nested_split_h());

        // Inner split is horizontal: Up/Down don't match the axis → no-op.
        server.apply_action(KeyAction::PaneResizeUp);
        let (_, _, ratio_before, _) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(ratio_before, 50, "Up on horizontal split is no-op");

        server.apply_action(KeyAction::PaneResizeDown);
        let (_, _, ratio_after, _) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(ratio_after, 50, "Down on horizontal split is no-op");
    }

    #[test]
    fn pane_resize_on_single_pane_is_noop() {
        let (mut server, _state) = make_tracking_server(single_layout());

        // Single pane has no parent split → no-op.
        server.apply_action(KeyAction::PaneResizeRight);

        assert!(
            server.parent_split_of_focused().is_none(),
            "single pane has no parent split"
        );
    }

    #[test]
    fn pane_resize_child1_right_decreases_ratio() {
        // Pane 1 is the RIGHT child of the inner horizontal split.
        // For child_idx=1, the delta is inverted:
        // Right → delta=+2 → adjusted=-2 → ratio decreases.
        let (mut server, _state) = make_tracking_server(nested_split_h());
        server.set_focus(1); // focus pane 1 (right child of inner split)

        server.apply_action(KeyAction::PaneResizeRight);

        let (_, _, ratio, child_idx) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(child_idx, 1, "pane 1 is child index 1");
        assert_eq!(ratio, 48, "Right for child 1 decreases ratio");
    }

    #[test]
    fn pane_resize_ratio_clamps_at_boundaries() {
        let (mut server, _state) = make_tracking_server(nested_split_h());

        // Repeatedly resize left to hit the minimum.
        for _ in 0..30 {
            server.apply_action(KeyAction::PaneResizeLeft);
        }

        let (_, _, ratio, _) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(ratio, 1, "ratio should clamp to minimum of 1");

        // Reset and resize right to hit the maximum.
        server.layout_root = nested_split_h();
        server.relayout(80, 24);
        for _ in 0..30 {
            server.apply_action(KeyAction::PaneResizeRight);
        }

        let (_, _, ratio, _) = server
            .parent_split_of_focused()
            .expect("should find parent split");
        assert_eq!(ratio, 99, "ratio should clamp to maximum of 99");
    }

    // ------------------------------------------------------------------
    // Mouse modifier combination tests: Alt+scroll, Ctrl+click,
    // multi-button, drag forwarding
    // ------------------------------------------------------------------

    #[test]
    fn handle_input_alt_scroll_forwards_sgr_with_alt_modifier() {
        let (mut server, state) = make_tracking_server(single_layout());

        let scroll = mouse_event(MouseEventKind::ScrollUp, 10, 5, KeyModifiers::ALT);
        server.handle_input(&scroll);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert_eq!(calls.len(), 1, "expected one write_input call");
        // ALT modifier = 8, button = 64 | 8 = 72.
        let expected = format!("\x1b[<{};{};{}{}", 72, 11, 5, 'M');
        assert_eq!(calls[0], expected.as_bytes());
    }

    #[test]
    fn handle_input_ctrl_scroll_forwards_sgr_with_ctrl_modifier() {
        let (mut server, state) = make_tracking_server(single_layout());

        let scroll = mouse_event(MouseEventKind::ScrollDown, 20, 10, KeyModifiers::CONTROL);
        server.handle_input(&scroll);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert_eq!(calls.len(), 1);
        // CONTROL modifier = 16, button = 65 | 16 = 81.
        let expected = format!("\x1b[<{};{};{}{}", 81, 21, 10, 'M');
        assert_eq!(calls[0], expected.as_bytes());
    }

    #[test]
    fn handle_input_middle_click_forwards_sgr_button_1() {
        let (mut server, state) = make_tracking_server(single_layout());

        let down = mouse_event(
            MouseEventKind::Down(MouseButton::Middle),
            15,
            8,
            KeyModifiers::NONE,
        );
        server.handle_input(&down);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert!(!calls.is_empty());
        // Middle button = 1, modifiers = 0.
        let expected = format!("\x1b[<{};{};{}{}", 1, 16, 8, 'M');
        assert_eq!(calls.last().unwrap(), expected.as_bytes());
    }

    #[test]
    fn handle_input_right_click_forwards_sgr_button_2() {
        let (mut server, state) = make_tracking_server(single_layout());

        let down = mouse_event(
            MouseEventKind::Down(MouseButton::Right),
            30,
            12,
            KeyModifiers::NONE,
        );
        server.handle_input(&down);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert!(!calls.is_empty());
        // Right button = 2, modifiers = 0.
        let expected = format!("\x1b[<{};{};{}{}", 2, 31, 12, 'M');
        assert_eq!(calls.last().unwrap(), expected.as_bytes());
    }

    #[test]
    fn handle_input_middle_up_forwards_sgr_with_m_suffix() {
        let (mut server, state) = make_tracking_server(single_layout());

        let up = mouse_event(
            MouseEventKind::Up(MouseButton::Middle),
            15,
            8,
            KeyModifiers::NONE,
        );
        server.handle_input(&up);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert!(!calls.is_empty());
        // Middle up: button=1, suffix='m'.
        let expected = format!("\x1b[<{};{};{}{}", 1, 16, 8, 'm');
        assert_eq!(calls.last().unwrap(), expected.as_bytes());
    }

    #[test]
    fn handle_input_drag_without_alt_is_noop() {
        let (mut server, state) = make_tracking_server(single_layout());
        assert!(server.drag_state.is_none());

        // Drag(MouseButton::Left) without drag_state → the code
        // checks self.drag_state.is_some() and skips. It does NOT
        // forward to PTY (that branch only exists in the non-drag
        // mouse event paths).
        let drag = mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            20,
            5,
            KeyModifiers::NONE,
        );
        server.handle_input(&drag);

        // No drag resize should have started.
        assert!(server.drag_state.is_none());
        // No bytes forwarded — the drag was silently ignored.
        assert!(
            state.lock().unwrap().write_input_bufs.is_empty(),
            "Left drag without drag_state should be silently ignored"
        );
    }

    #[test]
    fn handle_input_ctrl_shift_scroll_combines_modifiers() {
        let (mut server, state) = make_tracking_server(single_layout());

        let scroll = mouse_event(
            MouseEventKind::ScrollUp,
            5,
            3,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        server.handle_input(&scroll);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert_eq!(calls.len(), 1);
        // SHIFT=4, CONTROL=16, combined=20. button=64|20=84.
        let expected = format!("\x1b[<{};{};{}{}", 84, 6, 3, 'M');
        assert_eq!(calls[0], expected.as_bytes());
    }

    #[test]
    fn handle_input_all_modifiers_scroll() {
        let (mut server, state) = make_tracking_server(single_layout());

        let scroll = mouse_event(
            MouseEventKind::ScrollDown,
            40,
            20,
            KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL,
        );
        server.handle_input(&scroll);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert_eq!(calls.len(), 1);
        // SHIFT=4, ALT=8, CONTROL=16, combined=28. button=65|28=93.
        let expected = format!("\x1b[<{};{};{}{}", 93, 41, 20, 'M');
        assert_eq!(calls[0], expected.as_bytes());
    }

    #[test]
    fn handle_input_alt_right_click_forwards_sgr() {
        let (mut server, state) = make_tracking_server(single_layout());

        let down = mouse_event(
            MouseEventKind::Down(MouseButton::Right),
            25,
            10,
            KeyModifiers::ALT,
        );
        server.handle_input(&down);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert!(!calls.is_empty());
        // Right=2, ALT=8, combined=10.
        let expected = format!("\x1b[<{};{};{}{}", 10, 26, 10, 'M');
        assert_eq!(calls.last().unwrap(), expected.as_bytes());
    }

    #[test]
    fn handle_input_ctrl_left_click_forwards_sgr() {
        let (mut server, state) = make_tracking_server(single_layout());

        let down = mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            35,
            15,
            KeyModifiers::CONTROL,
        );
        server.handle_input(&down);

        let calls = &state.lock().unwrap().write_input_bufs;
        assert!(!calls.is_empty());
        // Left=0, CONTROL=16, combined=16.
        let expected = format!("\x1b[<{};{};{}{}", 16, 36, 15, 'M');
        assert_eq!(calls.last().unwrap(), expected.as_bytes());
    }

    // ------------------------------------------------------------------
    // Paste event handling tests: handle_paste, prepare_paste_bytes,
    // bracketed paste, forward_focus_event_to_focused_pane
    // ------------------------------------------------------------------

    #[test]
    fn handle_paste_forwards_raw_text_to_pty() {
        let (mut server, state) = make_tracking_server(single_layout());

        // No bracketed paste enabled → text sent as-is.
        server.handle_paste("hello world");

        let bufs = &state.lock().unwrap().write_input_bufs;
        assert_eq!(bufs.len(), 1, "expected one write_input call");
        assert_eq!(bufs[0], b"hello world");
    }

    #[test]
    fn handle_paste_empty_string_forwards_empty_bytes() {
        let (mut server, state) = make_tracking_server(single_layout());

        server.handle_paste("");

        let bufs = &state.lock().unwrap().write_input_bufs;
        assert_eq!(bufs.len(), 1);
        assert!(bufs[0].is_empty());
    }

    #[test]
    fn handle_paste_with_bracketed_paste_enabled_wraps_text() {
        let (mut server, state) = make_tracking_server(single_layout());

        // Enable bracketed paste for the focused pane.
        let lid = server.runners[0].layer_id();
        server.pane_bracketed_paste.insert(lid, true);
        // Enable host terminal capability.
        server.caps.bracketed_paste = true;

        server.handle_paste("test");

        let bufs = &state.lock().unwrap().write_input_bufs;
        assert_eq!(bufs.len(), 1);
        // Bracketed paste: ESC[200~ + text + ESC[201~
        let expected = b"\x1b[200~test\x1b[201~";
        assert_eq!(bufs[0], expected);
    }

    #[test]
    fn handle_paste_bracketed_paste_without_host_cap_sends_raw() {
        let (mut server, state) = make_tracking_server(single_layout());

        // Pane requests bracketed paste, but host doesn't support it.
        let lid = server.runners[0].layer_id();
        server.pane_bracketed_paste.insert(lid, true);
        server.caps.bracketed_paste = false;

        server.handle_paste("raw");

        let bufs = &state.lock().unwrap().write_input_bufs;
        assert_eq!(bufs.len(), 1);
        // Both conditions required: pane enabled AND host cap.
        // Without host cap → raw text.
        assert_eq!(bufs[0], b"raw");
    }

    #[test]
    fn handle_paste_host_cap_without_pane_flag_sends_raw() {
        let (mut server, state) = make_tracking_server(single_layout());

        // Host supports bracketed paste, but pane didn't request it.
        server.caps.bracketed_paste = true;
        // pane_bracketed_paste is empty (default false).

        server.handle_paste("plain");

        let bufs = &state.lock().unwrap().write_input_bufs;
        assert_eq!(bufs.len(), 1);
        assert_eq!(bufs[0], b"plain");
    }

    #[test]
    fn handle_paste_empty_runners_is_noop() {
        let (mut server, state) = make_tracking_server(single_layout());

        // Clear all runners.
        server.runners.clear();

        server.handle_paste("should not be sent");

        assert!(
            state.lock().unwrap().write_input_bufs.is_empty(),
            "no runners → no write_input calls"
        );
    }

    #[test]
    fn prepare_paste_bytes_bracketed() {
        let (mut server, _state) = make_tracking_server(single_layout());

        let lid = server.runners[0].layer_id();
        server.pane_bracketed_paste.insert(lid, true);
        server.caps.bracketed_paste = true;

        let bytes = server.prepare_paste_bytes("abc");
        assert_eq!(bytes, b"\x1b[200~abc\x1b[201~");
    }

    #[test]
    fn prepare_paste_bytes_not_bracketed() {
        let (server, _state) = make_tracking_server(single_layout());

        // Default: no bracketed paste.
        let bytes = server.prepare_paste_bytes("abc");
        assert_eq!(bytes, b"abc");
    }

    #[test]
    fn prepare_paste_bytes_special_characters() {
        let (mut server, _state) = make_tracking_server(single_layout());

        let lid = server.runners[0].layer_id();
        server.pane_bracketed_paste.insert(lid, true);
        server.caps.bracketed_paste = true;

        let bytes = server.prepare_paste_bytes("line1\nline2\ttab");
        assert_eq!(bytes, b"\x1b[200~line1\nline2\ttab\x1b[201~");
    }

    #[test]
    fn focused_bracketed_paste_enabled_returns_false_by_default() {
        let (server, _state) = make_tracking_server(single_layout());

        assert!(
            !server.focused_bracketed_paste_enabled(),
            "default: bracketed paste not enabled"
        );
    }

    #[test]
    fn focused_bracketed_paste_enabled_returns_true_when_set() {
        let (mut server, _state) = make_tracking_server(single_layout());

        let lid = server.runners[0].layer_id();
        server.pane_bracketed_paste.insert(lid, true);

        assert!(server.focused_bracketed_paste_enabled());
    }

    #[test]
    fn focused_bracketed_paste_enabled_empty_runners() {
        let (mut server, _state) = make_tracking_server(single_layout());

        server.runners.clear();

        assert!(
            !server.focused_bracketed_paste_enabled(),
            "no runners → false"
        );
    }

    // --- forward_focus_event_to_focused_pane ---

    #[test]
    fn focus_gained_sends_csi_i() {
        let (mut server, state) = make_tracking_server(single_layout());

        // Enable focus reporting for the pane and host cap.
        let lid = server.runners[0].layer_id();
        server.pane_focus_reporting.insert(lid, true);
        state.lock().unwrap().focus_reporting = true;
        server.caps.focus_events = true;

        server.forward_focus_event_to_focused_pane(true);

        let bufs = &state.lock().unwrap().write_input_bufs;
        assert_eq!(bufs.len(), 1);
        assert_eq!(bufs[0], b"\x1b[I", "gained focus → CSI I");
    }

    #[test]
    fn focus_lost_sends_csi_o() {
        let (mut server, state) = make_tracking_server(single_layout());

        let lid = server.runners[0].layer_id();
        server.pane_focus_reporting.insert(lid, true);
        state.lock().unwrap().focus_reporting = true;
        server.caps.focus_events = true;

        server.forward_focus_event_to_focused_pane(false);

        let bufs = &state.lock().unwrap().write_input_bufs;
        assert_eq!(bufs.len(), 1);
        assert_eq!(bufs[0], b"\x1b[O", "lost focus → CSI O");
    }

    #[test]
    fn focus_event_noop_without_host_cap() {
        let (mut server, state) = make_tracking_server(single_layout());

        let lid = server.runners[0].layer_id();
        server.pane_focus_reporting.insert(lid, true);
        state.lock().unwrap().focus_reporting = true;
        // Host doesn't support focus events.
        server.caps.focus_events = false;

        server.forward_focus_event_to_focused_pane(true);

        assert!(
            state.lock().unwrap().write_input_bufs.is_empty(),
            "no host cap → no bytes sent"
        );
    }

    #[test]
    fn focus_event_noop_without_pane_reporting() {
        let (mut server, state) = make_tracking_server(single_layout());

        // Host supports focus events, but pane didn't request them.
        server.caps.focus_events = true;
        // pane_focus_reporting is empty (default false).
        state.lock().unwrap().focus_reporting = false;

        server.forward_focus_event_to_focused_pane(true);

        assert!(
            state.lock().unwrap().write_input_bufs.is_empty(),
            "pane didn't request focus reporting → no bytes"
        );
    }

    #[test]
    fn focus_event_noop_empty_runners() {
        let (mut server, state) = make_tracking_server(single_layout());

        server.runners.clear();
        server.caps.focus_events = true;

        server.forward_focus_event_to_focused_pane(true);

        assert!(
            state.lock().unwrap().write_input_bufs.is_empty(),
            "no runners → no bytes"
        );
    }

    // ------------------------------------------------------------------
    // Config reload tests: apply_config_reload with layout changes,
    // theme updates, status bar, presets
    // ------------------------------------------------------------------

    fn default_config_reload() -> ConfigReload {
        ConfigReload {
            keybinds: vec![],
            presets: BTreeMap::new(),
            layout_root: None,
            status_bar: None,
            theme: None,
        }
    }

    #[test]
    fn config_reload_theme_some_updates_theme() {
        let mut server = make_server(single_layout(), 0);
        assert_eq!(server.theme, cmdash_config::Theme::default());

        let new_theme = cmdash_config::Theme {
            default_fg: Some(ratatui::style::Color::Red),
            ..Default::default()
        };
        let mut reload = default_config_reload();
        reload.theme = Some(new_theme.clone());

        server.apply_config_reload(reload);
        assert_eq!(server.theme, new_theme);
    }

    #[test]
    fn config_reload_theme_none_resets_to_default() {
        let mut server = make_server(single_layout(), 0);
        let custom_theme = cmdash_config::Theme {
            default_fg: Some(ratatui::style::Color::Blue),
            ..Default::default()
        };
        server.theme = custom_theme;
        assert_ne!(server.theme, cmdash_config::Theme::default());

        let reload = default_config_reload();
        server.apply_config_reload(reload);
        assert_eq!(server.theme, cmdash_config::Theme::default());
    }

    #[test]
    fn config_reload_status_bar_updates() {
        let mut server = make_server(single_layout(), 0);
        assert!(server.status_bar.is_none());

        let bar = cmdash_config::Bar {
            enabled: true,
            position: cmdash_config::BarPosition::Bottom,
            show_clock: true,
            show_pane_title: false,
            show_mode: true,
        };
        let mut reload = default_config_reload();
        reload.status_bar = Some(bar.clone());

        server.apply_config_reload(reload);
        assert_eq!(server.status_bar, Some(bar));
    }

    #[test]
    fn config_reload_presets_updates() {
        let mut server = make_server(single_layout(), 0);
        assert!(server.presets.is_empty());

        let mut presets = BTreeMap::new();
        presets.insert("coding".to_string(), single_layout());
        let mut reload = default_config_reload();
        reload.presets = presets.clone();

        server.apply_config_reload(reload);
        assert_eq!(server.presets.len(), 1);
        assert!(server.presets.contains_key("coding"));

        // Replacing presets via wholesale assignment.
        let reload2 = default_config_reload();
        server.apply_config_reload(reload2);
        assert!(server.presets.is_empty());
    }

    #[tokio::test]
    async fn config_reload_layout_change_triggers_wholesale_reconcile() {
        let mut server = make_server(single_layout(), 0);
        let initial_layer_ids: Vec<_> = server.runners.iter().map(|r| r.computed().id).collect();

        let mut reload = default_config_reload();
        reload.layout_root = Some(split_h_layout());

        server.apply_config_reload(reload);

        assert_eq!(server.runners.len(), 2);
        let new_layer_ids: Vec<_> = server.runners.iter().map(|r| r.computed().id).collect();
        assert_ne!(initial_layer_ids, new_layer_ids);
    }

    #[test]
    fn config_reload_same_layout_does_not_reconcile() {
        let mut server = make_server(single_layout(), 0);
        let initial_layer_ids: Vec<_> = server.runners.iter().map(|r| r.computed().id).collect();

        let mut reload = default_config_reload();
        reload.layout_root = Some(single_layout());

        server.apply_config_reload(reload);

        assert_eq!(server.runners.len(), 1);
        let after_layer_ids: Vec<_> = server.runners.iter().map(|r| r.computed().id).collect();
        assert_eq!(initial_layer_ids, after_layer_ids);
    }

    #[test]
    fn config_reload_no_layout_root_does_not_reconcile() {
        let mut server = make_server(split_h_layout(), 0);
        let initial_count = server.runners.len();
        let initial_layer_ids: Vec<_> = server.runners.iter().map(|r| r.computed().id).collect();

        let reload = default_config_reload();
        server.apply_config_reload(reload);

        assert_eq!(server.runners.len(), initial_count);
        let after_layer_ids: Vec<_> = server.runners.iter().map(|r| r.computed().id).collect();
        assert_eq!(initial_layer_ids, after_layer_ids);
    }

    #[tokio::test]
    async fn config_reload_layout_change_resets_stack_focus() {
        let mut server = make_server(split_h_layout(), 0);
        let pane_id = server.runners[0].computed().id;
        server.stack_focus.insert(pane_id, 1);

        let mut reload = default_config_reload();
        reload.layout_root = Some(single_layout());

        server.apply_config_reload(reload);

        assert!(server.stack_focus.is_empty());
    }

    #[test]
    fn config_reload_preserves_focus_after_same_layout() {
        let mut server = make_server(split_h_layout(), 1);
        assert_eq!(server.focus, 1);

        let mut reload = default_config_reload();
        reload.layout_root = Some(split_h_layout());
        let bar = cmdash_config::Bar {
            enabled: true,
            position: cmdash_config::BarPosition::Top,
            show_clock: false,
            show_pane_title: true,
            show_mode: false,
        };
        reload.status_bar = Some(bar.clone());

        server.apply_config_reload(reload);

        assert_eq!(server.focus, 1);
        assert_eq!(server.status_bar, Some(bar));
    }

    // ------------------------------------------------------------------
    // Tab operation tests: create_new_tab, close_active_tab,
    // switch_to_tab with runner reconciliation
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn create_new_tab_increments_tab_count() {
        let mut server = make_server(single_layout(), 0);
        assert_eq!(server.tabs.len(), 1);

        server.create_new_tab();
        assert_eq!(server.tabs.len(), 2);

        server.create_new_tab();
        assert_eq!(server.tabs.len(), 3);
    }

    #[tokio::test]
    async fn create_new_tab_sets_default_shell_layout() {
        let mut server = make_server(single_layout(), 0);
        server.create_new_tab();

        // The new tab's layout_root should be a single Shell pane.
        let new_tab = server.tabs.get(1).unwrap();
        match &new_tab.state.layout_root {
            LayoutNode::Pane(p) => assert_eq!(p.kind, PaneKind::Shell),
            other => panic!("expected Pane(Shell), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn create_new_tab_switches_active_to_new_tab() {
        let mut server = make_server(single_layout(), 0);
        assert_eq!(server.tabs.active_idx(), 0);

        server.create_new_tab();
        // create_new_tab pushes then syncs from active, which should
        // be the newly created tab.
        assert_eq!(server.tabs.active_idx(), 1);
    }

    #[tokio::test]
    async fn create_new_tab_spawned_runners_match_new_layout() {
        let mut server = make_server(single_layout(), 0);
        let initial_layer_id = server.runners[0].layer_id();

        server.create_new_tab();

        // The new tab has a single Shell pane layout, so after
        // reconciliation there should be exactly 1 runner.
        assert_eq!(server.runners.len(), 1);
        // PaneLayerId changes — new runners get fresh IDs from
        // alloc_layer_id(), even though PaneId is the same.
        assert_ne!(server.runners[0].layer_id(), initial_layer_id);
    }

    #[tokio::test]
    async fn close_active_tab_decrements_tab_count() {
        let mut server = make_server(single_layout(), 0);
        server.create_new_tab(); // tab 1 (active)
        assert_eq!(server.tabs.len(), 2);

        server.close_active_tab();
        assert_eq!(server.tabs.len(), 1);
    }

    #[tokio::test]
    async fn close_active_tab_last_tab_sets_running_false() {
        let mut server = make_server(single_layout(), 0);
        assert!(server.running);

        server.close_active_tab();
        assert!(!server.running);
    }

    #[tokio::test]
    async fn close_active_tab_restores_previous_tab_state() {
        let mut server = make_server(split_h_layout(), 0);
        let tab0_layer_ids: Vec<_> = server.runners.iter().map(|r| r.layer_id()).collect();

        server.create_new_tab(); // tab 1 with single pane
        assert_eq!(server.runners.len(), 1);

        server.close_active_tab(); // back to tab 0
                                   // After closing tab 1, we should be back to tab 0's layout.
        assert_eq!(server.tabs.active_idx(), 0);
        assert_eq!(server.runners.len(), 2); // split_h has 2 panes

        // PaneIds are the same (same layout tree), but PaneLayerIds
        // change because Wholesale reconcile creates fresh runners.
        let restored_layer_ids: Vec<_> = server.runners.iter().map(|r| r.layer_id()).collect();
        assert_ne!(tab0_layer_ids, restored_layer_ids);
    }

    #[tokio::test]
    async fn close_active_tab_adjusts_active_index() {
        let mut server = make_server(single_layout(), 0);
        server.create_new_tab(); // tab 1
        server.create_new_tab(); // tab 2 (active)
        assert_eq!(server.tabs.active_idx(), 2);
        assert_eq!(server.tabs.len(), 3);

        // Close tab 2 (the last tab). active_idx should move to 1.
        server.close_active_tab();
        assert_eq!(server.tabs.active_idx(), 1);
        assert_eq!(server.tabs.len(), 2);
    }

    #[tokio::test]
    async fn switch_to_tab_changes_active_index() {
        let mut server = make_server(single_layout(), 0);
        server.create_new_tab(); // tab 1 (active)
        assert_eq!(server.tabs.active_idx(), 1);

        server.switch_to_tab(0);
        assert_eq!(server.tabs.active_idx(), 0);
    }

    #[tokio::test]
    async fn switch_to_tab_syncs_layout_and_runners() {
        let mut server = make_server(single_layout(), 0);

        // Save tab 0's PaneLayerId.
        let tab0_layer_id = server.runners[0].layer_id();

        // Create tab 1 with a different layout (split_h).
        let new_state = TabState {
            runners: Vec::new(),
            focus: 0,
            layout_root: split_h_layout(),
            stack_focus: BTreeMap::new(),
        };
        server.tabs.push(new_state);
        server.switch_to_tab(1);

        // Tab 1 has split_h (2 panes), so runners should reflect that.
        assert_eq!(server.runners.len(), 2);
        assert_eq!(server.tabs.active_idx(), 1);

        // Switch back to tab 0 (single pane).
        server.switch_to_tab(0);
        assert_eq!(server.runners.len(), 1);
        assert_eq!(server.tabs.active_idx(), 0);

        // PaneLayerId changes because Wholesale reconcile creates fresh
        // runners each time, even when returning to the same layout.
        let new_tab0_layer_id = server.runners[0].layer_id();
        assert_ne!(tab0_layer_id, new_tab0_layer_id);
    }

    #[tokio::test]
    async fn switch_to_tab_out_of_range_is_noop() {
        let mut server = make_server(single_layout(), 0);
        let initial_layer_id = server.runners[0].layer_id();

        // switch_to_tab with out-of-range index should be a no-op.
        server.switch_to_tab(99);
        assert_eq!(server.tabs.active_idx(), 0);
        assert_eq!(server.tabs.len(), 1);

        // Runners should be unchanged (no reconciliation).
        assert_eq!(server.runners[0].layer_id(), initial_layer_id);
    }

    #[tokio::test]
    async fn switch_to_same_tab_still_reconciles() {
        let mut server = make_server(single_layout(), 0);
        let initial_layer_id = server.runners[0].layer_id();

        // switch_to_tab(0) when already on tab 0: tabs.switch_to(0)
        // returns true (0 < len), so sync + reconcile happen.
        // PaneId stays the same (same layout), but PaneLayerId
        // changes because Wholesale creates fresh runners.
        server.switch_to_tab(0);
        assert_eq!(server.tabs.active_idx(), 0);
        assert_ne!(server.runners[0].layer_id(), initial_layer_id);
    }

    #[tokio::test]
    async fn create_and_close_multiple_tabs_maintains_consistency() {
        let mut server = make_server(single_layout(), 0);

        // Create 4 tabs, then close them one by one.
        for _ in 0..4 {
            server.create_new_tab();
        }
        assert_eq!(server.tabs.len(), 5);
        assert_eq!(server.tabs.active_idx(), 4);

        // Close tabs from the end.
        for i in (1..5).rev() {
            server.close_active_tab();
            assert_eq!(server.tabs.len(), i);
        }

        // Should be back to tab 0.
        assert_eq!(server.tabs.len(), 1);
        assert_eq!(server.tabs.active_idx(), 0);
        assert!(server.running);
    }

    #[tokio::test]
    async fn apply_action_tab_new_dispatches_correctly() {
        let mut server = make_server(single_layout(), 0);
        assert_eq!(server.tabs.len(), 1);

        server.apply_action(KeyAction::TabNew);
        assert_eq!(server.tabs.len(), 2);
    }

    #[tokio::test]
    async fn apply_action_tab_switch_dispatches_correctly() {
        let mut server = make_server(single_layout(), 0);
        server.create_new_tab(); // tab 1 (active)
        assert_eq!(server.tabs.active_idx(), 1);

        server.apply_action(KeyAction::TabSwitch(0));
        assert_eq!(server.tabs.active_idx(), 0);
    }

    #[tokio::test]
    async fn apply_action_tab_close_dispatches_correctly() {
        let mut server = make_server(single_layout(), 0);
        server.create_new_tab(); // tab 1 (active)
        assert_eq!(server.tabs.len(), 2);

        server.apply_action(KeyAction::TabClose);
        assert_eq!(server.tabs.len(), 1);
        assert!(server.running);
    }

    // ------------------------------------------------------------------
    // apply_action tests: AppNewPane, PaneClose, PanePreset
    // ------------------------------------------------------------------

    // --- AppNewPane (split_focused_for_new_pane) ---

    #[tokio::test]
    async fn app_new_pane_splits_single_into_two() {
        let mut server = make_server(single_layout(), 0);
        assert_eq!(server.runners.len(), 1);

        server.apply_action(KeyAction::AppNewPane);

        // single_layout() pane split → 2 panes.
        assert_eq!(server.runners.len(), 2);
        // layout_root should now be a Split.
        assert!(matches!(server.layout_root, LayoutNode::Split { .. }));
    }

    #[tokio::test]
    async fn app_new_pane_preserves_original_pane_pre_order() {
        let mut server = make_server(single_layout(), 0);
        let original_pre_order = server.runners[0].computed().id.pre_order();

        server.apply_action(KeyAction::AppNewPane);

        // The original pane becomes child 0 of the new Split.
        // Its pre_order index is preserved (path grows by one
        // level, but the monotonic pre_order counter is stable).
        let survivor_pre_order = server.runners[0].computed().id.pre_order();
        assert_eq!(original_pre_order, survivor_pre_order);
        // path_len grows because the pane is now nested one level deeper.
        assert!(server.runners[0].computed().id.path_len() > 1);
    }

    #[tokio::test]
    async fn app_new_pane_focused_on_split_child() {
        let mut server = make_server(split_h_layout(), 0);
        assert_eq!(server.runners.len(), 2);

        // Focus pane 0 and split it.
        server.set_focus(0);
        server.apply_action(KeyAction::AppNewPane);

        // split_h has 2 panes; splitting pane 0 adds 1 more → 3.
        assert_eq!(server.runners.len(), 3);
    }

    #[tokio::test]
    async fn app_new_pane_no_runners_is_noop() {
        let mut server = make_server(single_layout(), 0);
        server.runners.clear();

        server.apply_action(KeyAction::AppNewPane);

        assert!(server.runners.is_empty());
    }

    // --- PaneClose (close_focused_and_rebalance) ---

    #[tokio::test]
    async fn pane_close_on_split_reduces_pane_count() {
        let mut server = make_server(split_h_layout(), 0);
        assert_eq!(server.runners.len(), 2);

        server.apply_action(KeyAction::PaneClose);

        // Closing one of 2 panes in a split collapses to 1.
        assert_eq!(server.runners.len(), 1);
        assert!(server.running);
    }

    #[tokio::test]
    async fn pane_close_root_leaf_sets_running_false() {
        let mut server = make_server(single_layout(), 0);
        assert_eq!(server.runners.len(), 1);
        assert!(server.running);

        server.apply_action(KeyAction::PaneClose);

        // Closing the only pane (root leaf) → running = false.
        assert!(!server.running);
    }

    #[tokio::test]
    async fn pane_close_adjusts_focus_when_needed() {
        let mut server = make_server(split_h_layout(), 1);
        assert_eq!(server.focus, 1);
        assert_eq!(server.runners.len(), 2);

        // Close pane 1 (the focused pane, last index).
        server.apply_action(KeyAction::PaneClose);

        // After close, focus should adjust to the remaining pane.
        assert_eq!(server.runners.len(), 1);
        assert_eq!(server.focus, 0);
        assert!(server.running);
    }

    #[tokio::test]
    async fn pane_close_no_runners_is_noop() {
        let mut server = make_server(single_layout(), 0);
        server.runners.clear();

        server.apply_action(KeyAction::PaneClose);

        // No runners → nothing to close.
        assert!(server.runners.is_empty());
    }

    #[tokio::test]
    async fn pane_close_on_split_h_collapses_to_single() {
        let mut server = make_server(split_h_layout(), 0);

        // Close pane 0; pane 1 should absorb upward.
        server.apply_action(KeyAction::PaneClose);

        // The split collapses to a single leaf.
        assert_eq!(server.runners.len(), 1);
        assert!(server.running);
    }

    #[tokio::test]
    async fn pane_close_nested_split_collapses_upward() {
        let mut server = make_server(nested_split_h(), 0);
        // nested_split_h: outer V(split_h, single) → 3 panes.
        assert_eq!(server.runners.len(), 3);

        // Close pane 1 (right child of inner split).
        // The inner split should collapse to its survivor (pane 0).
        server.set_focus(1);
        server.apply_action(KeyAction::PaneClose);

        // Inner split collapses: 3 panes → 2.
        assert_eq!(server.runners.len(), 2);
        assert!(server.running);
    }

    // --- PanePreset (swap_to_preset) ---

    #[tokio::test]
    async fn pane_preset_swaps_layout() {
        let mut server = make_server(single_layout(), 0);
        assert_eq!(server.runners.len(), 1);

        // Register a preset with split_h layout.
        server.presets.insert("debug".to_string(), split_h_layout());

        server.apply_action(KeyAction::PanePreset("debug".to_string()));

        // After preset swap, runners should reflect the new layout.
        assert_eq!(server.runners.len(), 2);
        assert!(server.running);
    }

    #[tokio::test]
    async fn pane_preset_resets_focus_to_zero() {
        let mut server = make_server(split_h_layout(), 1);
        assert_eq!(server.focus, 1);

        server.presets.insert("coding".to_string(), single_layout());
        server.apply_action(KeyAction::PanePreset("coding".to_string()));

        // swap_to_preset resets focus to 0.
        assert_eq!(server.focus, 0);
    }

    #[tokio::test]
    async fn pane_preset_unknown_name_is_noop() {
        let mut server = make_server(single_layout(), 0);
        let initial_count = server.runners.len();
        let initial_ids: Vec<_> = server.runners.iter().map(|r| r.layer_id()).collect();

        server.apply_action(KeyAction::PanePreset("nonexistent".to_string()));

        // Unknown preset → no-op.
        assert_eq!(server.runners.len(), initial_count);
        let after_ids: Vec<_> = server.runners.iter().map(|r| r.layer_id()).collect();
        assert_eq!(initial_ids, after_ids);
    }

    #[tokio::test]
    async fn pane_preset_updates_layout_root() {
        let mut server = make_server(single_layout(), 0);

        server.presets.insert("wide".to_string(), split_h_layout());
        server.apply_action(KeyAction::PanePreset("wide".to_string()));

        // layout_root should now be the preset's layout.
        assert!(matches!(server.layout_root, LayoutNode::Split { .. }));
    }

    #[tokio::test]
    async fn pane_preset_with_same_layout_still_reconciles() {
        let mut server = make_server(single_layout(), 0);
        let initial_layer_id = server.runners[0].layer_id();

        // Register preset with the same layout as current.
        server.presets.insert("same".to_string(), single_layout());
        server.apply_action(KeyAction::PanePreset("same".to_string()));

        // Wholesale reconcile always creates fresh runners.
        assert_ne!(server.runners[0].layer_id(), initial_layer_id);
    }

    // --- Combined action sequences ---

    #[tokio::test]
    async fn new_pane_then_close_restores_original_state() {
        let mut server = make_server(single_layout(), 0);

        // Split into 2 panes.
        server.apply_action(KeyAction::AppNewPane);
        assert_eq!(server.runners.len(), 2);

        // Close the newly created pane (focus should be on it or pane 0).
        // After close, should collapse back to 1 pane.
        server.apply_action(KeyAction::PaneClose);
        assert_eq!(server.runners.len(), 1);
        assert!(server.running);
    }

    #[tokio::test]
    async fn preset_swap_then_new_pane() {
        let mut server = make_server(single_layout(), 0);

        // Swap to split_h preset.
        server.presets.insert("debug".to_string(), split_h_layout());
        server.apply_action(KeyAction::PanePreset("debug".to_string()));
        assert_eq!(server.runners.len(), 2);

        // Now split one of the panes.
        server.apply_action(KeyAction::AppNewPane);
        assert_eq!(server.runners.len(), 3);
    }

    // ------------------------------------------------------------------
    // Copy mode unit tests: enter, exit, movement, selection, yank
    // ------------------------------------------------------------------

    #[test]
    fn enter_copy_mode_sets_state_and_mode() {
        let mut server = make_server(single_layout(), 0);
        assert!(server.copy_mode.is_none());

        server.enter_copy_mode();

        assert!(server.copy_mode.is_some());
        let state = server.copy_mode.unwrap();
        assert_eq!(state.cursor_x, 0);
        assert_eq!(state.cursor_y, 0);
        assert!(state.selection_start.is_none());
        assert_eq!(server.mode, Mode::Copy);
    }

    #[test]
    fn enter_copy_mode_no_runners_is_noop() {
        let mut server = make_server(single_layout(), 0);
        server.runners.clear();

        server.enter_copy_mode();

        assert!(server.copy_mode.is_none());
    }

    #[test]
    fn enter_copy_mode_reentry_resets_state() {
        let mut server = make_server(single_layout(), 0);
        server.enter_copy_mode();

        // Move cursor and start selection.
        server.copy_mode_move(3, 2);
        server.copy_mode_start_selection();
        assert!(server.copy_mode.unwrap().selection_start.is_some());

        // Re-enter copy mode — should reset to fresh state.
        server.enter_copy_mode();
        let state = server.copy_mode.unwrap();
        assert_eq!(state.cursor_x, 0);
        assert_eq!(state.cursor_y, 0);
        assert!(state.selection_start.is_none());
    }

    #[test]
    fn copy_mode_move_updates_cursor() {
        let mut server = make_server(single_layout(), 0);
        server.enter_copy_mode();

        server.copy_mode_move(5, 3);
        let state = server.copy_mode.unwrap();
        assert_eq!(state.cursor_x, 5);
        assert_eq!(state.cursor_y, 3);
    }

    #[test]
    fn copy_mode_move_negative_direction() {
        let mut server = make_server(single_layout(), 0);
        server.enter_copy_mode();

        // Move right-down first.
        server.copy_mode_move(5, 3);
        // Then move left-up.
        server.copy_mode_move(-2, -1);
        let state = server.copy_mode.unwrap();
        assert_eq!(state.cursor_x, 3);
        assert_eq!(state.cursor_y, 2);
    }

    #[test]
    fn copy_mode_move_noop_without_copy_mode() {
        let mut server = make_server(single_layout(), 0);
        // No copy mode active.
        server.copy_mode_move(5, 3);
        assert!(server.copy_mode.is_none());
    }

    #[test]
    fn copy_mode_start_selection_records_anchor() {
        let mut server = make_server(single_layout(), 0);
        server.enter_copy_mode();
        server.copy_mode_move(3, 2);

        server.copy_mode_start_selection();

        let state = server.copy_mode.unwrap();
        assert_eq!(state.selection_start, Some((3, 2)));
    }

    #[test]
    fn copy_mode_start_selection_toggles_off() {
        let mut server = make_server(single_layout(), 0);
        server.enter_copy_mode();

        // Start selection.
        server.copy_mode_start_selection();
        assert!(server.copy_mode.unwrap().selection_start.is_some());

        // Toggle off.
        server.copy_mode_start_selection();
        assert!(server.copy_mode.unwrap().selection_start.is_none());
    }

    #[test]
    fn copy_mode_start_selection_noop_without_copy_mode() {
        let mut server = make_server(single_layout(), 0);
        server.copy_mode_start_selection();
        assert!(server.copy_mode.is_none());
    }

    #[test]
    fn copy_mode_copy_without_snapshot_is_noop() {
        let mut server = make_server(single_layout(), 0);
        server.enter_copy_mode();
        assert_eq!(server.mode, Mode::Copy);

        // copy_mode_copy returns early when last_focused_snapshot is
        // None — it does NOT clear copy_mode or reset mode.
        let result = server.copy_mode_copy();
        assert!(result.is_ok());
        assert!(server.copy_mode.is_some());
        assert_eq!(server.mode, Mode::Copy);
    }

    #[test]
    fn copy_mode_copy_noop_without_copy_mode() {
        let mut server = make_server(single_layout(), 0);
        let result = server.copy_mode_copy();
        assert!(result.is_ok());
        assert!(server.copy_mode.is_none());
    }

    #[test]
    fn mode_exit_clears_copy_mode_and_snapshot() {
        let mut server = make_server(single_layout(), 0);
        server.enter_copy_mode();
        server.copy_mode_move(3, 2);
        assert_eq!(server.mode, Mode::Copy);

        server.apply_action(KeyAction::ModeExit);

        assert!(server.copy_mode.is_none());
        assert!(server.last_focused_snapshot.is_none());
        assert_eq!(server.mode, Mode::Normal);
    }

    #[test]
    fn apply_action_enter_copy_mode_dispatches() {
        let mut server = make_server(single_layout(), 0);
        assert_eq!(server.mode, Mode::Normal);

        server.apply_action(KeyAction::EnterCopyMode);

        assert_eq!(server.mode, Mode::Copy);
        assert!(server.copy_mode.is_some());
    }

    #[test]
    fn apply_action_copy_mode_move_dispatches() {
        let mut server = make_server(single_layout(), 0);
        server.enter_copy_mode();

        server.apply_action(KeyAction::CopyModeMoveRight);
        assert_eq!(server.copy_mode.unwrap().cursor_x, 1);

        server.apply_action(KeyAction::CopyModeMoveDown);
        assert_eq!(server.copy_mode.unwrap().cursor_y, 1);
    }

    #[test]
    fn apply_action_copy_mode_start_selection_dispatches() {
        let mut server = make_server(single_layout(), 0);
        server.enter_copy_mode();

        server.apply_action(KeyAction::CopyModeStartSelection);
        assert!(server.copy_mode.unwrap().selection_start.is_some());
    }

    #[test]
    fn copy_mode_move_all_four_directions() {
        let mut server = make_server(split_h_layout(), 0);
        server.enter_copy_mode();

        // Start at (0,0), move right 5, down 3.
        server.apply_action(KeyAction::CopyModeMoveRight);
        server.apply_action(KeyAction::CopyModeMoveRight);
        server.apply_action(KeyAction::CopyModeMoveRight);
        server.apply_action(KeyAction::CopyModeMoveRight);
        server.apply_action(KeyAction::CopyModeMoveRight);
        server.apply_action(KeyAction::CopyModeMoveDown);
        server.apply_action(KeyAction::CopyModeMoveDown);
        server.apply_action(KeyAction::CopyModeMoveDown);

        let state = server.copy_mode.unwrap();
        assert_eq!(state.cursor_x, 5);
        assert_eq!(state.cursor_y, 3);

        // Move back left 2, up 1.
        server.apply_action(KeyAction::CopyModeMoveLeft);
        server.apply_action(KeyAction::CopyModeMoveLeft);
        server.apply_action(KeyAction::CopyModeMoveUp);

        let state = server.copy_mode.unwrap();
        assert_eq!(state.cursor_x, 3);
        assert_eq!(state.cursor_y, 2);
    }

    #[test]
    fn copy_mode_full_workflow_select_and_exit() {
        let mut server = make_server(single_layout(), 0);

        // Enter copy mode.
        server.apply_action(KeyAction::EnterCopyMode);
        assert_eq!(server.mode, Mode::Copy);

        // Move cursor.
        server.apply_action(KeyAction::CopyModeMoveRight);
        server.apply_action(KeyAction::CopyModeMoveDown);

        // Start selection.
        server.apply_action(KeyAction::CopyModeStartSelection);
        assert!(server.copy_mode.unwrap().selection_start.is_some());

        // Move to extend selection.
        server.apply_action(KeyAction::CopyModeMoveRight);
        server.apply_action(KeyAction::CopyModeMoveRight);
        let state = server.copy_mode.unwrap();
        assert_eq!(state.cursor_x, 3);
        assert_eq!(state.cursor_y, 1);
        assert_eq!(state.selection_start, Some((1, 1)));

        // Exit copy mode via ModeExit (copy_mode_copy requires a
        // last_focused_snapshot which isn't available in unit tests).
        server.apply_action(KeyAction::ModeExit);
        assert!(server.copy_mode.is_none());
        assert_eq!(server.mode, Mode::Normal);
    }

    // ------------------------------------------------------------------
    // Directional focus navigation tests: focus_by_direction with
    // 4-direction adjacency
    // ------------------------------------------------------------------

    fn grid_2x2() -> LayoutNode {
        LayoutNode::Split {
            axis: SplitAxis::Vertical,
            ratio: Ratio(50),
            children: vec![split_h_layout(), split_h_layout()],
        }
    }

    #[test]
    fn focus_right_moves_to_right_pane() {
        let mut server = make_server(split_h_layout(), 0);
        assert_eq!(server.focus, 0);

        server.focus_by_direction(Direction::Right);
        assert_eq!(server.focus, 1);
    }

    #[test]
    fn focus_left_moves_to_left_pane() {
        let mut server = make_server(split_h_layout(), 1);
        assert_eq!(server.focus, 1);

        server.focus_by_direction(Direction::Left);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_right_then_left_returns_to_original() {
        let mut server = make_server(split_h_layout(), 0);

        server.focus_by_direction(Direction::Right);
        assert_eq!(server.focus, 1);

        server.focus_by_direction(Direction::Left);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_right_on_rightmost_pane_is_noop() {
        let mut server = make_server(split_h_layout(), 1);

        server.focus_by_direction(Direction::Right);
        // No pane to the right → focus unchanged.
        assert_eq!(server.focus, 1);
    }

    #[test]
    fn focus_left_on_leftmost_pane_is_noop() {
        let mut server = make_server(split_h_layout(), 0);

        server.focus_by_direction(Direction::Left);
        // No pane to the left → focus unchanged.
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_down_moves_to_lower_pane() {
        let mut server = make_server(split_v_layout(), 0);
        assert_eq!(server.focus, 0);

        server.focus_by_direction(Direction::Down);
        assert_eq!(server.focus, 1);
    }

    #[test]
    fn focus_up_moves_to_upper_pane() {
        let mut server = make_server(split_v_layout(), 1);
        assert_eq!(server.focus, 1);

        server.focus_by_direction(Direction::Up);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_down_then_up_returns_to_original() {
        let mut server = make_server(split_v_layout(), 0);

        server.focus_by_direction(Direction::Down);
        assert_eq!(server.focus, 1);

        server.focus_by_direction(Direction::Up);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_down_on_bottom_pane_is_noop() {
        let mut server = make_server(split_v_layout(), 1);

        server.focus_by_direction(Direction::Down);
        assert_eq!(server.focus, 1);
    }

    #[test]
    fn focus_up_on_top_pane_is_noop() {
        let mut server = make_server(split_v_layout(), 0);

        server.focus_by_direction(Direction::Up);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_right_on_single_pane_is_noop() {
        let mut server = make_server(single_layout(), 0);

        server.focus_by_direction(Direction::Right);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_left_on_single_pane_is_noop() {
        let mut server = make_server(single_layout(), 0);

        server.focus_by_direction(Direction::Left);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_up_on_single_pane_is_noop() {
        let mut server = make_server(single_layout(), 0);

        server.focus_by_direction(Direction::Up);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_down_on_single_pane_is_noop() {
        let mut server = make_server(single_layout(), 0);

        server.focus_by_direction(Direction::Down);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_empty_runners_is_noop() {
        let mut server = make_server(single_layout(), 0);
        server.runners.clear();

        server.focus_by_direction(Direction::Right);
        // No crash, no change.
    }

    #[test]
    fn focus_2x2_grid_all_directions() {
        // grid_2x2: outer V over two inner H splits.
        // Pane layout:
        //   pane 0 (top-left)     pane 1 (top-right)
        //   pane 2 (bottom-left)  pane 3 (bottom-right)
        let mut server = make_server(grid_2x2(), 0);

        // From pane 0: Right → pane 1.
        server.focus_by_direction(Direction::Right);
        assert_eq!(server.focus, 1);

        // From pane 1: Down → pane 3.
        server.focus_by_direction(Direction::Down);
        assert_eq!(server.focus, 3);

        // From pane 3: Left → pane 2.
        server.focus_by_direction(Direction::Left);
        assert_eq!(server.focus, 2);

        // From pane 2: Up → pane 0.
        server.focus_by_direction(Direction::Up);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_2x2_grid_diagonal_is_noop() {
        // From pane 0 (top-left), going Left or Up should be noop.
        let mut server = make_server(grid_2x2(), 0);

        server.focus_by_direction(Direction::Left);
        assert_eq!(server.focus, 0);

        server.focus_by_direction(Direction::Up);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn focus_2x2_grid_opposite_corner() {
        // From pane 0 (top-left) to pane 3 (bottom-right) requires two hops.
        let mut server = make_server(grid_2x2(), 0);

        server.focus_by_direction(Direction::Down);
        assert_eq!(server.focus, 2);

        server.focus_by_direction(Direction::Right);
        assert_eq!(server.focus, 3);
    }

    #[test]
    fn apply_action_pane_focus_right_dispatches() {
        let mut server = make_server(split_h_layout(), 0);

        server.apply_action(KeyAction::PaneFocusRight);
        assert_eq!(server.focus, 1);
    }

    #[test]
    fn apply_action_pane_focus_left_dispatches() {
        let mut server = make_server(split_h_layout(), 1);

        server.apply_action(KeyAction::PaneFocusLeft);
        assert_eq!(server.focus, 0);
    }

    #[test]
    fn apply_action_pane_focus_down_dispatches() {
        let mut server = make_server(split_v_layout(), 0);

        server.apply_action(KeyAction::PaneFocusDown);
        assert_eq!(server.focus, 1);
    }

    #[test]
    fn apply_action_pane_focus_up_dispatches() {
        let mut server = make_server(split_v_layout(), 1);

        server.apply_action(KeyAction::PaneFocusUp);
        assert_eq!(server.focus, 0);
    }

    // ------------------------------------------------------------------
    // ZStack overlay tests: stack_cycle and crosstack_member
    // ------------------------------------------------------------------

    fn zstack() -> LayoutNode {
        LayoutNode::ZStack {
            panes: vec![single_layout(), single_layout(), single_layout()],
        }
    }

    fn split_with_zstack() -> LayoutNode {
        LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(50),
            children: vec![zstack(), single_layout()],
        }
    }

    fn split_with_zstack_left() -> LayoutNode {
        LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(50),
            children: vec![single_layout(), zstack()],
        }
    }

    #[test]
    fn stack_cycle_advances_focus_to_next_member() {
        let mut server = make_server(zstack(), 0);
        assert_eq!(server.focus, 0);

        server.handle_stack_cycle();

        assert_eq!(server.focus, 1);
        let id = server.runners[server.focus].computed().id;
        assert_eq!(server.stack_focus.get(&id), Some(&1));
    }

    #[test]
    fn stack_cycle_wraps_around_to_first_member() {
        let mut server = make_server(zstack(), 0);
        // Focus last member.
        server.set_focus(2);

        server.handle_stack_cycle();

        assert_eq!(server.focus, 0);
        let id = server.runners[server.focus].computed().id;
        assert_eq!(server.stack_focus.get(&id), Some(&0));
    }

    #[test]
    fn stack_cycle_noop_when_not_in_zstack() {
        let mut server = make_server(split_h_layout(), 0);
        assert_eq!(server.focus, 0);

        server.handle_stack_cycle();

        assert_eq!(server.focus, 0);
        assert!(server.stack_focus.is_empty());
    }

    #[test]
    fn stack_cycle_noop_with_single_member_zstack() {
        let mut server = make_server(
            LayoutNode::ZStack {
                panes: vec![single_layout()],
            },
            0,
        );

        server.handle_stack_cycle();

        assert_eq!(server.focus, 0);
    }

    #[test]
    fn stack_cycle_noop_with_empty_runners() {
        let mut server = make_server(zstack(), 0);
        server.runners.clear();

        server.handle_stack_cycle();

        assert!(server.runners.is_empty());
    }

    #[test]
    fn crosstack_member_advances_to_next_member() {
        let mut server = make_server(zstack(), 0);

        server.crosstack_member(Direction::Right, true);

        assert_eq!(server.focus, 1);
        let id = server.runners[server.focus].computed().id;
        assert_eq!(server.stack_focus.get(&id), Some(&1));
    }

    #[test]
    fn crosstack_member_advances_to_previous_member() {
        let mut server = make_server(zstack(), 0);
        server.set_focus(2);

        server.crosstack_member(Direction::Right, false);

        assert_eq!(server.focus, 1);
        let id = server.runners[server.focus].computed().id;
        assert_eq!(server.stack_focus.get(&id), Some(&1));
    }

    #[test]
    fn crosstack_member_handoff_at_last_member_advances_direction() {
        // split_with_zstack: pane 0-2 are ZStack members, pane 3 is single.
        // Focus the last ZStack member and advance forward → handoff Right
        // should move focus to the single pane at index 3.
        let mut server = make_server(split_with_zstack(), 2);
        assert_eq!(server.focus, 2);

        server.crosstack_member(Direction::Right, true);

        assert_eq!(server.focus, 3);
        // stack_focus only tracks ZStack member indices; the handoff target
        // is a regular pane, so it has no entry.
        let id = server.runners[server.focus].computed().id;
        assert!(!server.stack_focus.contains_key(&id));
    }

    #[test]
    fn crosstack_member_handoff_at_first_member_reverses_direction() {
        // split_with_zstack_left: pane 0 is single, pane 1-3 are ZStack members.
        // Focus the first ZStack member and advance backward → handoff Left
        // should move focus to the single pane at index 0.
        let mut server = make_server(split_with_zstack_left(), 1);
        assert_eq!(server.focus, 1);

        server.crosstack_member(Direction::Left, false);

        assert_eq!(server.focus, 0);
        // stack_focus only tracks ZStack member indices; the handoff target
        // is a regular pane, so it has no entry.
        let id = server.runners[server.focus].computed().id;
        assert!(!server.stack_focus.contains_key(&id));
    }

    #[test]
    fn crosstack_member_handoff_blocked_when_no_adjacent_pane() {
        // split_with_zstack_left: single on left, ZStack on right.
        // Focus the last ZStack member and try to handoff Right from the
        // right edge of the layout — there is no pane to the right, so
        // focus should stay on the current member.
        let mut server = make_server(split_with_zstack_left(), 3);
        assert_eq!(server.focus, 3);

        server.crosstack_member(Direction::Right, true);

        assert_eq!(server.focus, 3);
    }

    #[test]
    fn crosstack_member_noop_when_not_in_zstack() {
        let mut server = make_server(split_h_layout(), 0);

        server.crosstack_member(Direction::Right, true);

        assert_eq!(server.focus, 0);
    }

    #[test]
    fn crosstack_member_noop_when_focused_on_non_zstack_sibling() {
        // split_with_zstack: ZStack on left, single pane on right.
        // Focus the non-ZStack sibling and try to navigate inside the stack.
        let mut server = make_server(split_with_zstack(), 3);
        assert_eq!(server.focus, 3);

        server.crosstack_member(Direction::Right, true);

        assert_eq!(server.focus, 3);
    }

    #[test]
    fn crosstack_member_noop_with_empty_runners() {
        let mut server = make_server(zstack(), 0);
        server.runners.clear();

        server.crosstack_member(Direction::Right, true);

        assert!(server.runners.is_empty());
    }

    #[test]
    fn apply_action_pane_stack_cycle_dispatches() {
        let mut server = make_server(zstack(), 0);

        server.apply_action(KeyAction::PaneStackCycle);

        assert_eq!(server.focus, 1);
    }

    #[test]
    fn apply_action_pane_stack_down_dispatches() {
        let mut server = make_server(zstack(), 0);

        server.apply_action(KeyAction::PaneStackDown);

        assert_eq!(server.focus, 1);
    }

    #[test]
    fn apply_action_pane_stack_down_from_middle_member() {
        let mut server = make_server(zstack(), 1);

        server.apply_action(KeyAction::PaneStackDown);

        assert_eq!(server.focus, 2);
    }

    #[test]
    fn apply_action_pane_stack_up_dispatches() {
        let mut server = make_server(zstack(), 0);
        server.set_focus(2);

        server.apply_action(KeyAction::PaneStackUp);

        assert_eq!(server.focus, 1);
    }

    #[test]
    fn apply_action_pane_stack_left_dispatches() {
        let mut server = make_server(zstack(), 0);
        server.set_focus(2);

        server.apply_action(KeyAction::PaneStackLeft);

        assert_eq!(server.focus, 1);
    }

    #[test]
    fn apply_action_pane_stack_right_dispatches() {
        let mut server = make_server(zstack(), 0);

        server.apply_action(KeyAction::PaneStackRight);

        assert_eq!(server.focus, 1);
    }

    // ------------------------------------------------------------------
    // relayout tests: resize propagation and runner resize calls
    // ------------------------------------------------------------------

    #[test]
    fn relayout_propagates_resize_to_all_runners() {
        let (mut server, state) = make_tracking_server(split_h_layout());

        server.relayout(80, 24);

        let calls = state.lock().unwrap().resize_calls.clone();
        assert_eq!(calls.len(), 2, "both runners should be resized");
        // split_h divides 80x24 (minus tab bar) into two equal halves.
        // Each pane should be roughly 40 columns wide.
        let widths: Vec<u16> = calls.iter().map(|(w, _)| *w).collect();
        assert!(widths.iter().all(|w| *w == 40));
    }

    #[test]
    fn relayout_updates_last_area() {
        let (mut server, _state) = make_tracking_server(single_layout());
        let old_area = server.last_area;

        server.relayout(100, 50);

        assert_ne!(server.last_area, old_area);
        assert_eq!(server.last_area.w, 100);
        assert_eq!(server.last_area.h, 50 - TAB_BAR_HEIGHT);
    }

    #[test]
    fn relayout_with_status_bar_reduces_layout_height() {
        let (mut server, _state) = make_tracking_server(single_layout());
        server.status_bar = Some(cmdash_config::Bar {
            enabled: true,
            ..Default::default()
        });

        server.relayout(80, 24);

        assert_eq!(server.last_area.h, 24 - TAB_BAR_HEIGHT - STATUS_BAR_HEIGHT);
    }

    #[test]
    fn relayout_mismatched_pane_count_is_noop() {
        let (mut server, state) = make_tracking_server(single_layout());
        // Remove a runner to create a mismatch (fewer runners than panes).
        server.runners.pop();

        server.relayout(80, 24);

        assert!(state.lock().unwrap().resize_calls.is_empty());
    }

    #[test]
    fn relayout_updates_runner_rects() {
        let (mut server, _state) = make_tracking_server(split_h_layout());

        server.relayout(80, 24);

        let first = &server.runners[0];
        let second = &server.runners[1];
        assert_eq!(first.computed().rect.w, 40);
        assert_eq!(second.computed().rect.w, 40);
        assert_eq!(first.computed().rect.x, 0);
        assert_eq!(second.computed().rect.x, 40);
        // The two panes should fill the full width.
        assert_eq!(first.computed().rect.w + second.computed().rect.w, 80);
    }

    // ------------------------------------------------------------------
    // process_pending_resize tests: ClientMessage::Resize handling and
    // integration with relayout
    // ------------------------------------------------------------------

    #[test]
    fn process_pending_resize_consumes_slot_and_relayouts() {
        let (mut server, state) = make_tracking_server(single_layout());
        server.pending_resize = Some((100, 50));

        server.process_pending_resize();

        assert!(
            server.pending_resize.is_none(),
            "pending resize slot should be consumed"
        );
        assert_eq!(server.last_area.w, 100);
        assert_eq!(server.last_area.h, 50 - TAB_BAR_HEIGHT);
        let calls = state.lock().unwrap().resize_calls.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, 100);
    }

    #[test]
    fn process_pending_resize_noop_without_pending() {
        let (mut server, state) = make_tracking_server(single_layout());
        let old_area = server.last_area;

        server.process_pending_resize();

        assert!(server.pending_resize.is_none());
        assert_eq!(server.last_area, old_area);
        assert!(state.lock().unwrap().resize_calls.is_empty());
    }
    #[test]
    fn process_pending_resize_last_wins() {
        let (mut server, state) = make_tracking_server(single_layout());
        // Simulate multiple rapid resize messages; only the last should apply.
        server.pending_resize = Some((80, 24));
        server.pending_resize = Some((120, 40));
        server.pending_resize = Some((100, 50));

        server.process_pending_resize();

        assert!(server.pending_resize.is_none());
        assert_eq!(server.last_area.w, 100);
        assert_eq!(server.last_area.h, 50 - TAB_BAR_HEIGHT);
        let calls = state.lock().unwrap().resize_calls.clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, 100);
    }

    #[test]
    fn process_pending_resize_zero_area_consumes_slot_without_changing_area() {
        let (mut server, state) = make_tracking_server(single_layout());
        let old_area = server.last_area;
        server.pending_resize = Some((0, 0));

        server.process_pending_resize();

        assert!(
            server.pending_resize.is_none(),
            "pending resize slot should be consumed"
        );
        assert_eq!(server.last_area, old_area);
        assert!(state.lock().unwrap().resize_calls.is_empty());
    }

    #[tokio::test]
    async fn run_loop_applies_client_resize() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());

        // Run the server loop in a background task.
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await.unwrap();
        });

        // Wait for the initial tick so the server has started ticking.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Send a resize request through the client channel.
        client_tx.send(ClientMessage::Resize(132, 60)).unwrap();

        // Give the server a tick to process the resize.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;

        // Verify the resize was applied by checking emitted frames.
        let found = frames.iter().any(|msg| match msg {
            ServerMessage::FrameIncremental { layout, .. } => {
                layout.total.w == 132 && layout.total.h == 60 - TAB_BAR_HEIGHT
            }
            _ => false,
        });
        assert!(found, "resize should be reflected in an emitted frame");

        // Shut down cleanly.
        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn run_loop_coalesces_multiple_client_resizes() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());

        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await.unwrap();
        });

        // Wait for the initial tick.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Send multiple rapid resize messages; only the last should apply.
        client_tx.send(ClientMessage::Resize(80, 24)).unwrap();
        client_tx.send(ClientMessage::Resize(120, 40)).unwrap();
        client_tx.send(ClientMessage::Resize(160, 80)).unwrap();

        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;

        let found = frames.iter().any(|msg| match msg {
            ServerMessage::FrameIncremental { layout, .. } => {
                layout.total.w == 160 && layout.total.h == 80 - TAB_BAR_HEIGHT
            }
            _ => false,
        });
        assert!(
            found,
            "only the last resize should be reflected in emitted frames"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    // ------------------------------------------------------------------
    // Integration tests: host resize drives per-pane PTY resize
    // through the full async ServerTask loop.
    // ------------------------------------------------------------------

    /// Build a server with `TrackingPty` runners and expose the
    /// async channels so the test can drive the run loop.
    #[allow(clippy::type_complexity)]
    fn make_tracking_server_with_channels(
        layout_root: LayoutNode,
    ) -> (
        ServerTask,
        tokio::sync::mpsc::UnboundedSender<ClientMessage>,
        tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
        tokio::sync::mpsc::UnboundedSender<PaneLayerId>,
        Arc<Mutex<TrackingState>>,
    ) {
        let area = LayoutRect {
            x: 0,
            y: 0,
            w: 80,
            h: 24,
        };
        let layout = ComputedLayout::compute(&layout_root, area).unwrap();
        let state = Arc::new(Mutex::new(TrackingState::default()));
        let runners: Vec<PaneRunner> = layout
            .panes
            .iter()
            .enumerate()
            .map(|(i, pane)| {
                let lid = PaneLayerId(i as u64 + 1);
                let pty: Box<dyn PanePtyOps + Send> = Box::new(TrackingPty {
                    layer_id: lid,
                    state: Arc::clone(&state),
                });
                PaneRunner::with_pty_for_test(pane.clone(), lid, pty, None)
            })
            .collect();
        let config = ServerConfig {
            layout_root,
            presets: BTreeMap::new(),
            shell: cmdash_pty::ShellSpec::LoginShell,
            status_bar: None,
            theme: cmdash_config::Theme::default(),
            widget_factories: HashMap::new(),
        };
        let (client_tx, client_rx) = unbounded_channel();
        let (server_tx, server_rx) = unbounded_channel();
        let (close_tx, close_rx) = unbounded_channel();
        let close_tx_handle = close_tx.clone();
        let server = ServerTask::new(
            config,
            runners,
            0,
            area,
            super::ServerChannels {
                close_tx,
                close_rx,
                config_reload_rx: None,
                client_rx,
                server_tx,
            },
        );
        (server, client_tx, server_rx, close_tx_handle, state)
    }

    #[tokio::test]
    async fn host_resize_drives_single_pane_pty_resize() {
        let (server, client_tx, mut server_rx, _close_tx, state) =
            make_tracking_server_with_channels(single_layout());

        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await.unwrap();
        });

        // Wait for the initial tick.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Send a host resize through the client channel.
        client_tx.send(ClientMessage::Resize(132, 50)).unwrap();

        // Wait for the tick that processes the resize.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Verify the PTY received a resize call with the new dimensions.
        let calls = state.lock().unwrap().resize_calls.clone();
        assert_eq!(
            calls.len(),
            1,
            "single pane should receive exactly one resize call"
        );
        assert_eq!(calls[0], (132, 50 - TAB_BAR_HEIGHT));

        // Verify the resize is reflected in emitted frames.
        let frames = drain_frames(&mut server_rx, 5).await;
        let found = frames.iter().any(|msg| match msg {
            ServerMessage::FrameIncremental { layout, .. } => {
                layout.total.w == 132 && layout.total.h == 50 - TAB_BAR_HEIGHT
            }
            _ => false,
        });
        assert!(found, "resize should be reflected in an emitted frame");

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn host_resize_drives_per_pane_pty_resize_in_split() {
        let (server, client_tx, mut server_rx, _close_tx, state) =
            make_tracking_server_with_channels(split_h_layout());

        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await.unwrap();
        });

        // Wait for the initial tick.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Send a host resize through the client channel.
        client_tx.send(ClientMessage::Resize(100, 40)).unwrap();

        // Wait for the tick that processes the resize.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Verify both panes received resize calls.
        let calls = state.lock().unwrap().resize_calls.clone();
        assert_eq!(calls.len(), 2, "split layout should resize both panes");
        // Each pane should be roughly half the width.
        assert_eq!(calls[0].0, 50);
        assert_eq!(calls[1].0, 50);
        assert_eq!(calls[0].1, 40 - TAB_BAR_HEIGHT);
        assert_eq!(calls[1].1, 40 - TAB_BAR_HEIGHT);

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn host_resize_zero_area_is_ignored() {
        let (server, client_tx, mut server_rx, _close_tx, state) =
            make_tracking_server_with_channels(single_layout());

        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await.unwrap();
        });

        // Wait for the initial tick.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Send a zero-area resize.
        client_tx.send(ClientMessage::Resize(0, 0)).unwrap();

        // Wait for the tick that processes the resize.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Verify no resize calls were made.
        let calls = state.lock().unwrap().resize_calls.clone();
        assert!(
            calls.is_empty(),
            "zero-area resize should not trigger PTY resize"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn host_resize_coalesces_multiple_rapid_resizes() {
        let (server, client_tx, mut server_rx, _close_tx, state) =
            make_tracking_server_with_channels(single_layout());

        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await.unwrap();
        });

        // Wait for the initial tick.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Send multiple rapid resize messages.
        client_tx.send(ClientMessage::Resize(80, 24)).unwrap();
        client_tx.send(ClientMessage::Resize(120, 40)).unwrap();
        client_tx.send(ClientMessage::Resize(160, 80)).unwrap();

        // Wait for the tick that processes the last resize.
        tokio::time::sleep(Duration::from_millis(80)).await;

        // Verify only the last resize was applied.
        let calls = state.lock().unwrap().resize_calls.clone();
        assert_eq!(
            calls.len(),
            1,
            "only one resize should be applied after coalescing"
        );
        assert_eq!(calls[0], (160, 80 - TAB_BAR_HEIGHT));

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    // ------------------------------------------------------------------
    // drain_frames shared-helper validation
    // ------------------------------------------------------------------

    /// Validate that `drain_frames` correctly collects frames from a real
    /// running server and respects the `max` count limit.
    #[tokio::test]
    async fn drain_frames_collects_from_running_server() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        // Wait for the server to produce its first tick.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // drain_frames should collect at least one FrameIncremental.
        let frames = drain_frames(&mut server_rx, 5).await;
        assert!(
            !frames.is_empty(),
            "drain_frames should collect frames from a running server"
        );
        assert!(
            frames
                .iter()
                .any(|msg| matches!(msg, ServerMessage::FrameIncremental { .. })),
            "at least one frame should be a FrameIncremental"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    /// Validate that `drain_frames` with max=1 returns at most one frame
    /// even when the server has produced multiple frames.
    #[tokio::test]
    async fn drain_frames_respects_max_count() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        // Let the server tick a few times.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Request only 1 frame — should get at most 1 even if more are available.
        let frames = drain_frames(&mut server_rx, 1).await;
        assert_eq!(
            frames.len(),
            1,
            "drain_frames(max=1) should return exactly one frame"
        );
        assert!(
            matches!(frames[0], ServerMessage::FrameIncremental { .. }),
            "the single frame should be a FrameIncremental"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
    /// Validate that `drain_frames` returns quickly when called on a
    /// channel that has been recently drained — it should not hang.
    #[tokio::test]
    async fn drain_frames_returns_quickly_when_channel_is_idle() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        // Wait for frames, then drain them all.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let first_batch = drain_frames(&mut server_rx, 50).await;
        assert!(
            !first_batch.is_empty(),
            "should have frames after first drain"
        );

        // Immediately drain with max=1 — should return quickly.
        // The server ticks at ~30fps so we might get 0 or 1 frames;
        // the key assertion is that the helper completes without hanging.
        let _start = std::time::Instant::now();
        let elapsed = {
            let start = std::time::Instant::now();
            let _ = drain_frames(&mut server_rx, 1).await;
            start.elapsed()
        };
        // max=1 means we get at most one frame; the key assertion
        // is that the helper completes without hanging.
        assert!(
            elapsed < Duration::from_millis(500),
            "drain_frames should return quickly, took {:?}",
            elapsed
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    /// Validate that `last_frame_focus` correctly extracts the focus index
    /// from frames collected by `drain_frames`.
    #[tokio::test]
    async fn drain_frames_with_last_frame_focus_on_split() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(split_h_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        // Wait for initial frame.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let initial = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_focus(&initial),
            Some(0),
            "initial focus should be 0"
        );

        // Focus next.
        client_tx
            .send(ClientMessage::Action(KeyAction::PaneFocusNext))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let updated = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_focus(&updated),
            Some(1),
            "focus should be 1 after PaneFocusNext"
        );

        // Focus next again (wraps to 0).
        client_tx
            .send(ClientMessage::Action(KeyAction::PaneFocusNext))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let wrapped = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_focus(&wrapped),
            Some(0),
            "focus should wrap back to 0"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    // ------------------------------------------------------------------
    // drain_frames + last_frame_mode: validates the shared helper
    // extracts the correct keybind mode from server output.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn last_frame_mode_extracts_normal_from_initial_frames() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert!(!frames.is_empty(), "should have frames after sleep");

        // Server starts in Normal mode.
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::Normal),
            "initial mode should be Normal"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn last_frame_mode_reflects_pane_resize_mode() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(split_h_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        // Wait for initial frames in Normal mode.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::Normal),
            "should start in Normal mode"
        );

        // Enter PaneResize mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::EnterPaneResize))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::PaneResize),
            "mode should change to PaneResize after EnterPaneResize"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn last_frame_mode_reflects_tab_switch_mode() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Enter TabSwitch mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::EnterTabSwitch))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::TabSwitch),
            "mode should change to TabSwitch after EnterTabSwitch"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[test]
    fn last_frame_mode_returns_none_for_empty_frames() {
        assert_eq!(
            last_frame_mode(&[]),
            None,
            "empty frame list should yield None"
        );
    }

    #[tokio::test]
    async fn last_frame_mode_transitions_normal_to_resize_and_back() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Enter PaneResize mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::EnterPaneResize))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::PaneResize)
        );

        // Escape back to Normal mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::ModeExit))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::Normal),
            "ModeExit should return to Normal mode"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    // ------------------------------------------------------------------
    // last_frame_mode: PresetPick mode transitions
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn last_frame_mode_reflects_preset_pick_mode() {
        use crate::test_helpers::build_server;
        use cmdash_config::{LayoutNode, Pane, PaneKind, Ratio, SplitAxis};
        use std::collections::BTreeMap;

        // Create a preset named "coding" with a split layout.
        let coding_preset = LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(60),
            children: vec![
                LayoutNode::Pane(Pane {
                    kind: PaneKind::Shell,
                    label: Some("editor".to_string()),
                    command: None,
                    scrollback_capacity: None,
                }),
                LayoutNode::Pane(Pane {
                    kind: PaneKind::Shell,
                    label: Some("terminal".to_string()),
                    command: None,
                    scrollback_capacity: None,
                }),
            ],
        };
        let mut presets = BTreeMap::new();
        presets.insert("coding".to_string(), coding_preset);

        let (server, client_tx, mut server_rx, _close_tx) =
            build_server(crate::test_helpers::single_layout(), 0, presets, |lid| {
                Box::new(StubPty { layer_id: lid })
            });
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Enter PresetPick mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::EnterPresetPick))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::PresetPick),
            "mode should change to PresetPick after EnterPresetPick"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn last_frame_mode_transitions_preset_pick_to_normal() {
        use crate::test_helpers::build_server;
        use cmdash_config::{LayoutNode, Pane, PaneKind};
        use std::collections::BTreeMap;

        // Create a preset named "debug" with a single pane.
        let debug_preset = LayoutNode::Pane(Pane {
            kind: PaneKind::Shell,
            label: Some("debug".to_string()),
            command: None,
            scrollback_capacity: None,
        });
        let mut presets = BTreeMap::new();
        presets.insert("debug".to_string(), debug_preset);

        let (server, client_tx, mut server_rx, _close_tx) =
            build_server(crate::test_helpers::single_layout(), 0, presets, |lid| {
                Box::new(StubPty { layer_id: lid })
            });
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Enter PresetPick mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::EnterPresetPick))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::PresetPick),
            "should be in PresetPick mode"
        );

        // Exit back to Normal mode via ModeExit.
        client_tx
            .send(ClientMessage::Action(KeyAction::ModeExit))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::Normal),
            "ModeExit should return to Normal mode"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn last_frame_mode_preset_swap_does_not_change_mode() {
        use crate::test_helpers::build_server;
        use cmdash_config::{LayoutNode, Pane, PaneKind, Ratio, SplitAxis};
        use std::collections::BTreeMap;

        // Create a preset named "coding" with a split layout.
        let coding_preset = LayoutNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: Ratio(50),
            children: vec![
                LayoutNode::Pane(Pane {
                    kind: PaneKind::Shell,
                    label: Some("left".to_string()),
                    command: None,
                    scrollback_capacity: None,
                }),
                LayoutNode::Pane(Pane {
                    kind: PaneKind::Shell,
                    label: Some("right".to_string()),
                    command: None,
                    scrollback_capacity: None,
                }),
            ],
        };
        let mut presets = BTreeMap::new();
        presets.insert("coding".to_string(), coding_preset);

        let (server, client_tx, mut server_rx, _close_tx) =
            build_server(crate::test_helpers::single_layout(), 0, presets, |lid| {
                Box::new(StubPty { layer_id: lid })
            });
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Apply a preset directly (via PanePreset action) while in Normal mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::PanePreset(
                "coding".to_string(),
            )))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::Normal),
            "PanePreset should not change the mode"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    // ------------------------------------------------------------------
    // last_frame_mode: Copy mode transitions
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn last_frame_mode_reflects_copy_mode() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Enter Copy mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::EnterCopyMode))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::Copy),
            "mode should change to Copy after EnterCopyMode"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn last_frame_mode_transitions_copy_to_normal() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Enter Copy mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::EnterCopyMode))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::Copy),
            "should be in Copy mode"
        );

        // Exit Copy mode via ModeExit.
        client_tx
            .send(ClientMessage::Action(KeyAction::ModeExit))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::Normal),
            "ModeExit should return to Normal mode"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn last_frame_mode_copy_start_selection_stays_in_copy() {
        let (server, client_tx, mut server_rx, _close_tx) =
            make_server_with_channels(single_layout());
        let handle = tokio::spawn(async move {
            let mut s = server;
            s.run().await
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = drain_frames(&mut server_rx, 5).await;

        // Enter Copy mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::EnterCopyMode))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(last_frame_mode(&frames), Some(cmdash_keybinds::Mode::Copy));

        // Start selection — should stay in Copy mode.
        client_tx
            .send(ClientMessage::Action(KeyAction::CopyModeStartSelection))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let frames = drain_frames(&mut server_rx, 5).await;
        assert_eq!(
            last_frame_mode(&frames),
            Some(cmdash_keybinds::Mode::Copy),
            "CopyModeStartSelection should stay in Copy mode"
        );

        client_tx
            .send(ClientMessage::Action(KeyAction::AppClose))
            .unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }
}
