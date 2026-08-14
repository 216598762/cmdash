pub mod backend;
pub mod command;
pub mod compositor;
pub mod config;
pub mod dashboard;
pub mod input;
pub mod layout;
pub mod plugin;
pub mod scene;
pub mod session;
pub mod state;
pub mod widget;

pub use backend::{Backend, BackendCapabilities, CrosstermBackend, OutputMetrics};
pub use command::{
    Command, CommandEffect, FocusCommand, OverlayCommand, SurfaceCommand, TabCommand,
};
pub use compositor::{CellSpan, Compositor, FrameDiff};
pub use config::{
    AppConfig, CURRENT_CONFIG_VERSION, ConfigError, ConfigFileError, LayoutConfig, OverlayConfig,
    WidgetInstanceConfig, WorkspaceConfig,
};
pub use input::command_for_key;
pub use layout::{LayoutError, LayoutNode, LayoutTree};
pub use plugin::{
    ExternalTextPlugin, PLUGIN_ABI_VERSION, PluginDescriptorV1, PluginError, PluginHostV1,
    PluginModule, PluginRegistry,
};
pub use scene::{Cell, CellStyle, CellWidth, Color, Scene};
pub use session::{SessionError, TerminalSession, TerminalSize};
pub use state::{
    AppState, AppStateConfigError, CommandError, FocusState, FocusTarget, Overlay, OverlayId,
    OverlayPrimitive, SessionId, Surface, SurfaceId, WidgetId, WorkspaceId, WorkspaceState,
};
pub use widget::{
    Widget, WidgetError, WidgetHealth, WidgetRegistry, WidgetRuntime, WidgetStatus, WidgetUpdate,
    WidgetUpdateReport,
};
