pub mod backend;
pub mod command;
pub mod compositor;
pub mod config;
pub mod dashboard;
pub mod input;
pub mod scene;
pub mod state;
pub mod widget;

pub use backend::{Backend, BackendCapabilities, CrosstermBackend, OutputMetrics};
pub use command::{Command, CommandEffect, FocusCommand, OverlayCommand, SurfaceCommand};
pub use compositor::{CellSpan, Compositor, FrameDiff};
pub use config::{
    AppConfig, CURRENT_CONFIG_VERSION, ConfigError, ConfigFileError, WidgetInstanceConfig,
    WorkspaceConfig,
};
pub use input::command_for_key;
pub use scene::{Cell, CellStyle, CellWidth, Color, Scene};
pub use state::{
    AppState, AppStateConfigError, CommandError, FocusState, FocusTarget, Overlay, OverlayId,
    OverlayPrimitive, SessionId, Surface, SurfaceId, WidgetId, WorkspaceId, WorkspaceState,
};
pub use widget::{
    Widget, WidgetError, WidgetHealth, WidgetRegistry, WidgetRuntime, WidgetStatus, WidgetUpdate,
    WidgetUpdateReport,
};
