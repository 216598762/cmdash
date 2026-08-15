pub mod backend;
pub mod command;
pub mod compositor;
pub mod config;
pub mod crash;
pub mod dashboard;
pub mod graphics;
pub mod input;
pub mod layout;
pub mod notification;
pub mod plugin;
pub mod reload;
pub mod scene;
pub mod session;
#[cfg(feature = "sixel")]
pub mod sixel;
pub mod state;
#[cfg(feature = "wasm-plugins")]
pub mod wasm_plugin;
pub mod widget;

pub use backend::{Backend, BackendCapabilities, CrosstermBackend, OutputMetrics};
pub use command::{
    Command, CommandEffect, FocusCommand, FocusDirection, OverlayCommand, PaneCommand,
    SurfaceCommand, TabCommand,
};
pub use compositor::{CellSpan, Compositor, FrameDiff};
pub use config::{
    AppConfig, CONFIG_SCHEMA, CURRENT_CONFIG_VERSION, ConfigError, ConfigFileError,
    ConfigMigration, LayoutConfig, LoadedConfig, OverlayConfig, PluginConfig, SplitDirection,
    WidgetInstanceConfig, WorkspaceConfig,
};
pub use crash::CrashReport;
pub use graphics::{
    GraphicsDiagnostic, GraphicsError, GraphicsLimits, GraphicsPlacement, GraphicsResourceId,
    GraphicsSubmission, SessionGraphicsStore,
};
pub use input::command_for_key;
pub use layout::{LayoutError, LayoutNode, LayoutTree};
pub use notification::{copy_notification, extract_urls};
pub use plugin::{
    ExternalTextPlugin, PLUGIN_ABI_VERSION, PLUGIN_API_VERSION, PLUGIN_MANIFEST_VERSION,
    PluginDescriptorV1, PluginError, PluginHostV1, PluginManifestError, PluginManifestV1,
    PluginModule, PluginRegistry, PluginRuntime, PluginWidgetManifest,
};
pub use reload::{ConfigReloader, ReloadError};
pub use scene::{Cell, CellStyle, CellWidth, Color, Scene};
pub use session::{SessionError, TerminalSession, TerminalSize, kitty_stream_stats};
pub use state::{
    AppState, AppStateConfigError, CommandError, FocusState, FocusTarget, Overlay, OverlayId,
    OverlayPrimitive, SessionId, Surface, SurfaceId, WidgetId, WorkspaceId, WorkspaceState,
};
#[cfg(feature = "wasm-plugins")]
pub use wasm_plugin::{WasmLimits, WasmPluginError, WasmPluginHost, WasmPluginInstance};
pub use widget::{
    Widget, WidgetError, WidgetHealth, WidgetRegistry, WidgetRuntime, WidgetStatus, WidgetUpdate,
    WidgetUpdateReport,
};
