pub mod animation;
pub mod api;
pub mod appearance;
pub mod backend;
pub mod command;
pub mod compositor;
pub mod config;
pub mod crash;
pub mod dashboard;
pub mod graphics;
pub mod input;
pub mod keymap;
pub mod layout;
pub mod notification;
pub mod plugin;
pub mod reload;
pub mod scene;
pub mod script;
pub mod session;
pub mod session_events;
#[cfg(feature = "sixel")]
pub mod sixel;
pub mod state;
pub mod virtual_buffer;
#[cfg(feature = "wasm-plugins")]
pub mod wasm_plugin;
pub mod widget;

pub use animation::{
    AnimationDirection, AnimationFrame, AnimationKey, AnimationManager, AnimationSample,
    AnimationSettings, AnimationSpec, Easing, FillMode,
};
pub use api::{
    API_VERSION, ApiAction, ApiCapabilities, ApiCommand, ApiError, ApiExecution, ApiRequest,
    ApiResponse, ApiServer, ApiSnapshot, ApiTransport,
};
pub use appearance::{AppearanceError, Theme};
pub use backend::{
    Backend, BackendCapabilities, CrosstermBackend, GraphicsCapabilityConfidence,
    GraphicsCapabilityProbe, GraphicsCapabilityReport, GraphicsCapabilitySource,
    GraphicsOuterAcknowledgement, GraphicsProbeState, GraphicsSubmissionStatus, KittyGraphicsMode,
    OuterInputBatch, OutputMetrics, TerminalWindowSize,
};
pub use command::{
    Command, CommandEffect, FocusCommand, FocusDirection, OverlayCommand, PaneCommand,
    SurfaceCommand, TabCommand,
};
pub use compositor::{CellSpan, Compositor, FrameDiff};
pub use config::{
    AnimationConfig, ApiConfig, AppConfig, AppearanceConfig, CONFIG_SCHEMA, CURRENT_CONFIG_VERSION,
    ConfigError, ConfigFileError, ConfigMigration, LabelPolicy, LayoutConfig, LoadedConfig,
    OverlayConfig, PluginConfig, SplitDirection, WidgetInstanceConfig, WorkspaceConfig,
};
pub use crash::CrashReport;
pub use graphics::{
    GraphicsAnimationState, GraphicsDiagnostic, GraphicsError, GraphicsGridAnchor,
    GraphicsInputDemultiplexer, GraphicsLimits, GraphicsPlaceholderLayer, GraphicsPlacement,
    GraphicsProtocolAdapter, GraphicsProtocolBroker, GraphicsProtocolCommand,
    GraphicsProtocolError, GraphicsProtocolEvent, GraphicsResourceId, GraphicsResponse,
    GraphicsResponseDestination, GraphicsScreen, GraphicsScrollRegion, GraphicsSourceRect,
    GraphicsSubmission, OuterInputEvent, SessionGraphicsStore, kitty_error_response,
};
pub use input::{command_for_key, terminal_capture_command};
pub use keymap::{KeyAction, KeyChord, Keymap, KeymapError};
pub use layout::{LayoutError, LayoutNode, LayoutTree};
pub use notification::{copy_notification, extract_urls};
pub use plugin::{
    ExternalTextPlugin, PLUGIN_ABI_VERSION, PLUGIN_API_VERSION, PLUGIN_MANIFEST_VERSION,
    PluginDescriptorV1, PluginError, PluginHostV1, PluginManifestError, PluginManifestV1,
    PluginModule, PluginRegistry, PluginRuntime, PluginWidgetManifest,
};
pub use reload::{ConfigReloader, ReloadError};
pub use scene::{Cell, CellStyle, CellWidth, Color, Scene, SceneCursor, Underline};
pub use session::{
    SessionError, SessionWakeup, TerminalSession, TerminalSize, UiEvent, kitty_stream_stats,
    ui_event_channel,
};
pub use session_events::{
    SessionContextSnapshot, SessionEvent, SessionEventBus, SessionEventKind, SessionEventMode,
    SessionEventReceiver, format_session_event,
};
#[cfg(feature = "sixel")]
pub use sixel::{SixelError, SixelImage, SixelSubmission, encode_rgb};
pub use state::{
    AppState, AppStateConfigError, CommandError, FocusState, FocusTarget, Overlay, OverlayId,
    OverlayPrimitive, SessionId, Surface, SurfaceId, WidgetId, WorkspaceId, WorkspaceState,
};
pub use virtual_buffer::{
    GraphicsCommand, ImageIdentityRegistry, ImageObject, ImageObjectId, ImagePlacement,
    ImageResource, VirtualBuffer, VirtualRow,
};
#[cfg(feature = "wasm-plugins")]
pub use wasm_plugin::{WasmLimits, WasmPluginError, WasmPluginHost, WasmPluginInstance};
pub use widget::{
    CursorBlinkSettings, Widget, WidgetAppearance, WidgetBorderStyle, WidgetError, WidgetFactory,
    WidgetHealth, WidgetRegistry, WidgetRuntime, WidgetRuntimeContext, WidgetStatus, WidgetUpdate,
    WidgetUpdateReport, widget_content_area,
};
