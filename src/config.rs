use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{
    animation::{AnimationDirection, Easing, FillMode},
    api::ApiTransport,
    state::{OverlayId, WidgetId},
};

pub const CURRENT_CONFIG_VERSION: u32 = 1;
pub const LEGACY_CONFIG_VERSION: u32 = 0;
pub const CONFIG_SCHEMA: &str = "cmdash.workspace";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct AppConfig {
    pub version: u32,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub appearance: AppearanceConfig,
    #[serde(default)]
    pub animation: AnimationConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
    #[serde(default)]
    pub keybindings: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct AppearanceConfig {
    #[serde(default = "default_appearance_theme")]
    pub theme: String,
    #[serde(default)]
    pub colors: BTreeMap<String, String>,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: default_appearance_theme(),
            colors: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub struct AnimationConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub reduced_motion: bool,
    #[serde(default = "default_animation_duration_ms")]
    pub duration_ms: u64,
    #[serde(default)]
    pub delay_ms: u64,
    #[serde(default)]
    pub easing: Easing,
    #[serde(default)]
    pub repeat: u16,
    #[serde(default)]
    pub direction: AnimationDirection,
    #[serde(default)]
    pub fill: FillMode,
    #[serde(default = "default_animation_max_concurrent")]
    pub max_concurrent: usize,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reduced_motion: false,
            duration_ms: default_animation_duration_ms(),
            delay_ms: 0,
            easing: Easing::Linear,
            repeat: 0,
            direction: AnimationDirection::Normal,
            fill: FillMode::Forwards,
            max_concurrent: default_animation_max_concurrent(),
        }
    }
}

fn default_animation_duration_ms() -> u64 {
    180
}

fn default_animation_max_concurrent() -> usize {
    16
}

fn default_appearance_theme() -> String {
    "inherit".to_owned()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default)]
pub struct ApiConfig {
    pub enabled: bool,
    pub transport: ApiTransport,
    pub socket: String,
    pub read_only: bool,
    pub max_clients: usize,
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub event_queue_depth: usize,
    pub frame_history_depth: usize,
    pub expose_graphics: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            transport: ApiTransport::Unix,
            socket: "~/.cache/cmdash/cmdash.sock".to_owned(),
            read_only: true,
            max_clients: 4,
            max_request_bytes: 65_536,
            max_response_bytes: 1_048_576,
            event_queue_depth: 64,
            frame_history_depth: 4,
            expose_graphics: false,
        }
    }
}

impl ApiConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.socket.trim().is_empty() || self.socket.contains('\0') {
            return Err("api socket path must be non-empty and contain no NUL".to_owned());
        }
        if self.socket.len() > 100 {
            return Err("api socket path must be at most 100 bytes".to_owned());
        }
        if !self.socket.starts_with("~/") && !std::path::Path::new(&self.socket).is_absolute() {
            return Err("api socket path must be absolute or use ~/".to_owned());
        }
        if std::path::Path::new(&self.socket)
            .components()
            .any(|component| component == std::path::Component::ParentDir)
        {
            return Err("api socket path must not contain '..'".to_owned());
        }
        if self.max_clients == 0 || self.max_clients > 64 {
            return Err("api max_clients must be between 1 and 64".to_owned());
        }
        if !(1024..=1_048_576).contains(&self.max_request_bytes) {
            return Err("api max_request_bytes must be between 1024 and 1048576".to_owned());
        }
        if !(4096..=8_388_608).contains(&self.max_response_bytes) {
            return Err("api max_response_bytes must be between 4096 and 8388608".to_owned());
        }
        if self.event_queue_depth == 0 || self.event_queue_depth > 1024 {
            return Err("api event_queue_depth must be between 1 and 1024".to_owned());
        }
        if self.frame_history_depth > 64 {
            return Err("api frame_history_depth must be at most 64".to_owned());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub migrations: Vec<ConfigMigration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigMigration {
    AddedVersion,
    LegacyVersion { from: u32, to: u32 },
    WidgetTypeRewritten { kind: String, id: u64 },
}

impl ConfigMigration {
    pub fn warning(&self) -> String {
        match self {
            Self::AddedVersion => "configuration had no version; assumed version 1".to_owned(),
            Self::LegacyVersion { from, to } => {
                format!("configuration version {from} migrated to version {to}")
            }
            Self::WidgetTypeRewritten { kind, id } => {
                format!("widget {id} type {kind:?} migrated to \"widget\" (shell script)")
            }
        }
    }
}

/// The dashboard item types that were removed in Phase 17 and now migrate to
/// script-driven `widget` items.
pub const REMOVED_WIDGET_KINDS: &[&str] = &[
    "text",
    "clock",
    "system",
    "status",
    "key_value",
    "gauge",
    "list",
    "log",
    "sparkline",
    "separator",
    "spacer",
];

/// Single-quote escapes `value` so it can be embedded in a shell command.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Rewrites a removed data-widget type into an equivalent script-driven
/// `widget` in place, returning the migration record when a rewrite happened.
fn migrate_widget_config(widget: &mut WidgetInstanceConfig) -> Option<ConfigMigration> {
    if !REMOVED_WIDGET_KINDS.contains(&widget.kind.as_str()) {
        return None;
    }
    let old_kind = widget.kind.clone();
    let text = widget.text.as_deref().unwrap_or_default();
    let (command, mode, parse_tags) = match old_kind.as_str() {
        "text" => (format!("printf {}", sh_quote(text)), None, false),
        "clock" => {
            let spec = match widget.format.as_deref() {
                Some("HH:MM") => "+%H:%M",
                _ => "+%H:%M:%S",
            };
            (format!("date {}", sh_quote(spec)), Some("interval"), false)
        }
        "system" => ("uname -sm".to_owned(), Some("interval"), false),
        "status" => {
            let tag = match widget.settings.get("state").map(String::as_str) {
                Some("success" | "ok" | "healthy" | "up" | "green" | "passing") => "[ok]",
                Some("warning" | "warn" | "degraded" | "yellow") => "[warn]",
                Some("error" | "err" | "failed" | "failure" | "down" | "red" | "critical") => {
                    "[err]"
                }
                _ => "",
            };
            (
                format!("printf {}", sh_quote(&format!("{tag}{text}"))),
                None,
                true,
            )
        }
        "key_value" => {
            let key = widget.settings.get("key").cloned().unwrap_or_default();
            (
                format!("printf {}", sh_quote(&format!("{key}: {text}"))),
                None,
                false,
            )
        }
        "gauge" => {
            let value = widget
                .settings
                .get("value")
                .cloned()
                .unwrap_or_else(|| "0".to_owned());
            (
                format!("printf {}", sh_quote(&format!("{value}%"))),
                None,
                false,
            )
        }
        "list" => (
            format!("printf {}", sh_quote(&format!("{text}\n"))),
            None,
            false,
        ),
        "log" => (
            format!("printf {}", sh_quote(&format!("{text}\n"))),
            None,
            true,
        ),
        "sparkline" => {
            let values = widget
                .settings
                .get("values")
                .cloned()
                .unwrap_or_else(|| text.to_owned());
            (format!("printf {}", sh_quote(&values)), None, false)
        }
        "separator" => ("printf '────────'".to_owned(), None, false),
        "spacer" => ("printf ''".to_owned(), None, false),
        _ => return None,
    };
    widget.kind = "widget".to_owned();
    widget.command = Some(command);
    widget.text = None;
    widget.format = None;
    if let Some(mode) = mode {
        widget
            .settings
            .entry("mode".to_owned())
            .or_insert_with(|| mode.to_owned());
        widget
            .settings
            .entry("interval_ms".to_owned())
            .or_insert_with(|| "1000".to_owned());
    }
    if parse_tags {
        widget
            .settings
            .insert("parse_tags".to_owned(), "true".to_owned());
    }
    Some(ConfigMigration::WidgetTypeRewritten {
        kind: old_kind,
        id: widget.id,
    })
}

/// Rewrites a removed data-widget table in the raw TOML source so that
/// `--migrate-config` persists the same change the typed migration applies.
fn migrate_widget_value(widget: &mut toml::Table) {
    let Some(kind) = widget
        .get("type")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
    else {
        return;
    };
    if !REMOVED_WIDGET_KINDS.contains(&kind.as_str()) {
        return;
    }
    let text = widget
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let format = widget
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned();
    let settings: BTreeMap<String, String> = widget
        .get("settings")
        .and_then(|v| v.as_table())
        .map(|table| {
            table
                .iter()
                .filter_map(|(key, value)| value.as_str().map(|s| (key.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let mut migrated = WidgetInstanceConfig {
        id: widget.get("id").and_then(|v| v.as_integer()).unwrap_or(0) as u64,
        kind,
        title: widget
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        label: LabelPolicy::Auto,
        text: (!text.is_empty()).then_some(text),
        format: (!format.is_empty()).then_some(format),
        command: widget
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        settings,
    };
    if migrate_widget_config(&mut migrated).is_none() {
        return;
    }
    widget.insert("type".to_owned(), toml::Value::String(migrated.kind));
    widget.insert(
        "command".to_owned(),
        toml::Value::String(migrated.command.unwrap_or_default()),
    );
    widget.remove("text");
    widget.remove("format");
    let mut settings_table = toml::Table::new();
    for (key, value) in &migrated.settings {
        settings_table.insert(key.clone(), toml::Value::String(value.clone()));
    }
    widget.insert("settings".to_owned(), toml::Value::Table(settings_table));
}

impl AppConfig {
    pub fn parse(source: &str) -> Result<Self, ConfigError> {
        Self::parse_with_migrations(source).map(|(config, _)| config)
    }

    pub fn parse_with_migrations(
        source: &str,
    ) -> Result<(Self, Vec<ConfigMigration>), ConfigError> {
        #[derive(Deserialize)]
        struct RawAppConfig {
            #[serde(default)]
            version: Option<u32>,
            #[serde(default)]
            workspace: WorkspaceConfig,
            #[serde(default)]
            appearance: AppearanceConfig,
            #[serde(default)]
            animation: AnimationConfig,
            #[serde(default)]
            api: ApiConfig,
            #[serde(default)]
            plugins: Vec<PluginConfig>,
            #[serde(default)]
            keybindings: BTreeMap<String, String>,
        }

        let mut raw: RawAppConfig =
            toml::from_str(source).map_err(|error| ConfigError::Parse(error.to_string()))?;
        let (source_version, migration) = match raw.version {
            None => (CURRENT_CONFIG_VERSION, Some(ConfigMigration::AddedVersion)),
            Some(LEGACY_CONFIG_VERSION) => (
                CURRENT_CONFIG_VERSION,
                Some(ConfigMigration::LegacyVersion {
                    from: LEGACY_CONFIG_VERSION,
                    to: CURRENT_CONFIG_VERSION,
                }),
            ),
            Some(version) => (version, None),
        };
        if source_version != CURRENT_CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(source_version));
        }
        let mut migrations: Vec<ConfigMigration> = migration.into_iter().collect();
        for widget in &mut raw.workspace.widgets {
            if let Some(rewritten) = migrate_widget_config(widget) {
                migrations.push(rewritten);
            }
        }
        let config = Self {
            version: CURRENT_CONFIG_VERSION,
            workspace: raw.workspace,
            appearance: raw.appearance,
            animation: raw.animation,
            api: raw.api,
            plugins: raw.plugins,
            keybindings: raw.keybindings,
        };
        config.validate()?;
        Ok((config, migrations))
    }

    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, ConfigFileError> {
        Self::load_file_with_migrations(path).map(|loaded| loaded.config)
    }

    pub fn load_file_with_migrations(
        path: impl AsRef<Path>,
    ) -> Result<LoadedConfig, ConfigFileError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|error| ConfigFileError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let (config, migrations) =
            Self::parse_with_migrations(&source).map_err(ConfigFileError::Invalid)?;
        Ok(LoadedConfig { config, migrations })
    }

    pub fn migrate_source(source: &str) -> Result<(String, Vec<ConfigMigration>), ConfigError> {
        let (_, migrations) = Self::parse_with_migrations(source)?;
        if migrations.is_empty() {
            return Ok((source.to_owned(), migrations));
        }
        let mut value: toml::Value =
            toml::from_str(source).map_err(|error| ConfigError::Parse(error.to_string()))?;
        if let Some(table) = value.as_table_mut() {
            table.insert(
                "version".to_owned(),
                toml::Value::Integer(i64::from(CURRENT_CONFIG_VERSION)),
            );
            if let Some(workspace) = table.get_mut("workspace").and_then(|w| w.as_table_mut())
                && let Some(widgets) = workspace.get_mut("widgets").and_then(|w| w.as_array_mut())
            {
                for entry in widgets {
                    if let Some(widget) = entry.as_table_mut() {
                        migrate_widget_value(widget);
                    }
                }
            }
        }
        Ok((value.to_string(), migrations))
    }

    pub fn rewrite_file(path: impl AsRef<Path>) -> Result<Vec<ConfigMigration>, ConfigFileError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|error| ConfigFileError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let (rewritten, migrations) =
            Self::migrate_source(&source).map_err(ConfigFileError::Invalid)?;
        if migrations.is_empty() {
            return Ok(migrations);
        }
        let temporary = path.with_extension("toml.cmdash-migrate");
        fs::write(&temporary, rewritten).map_err(|error| ConfigFileError::Write {
            path: temporary.clone(),
            message: error.to_string(),
        })?;
        fs::rename(&temporary, path).map_err(|error| ConfigFileError::Write {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        Ok(migrations)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CURRENT_CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.version));
        }
        if self.workspace.name.trim().is_empty() {
            return Err(ConfigError::EmptyWorkspaceName);
        }
        crate::appearance::Theme::from_config(&self.appearance)
            .map_err(|error| ConfigError::InvalidAppearance(error.to_string()))?;
        if self.animation.duration_ms == 0 || self.animation.duration_ms > 60_000 {
            return Err(ConfigError::InvalidAnimation(
                "animation duration_ms must be between 1 and 60000".to_owned(),
            ));
        }
        if self.animation.delay_ms > 60_000 {
            return Err(ConfigError::InvalidAnimation(
                "animation delay_ms must be at most 60000".to_owned(),
            ));
        }
        if self.animation.max_concurrent == 0 || self.animation.max_concurrent > 128 {
            return Err(ConfigError::InvalidAnimation(
                "animation max_concurrent must be between 1 and 128".to_owned(),
            ));
        }
        self.api.validate().map_err(ConfigError::InvalidApi)?;
        crate::keymap::Keymap::from_overrides(&self.keybindings)
            .map_err(|error| ConfigError::InvalidKeybindings(error.to_string()))?;

        let mut ids = BTreeSet::new();
        for widget in &self.workspace.widgets {
            if widget.kind.trim().is_empty() {
                return Err(ConfigError::EmptyWidgetType);
            }
            let id = WidgetId::new(widget.id);
            if !ids.insert(id) {
                return Err(ConfigError::DuplicateWidgetId(id));
            }
            if widget.kind == "widget"
                && widget
                    .command
                    .as_deref()
                    .is_none_or(|command| command.trim().is_empty())
            {
                return Err(ConfigError::WidgetRequiresCommand(id));
            }
        }

        let mut overlay_ids = BTreeSet::new();
        for overlay in &self.workspace.overlays {
            let id = OverlayId::new(overlay.id);
            if !overlay_ids.insert(id) {
                return Err(ConfigError::DuplicateOverlayId(id));
            }
            if overlay.width == 0 || overlay.height == 0 {
                return Err(ConfigError::InvalidOverlayArea(id));
            }
        }
        if let Some(layout) = &self.workspace.layout {
            validate_layout(layout, &ids, &overlay_ids)?;
        }

        let mut plugin_names = BTreeSet::new();
        for plugin in &self.plugins {
            if plugin.name.trim().is_empty() || plugin.manifest.trim().is_empty() {
                return Err(ConfigError::InvalidPluginConfig);
            }
            if !plugin_names.insert(&plugin.name) {
                return Err(ConfigError::DuplicatePluginName(plugin.name.clone()));
            }
        }
        Ok(())
    }
}

fn validate_layout(
    layout: &LayoutConfig,
    widgets: &BTreeSet<WidgetId>,
    overlays: &BTreeSet<OverlayId>,
) -> Result<(), ConfigError> {
    match layout {
        LayoutConfig::Leaf { widget } => {
            let id = WidgetId::new(*widget);
            if !widgets.contains(&id) {
                return Err(ConfigError::LayoutWidgetNotFound(id));
            }
        }
        LayoutConfig::Columns { children }
        | LayoutConfig::Stack { children }
        | LayoutConfig::Split { children, .. } => {
            if children.is_empty() {
                return Err(ConfigError::EmptyLayoutChildren);
            }
            for child in children {
                validate_layout(child, widgets, overlays)?;
            }
        }
        LayoutConfig::Tabs { active, children } => {
            if children.is_empty() {
                return Err(ConfigError::EmptyLayoutChildren);
            }
            if *active >= children.len() {
                return Err(ConfigError::InvalidActiveTab(*active));
            }
            for child in children {
                validate_layout(child, widgets, overlays)?;
            }
        }
        LayoutConfig::Overlay { overlay } => {
            let id = OverlayId::new(*overlay);
            if !overlays.contains(&id) {
                return Err(ConfigError::LayoutOverlayNotFound(id));
            }
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfigFileError {
    #[error("could not read config {}: {message}", .path.display())]
    Read { path: PathBuf, message: String },
    #[error("could not rewrite config {}: {message}", .path.display())]
    Write { path: PathBuf, message: String },
    #[error("invalid config: {0}")]
    Invalid(ConfigError),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct PluginConfig {
    pub name: String,
    pub manifest: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceConfig {
    pub name: String,
    pub widgets: Vec<WidgetInstanceConfig>,
    pub layout: Option<LayoutConfig>,
    pub overlays: Vec<OverlayConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayoutConfig {
    Leaf {
        widget: u64,
    },
    Columns {
        children: Vec<LayoutConfig>,
    },
    Tabs {
        #[serde(default)]
        active: usize,
        children: Vec<LayoutConfig>,
    },
    Stack {
        children: Vec<LayoutConfig>,
    },
    Split {
        direction: SplitDirection,
        #[serde(default)]
        ratios: Vec<u16>,
        children: Vec<LayoutConfig>,
    },
    Overlay {
        overlay: u64,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct OverlayConfig {
    pub id: u64,
    #[serde(default)]
    pub x: u16,
    #[serde(default)]
    pub y: u16,
    #[serde(default = "default_overlay_width")]
    pub width: u16,
    #[serde(default = "default_overlay_height")]
    pub height: u16,
    #[serde(default)]
    pub z_index: i16,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

fn default_overlay_width() -> u16 {
    24
}

fn default_overlay_height() -> u16 {
    5
}

fn default_true() -> bool {
    true
}

impl<'de> Deserialize<'de> for WorkspaceConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawWorkspaceConfig {
            #[serde(default = "default_workspace_name")]
            name: String,
            #[serde(default)]
            widgets: Vec<WidgetInstanceConfig>,
            #[serde(default)]
            layout: Option<LayoutConfig>,
            #[serde(default)]
            overlays: Vec<OverlayConfig>,
        }

        let raw = RawWorkspaceConfig::deserialize(deserializer)?;
        Ok(Self {
            name: raw.name,
            widgets: raw.widgets,
            layout: raw.layout,
            overlays: raw.overlays,
        })
    }
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            name: default_workspace_name(),
            widgets: Vec::new(),
            layout: None,
            overlays: Vec::new(),
        }
    }
}

fn default_workspace_name() -> String {
    "default".to_owned()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct WidgetInstanceConfig {
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub label: LabelPolicy,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LabelPolicy {
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfigError {
    #[error("TOML parse error: {0}")]
    Parse(String),
    #[error("unsupported config version {0}; expected {current}", current = CURRENT_CONFIG_VERSION)]
    UnsupportedVersion(u32),
    #[error("workspace name cannot be empty")]
    EmptyWorkspaceName,
    #[error("widget type cannot be empty")]
    EmptyWidgetType,
    #[error("widget {} requires a command (the shell script to run)", .0.get())]
    WidgetRequiresCommand(WidgetId),
    #[error("duplicate widget id {}", .0.get())]
    DuplicateWidgetId(WidgetId),
    #[error("duplicate overlay id {}", .0.get())]
    DuplicateOverlayId(OverlayId),
    #[error("overlay {} has an empty area", .0.get())]
    InvalidOverlayArea(OverlayId),
    #[error("layout references missing widget {}", .0.get())]
    LayoutWidgetNotFound(WidgetId),
    #[error("layout references missing overlay {}", .0.get())]
    LayoutOverlayNotFound(OverlayId),
    #[error("layout nodes must have children")]
    EmptyLayoutChildren,
    #[error("active tab index {0} is out of range")]
    InvalidActiveTab(usize),
    #[error("plugin name and manifest path cannot be empty")]
    InvalidPluginConfig,
    #[error("invalid appearance: {0}")]
    InvalidAppearance(String),
    #[error("invalid animation: {0}")]
    InvalidAnimation(String),
    #[error("invalid api: {0}")]
    InvalidApi(String),
    #[error("invalid keybindings: {0}")]
    InvalidKeybindings(String),
    #[error("duplicate plugin name {0:?}")]
    DuplicatePluginName(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_versioned_workspace_with_widget_instances() {
        let config = AppConfig::parse(
            r#"
            version = 1

            [workspace]
            name = "monitor"

            [[workspace.widgets]]
            id = 7
            type = "text"
            title = "hello"
            label = "never"
            text = "world"
            "#,
        )
        .unwrap();

        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        assert_eq!(config.workspace.name, "monitor");
        assert_eq!(config.workspace.widgets[0].id, 7);
        // Phase 17: the removed `text` type migrates to a script `widget`.
        assert_eq!(config.workspace.widgets[0].kind, "widget");
        assert_eq!(
            config.workspace.widgets[0].command.as_deref(),
            Some("printf 'world'")
        );
        assert_eq!(config.workspace.widgets[0].label, LabelPolicy::Never);
    }

    #[test]
    fn parses_appearance_modes_and_color_overrides() {
        let config = AppConfig::parse(
            r##"
            version = 1
            [appearance]
            theme = "fallback"
            [appearance.colors]
            focus = "#facc15"
            muted = "ansi:8"
            "##,
        )
        .unwrap();

        assert_eq!(config.appearance.theme, "fallback");
        assert_eq!(config.appearance.colors["muted"], "ansi:8");
    }

    #[test]
    fn parses_bounded_animation_options_and_defaults_them_off() {
        let defaults = AppConfig::parse("version = 1").unwrap();
        assert!(!defaults.animation.enabled);

        let config = AppConfig::parse(
            r#"
            version = 1
            [animation]
            enabled = true
            reduced_motion = true
            duration_ms = 240
            delay_ms = 20
            easing = "easeinout"
            repeat = 2
            direction = "alternate"
            fill = "forwards"
            max_concurrent = 8
            "#,
        )
        .unwrap();
        assert_eq!(config.animation.duration_ms, 240);
        assert_eq!(config.animation.max_concurrent, 8);
        assert!(config.animation.reduced_motion);
    }

    #[test]
    fn rejects_unbounded_animation_options() {
        let error = AppConfig::parse("version = 1\n[animation]\nmax_concurrent = 129").unwrap_err();
        assert!(matches!(error, ConfigError::InvalidAnimation(_)));
    }

    #[test]
    fn validates_local_api_paths_and_limits() {
        let config =
            AppConfig::parse("version = 1\n[api]\nenabled = true\nsocket = \"relative.sock\"\n");
        assert!(matches!(config, Err(ConfigError::InvalidApi(_))));
        let config = AppConfig::parse(
            "version = 1\n[api]\nmax_request_bytes = 2048\nsocket = \"/tmp/cmdash.sock\"\n",
        )
        .unwrap();
        assert_eq!(config.api.max_request_bytes, 2048);
    }

    #[test]
    fn defaults_the_workspace_when_only_the_version_is_configured() {
        let config = AppConfig::parse("version = 1").unwrap();

        assert_eq!(config.workspace, WorkspaceConfig::default());
    }

    #[test]
    fn loads_a_user_config_file() {
        let path = std::env::temp_dir().join(format!(
            "cmdash-config-{}-{}.toml",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, "version = 1\n").unwrap();

        let config = AppConfig::load_file(&path).unwrap();

        fs::remove_file(path).unwrap();
        assert_eq!(config.workspace.name, "default");
    }

    #[test]
    fn reports_missing_user_config_files_clearly() {
        let path = std::env::temp_dir().join("cmdash-config-does-not-exist.toml");

        let error = AppConfig::load_file(&path).unwrap_err();

        assert!(matches!(error, ConfigFileError::Read { .. }));
        assert!(error.to_string().contains("could not read config"));
    }

    #[test]
    fn validates_layout_tabs_and_overlay_references() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "text"
            [[workspace.widgets]]
            id = 2
            type = "clock"
            [[workspace.overlays]]
            id = 9
            text = "notice"
            [workspace.layout]
            type = "stack"
            children = [
              { type = "tabs", active = 0, children = [
                { type = "leaf", widget = 1 },
                { type = "leaf", widget = 2 }
              ] },
              { type = "overlay", overlay = 9 }
            ]
            "#,
        )
        .unwrap();

        assert!(matches!(
            config.workspace.layout,
            Some(LayoutConfig::Stack { .. })
        ));
        assert_eq!(config.workspace.overlays[0].id, 9);
    }

    #[test]
    fn validates_plugin_names_and_widget_settings() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[plugins]]
            name = "example"
            manifest = "example.toml"
            [[workspace.widgets]]
            id = 1
            type = "external-text"
            [workspace.widgets.settings]
            theme = "dark"
            "#,
        )
        .unwrap();
        assert!(config.plugins[0].enabled);
        assert_eq!(config.workspace.widgets[0].settings["theme"], "dark");
    }

    #[test]
    fn migration_adds_missing_or_legacy_versions() {
        let (rewritten, missing) =
            AppConfig::migrate_source("[workspace]\nname = \"legacy\"\n").unwrap();
        assert_eq!(missing, [ConfigMigration::AddedVersion]);
        assert!(rewritten.contains("version = 1"));
        let (_, legacy) = AppConfig::parse_with_migrations("version = 0\n").unwrap();
        assert_eq!(legacy, [ConfigMigration::LegacyVersion { from: 0, to: 1 }]);
        assert!(missing[0].warning().contains("assumed"));
    }

    #[test]
    fn removed_widget_types_migrate_to_script_widgets() {
        let (config, migrations) = AppConfig::parse_with_migrations(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "text"
            text = "hello"
            [[workspace.widgets]]
            id = 2
            type = "clock"
            format = "HH:MM"
            "#,
        )
        .unwrap();
        assert_eq!(config.workspace.widgets[0].kind, "widget");
        assert_eq!(
            config.workspace.widgets[0].command.as_deref(),
            Some("printf 'hello'")
        );
        assert_eq!(config.workspace.widgets[1].kind, "widget");
        assert_eq!(
            config.workspace.widgets[1].command.as_deref(),
            Some("date '+%H:%M'")
        );
        assert_eq!(
            config.workspace.widgets[1]
                .settings
                .get("mode")
                .map(String::as_str),
            Some("interval")
        );
        assert_eq!(migrations.len(), 2);
        assert!(migrations[0].warning().contains("migrated"));
    }

    #[test]
    fn widget_type_requires_a_command() {
        let error = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "widget"
            "#,
        )
        .unwrap_err();
        assert!(matches!(error, ConfigError::WidgetRequiresCommand(_)));
    }

    #[test]
    fn migrate_source_rewrites_widget_types_in_the_toml() {
        let source = r#"
version = 1
[[workspace.widgets]]
id = 3
type = "text"
text = "world"
"#;
        let (rewritten, migrations) = AppConfig::migrate_source(source).unwrap();
        assert_eq!(migrations.len(), 1);
        assert!(rewritten.contains("type = \"widget\""));
        assert!(rewritten.contains("command = \"printf 'world'\""));
        assert!(!rewritten.contains("type = \"text\""));
    }

    #[test]
    fn checked_in_default_configuration_is_valid() {
        let config = AppConfig::parse(include_str!("../config/default.toml")).unwrap();

        assert_eq!(config.workspace.widgets.len(), 3);
        assert!(config.workspace.layout.is_some());
    }

    #[test]
    fn rewrite_file_updates_legacy_versions_atomically() {
        let path = std::env::temp_dir().join(format!(
            "cmdash-migrate-{}-{}.toml",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::write(&path, "[workspace]\nname = \"legacy\"\n").unwrap();

        let migrations = AppConfig::rewrite_file(&path).unwrap();
        let rewritten = fs::read_to_string(&path).unwrap();
        fs::remove_file(path).unwrap();

        assert_eq!(migrations, [ConfigMigration::AddedVersion]);
        assert!(rewritten.contains("version = 1"));
    }

    #[test]
    fn parses_and_defaults_keybindings() {
        let defaults = AppConfig::parse("version = 1").unwrap();
        assert!(defaults.keybindings.is_empty());

        let config = AppConfig::parse(
            r#"
            version = 1
            [keybindings]
            quit = "ctrl+q"
            focus_next = "ctrl+j"
            "#,
        )
        .unwrap();
        assert_eq!(config.keybindings["quit"], "ctrl+q");
        assert_eq!(config.keybindings["focus_next"], "ctrl+j");
    }

    #[test]
    fn rejects_unknown_conflicting_and_invalid_keybindings() {
        let unknown = AppConfig::parse("version = 1\n[keybindings]\nexplode = \"x\"\n");
        assert!(matches!(
            unknown.unwrap_err(),
            ConfigError::InvalidKeybindings(_)
        ));

        let conflict = AppConfig::parse("version = 1\n[keybindings]\nquit = \"tab\"\n");
        assert!(matches!(
            conflict.unwrap_err(),
            ConfigError::InvalidKeybindings(_)
        ));

        let invalid = AppConfig::parse("version = 1\n[keybindings]\nquit = \"not+a+key\"\n");
        assert!(matches!(
            invalid.unwrap_err(),
            ConfigError::InvalidKeybindings(_)
        ));
    }

    #[test]
    fn rejects_unsupported_versions_and_duplicate_widget_ids() {
        assert_eq!(
            AppConfig::parse("version = 2").unwrap_err(),
            ConfigError::UnsupportedVersion(2)
        );

        let duplicate = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "text"
            [[workspace.widgets]]
            id = 1
            type = "text"
            "#,
        );
        assert_eq!(
            duplicate.unwrap_err(),
            ConfigError::DuplicateWidgetId(WidgetId::new(1))
        );
    }
}
