//! Frontend-side task for Milestone 1 of session persistence.
//!
//! `FrontendTask` owns the ratatui terminal, the termcompositor
//! `GraphicsState`, the keybind router, and copy-mode UI state. It
//! receives `RenderFrame` payloads from the server and sends input
//! events/actions back.

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use cmdash_config::KeyAction;
use cmdash_keybinds::{Mode, Router};
use ratatui::Terminal;
use tracing::{debug, warn};

use crate::graphics::{GraphicsState, TabBarData};
use crate::protocol::{ClientMessage, ServerMessage};
use crate::render::{blit_cursor, blit_grid, blit_selection};

/// Frontend-side task. Owns terminal, graphics, router, and
/// copy-mode state.
pub struct FrontendTask<'a, B: ratatui::backend::Backend> {
    terminal: &'a mut Terminal<B>,
    graphics: GraphicsState,
    bindings: Router,
    client_tx: UnboundedSender<ClientMessage>,
    server_rx: UnboundedReceiver<ServerMessage>,
    copy_mode: Option<crate::protocol::CopyModeState>,
    #[allow(dead_code)]
    last_focused_snapshot: Option<cmdash_pty::PaneTerminalState>,
    host_keyboard_flags: u8,
    host_keyboard_pushed: bool,
    host_bracketed_paste: bool,
    host_bracketed_paste_pushed: bool,
    host_focus_reporting: bool,
    host_focus_reporting_pushed: bool,
    #[allow(dead_code)]
    host_focused: bool,
    running: bool,
}

impl<'a, B: ratatui::backend::Backend> FrontendTask<'a, B>
where
    B::Error: Send + Sync + 'static,
{
    /// Construct a new `FrontendTask`.
    pub fn new(
        terminal: &'a mut Terminal<B>,
        graphics: GraphicsState,
        bindings: Router,
        client_tx: UnboundedSender<ClientMessage>,
        server_rx: UnboundedReceiver<ServerMessage>,
    ) -> Self {
        Self {
            terminal,
            graphics,
            bindings,
            client_tx,
            server_rx,
            copy_mode: None,
            last_focused_snapshot: None,
            host_keyboard_flags: 0,
            host_keyboard_pushed: false,
            host_bracketed_paste: false,
            host_bracketed_paste_pushed: false,
            host_focus_reporting: false,
            host_focus_reporting_pushed: false,
            host_focused: true,
            running: true,
        }
    }

    /// Run the frontend loop until the server sends `Quit` or
    /// the user closes the app.
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Spawn the crossterm input reader off the async runtime.
        let (input_tx, input_rx) =
            tokio::sync::mpsc::unbounded_channel::<crossterm::event::Event>();
        tokio::task::spawn_blocking(move || loop {
            match crossterm::event::read() {
                Ok(evt) => {
                    if input_tx.send(evt).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "crossterm input reader error");
                    break;
                }
            }
        });

        self.run_with_input(input_rx).await
    }

    /// Run the frontend event loop with an externally-provided
    /// input channel. This is the core select! loop shared by
    /// `run()` (which spawns a crossterm reader) and integration
    /// tests (which inject events directly).
    pub async fn run_with_input(
        &mut self,
        mut input_rx: UnboundedReceiver<crossterm::event::Event>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        loop {
            tokio::select! {
                evt = input_rx.recv() => {
                    if let Some(evt) = evt {
                        self.handle_event(&evt)?;
                    }
                }

                msg = self.server_rx.recv() => {
                    match msg {
                        Some(ServerMessage::Quit) | None => break,
                        Some(ServerMessage::SyncFull { layout, grids, graphics, mode_flags, focus, tabs, running, mode, copy_mode }) => {
                            self.render_frame(&layout, &grids, &graphics, mode_flags, focus, &tabs)?;
                            self.running = running;
                            self.bindings.set_mode(mode);
                            self.copy_mode = copy_mode;
                        }
                        Some(ServerMessage::FrameIncremental { layout, frame, mode_flags, focus, tabs, running, mode, copy_mode }) => {
                            self.render_frame(&layout, &frame.grids, &frame.graphics, mode_flags, focus, &tabs)?;
                            self.running = running;
                            self.bindings.set_mode(mode);
                            self.copy_mode = copy_mode;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Render a single frame received from the server.
    fn render_frame(
        &mut self,
        layout: &cmdash_layout::ComputedLayout,
        grids: &std::collections::HashMap<cmdash_pty::PaneLayerId, cmdash_pty::TextGrid>,
        graphics: &[(cmdash_pty::PaneLayerId, cmdash_pty::KittyGraphicCmd)],
        mode_flags: crate::protocol::HostModeFlags,
        focus: usize,
        tabs: &crate::protocol::TabBarDataOwned,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Update graphics state from server frame.
        for (layer_id, cmd) in graphics {
            self.graphics.apply_kitty_event(*layer_id, cmd);
        }

        // Build tab bar data for the graphics overlay.
        let tab_bar_data = TabBarData {
            labels: tabs.labels.iter().map(|l| l.as_deref()).collect(),
            active_idx: tabs.active_idx,
            bar_width_cells: tabs.bar_width_cells,
        };
        self.graphics.update_tab_bar(&tab_bar_data);

        self.terminal.draw(|frame| {
            for (idx, pane) in layout.panes.iter().enumerate() {
                let area = ratatui::layout::Rect::new(
                    pane.rect.x,
                    pane.rect.y + 1, // TAB_BAR_HEIGHT
                    pane.rect.w,
                    pane.rect.h,
                );
                let layer_id = crate::derive_layer_id(&pane.id);
                if let Some(grid) = grids.get(&layer_id) {
                    debug!(
                        layer_id = ?layer_id,
                        rect.w = pane.rect.w,
                        rect.h = pane.rect.h,
                        "blitting pane"
                    );
                    blit_grid(grid, frame.buffer_mut(), area);
                    blit_cursor(grid, frame.buffer_mut(), area);
                    if idx == focus {
                        if let Some(state) = self.copy_mode.as_ref() {
                            let cursor = (state.cursor_x, state.cursor_y);
                            let end = state.selection_start.unwrap_or(cursor);
                            blit_selection(frame.buffer_mut(), area, cursor, end);
                        }
                    }
                }
            }
        })?;

        // Emit termcompositor graphics overlay.
        let mut stdout = std::io::stdout();
        if let Err(e) = self.graphics.render_and_write(&mut stdout) {
            warn!(error = %e, "graphics emit failed");
        }

        // Synchronize host terminal mode flags.
        self.sync_host_mode_flags(mode_flags);

        Ok(())
    }

    fn sync_host_mode_flags(&mut self, mode_flags: crate::protocol::HostModeFlags) {
        if mode_flags.kitty_keyboard != self.host_keyboard_flags {
            if mode_flags.kitty_keyboard == 0 {
                self.pop_host_keyboard_flags();
            } else {
                self.push_host_keyboard_flags(mode_flags.kitty_keyboard);
            }
            self.host_keyboard_flags = mode_flags.kitty_keyboard;
        }
        if mode_flags.bracketed_paste != self.host_bracketed_paste {
            if mode_flags.bracketed_paste {
                self.push_host_bracketed_paste();
            } else {
                self.pop_host_bracketed_paste();
            }
            self.host_bracketed_paste = mode_flags.bracketed_paste;
        }
        if mode_flags.focus_reporting != self.host_focus_reporting {
            if mode_flags.focus_reporting {
                self.push_host_focus_reporting();
            } else {
                self.pop_host_focus_reporting();
            }
            self.host_focus_reporting = mode_flags.focus_reporting;
        }
    }

    fn push_host_keyboard_flags(&mut self, flags: u8) {
        use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
        use crossterm::execute;
        let flags = KeyboardEnhancementFlags::from_bits_truncate(flags);
        if let Err(e) = execute!(std::io::stdout(), PushKeyboardEnhancementFlags(flags)) {
            warn!(error = ?e, "failed to push keyboard enhancement flags");
            return;
        }
        self.host_keyboard_pushed = true;
    }

    fn pop_host_keyboard_flags(&mut self) {
        use crossterm::event::PopKeyboardEnhancementFlags;
        use crossterm::execute;
        if !self.host_keyboard_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), PopKeyboardEnhancementFlags) {
            warn!(error = ?e, "failed to pop keyboard enhancement flags");
            return;
        }
        self.host_keyboard_pushed = false;
    }

    fn push_host_bracketed_paste(&mut self) {
        use crossterm::event::EnableBracketedPaste;
        use crossterm::execute;
        if self.host_bracketed_paste_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), EnableBracketedPaste) {
            warn!(error = ?e, "failed to enable bracketed paste");
            return;
        }
        self.host_bracketed_paste_pushed = true;
    }

    fn pop_host_bracketed_paste(&mut self) {
        use crossterm::event::DisableBracketedPaste;
        use crossterm::execute;
        if !self.host_bracketed_paste_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), DisableBracketedPaste) {
            warn!(error = ?e, "failed to disable bracketed paste");
            return;
        }
        self.host_bracketed_paste_pushed = false;
    }

    fn push_host_focus_reporting(&mut self) {
        use crossterm::event::EnableFocusChange;
        use crossterm::execute;
        if self.host_focus_reporting_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), EnableFocusChange) {
            warn!(error = ?e, "failed to enable focus-change reporting");
            return;
        }
        self.host_focus_reporting_pushed = true;
    }

    fn pop_host_focus_reporting(&mut self) {
        use crossterm::event::DisableFocusChange;
        use crossterm::execute;
        if !self.host_focus_reporting_pushed {
            return;
        }
        if let Err(e) = execute!(std::io::stdout(), DisableFocusChange) {
            warn!(error = ?e, "failed to disable focus-change reporting");
            return;
        }
        self.host_focus_reporting_pushed = false;
    }

    /// Handle a single crossterm event.
    fn handle_event(
        &mut self,
        evt: &crossterm::event::Event,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        eprintln!("frontend handle_event: {:?}", evt);
        if let Some(action) = self.bindings.dispatch_crossterm(evt) {
            // Apply mode changes to the Router locally BEFORE
            // forwarding to the server, so the next key press
            // is dispatched in the correct mode without waiting
            // for a frame round-trip.
            match &action {
                KeyAction::EnterPaneResize => self.bindings.set_mode(Mode::PaneResize),
                KeyAction::EnterTabSwitch => self.bindings.set_mode(Mode::TabSwitch),
                KeyAction::EnterPresetPick => self.bindings.set_mode(Mode::PresetPick),
                KeyAction::EnterCopyMode => self.bindings.set_mode(Mode::Copy),
                KeyAction::ModeExit => self.bindings.set_mode(Mode::Normal),
                _ => {}
            }
            self.client_tx.send(ClientMessage::Action(action))?;
            return Ok(());
        }

        match evt {
            crossterm::event::Event::Resize(w, h) => {
                self.client_tx.send(ClientMessage::Resize(*w, *h))?;
            }
            _ => {
                self.client_tx.send(ClientMessage::Input(evt.clone()))?;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::{GraphicsState, Metrics};

    use cmdash_config::{KeyAction, KeyToken, Keybind, Modifiers as CfgModifiers};
    use cmdash_keybinds::Router;
    use crossterm::event::{Event, KeyCode, KeyModifiers};

    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn new_defaults() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, _) = unbounded_channel();
        let (_, server_rx) = unbounded_channel();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![]);
        let frontend = FrontendTask::new(&mut terminal, graphics, bindings, client_tx, server_rx);
        assert!(frontend.running);
        assert!(frontend.copy_mode.is_none());
        assert_eq!(frontend.host_keyboard_flags, 0);
        assert!(!frontend.host_keyboard_pushed);
        assert!(!frontend.host_bracketed_paste);
        assert!(!frontend.host_bracketed_paste_pushed);
        assert!(!frontend.host_focus_reporting);
        assert!(!frontend.host_focus_reporting_pushed);
        assert!(frontend.host_focused);
    }

    #[test]
    fn handle_event_unmatched_key_sends_input() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, mut client_rx) = unbounded_channel();
        let (_, server_rx) = unbounded_channel();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![]);
        let mut frontend =
            FrontendTask::new(&mut terminal, graphics, bindings, client_tx, server_rx);
        frontend
            .handle_event(&key(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(
            client_rx.try_recv().unwrap(),
            ClientMessage::Input(_)
        ));
    }

    #[test]
    fn handle_event_matched_action_sends_action() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, mut client_rx) = unbounded_channel();
        let (_, server_rx) = unbounded_channel();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('q'),
            action: KeyAction::AppClose,
        }]);
        let mut frontend =
            FrontendTask::new(&mut terminal, graphics, bindings, client_tx, server_rx);
        frontend
            .handle_event(&key(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(
            client_rx.try_recv().unwrap(),
            ClientMessage::Action(KeyAction::AppClose)
        ));
    }

    #[test]
    fn handle_event_resize_sends_resize() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, mut client_rx) = unbounded_channel();
        let (_, server_rx) = unbounded_channel();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![]);
        let mut frontend =
            FrontendTask::new(&mut terminal, graphics, bindings, client_tx, server_rx);
        frontend.handle_event(&Event::Resize(120, 40)).unwrap();
        match client_rx.try_recv().unwrap() {
            ClientMessage::Resize(w, h) => {
                assert_eq!(w, 120);
                assert_eq!(h, 40);
            }
            other => panic!("expected Resize, got {:?}", other),
        }
    }

    #[test]
    fn handle_event_mode_interception() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, mut client_rx) = unbounded_channel();
        let (_, server_rx) = unbounded_channel();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('r'),
            action: KeyAction::EnterPaneResize,
        }]);
        let mut frontend =
            FrontendTask::new(&mut terminal, graphics, bindings, client_tx, server_rx);
        assert_eq!(frontend.bindings.mode(), Mode::Normal);
        frontend
            .handle_event(&key(KeyCode::Char('r'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(
            client_rx.try_recv().unwrap(),
            ClientMessage::Action(KeyAction::EnterPaneResize)
        ));
        // Mode interception: Router should locally switch mode
        // BEFORE forwarding to the server.
        assert_eq!(frontend.bindings.mode(), Mode::PaneResize);
    }

    #[test]
    fn handle_event_mode_exit_restores_normal() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, mut client_rx) = unbounded_channel();
        let (_, server_rx) = unbounded_channel();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('e'),
            action: KeyAction::ModeExit,
        }]);
        let mut frontend =
            FrontendTask::new(&mut terminal, graphics, bindings, client_tx, server_rx);
        // Simulate being in a non-Normal mode.
        frontend.bindings.set_mode(Mode::PaneResize);
        assert_eq!(frontend.bindings.mode(), Mode::PaneResize);
        frontend
            .handle_event(&key(KeyCode::Char('e'), KeyModifiers::NONE))
            .unwrap();
        assert!(matches!(
            client_rx.try_recv().unwrap(),
            ClientMessage::Action(KeyAction::ModeExit)
        ));
        assert_eq!(frontend.bindings.mode(), Mode::Normal);
    }

    #[test]
    fn handle_event_mouse_sends_input() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, mut client_rx) = unbounded_channel();
        let (_, server_rx) = unbounded_channel();
        let graphics = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![]);
        let mut frontend =
            FrontendTask::new(&mut terminal, graphics, bindings, client_tx, server_rx);
        let mouse_evt = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 10,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        frontend
            .handle_event(&crossterm::event::Event::Mouse(mouse_evt))
            .unwrap();
        assert!(matches!(
            client_rx.try_recv().unwrap(),
            ClientMessage::Input(_)
        ));
    }

    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Integration tests: async run_with_input() loop
    // ------------------------------------------------------------------

    use crate::protocol::{FrameData, HostModeFlags, TabBarDataOwned};
    use cmdash_config::PaneKind;
    use cmdash_layout::{ComputedLayout, Rect as LayoutRect};
    use cmdash_pty::PaneLayerId;
    use std::collections::HashMap;
    use std::time::Duration;

    /// Build a minimal `ComputedLayout` for testing render_frame
    /// by computing from a single-pane layout config.
    fn test_layout() -> ComputedLayout {
        use cmdash_config::{LayoutNode, Pane as CfgPane};
        let root = LayoutNode::Pane(CfgPane {
            kind: PaneKind::Shell,
            label: None,
            command: None,
            scrollback_capacity: None,
        });
        ComputedLayout::compute(
            &root,
            LayoutRect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        )
        .unwrap()
    }

    fn test_tabs() -> TabBarDataOwned {
        TabBarDataOwned {
            labels: vec![None],
            active_idx: 0,
            bar_width_cells: 80,
        }
    }

    fn test_mode_flags() -> HostModeFlags {
        HostModeFlags {
            kitty_keyboard: 0,
            bracketed_paste: false,
            focus_reporting: false,
        }
    }

    #[tokio::test]
    async fn run_with_input_quit_exits_loop() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, _client_rx) = unbounded_channel();
        let (server_tx, server_rx) = unbounded_channel();
        let gs = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![]);
        let (_input_tx, input_rx) = unbounded_channel();
        let mut frontend = FrontendTask::new(&mut terminal, gs, bindings, client_tx, server_rx);
        server_tx.send(ServerMessage::Quit).unwrap();
        let result =
            tokio::time::timeout(Duration::from_secs(2), frontend.run_with_input(input_rx)).await;
        assert!(result.is_ok(), "run_with_input should not hang");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn run_with_input_server_channel_close_exits() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, _client_rx) = unbounded_channel();
        let (server_tx, server_rx) = unbounded_channel();
        let gs = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![]);
        let (_input_tx, input_rx) = unbounded_channel();
        let mut frontend = FrontendTask::new(&mut terminal, gs, bindings, client_tx, server_rx);
        drop(server_tx);
        let result =
            tokio::time::timeout(Duration::from_secs(2), frontend.run_with_input(input_rx)).await;
        assert!(result.is_ok(), "should exit when server channel closes");
        assert!(result.unwrap().is_ok());
    }

    #[tokio::test]
    async fn run_with_input_sync_full_updates_state() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, _client_rx) = unbounded_channel();
        let (server_tx, server_rx) = unbounded_channel();
        let gs = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![]);
        let (_input_tx, input_rx) = unbounded_channel();

        // Queue SyncFull then Quit on the SAME channel (FIFO guaranteed).
        let layout = test_layout();
        let mut grids = HashMap::new();
        grids.insert(PaneLayerId(1), cmdash_pty::TextGrid::new(80, 22));
        server_tx
            .send(ServerMessage::SyncFull {
                layout,
                grids,
                graphics: vec![],
                mode_flags: test_mode_flags(),
                focus: 0,
                tabs: test_tabs(),
                running: false,
                mode: cmdash_keybinds::Mode::PaneResize,
                copy_mode: None,
            })
            .unwrap();
        server_tx.send(ServerMessage::Quit).unwrap();

        let mut frontend = FrontendTask::new(&mut terminal, gs, bindings, client_tx, server_rx);

        // Run: processes SyncFull (updates state) then Quit (exits).
        let result =
            tokio::time::timeout(Duration::from_secs(2), frontend.run_with_input(input_rx)).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());

        // State was updated by SyncFull before Quit.
        assert!(!frontend.running, "running should be false after SyncFull");
        assert_eq!(frontend.bindings.mode(), cmdash_keybinds::Mode::PaneResize,);
    }

    #[tokio::test]
    async fn run_with_input_frame_incremental_updates_state() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let (client_tx, _client_rx) = unbounded_channel();
        let (server_tx, server_rx) = unbounded_channel();
        let gs = GraphicsState::new(Metrics::default(), (80, 24));
        let bindings = Router::new(vec![]);
        let (_input_tx, input_rx) = unbounded_channel();

        // Queue FrameIncremental then Quit on the same channel.
        let layout = test_layout();
        let mut grids = HashMap::new();
        grids.insert(PaneLayerId(1), cmdash_pty::TextGrid::new(80, 22));
        let copy_state = crate::protocol::CopyModeState {
            cursor_x: 10,
            cursor_y: 5,
            selection_start: None,
        };
        server_tx
            .send(ServerMessage::FrameIncremental {
                layout,
                frame: FrameData {
                    grids,
                    graphics: vec![],
                    cursors: HashMap::new(),
                },
                mode_flags: test_mode_flags(),
                focus: 0,
                tabs: test_tabs(),
                running: true,
                mode: cmdash_keybinds::Mode::Copy,
                copy_mode: Some(copy_state),
            })
            .unwrap();
        server_tx.send(ServerMessage::Quit).unwrap();

        let mut frontend = FrontendTask::new(&mut terminal, gs, bindings, client_tx, server_rx);

        let result =
            tokio::time::timeout(Duration::from_secs(2), frontend.run_with_input(input_rx)).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());

        assert!(frontend.running);
        assert_eq!(frontend.bindings.mode(), cmdash_keybinds::Mode::Copy);
        assert!(frontend.copy_mode.is_some());
        assert_eq!(frontend.copy_mode.as_ref().unwrap().cursor_x, 10);
        assert_eq!(frontend.copy_mode.as_ref().unwrap().cursor_y, 5);
    } // ------------------------------------------------------------------
      // Full-loop integration tests: FrontendTask + ServerTask
      // ------------------------------------------------------------------

    // Re-export shared full-loop test infrastructure.
    use crate::test_helpers::{
        key, mouse_event, quit_router, resize_router, run_full_loop_test, single_layout,
        split_h_layout, split_v_layout, tab_router, wait_for_input_writes,
        wait_for_no_input_writes, wait_for_resize_calls,
    };

    #[tokio::test]
    async fn frontend_resize_drives_single_pane_pty_resize() {
        let calls = run_full_loop_test(single_layout(), quit_router(), |input_tx, state| {
            tokio::spawn(async move {
                input_tx.send(Event::Resize(120, 40)).unwrap();
                let calls = wait_for_resize_calls(state, 1).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                calls
            })
        })
        .await;

        assert_eq!(
            calls.len(),
            1,
            "single pane should receive exactly one resize"
        );
        assert_eq!(calls[0], (120, 40 - crate::tick_context::TAB_BAR_HEIGHT));
    }

    #[tokio::test]
    async fn frontend_resize_drives_per_pane_pty_resize_in_split() {
        let calls = run_full_loop_test(split_h_layout(), quit_router(), |input_tx, state| {
            tokio::spawn(async move {
                input_tx.send(Event::Resize(100, 40)).unwrap();
                let calls = wait_for_resize_calls(state, 2).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                calls
            })
        })
        .await;

        let expected_height = 40 - crate::tick_context::TAB_BAR_HEIGHT;
        assert_eq!(calls.len(), 2, "split layout should resize both panes");
        assert_eq!(calls[0], (50, expected_height));
        assert_eq!(calls[1], (50, expected_height));
    }

    #[tokio::test]
    async fn frontend_resize_zero_area_is_ignored() {
        let calls = run_full_loop_test(single_layout(), quit_router(), |input_tx, state| {
            tokio::spawn(async move {
                input_tx.send(Event::Resize(0, 0)).unwrap();
                // Give the server a chance to process; zero-area should be ignored.
                tokio::time::sleep(Duration::from_millis(80)).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                state.lock().unwrap().resize_calls.clone()
            })
        })
        .await;

        assert!(
            calls.is_empty(),
            "zero-area resize should not trigger PTY resize"
        );
    }

    #[tokio::test]
    async fn frontend_resize_coalesces_multiple_rapid_resizes() {
        let calls = run_full_loop_test(single_layout(), quit_router(), |input_tx, state| {
            tokio::spawn(async move {
                input_tx.send(Event::Resize(100, 40)).unwrap();
                input_tx.send(Event::Resize(120, 50)).unwrap();
                input_tx.send(Event::Resize(140, 60)).unwrap();
                let calls = wait_for_resize_calls(state, 1).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                calls
            })
        })
        .await;

        assert_eq!(
            calls.len(),
            1,
            "rapid resizes should coalesce to one PTY resize"
        );
        assert_eq!(calls[0], (140, 60 - crate::tick_context::TAB_BAR_HEIGHT));
    }

    // ------------------------------------------------------------------
    // Full-loop integration tests: key input forwarding
    // ------------------------------------------------------------------

    /// Build a frontend with a custom router for key-input tests.
    #[tokio::test]
    async fn frontend_forwards_unmatched_key_to_pty() {
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('q'),
            action: KeyAction::AppClose,
        }]);
        let bufs = run_full_loop_test(single_layout(), bindings, |input_tx, state| {
            tokio::spawn(async move {
                input_tx
                    .send(key(KeyCode::Char('x'), KeyModifiers::NONE))
                    .unwrap();
                let bufs = wait_for_input_writes(state, 1).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                bufs
            })
        })
        .await;

        assert_eq!(bufs.len(), 1, "unmatched key should be forwarded once");
        assert_eq!(bufs[0], b"x", "unmatched key should forward 'x' bytes");
    }

    #[tokio::test]
    async fn frontend_does_not_forward_matched_action_as_input() {
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('a'),
            action: KeyAction::AppClose,
        }]);
        let bufs = run_full_loop_test(single_layout(), bindings, |input_tx, state| {
            tokio::spawn(async move {
                input_tx
                    .send(key(KeyCode::Char('a'), KeyModifiers::NONE))
                    .unwrap();
                // Poll for a short window to confirm no PTY input is ever
                // written for a matched action. The server will exit as a
                // side effect of AppClose, but we want to be sure the
                // action itself was not forwarded as input.
                wait_for_no_input_writes(state, Duration::from_millis(200)).await
            })
        })
        .await;

        assert!(
            bufs.is_empty(),
            "matched action should not be forwarded as PTY input"
        );
    }

    #[tokio::test]
    async fn frontend_forwards_special_keys_to_pty() {
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('q'),
            action: KeyAction::AppClose,
        }]);
        let bufs = run_full_loop_test(single_layout(), bindings, |input_tx, state| {
            tokio::spawn(async move {
                input_tx
                    .send(key(KeyCode::Enter, KeyModifiers::NONE))
                    .unwrap();
                let bufs = wait_for_input_writes(state, 1).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                bufs
            })
        })
        .await;

        assert_eq!(bufs.len(), 1, "Enter should be forwarded once");
        assert_eq!(bufs[0], b"\r", "Enter should forward CR");
    }

    #[tokio::test]
    async fn frontend_forwards_key_with_modifiers_to_pty() {
        let bindings = Router::new(vec![Keybind {
            mods: CfgModifiers::default(),
            key: KeyToken::Char('q'),
            action: KeyAction::AppClose,
        }]);
        let bufs = run_full_loop_test(single_layout(), bindings, |input_tx, state| {
            tokio::spawn(async move {
                input_tx
                    .send(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                    .unwrap();
                let bufs = wait_for_input_writes(state, 1).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                bufs
            })
        })
        .await;

        assert_eq!(bufs.len(), 1, "Ctrl+c should be forwarded once");
        // event_to_bytes currently ignores modifiers, so Ctrl+c produces 'c'.
        assert_eq!(bufs[0], b"c", "Ctrl+c currently forwards bare 'c' bytes");
    }

    // ------------------------------------------------------------------
    // Full-loop integration tests: mouse click-to-focus and Alt+drag resize
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn frontend_mouse_click_to_focus_changes_focused_pane() {
        // split_h_layout: pane 0 at x=0..40, pane 1 at x=40..80.
        let events = run_full_loop_test(split_h_layout(), quit_router(), |input_tx, state| {
            tokio::spawn(async move {
                // Click inside the right pane.
                input_tx
                    .send(mouse_event(
                        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                        50,
                        5,
                        KeyModifiers::NONE,
                    ))
                    .unwrap();
                // Send a key so we can observe which pane received input.
                input_tx
                    .send(key(KeyCode::Char('x'), KeyModifiers::NONE))
                    .unwrap();
                let _ = wait_for_input_writes(state.clone(), 2).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                state.lock().unwrap().write_input_events.clone()
            })
        })
        .await;

        // The mouse down event and the 'x' key should both be forwarded
        // to the right pane (layer_id 2).
        let right_pane_events: Vec<_> = events
            .iter()
            .filter(|(lid, _)| lid.0 == 2)
            .map(|(_, bytes)| bytes.clone())
            .collect();
        assert_eq!(
            right_pane_events.len(),
            2,
            "right pane should receive mouse down and key input after click-to-focus"
        );
        assert_eq!(right_pane_events[1], b"x", "key should go to focused pane");
    }

    #[tokio::test]
    async fn frontend_alt_drag_resize_updates_pane_sizes() {
        // split_h_layout: pane 0 at x=0..40, pane 1 at x=40..80.
        let calls = run_full_loop_test(split_h_layout(), quit_router(), |input_tx, state| {
            tokio::spawn(async move {
                // Establish initial dimensions.
                input_tx.send(Event::Resize(80, 24)).unwrap();
                let _ = wait_for_resize_calls(state.clone(), 2).await;

                // Alt+drag the split boundary from column 40 to column 60.
                input_tx
                    .send(mouse_event(
                        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                        40,
                        5,
                        KeyModifiers::ALT,
                    ))
                    .unwrap();
                input_tx
                    .send(mouse_event(
                        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                        60,
                        5,
                        KeyModifiers::ALT,
                    ))
                    .unwrap();
                input_tx
                    .send(mouse_event(
                        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                        60,
                        5,
                        KeyModifiers::ALT,
                    ))
                    .unwrap();

                // Trigger a relayout so the new ratio is applied to runners.
                input_tx.send(Event::Resize(80, 24)).unwrap();
                let calls = wait_for_resize_calls(state, 6).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                calls
            })
        })
        .await;
        assert!(
            calls.len() >= 4,
            "should have initial resize + post-drag resize"
        );
        // After dragging the boundary right, the left pane should be wider
        // and the right pane narrower.
        assert_eq!(
            calls[calls.len() - 2],
            (60, 23),
            "left pane should widen to 60 columns"
        );
        assert_eq!(
            calls[calls.len() - 1],
            (20, 23),
            "right pane should narrow to 20 columns"
        );
    }

    #[tokio::test]
    async fn frontend_keyboard_resize_root_horizontal_split() {
        // split_h_layout: pane 0 at x=0..40, pane 1 at x=40..80.
        let calls = run_full_loop_test(split_h_layout(), resize_router(), |input_tx, state| {
            tokio::spawn(async move {
                // Establish initial dimensions.
                input_tx.send(Event::Resize(80, 24)).unwrap();
                let _ = wait_for_resize_calls(state.clone(), 2).await;

                // Enter pane-resize mode, then grow the left pane rightward.
                input_tx
                    .send(key(KeyCode::Char('r'), KeyModifiers::NONE))
                    .unwrap();
                input_tx
                    .send(key(KeyCode::Right, KeyModifiers::NONE))
                    .unwrap();
                // Exit resize mode and trigger a fresh relayout.
                input_tx
                    .send(key(KeyCode::Esc, KeyModifiers::NONE))
                    .unwrap();
                input_tx.send(Event::Resize(80, 24)).unwrap();

                let calls = wait_for_resize_calls(state, 6).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                calls
            })
        })
        .await;

        assert!(
            calls.len() >= 4,
            "should have initial resize + post-keyboard resize"
        );
        // Ratio moved from 50% to 52%; left pane widens, right narrows.
        assert_eq!(
            calls[calls.len() - 2],
            (41, 23),
            "left pane should widen to 41 columns"
        );
        assert_eq!(
            calls[calls.len() - 1],
            (39, 23),
            "right pane should narrow to 39 columns"
        );
    }
    #[tokio::test]
    async fn frontend_keyboard_resize_root_vertical_split() {
        // split_v_layout: pane 0 at y=0..11, pane 1 at y=11..22.
        let calls = run_full_loop_test(split_v_layout(), resize_router(), |input_tx, state| {
            tokio::spawn(async move {
                // Establish initial dimensions.
                input_tx.send(Event::Resize(80, 24)).unwrap();
                let _ = wait_for_resize_calls(state.clone(), 2).await;

                // Enter pane-resize mode, then grow the top pane downward.
                input_tx
                    .send(key(KeyCode::Char('r'), KeyModifiers::NONE))
                    .unwrap();
                // Two Down presses move the ratio from 50% to 54%, which
                // is enough to change the integer heights (12/11).
                input_tx
                    .send(key(KeyCode::Down, KeyModifiers::NONE))
                    .unwrap();
                input_tx
                    .send(key(KeyCode::Down, KeyModifiers::NONE))
                    .unwrap();
                // Exit resize mode and trigger a fresh relayout.
                input_tx
                    .send(key(KeyCode::Esc, KeyModifiers::NONE))
                    .unwrap();
                input_tx.send(Event::Resize(80, 24)).unwrap();

                let calls = wait_for_resize_calls(state, 8).await;
                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                calls
            })
        })
        .await;

        assert!(
            calls.len() >= 4,
            "should have initial resize + post-keyboard resize"
        );
        // Ratio moved from 50% to 54%; top pane grows, bottom shrinks.
        assert_eq!(
            calls[calls.len() - 2],
            (80, 12),
            "top pane should grow to 12 rows"
        );
        assert_eq!(
            calls[calls.len() - 1],
            (80, 11),
            "bottom pane should shrink to 11 rows"
        );
    }

    // ------------------------------------------------------------------
    // Full-loop integration tests: tab operations
    // ------------------------------------------------------------------
    #[tokio::test]
    async fn frontend_tab_close_on_last_tab_stops_server() {
        let result = run_full_loop_test(single_layout(), tab_router(), |input_tx, state| {
            tokio::spawn(async move {
                input_tx.send(Event::Resize(80, 24)).unwrap();
                let _ = wait_for_resize_calls(state.clone(), 1).await;

                // Close the only tab — server should set running=false
                // and exit the loop.
                input_tx
                    .send(key(KeyCode::Char('x'), KeyModifiers::NONE))
                    .unwrap();

                // Give the server a moment to process, then check state.
                tokio::time::sleep(Duration::from_millis(80)).await;
                state.lock().unwrap().resize_calls.clone()
            })
        })
        .await;

        // The server should have exited cleanly; result is the resize calls.
        assert!(
            !result.is_empty(),
            "at least the initial resize should have been recorded"
        );
    }

    // NOTE: TabNew and TabSwitch full-loop tests are not feasible here
    // because TabNew calls reconcile_runners(Wholesale), which spawns
    // real PTY processes (not TrackingPty). Those real processes exit
    // quickly in CI, causing the server to detect all_exited and shut
    // down. TabNew and TabSwitch are covered by server_task unit tests
    // (create_new_tab_*, close_active_tab_*, switch_to_tab_*).

    // ------------------------------------------------------------------
    // Full-loop integration tests: paste event handling
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn frontend_paste_forwards_text_to_pty() {
        let bufs = run_full_loop_test(single_layout(), quit_router(), |input_tx, state| {
            tokio::spawn(async move {
                input_tx.send(Event::Resize(80, 24)).unwrap();
                let _ = wait_for_resize_calls(state.clone(), 1).await;

                input_tx.send(Event::Paste("hello world".into())).unwrap();
                let bufs = wait_for_input_writes(state, 1).await;

                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                bufs
            })
        })
        .await;

        assert_eq!(bufs.len(), 1, "paste should produce one input write");
        assert_eq!(
            bufs[0], b"hello world",
            "paste bytes should match original text"
        );
    }

    #[tokio::test]
    async fn frontend_paste_empty_string_is_forwarded() {
        let bufs = run_full_loop_test(single_layout(), quit_router(), |input_tx, state| {
            tokio::spawn(async move {
                input_tx.send(Event::Resize(80, 24)).unwrap();
                let _ = wait_for_resize_calls(state.clone(), 1).await;

                input_tx.send(Event::Paste("".into())).unwrap();
                // Empty paste still produces a write (zero-length buffer).
                tokio::time::sleep(Duration::from_millis(50)).await;

                input_tx
                    .send(key(KeyCode::Char('q'), KeyModifiers::NONE))
                    .unwrap();
                state.lock().unwrap().write_input_bufs.clone()
            })
        })
        .await;

        // An empty paste produces a zero-length write, which the
        // tracking PTY records.
        assert!(
            bufs.iter().any(|b| b.is_empty()),
            "empty paste should produce at least one zero-length buffer"
        );
    }
}
