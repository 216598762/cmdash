use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
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
}

impl ConfigMigration {
    pub fn warning(&self) -> String {
        match self {
            Self::AddedVersion => "configuration had no version; assumed version 1".to_owned(),
            Self::LegacyVersion { from, to } => {
                format!("configuration version {from} migrated to version {to}")
            }
        }
    }
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
        }

        let raw: RawAppConfig =
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
        let config = Self {
            version: CURRENT_CONFIG_VERSION,
            workspace: raw.workspace,
            appearance: raw.appearance,
            animation: raw.animation,
            api: raw.api,
            plugins: raw.plugins,
        };
        config.validate()?;
        Ok((config, migration.into_iter().collect()))
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

        let mut ids = BTreeSet::new();
        for widget in &self.workspace.widgets {
            if widget.kind.trim().is_empty() {
                return Err(ConfigError::EmptyWidgetType);
            }
            let id = WidgetId::new(widget.id);
            if !ids.insert(id) {
                return Err(ConfigError::DuplicateWidgetId(id));
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigFileError {
    Read { path: PathBuf, message: String },
    Write { path: PathBuf, message: String },
    Invalid(ConfigError),
}

impl fmt::Display for ConfigFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, message } => {
                write!(
                    formatter,
                    "could not read config {}: {message}",
                    path.display()
                )
            }
            Self::Write { path, message } => {
                write!(
                    formatter,
                    "could not rewrite config {}: {message}",
                    path.display()
                )
            }
            Self::Invalid(error) => write!(formatter, "invalid config: {error}"),
        }
    }
}

impl std::error::Error for ConfigFileError {}

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    Parse(String),
    UnsupportedVersion(u32),
    EmptyWorkspaceName,
    EmptyWidgetType,
    DuplicateWidgetId(WidgetId),
    DuplicateOverlayId(OverlayId),
    InvalidOverlayArea(OverlayId),
    LayoutWidgetNotFound(WidgetId),
    LayoutOverlayNotFound(OverlayId),
    EmptyLayoutChildren,
    InvalidActiveTab(usize),
    InvalidPluginConfig,
    InvalidAppearance(String),
    InvalidAnimation(String),
    InvalidApi(String),
    DuplicatePluginName(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(message) => write!(formatter, "TOML parse error: {message}"),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "unsupported config version {version}; expected {CURRENT_CONFIG_VERSION}"
                )
            }
            Self::EmptyWorkspaceName => formatter.write_str("workspace name cannot be empty"),
            Self::EmptyWidgetType => formatter.write_str("widget type cannot be empty"),
            Self::DuplicateWidgetId(id) => write!(formatter, "duplicate widget id {}", id.get()),
            Self::DuplicateOverlayId(id) => write!(formatter, "duplicate overlay id {}", id.get()),
            Self::InvalidOverlayArea(id) => {
                write!(formatter, "overlay {} has an empty area", id.get())
            }
            Self::LayoutWidgetNotFound(id) => {
                write!(formatter, "layout references missing widget {}", id.get())
            }
            Self::LayoutOverlayNotFound(id) => {
                write!(formatter, "layout references missing overlay {}", id.get())
            }
            Self::EmptyLayoutChildren => formatter.write_str("layout nodes must have children"),
            Self::InvalidActiveTab(index) => {
                write!(formatter, "active tab index {index} is out of range")
            }
            Self::InvalidPluginConfig => {
                formatter.write_str("plugin name and manifest path cannot be empty")
            }
            Self::InvalidAppearance(message) => write!(formatter, "invalid appearance: {message}"),
            Self::InvalidAnimation(message) => write!(formatter, "invalid animation: {message}"),
            Self::InvalidApi(message) => write!(formatter, "invalid api: {message}"),
            Self::DuplicatePluginName(name) => {
                write!(formatter, "duplicate plugin name {name:?}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

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
        assert_eq!(config.workspace.widgets[0].kind, "text");
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
