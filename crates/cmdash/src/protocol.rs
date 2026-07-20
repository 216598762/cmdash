//! In-process message protocol for the Milestone 1
//! `FrontendTask` / `ServerTask` split.
//!
//! These messages are sent over `tokio::sync::mpsc` channels.
//! They intentionally mirror the wire protocol described in
//! `docs/session-persistence-architecture.md` so the later
//! migration to Unix-domain sockets is a mechanical swap of the
//! transport layer.

use std::collections::HashMap;

use cmdash_layout::ComputedLayout;
use cmdash_pty::{KittyGraphicCmd, PaneLayerId, TextGrid};

/// Messages sent from the frontend to the server.
#[derive(Debug)]
pub enum ClientMessage {
    /// A parsed keybind action (e.g. `PaneFocusNext`).
    Action(cmdash_config::KeyAction),
    /// Raw crossterm input event that the frontend did not
    /// consume locally (unmatched keys, mouse, resize, etc.).
    Input(crossterm::event::Event),
    /// Host terminal resized to `(cols, rows)`.
    Resize(u16, u16),
    /// Frontend is disconnecting (in-process: signals graceful
    /// shutdown).
    Detach,
}

/// Aggregated host-terminal mode flags derived from the union of
/// all pane requests.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostModeFlags {
    /// Union of kitty-keyboard progressive-enhancement flags.
    pub kitty_keyboard: u8,
    /// Whether any pane has requested bracketed-paste mode.
    pub bracketed_paste: bool,
    /// Whether any pane has requested focus reporting.
    pub focus_reporting: bool,
}

/// Per-frame incremental payload sent from the server to the
/// frontend.
#[derive(Debug, Default)]
pub struct FrameData {
    /// Full text-grid snapshot per pane. Milestone 1 sends the
    /// entire grid every frame to keep the in-process protocol
    /// simple; Milestone 2 will switch to dirty-row deltas.
    pub grids: HashMap<PaneLayerId, TextGrid>,
    /// Kitty graphics commands emitted by panes this frame.
    pub graphics: Vec<(PaneLayerId, KittyGraphicCmd)>,
    /// Cursor position per pane.
    pub cursors: HashMap<PaneLayerId, (u16, u16)>,
}

/// Owned tab-bar metadata derived from the server's `TabStack`.
#[derive(Debug, Clone, Default)]
pub struct TabBarDataOwned {
    /// Per-tab labels (`None` for tabs without a label).
    pub labels: Vec<Option<String>>,
    /// Index of the currently-active tab.
    pub active_idx: usize,
    /// Total tab bar width in cells (terminal columns).
    pub bar_width_cells: u16,
}

/// Messages sent from the server to the frontend.
#[derive(Debug)]
pub enum ServerMessage {
    /// Full state snapshot sent on the first frame after attach.
    SyncFull {
        layout: ComputedLayout,
        grids: HashMap<PaneLayerId, TextGrid>,
        graphics: Vec<(PaneLayerId, KittyGraphicCmd)>,
        mode_flags: HostModeFlags,
        focus: usize,
        tabs: TabBarDataOwned,
        running: bool,
        /// Current keybind mode (Normal, PaneResize, Copy, etc.).
        mode: cmdash_keybinds::Mode,
        /// Active copy-mode cursor/selection state, if any.
        copy_mode: Option<CopyModeState>,
    },
    /// Per-tick incremental update.
    FrameIncremental {
        layout: ComputedLayout,
        frame: FrameData,
        mode_flags: HostModeFlags,
        focus: usize,
        tabs: TabBarDataOwned,
        running: bool,
        /// Current keybind mode (Normal, PaneResize, Copy, etc.).
        mode: cmdash_keybinds::Mode,
        /// Active copy-mode cursor/selection state, if any.
        copy_mode: Option<CopyModeState>,
    },
    /// Server has shut down; frontend should exit its loop.
    Quit,
}

/// Parsed config payload sent from the filesystem watcher
/// thread to the main tick loop via an mpsc channel.
#[derive(Debug)]
pub struct ConfigReload {
    pub keybinds: Vec<cmdash_config::Keybind>,
    pub presets: std::collections::BTreeMap<String, cmdash_config::LayoutNode>,
    pub layout_root: Option<cmdash_config::LayoutNode>,
    pub status_bar: Option<cmdash_config::Bar>,
    pub theme: Option<cmdash_config::Theme>,
}

/// Active copy-mode state. When `Some`, the user is selecting
/// text in the focused pane to copy to the system clipboard.
/// Coordinates are stored in visual (pane-local) cell space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyModeState {
    /// Visual x coordinate of the copy-mode cursor (0-based,
    /// relative to the pane's left edge).
    pub cursor_x: u16,
    /// Visual y coordinate of the copy-mode cursor (0-based,
    /// relative to the pane's top edge).
    pub cursor_y: u16,
    /// Anchor of the selection, if any. When `Some`, the
    /// selection spans from the anchor to the current cursor
    /// position (inclusive).
    pub selection_start: Option<(u16, u16)>,
}

/// Loaded widget library and its C-ABI create function.
/// The `Library` is kept alive so the function pointer remains valid.
pub struct LoadedWidget {
    pub _library: libloading::Library,
    pub create: unsafe extern "C" fn(u32) -> *mut std::ffi::c_void,
}

impl std::fmt::Debug for LoadedWidget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedWidget")
            .field("create", &(self.create as *const ()))
            .finish()
    }
}

/// Map of widget `ref_name` to loaded library + create function.
pub type WidgetFactories = std::collections::HashMap<String, LoadedWidget>;

/// Initial configuration passed from `cmdash::run` to the server
/// task at startup.
#[derive(Debug)]
pub struct ServerConfig {
    /// Resolved layout tree root.
    pub layout_root: cmdash_config::LayoutNode,
    /// Saved layout presets.
    pub presets: std::collections::BTreeMap<String, cmdash_config::LayoutNode>,
    /// Default shell spec for runtime-spawned panes.
    pub shell: cmdash_pty::ShellSpec,
    /// Optional status bar configuration.
    pub status_bar: Option<cmdash_config::Bar>,
    /// Active theme.
    pub theme: cmdash_config::Theme,
    /// Loaded widget factories.
    pub widget_factories: WidgetFactories,
}

/// State required by the frontend to render a single frame.
#[derive(Debug)]
pub struct RenderFrame {
    /// Resolved pane layout.
    pub layout: ComputedLayout,
    /// Full grid snapshots per pane.
    pub grids: HashMap<PaneLayerId, TextGrid>,
    /// Kitty graphics commands to apply.
    pub graphics: Vec<(PaneLayerId, KittyGraphicCmd)>,
    /// Host mode flags.
    pub mode_flags: HostModeFlags,
    /// Focused pane index.
    pub focus: usize,
    /// Tab bar metadata.
    pub tabs: TabBarDataOwned,
    /// Whether the session is still running.
    pub running: bool,
}

impl RenderFrame {
    /// Convenience constructor used by the server when emitting
    /// either `SyncFull` or `FrameIncremental`.
    pub fn new(
        layout: ComputedLayout,
        grids: HashMap<PaneLayerId, TextGrid>,
        graphics: Vec<(PaneLayerId, KittyGraphicCmd)>,
        mode_flags: HostModeFlags,
        focus: usize,
        tabs: TabBarDataOwned,
        running: bool,
    ) -> Self {
        Self {
            layout,
            grids,
            graphics,
            mode_flags,
            focus,
            tabs,
            running,
        }
    }
}
