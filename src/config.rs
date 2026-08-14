use std::{
    collections::BTreeSet,
    fmt, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::state::{OverlayId, WidgetId};

pub const CURRENT_CONFIG_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct AppConfig {
    pub version: u32,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
}

impl AppConfig {
    pub fn parse(source: &str) -> Result<Self, ConfigError> {
        let config: Self =
            toml::from_str(source).map_err(|error| ConfigError::Parse(error.to_string()))?;

        config.validate()?;
        Ok(config)
    }

    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, ConfigFileError> {
        let path = path.as_ref();
        let source = fs::read_to_string(path).map_err(|error| ConfigFileError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        Self::parse(&source).map_err(ConfigFileError::Invalid)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CURRENT_CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.version));
        }
        if self.workspace.name.trim().is_empty() {
            return Err(ConfigError::EmptyWorkspaceName);
        }

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
        LayoutConfig::Columns { children } | LayoutConfig::Stack { children } => {
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
            Self::Invalid(error) => write!(formatter, "invalid config: {error}"),
        }
    }
}

impl std::error::Error for ConfigFileError {}

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
    Overlay {
        overlay: u64,
    },
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
    pub text: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
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
            text = "world"
            "#,
        )
        .unwrap();

        assert_eq!(config.version, CURRENT_CONFIG_VERSION);
        assert_eq!(config.workspace.name, "monitor");
        assert_eq!(config.workspace.widgets[0].id, 7);
        assert_eq!(config.workspace.widgets[0].kind, "text");
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
