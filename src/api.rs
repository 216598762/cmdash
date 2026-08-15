use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    backend::{BackendCapabilities, OutputMetrics},
    command::{Command, FocusCommand, PaneCommand, TabCommand},
    config::{ApiConfig, SplitDirection},
    scene::{Cell, CellStyle, CellWidth, Color, Scene},
    session::UiEvent,
    state::{AppState, FocusTarget},
    widget::WidgetHealth,
};

pub const API_VERSION: u32 = 1;
const MAX_API_BATCH: usize = 32;
const MAX_REQUEST_ID_BYTES: usize = 64;
const MAX_PATH_BYTES: usize = 256;
const MAX_FRAME_CELLS: usize = 100_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ApiTransport {
    #[default]
    Unix,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApiRequest {
    pub version: u32,
    pub request_id: String,
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub body: Option<Value>,
}

impl ApiRequest {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.version != API_VERSION {
            return Err(ApiError::new(
                "unsupported_version",
                format!("expected API version {API_VERSION}, got {}", self.version),
            ));
        }
        if self.request_id.is_empty() || self.request_id.len() > MAX_REQUEST_ID_BYTES {
            return Err(ApiError::new(
                "invalid_request_id",
                "request_id must be between 1 and 64 bytes",
            ));
        }
        if self.path.is_empty() || self.path.len() > MAX_PATH_BYTES || !self.path.starts_with('/') {
            return Err(ApiError::new(
                "invalid_path",
                "path must be an absolute API path no longer than 256 bytes",
            ));
        }
        if self.path.contains("..") {
            return Err(ApiError::new(
                "invalid_path",
                "path traversal is not allowed",
            ));
        }
        if !matches!(
            self.method.to_ascii_uppercase().as_str(),
            "GET" | "POST" | "DELETE"
        ) {
            return Err(ApiError::new(
                "unsupported_method",
                "only GET, POST, and DELETE are supported",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApiResponse {
    pub version: u32,
    pub request_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl ApiResponse {
    pub fn success(request_id: impl Into<String>, result: Value) -> Self {
        Self {
            version: API_VERSION,
            request_id: request_id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(request_id: impl Into<String>, error: ApiError) -> Self {
        Self {
            version: API_VERSION,
            request_id: request_id.into(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ApiCapabilities {
    pub api_version: u32,
    pub transport: &'static str,
    pub read_only: bool,
    pub operations: Vec<&'static str>,
    pub graphics_metadata: bool,
}

impl ApiCapabilities {
    fn from_config(config: &ApiConfig) -> Self {
        let mut operations = vec![
            "health",
            "capabilities",
            "workspace",
            "surfaces",
            "widgets",
            "compositor.frame",
            "compositor.diff",
            "metrics",
            "diagnostics",
            "subscriptions",
        ];
        if !config.read_only {
            operations.extend(["commands", "reload"]);
        }
        Self {
            api_version: API_VERSION,
            transport: "unix",
            read_only: config.read_only,
            operations,
            graphics_metadata: config.expose_graphics,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ApiSnapshot {
    pub generation: u64,
    pub health: String,
    pub capabilities: BackendCapabilitiesDto,
    pub workspace: WorkspaceDto,
    pub surfaces: Vec<SurfaceDto>,
    pub widgets: Vec<WidgetDto>,
    pub diagnostics: Vec<String>,
    pub metrics: MetricsDto,
    pub frame: FrameDto,
}

impl ApiSnapshot {
    pub fn from_state(
        state: &AppState,
        scene: &Scene,
        metrics: OutputMetrics,
        generation: u64,
        expose_graphics: bool,
    ) -> Self {
        let focus = state.focus().target();
        let surfaces = state
            .workspace()
            .surfaces()
            .values()
            .map(|surface| SurfaceDto {
                id: surface.id().get(),
                widget: surface.widget().map(|id| id.get()),
                area: RectDto::from(surface.area()),
                visible: surface.visible(),
                z_index: surface.z_index(),
                focused: matches!(focus, Some(FocusTarget::Surface(id)) if id == surface.id()),
            })
            .collect();
        let widgets = state
            .widget_runtime()
            .statuses()
            .map(|status| WidgetDto {
                id: status.id().get(),
                kind: status.kind().to_owned(),
                health: health_string(status.health()),
            })
            .collect();
        Self {
            generation,
            health: if state.quit_requested() {
                "stopping".to_owned()
            } else {
                "ok".to_owned()
            },
            capabilities: BackendCapabilitiesDto::from(state.backend_capabilities()),
            workspace: WorkspaceDto {
                id: state.workspace().id().get(),
                name: state.workspace().name().to_owned(),
                focus: focus_dto(focus),
            },
            surfaces,
            widgets,
            diagnostics: state.diagnostics().to_vec(),
            metrics: MetricsDto::from(metrics),
            frame: FrameDto::from_scene(scene, expose_graphics),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceDto {
    pub id: u64,
    pub name: String,
    pub focus: Option<FocusDto>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FocusDto {
    pub kind: &'static str,
    pub id: u64,
}

fn focus_dto(focus: Option<FocusTarget>) -> Option<FocusDto> {
    focus.map(|target| match target {
        FocusTarget::Surface(id) => FocusDto {
            kind: "surface",
            id: id.get(),
        },
        FocusTarget::Overlay(id) => FocusDto {
            kind: "overlay",
            id: id.get(),
        },
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct SurfaceDto {
    pub id: u64,
    pub widget: Option<u64>,
    pub area: RectDto,
    pub visible: bool,
    pub z_index: i16,
    pub focused: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct WidgetDto {
    pub id: u64,
    pub kind: String,
    pub health: String,
}

fn health_string(health: &WidgetHealth) -> String {
    match health {
        WidgetHealth::Healthy => "healthy".to_owned(),
        WidgetHealth::Degraded(message) => format!("degraded: {message}"),
        WidgetHealth::Failed(message) => format!("failed: {message}"),
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RectDto {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl From<Rect> for RectDto {
    fn from(rect: Rect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct FrameDto {
    pub viewport: RectDto,
    pub cells: Vec<CellDto>,
    pub truncated: bool,
    pub graphics: Vec<GraphicsDto>,
}

impl FrameDto {
    fn from_scene(scene: &Scene, expose_graphics: bool) -> Self {
        let cells = scene
            .cells()
            .iter()
            .take(MAX_FRAME_CELLS)
            .map(CellDto::from)
            .collect::<Vec<_>>();
        let total_cells = scene.area().width as usize * scene.area().height as usize;
        let graphics = if expose_graphics {
            scene.image_layers().iter().map(GraphicsDto::from).collect()
        } else {
            Vec::new()
        };
        Self {
            viewport: RectDto::from(scene.area()),
            cells,
            truncated: total_cells > MAX_FRAME_CELLS,
            graphics,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CellDto {
    pub symbol: String,
    pub width: &'static str,
    pub style: StyleDto,
}

impl From<&Cell> for CellDto {
    fn from(cell: &Cell) -> Self {
        Self {
            symbol: cell.symbol.to_string(),
            width: match cell.width {
                CellWidth::Narrow => "narrow",
                CellWidth::Wide => "wide",
                CellWidth::Continuation => "continuation",
            },
            style: StyleDto::from(cell.style),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StyleDto {
    pub foreground: ColorDto,
    pub background: ColorDto,
    pub bold: bool,
    pub dim: bool,
}

impl From<CellStyle> for StyleDto {
    fn from(style: CellStyle) -> Self {
        Self {
            foreground: ColorDto::from(style.foreground),
            background: ColorDto::from(style.background),
            bold: style.bold,
            dim: style.dim,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", content = "value")]
pub enum ColorDto {
    Rgb([u8; 3]),
    Ansi(u8),
    Reset,
}

impl From<Color> for ColorDto {
    fn from(color: Color) -> Self {
        match color {
            Color::Rgb { red, green, blue } => Self::Rgb([red, green, blue]),
            Color::Ansi(index) => Self::Ansi(index),
            Color::Reset => Self::Reset,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct GraphicsDto {
    pub session_id: u64,
    pub image_id: u32,
    pub format: u8,
    pub area: RectDto,
    pub z_index: i16,
}

impl From<&crate::graphics::GraphicsSubmission> for GraphicsDto {
    fn from(graphics: &crate::graphics::GraphicsSubmission) -> Self {
        let placement = graphics.placement();
        Self {
            session_id: graphics.resource().session().get(),
            image_id: graphics.resource().image(),
            format: graphics.format(),
            area: RectDto::from(placement.area()),
            z_index: placement.z_index(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct BackendCapabilitiesDto {
    pub truecolor: bool,
    pub mouse: bool,
    pub bracketed_paste: bool,
    pub kitty_graphics: bool,
    pub sixel: bool,
}

impl From<BackendCapabilities> for BackendCapabilitiesDto {
    fn from(capabilities: BackendCapabilities) -> Self {
        Self {
            truecolor: capabilities.truecolor,
            mouse: capabilities.mouse,
            bracketed_paste: capabilities.bracketed_paste,
            kitty_graphics: capabilities.kitty_graphics,
            sixel: capabilities.sixel,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct MetricsDto {
    pub frames_submitted: u64,
    pub frames_skipped: u64,
    pub bytes_written: u64,
    pub optimized_diff_bytes: u64,
    pub naive_diff_bytes: u64,
    pub bytes_saved: u64,
}

impl From<OutputMetrics> for MetricsDto {
    fn from(metrics: OutputMetrics) -> Self {
        Self {
            frames_submitted: metrics.frames_submitted,
            frames_skipped: metrics.frames_skipped,
            bytes_written: metrics.bytes_written,
            optimized_diff_bytes: metrics.optimized_diff_bytes,
            naive_diff_bytes: metrics.naive_diff_bytes,
            bytes_saved: metrics.bytes_saved,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiCommand {
    RequestRedraw,
    FocusNext,
    FocusPrevious,
    FocusSurface { id: u64 },
    FocusClear,
    TabNext,
    TabPrevious,
    PaneGrow,
    PaneShrink,
    PaneClose,
    PaneMerge,
    PaneSplit { direction: SplitDirection },
}

impl ApiCommand {
    fn into_command(self) -> Command {
        match self {
            Self::RequestRedraw => Command::RequestRedraw,
            Self::FocusNext => Command::Focus(FocusCommand::Next),
            Self::FocusPrevious => Command::Focus(FocusCommand::Previous),
            Self::FocusSurface { id } => {
                Command::Focus(FocusCommand::Surface(crate::state::SurfaceId::new(id)))
            }
            Self::FocusClear => Command::Focus(FocusCommand::Clear),
            Self::TabNext => Command::Tab(TabCommand::Next),
            Self::TabPrevious => Command::Tab(TabCommand::Previous),
            Self::PaneGrow => Command::Pane(PaneCommand::Grow),
            Self::PaneShrink => Command::Pane(PaneCommand::Shrink),
            Self::PaneClose => Command::Pane(PaneCommand::Close),
            Self::PaneMerge => Command::Pane(PaneCommand::Merge),
            Self::PaneSplit { direction } => Command::Pane(PaneCommand::Split(direction)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiAction {
    Reload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiExecution {
    Respond,
    Action(ApiAction),
}

struct IncomingRequest {
    raw: String,
    responder: SyncSender<ApiResponse>,
}

struct Subscription {
    generation: u64,
    events: VecDeque<Value>,
}

pub struct ApiServer {
    config: ApiConfig,
    incoming: Receiver<IncomingRequest>,
    stop: Option<SyncSender<()>>,
    listener: Option<JoinHandle<()>>,
    snapshot: Option<ApiSnapshot>,
    history: VecDeque<ApiSnapshot>,
    subscriptions: BTreeMap<u64, Subscription>,
    next_subscription: u64,
}

impl ApiServer {
    pub fn start(config: &ApiConfig, ui_sender: Sender<UiEvent>) -> io::Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        config
            .validate()
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
        if !matches!(config.transport, ApiTransport::Unix) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "only Unix API transport is supported",
            ));
        }
        #[cfg(unix)]
        {
            let path = resolve_socket_path(&config.socket)?;
            prepare_socket_path(&path)?;
            let listener = std::os::unix::net::UnixListener::bind(&path)?;
            set_socket_permissions(&path)?;
            listener.set_nonblocking(true)?;
            let (incoming_sender, incoming) = mpsc::sync_channel(config.event_queue_depth);
            let (stop_sender, stop_receiver) = mpsc::sync_channel(1);
            let active = Arc::new(AtomicUsize::new(0));
            let thread_active = Arc::clone(&active);
            let thread_config = config.clone();
            let thread_path = path.clone();
            let listener = thread::spawn(move || {
                listener_loop(
                    listener,
                    incoming_sender,
                    stop_receiver,
                    ui_sender,
                    thread_config,
                    thread_active,
                );
                let _ = fs::remove_file(thread_path);
            });
            Ok(Some(Self {
                config: config.clone(),
                incoming,
                stop: Some(stop_sender),
                listener: Some(listener),
                snapshot: None,
                history: VecDeque::new(),
                subscriptions: BTreeMap::new(),
                next_subscription: 1,
            }))
        }
        #[cfg(not(unix))]
        {
            let _ = ui_sender;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Unix API transport is unavailable on this platform",
            ))
        }
    }

    pub fn expose_graphics(&self) -> bool {
        self.config.expose_graphics
    }

    pub fn publish_snapshot(&mut self, snapshot: ApiSnapshot) {
        let generation = snapshot.generation;
        if let Some(previous) = self.snapshot.replace(snapshot)
            && self.config.frame_history_depth > 0
        {
            self.history.push_back(previous);
            while self.history.len() > self.config.frame_history_depth {
                self.history.pop_front();
            }
        }
        for subscription in self.subscriptions.values_mut() {
            if subscription.generation != generation {
                subscription.generation = generation;
                subscription.events.push_back(json!({
                    "type": "frame",
                    "generation": generation,
                }));
                while subscription.events.len() > self.config.event_queue_depth {
                    subscription.events.pop_front();
                }
            }
        }
    }

    pub fn process_pending<F>(&mut self, state: &mut AppState, mut reload: F)
    where
        F: FnMut(&mut AppState) -> Result<(), String>,
    {
        for _ in 0..MAX_API_BATCH {
            let Ok(incoming) = self.incoming.try_recv() else {
                break;
            };
            let (request, parse_error) = match serde_json::from_str::<ApiRequest>(&incoming.raw) {
                Ok(request) => (Some(request), None),
                Err(error) => (
                    None,
                    Some(ApiError::new("malformed_request", error.to_string())),
                ),
            };
            let response = if let Some(error) = parse_error {
                ApiResponse::error("", error)
            } else {
                let request = request.expect("request exists when parsing succeeded");
                match request.validate() {
                    Err(error) => ApiResponse::error(request.request_id, error),
                    Ok(()) => self.handle_request(&request, state, &mut reload),
                }
            };
            let _ = incoming.responder.send(response);
        }
    }

    fn handle_request<F>(
        &mut self,
        request: &ApiRequest,
        state: &mut AppState,
        reload: &mut F,
    ) -> ApiResponse
    where
        F: FnMut(&mut AppState) -> Result<(), String>,
    {
        let method = request.method.to_ascii_uppercase();
        let (path, query) = split_query(&request.path);
        let Some(snapshot) = self.snapshot.clone() else {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("not_ready", "the first compositor snapshot is not ready"),
            );
        };
        match (method.as_str(), path) {
            ("GET", "/v1/health") => ApiResponse::success(
                request.request_id.clone(),
                json!({
                    "status": snapshot.health,
                    "generation": snapshot.generation,
                    "api_version": API_VERSION,
                }),
            ),
            ("GET", "/v1/capabilities") => ApiResponse::success(
                request.request_id.clone(),
                serde_json::to_value(ApiCapabilities::from_config(&self.config))
                    .expect("capabilities serialize"),
            ),
            ("GET", "/v1/workspace") => response_value(request, &snapshot.workspace),
            ("GET", "/v1/surfaces") => response_value(request, &snapshot.surfaces),
            ("GET", "/v1/widgets") => response_value(request, &snapshot.widgets),
            ("GET", "/v1/compositor/frame") => response_value(request, &snapshot.frame),
            ("GET", "/v1/compositor/diff") => self.diff_response(request, query),
            ("GET", "/v1/metrics") => response_value(request, &snapshot.metrics),
            ("GET", "/v1/diagnostics") => response_value(request, &snapshot.diagnostics),
            ("POST", "/v1/commands") => self.command_response(request, state),
            ("POST", "/v1/reload") => self.reload_response(request, state, reload),
            ("POST", "/v1/subscriptions") => self.subscribe_response(request, snapshot.generation),
            ("DELETE", _) if path.starts_with("/v1/subscriptions/") => {
                self.unsubscribe_response(request, path)
            }
            ("GET", _) if path.starts_with("/v1/subscriptions/") => {
                self.subscription_status_response(request, path, snapshot.generation)
            }
            _ => ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("not_found", "unsupported API endpoint"),
            ),
        }
    }

    fn diff_response(&self, request: &ApiRequest, query: Option<&str>) -> ApiResponse {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("not_ready", "the first compositor snapshot is not ready"),
            );
        };
        let from = query
            .and_then(|query| query_value(query, "from"))
            .and_then(|value| value.parse::<u64>().ok());
        if from == Some(snapshot.generation) {
            return ApiResponse::success(
                request.request_id.clone(),
                json!({"generation": snapshot.generation, "changes": []}),
            );
        }
        if from.is_some_and(|generation| {
            self.history
                .iter()
                .any(|history| history.generation == generation)
        }) {
            return ApiResponse::success(
                request.request_id.clone(),
                json!({"generation": snapshot.generation, "snapshot_required": true}),
            );
        }
        ApiResponse::error(
            request.request_id.clone(),
            ApiError::new(
                "snapshot_required",
                "requested frame generation is no longer available",
            ),
        )
    }

    fn command_response(&self, request: &ApiRequest, state: &mut AppState) -> ApiResponse {
        if self.config.read_only {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("read_only", "mutating API operations are disabled"),
            );
        }
        let Some(body) = request.body.clone() else {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("invalid_command", "commands require a JSON body"),
            );
        };
        let command: ApiCommand = match serde_json::from_value(body) {
            Ok(command) => command,
            Err(error) => {
                return ApiResponse::error(
                    request.request_id.clone(),
                    ApiError::new("invalid_command", error.to_string()),
                );
            }
        };
        match state.dispatch(command.into_command()) {
            Ok(effect) => ApiResponse::success(
                request.request_id.clone(),
                json!({
                    "effect": format!("{effect:?}").to_ascii_lowercase(),
                }),
            ),
            Err(error) => ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("command_rejected", format!("{error:?}")),
            ),
        }
    }

    fn reload_response<F>(
        &self,
        request: &ApiRequest,
        state: &mut AppState,
        reload: &mut F,
    ) -> ApiResponse
    where
        F: FnMut(&mut AppState) -> Result<(), String>,
    {
        if self.config.read_only {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("read_only", "mutating API operations are disabled"),
            );
        }
        match reload(state) {
            Ok(()) => ApiResponse::success(request.request_id.clone(), json!({"reloaded": true})),
            Err(error) => ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("reload_rejected", error),
            ),
        }
    }

    fn subscribe_response(&mut self, request: &ApiRequest, generation: u64) -> ApiResponse {
        if self.subscriptions.len() >= self.config.event_queue_depth {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("subscription_limit", "subscription limit reached"),
            );
        }
        let id = self.next_subscription;
        self.next_subscription = self.next_subscription.saturating_add(1);
        self.subscriptions.insert(
            id,
            Subscription {
                generation,
                events: VecDeque::new(),
            },
        );
        ApiResponse::success(
            request.request_id.clone(),
            json!({
                "id": id,
                "generation": generation,
                "poll": format!("/v1/subscriptions/{id}"),
            }),
        )
    }

    fn unsubscribe_response(&mut self, request: &ApiRequest, path: &str) -> ApiResponse {
        let Some(id) = path
            .rsplit('/')
            .next()
            .and_then(|id| id.parse::<u64>().ok())
        else {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("invalid_subscription", "subscription ID is invalid"),
            );
        };
        if self.subscriptions.remove(&id).is_none() {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("subscription_not_found", "subscription does not exist"),
            );
        }
        ApiResponse::success(request.request_id.clone(), json!({"removed": true}))
    }

    fn subscription_status_response(
        &mut self,
        request: &ApiRequest,
        path: &str,
        generation: u64,
    ) -> ApiResponse {
        let Some(id) = path
            .rsplit('/')
            .next()
            .and_then(|id| id.parse::<u64>().ok())
        else {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("invalid_subscription", "subscription ID is invalid"),
            );
        };
        let Some(subscription) = self.subscriptions.get_mut(&id) else {
            return ApiResponse::error(
                request.request_id.clone(),
                ApiError::new("subscription_not_found", "subscription does not exist"),
            );
        };
        subscription.generation = generation;
        let events = subscription.events.drain(..).collect::<Vec<_>>();
        ApiResponse::success(
            request.request_id.clone(),
            json!({
                "id": id,
                "generation": generation,
                "events": events,
            }),
        )
    }

    pub fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn response_value<T: Serialize>(request: &ApiRequest, value: &T) -> ApiResponse {
    ApiResponse::success(
        request.request_id.clone(),
        serde_json::to_value(value).expect("API DTO serialization cannot fail"),
    )
}

fn split_query(path: &str) -> (&str, Option<&str>) {
    path.split_once('?')
        .map_or((path, None), |(path, query)| (path, Some(query)))
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|part| part.split_once('='))
        .find_map(|(name, value)| (name == key).then_some(value))
}

#[cfg(unix)]
fn resolve_socket_path(socket: &str) -> io::Result<PathBuf> {
    if let Some(rest) = socket.strip_prefix("~/") {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "HOME is required for ~/ API socket paths",
            )
        })?;
        Ok(PathBuf::from(home).join(rest))
    } else {
        Ok(PathBuf::from(socket))
    }
}

#[cfg(unix)]
fn prepare_socket_path(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "API socket path exists and is not a Unix socket",
            ));
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn listener_loop(
    listener: std::os::unix::net::UnixListener,
    incoming: SyncSender<IncomingRequest>,
    stop: Receiver<()>,
    ui_sender: Sender<UiEvent>,
    config: ApiConfig,
    active: Arc<AtomicUsize>,
) {
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if active.fetch_add(1, Ordering::AcqRel) >= config.max_clients {
                    active.fetch_sub(1, Ordering::AcqRel);
                    let _ = write_direct_response(
                        stream,
                        &config,
                        ApiResponse::error(
                            "",
                            ApiError::new("client_limit", "maximum API clients reached"),
                        ),
                    );
                    continue;
                }
                let thread_incoming = incoming.clone();
                let thread_sender = ui_sender.clone();
                let thread_config = config.clone();
                let thread_active = Arc::clone(&active);
                thread::spawn(move || {
                    client_loop(stream, thread_incoming, thread_sender, thread_config);
                    thread_active.fetch_sub(1, Ordering::AcqRel);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(unix)]
fn client_loop(
    stream: std::os::unix::net::UnixStream,
    incoming: SyncSender<IncomingRequest>,
    ui_sender: Sender<UiEvent>,
    config: ApiConfig,
) {
    let mut reader = BufReader::new(stream);
    let mut raw = String::new();
    let read_result = reader
        .by_ref()
        .take(config.max_request_bytes as u64 + 1)
        .read_line(&mut raw);
    if read_result.is_err() || raw.len() > config.max_request_bytes {
        let _ = write_direct_response(
            reader.into_inner(),
            &config,
            ApiResponse::error(
                "",
                ApiError::new("request_too_large", "request exceeds limit"),
            ),
        );
        return;
    }
    let (responder, response_receiver) = mpsc::sync_channel(1);
    if incoming
        .try_send(IncomingRequest { raw, responder })
        .is_err()
    {
        let _ = write_direct_response(
            reader.into_inner(),
            &config,
            ApiResponse::error("", ApiError::new("queue_full", "API request queue is full")),
        );
        return;
    }
    let _ = ui_sender.send(UiEvent::ApiWakeup);
    let response = response_receiver
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|_| {
            ApiResponse::error("", ApiError::new("timeout", "API request timed out"))
        });
    let _ = write_direct_response(reader.into_inner(), &config, response);
}

#[cfg(unix)]
fn write_direct_response(
    mut stream: std::os::unix::net::UnixStream,
    config: &ApiConfig,
    response: ApiResponse,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&response).unwrap_or_else(|_| {
        br#"{"version":1,"request_id":"","ok":false,"error":{"code":"serialization","message":"response serialization failed"}}"#.to_vec()
    });
    if bytes.len() > config.max_response_bytes {
        bytes = serde_json::to_vec(&ApiResponse::error(
            response.request_id,
            ApiError::new("response_too_large", "response exceeds configured limit"),
        ))
        .expect("bounded error response serializes");
    }
    bytes.push(b'\n');
    stream.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppConfig, BackendCapabilities, CellStyle, Color, WidgetRegistry, config::ApiConfig,
    };

    fn request(method: &str, path: &str, body: Option<Value>) -> ApiRequest {
        ApiRequest {
            version: API_VERSION,
            request_id: "test".to_owned(),
            method: method.to_owned(),
            path: path.to_owned(),
            body,
        }
    }

    fn state() -> AppState {
        AppState::from_config(
            BackendCapabilities {
                truecolor: true,
                mouse: true,
                bracketed_paste: true,
                kitty_graphics: false,
                sixel: false,
            },
            &WidgetRegistry::builtins(),
            &AppConfig::parse(
                "version = 1\n[[workspace.widgets]]\nid = 1\ntype = \"text\"\ntext = \"hello\"\n",
            )
            .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn request_validation_rejects_unknown_versions_and_traversal() {
        let mut unknown = request("GET", "/v1/health", None);
        unknown.version = 2;
        assert_eq!(unknown.validate().unwrap_err().code, "unsupported_version");
        assert_eq!(
            request("GET", "/v1/../health", None)
                .validate()
                .unwrap_err()
                .code,
            "invalid_path"
        );
    }

    #[test]
    fn snapshot_contains_only_the_explicit_scene_and_state_contract() {
        let state = state();
        let mut scene = Scene::new(Rect::new(0, 0, 2, 1));
        scene.set(0, 0, 'x', CellStyle::new(Color::reset(), Color::reset()));
        let snapshot = ApiSnapshot::from_state(&state, &scene, OutputMetrics::default(), 7, false);
        assert_eq!(snapshot.generation, 7);
        assert_eq!(snapshot.frame.cells.len(), 2);
        assert_eq!(snapshot.widgets[0].kind, "text");
        assert!(snapshot.frame.graphics.is_empty());
    }

    #[test]
    fn command_payload_is_an_allowlisted_conversion() {
        let command: ApiCommand = serde_json::from_value(serde_json::json!({
            "type": "tab_next"
        }))
        .unwrap();
        assert!(matches!(
            command.into_command(),
            Command::Tab(TabCommand::Next)
        ));
        assert!(
            serde_json::from_value::<ApiCommand>(serde_json::json!({
                "type": "shell"
            }))
            .is_err()
        );
    }

    fn test_server(read_only: bool) -> (ApiServer, SyncSender<IncomingRequest>) {
        let config = ApiConfig {
            read_only,
            ..ApiConfig::default()
        };
        let (sender, incoming) = mpsc::sync_channel(8);
        (
            ApiServer {
                config,
                incoming,
                stop: None,
                listener: None,
                snapshot: None,
                history: VecDeque::new(),
                subscriptions: BTreeMap::new(),
                next_subscription: 1,
            },
            sender,
        )
    }

    fn publish_test_snapshot(server: &mut ApiServer, state: &AppState) {
        server.publish_snapshot(ApiSnapshot::from_state(
            state,
            &Scene::new(Rect::new(0, 0, 2, 1)),
            OutputMetrics::default(),
            1,
            false,
        ));
    }

    fn send_request(
        sender: &SyncSender<IncomingRequest>,
        request: &ApiRequest,
    ) -> mpsc::Receiver<ApiResponse> {
        let (responder, receiver) = mpsc::sync_channel(1);
        sender
            .send(IncomingRequest {
                raw: serde_json::to_string(request).unwrap(),
                responder,
            })
            .unwrap();
        receiver
    }

    #[test]
    fn endpoints_return_versioned_snapshots_and_reject_mutations_in_read_only_mode() {
        let mut server = test_server(true).0;
        let mut state = state();
        publish_test_snapshot(&mut server, &state);
        let (sender, incoming) = mpsc::sync_channel(8);
        server.incoming = incoming;
        let health = send_request(&sender, &request("GET", "/v1/health", None));
        let command = send_request(
            &sender,
            &request(
                "POST",
                "/v1/commands",
                Some(serde_json::json!({"type": "request_redraw"})),
            ),
        );
        server.process_pending(&mut state, |_| Ok(()));
        assert!(health.recv().unwrap().ok);
        assert_eq!(command.recv().unwrap().error.unwrap().code, "read_only");
    }

    #[test]
    fn writable_commands_use_existing_state_validation_and_diff_has_fallback() {
        let (mut server, sender) = test_server(false);
        let mut state = state();
        publish_test_snapshot(&mut server, &state);
        let command = send_request(
            &sender,
            &request(
                "POST",
                "/v1/commands",
                Some(serde_json::json!({"type": "focus_surface", "id": 999})),
            ),
        );
        let current_diff =
            send_request(&sender, &request("GET", "/v1/compositor/diff?from=1", None));
        server.process_pending(&mut state, |_| Ok(()));
        assert_eq!(
            command.recv().unwrap().error.unwrap().code,
            "command_rejected"
        );
        assert!(current_diff.recv().unwrap().ok);
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_round_trips_a_bounded_request() {
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::fs::PermissionsExt,
            thread,
        };

        let path = std::env::temp_dir().join(format!(
            "cmdash-api-{}-{}.sock",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let config = ApiConfig {
            enabled: true,
            socket: path.to_string_lossy().into_owned(),
            ..ApiConfig::default()
        };
        let (sender, _receiver, _wakeup) = crate::ui_event_channel();
        let mut server = ApiServer::start(&config, sender).unwrap().unwrap();
        let mut stream = None;
        for _ in 0..100 {
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(candidate) => {
                    stream = Some(candidate);
                    break;
                }
                Err(_) => thread::sleep(Duration::from_millis(5)),
            }
        }
        let mut stream = stream.expect("API socket did not become ready");
        let request = request("GET", "/v1/health", None);
        stream
            .write_all(format!("{}\n", serde_json::to_string(&request).unwrap()).as_bytes())
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        let mut state = state();
        publish_test_snapshot(&mut server, &state);
        server.process_pending(&mut state, |_| Ok(()));
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).unwrap();
        let response: ApiResponse = serde_json::from_str(&response).unwrap();
        assert!(response.ok);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        server.shutdown();
        assert!(!path.exists());
    }

    #[test]
    fn subscriptions_report_new_frame_generations_and_drain_events() {
        let (mut server, sender) = test_server(true);
        let mut state = state();
        publish_test_snapshot(&mut server, &state);
        let create = send_request(&sender, &request("POST", "/v1/subscriptions", None));
        server.process_pending(&mut state, |_| Ok(()));
        let created = create.recv().unwrap();
        let id = created.result.unwrap()["id"].as_u64().unwrap();
        let mut next_state = state;
        server.publish_snapshot(ApiSnapshot::from_state(
            &next_state,
            &Scene::new(Rect::new(0, 0, 2, 1)),
            OutputMetrics::default(),
            2,
            false,
        ));
        let poll = send_request(
            &sender,
            &request("GET", &format!("/v1/subscriptions/{id}"), None),
        );
        server.process_pending(&mut next_state, |_| Ok(()));
        let events = poll.recv().unwrap().result.unwrap()["events"]
            .as_array()
            .unwrap()
            .to_vec();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["generation"], 2);
    }

    #[test]
    fn api_defaults_are_local_and_read_only() {
        let config = ApiConfig::default();
        assert!(!config.enabled);
        assert!(config.read_only);
        assert_eq!(config.transport, ApiTransport::Unix);
        assert!(config.validate().is_ok());
    }
}
