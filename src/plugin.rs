use std::collections::BTreeMap;

use ratatui::layout::Rect;
use serde::Deserialize;

use crate::{
    config::WidgetInstanceConfig,
    scene::{CellStyle, Scene},
    widget::{Widget, WidgetAppearance, WidgetError, WidgetRuntimeContext},
};

pub const PLUGIN_ABI_VERSION: u32 = 1;
pub const PLUGIN_API_VERSION: u32 = 1;
pub const PLUGIN_MANIFEST_VERSION: u32 = 1;
pub const PLUGIN_WIDGET_TYPE_MAX: usize = 32;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PluginRuntime {
    #[default]
    InProcess,
    Wasm,
}

fn default_manifest_version() -> u32 {
    PLUGIN_MANIFEST_VERSION
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct PluginManifestV1 {
    #[serde(default = "default_manifest_version")]
    pub manifest_version: u32,
    pub name: String,
    pub version: String,
    pub abi_version: u32,
    #[serde(default)]
    pub runtime: PluginRuntime,
    #[serde(default)]
    pub capabilities: u64,
    pub widgets: Vec<PluginWidgetManifest>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct PluginWidgetManifest {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub capabilities: u64,
}

impl PluginManifestV1 {
    pub fn parse(source: &str) -> Result<Self, PluginManifestError> {
        let manifest: Self = toml::from_str(source)
            .map_err(|error| PluginManifestError::Parse(error.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), PluginManifestError> {
        if self.manifest_version != PLUGIN_MANIFEST_VERSION {
            return Err(PluginManifestError::ManifestVersionMismatch {
                expected: PLUGIN_MANIFEST_VERSION,
                actual: self.manifest_version,
            });
        }
        if self.name.trim().is_empty() || self.version.trim().is_empty() {
            return Err(PluginManifestError::MissingIdentity);
        }
        if self.abi_version != PLUGIN_ABI_VERSION {
            return Err(PluginManifestError::AbiMismatch {
                expected: PLUGIN_ABI_VERSION,
                actual: self.abi_version,
            });
        }
        if self.widgets.is_empty() {
            return Err(PluginManifestError::NoWidgets);
        }
        let mut kinds = std::collections::BTreeSet::new();
        for widget in &self.widgets {
            if widget.kind.is_empty() || widget.kind.len() > PLUGIN_WIDGET_TYPE_MAX {
                return Err(PluginManifestError::InvalidWidgetType(widget.kind.clone()));
            }
            if !kinds.insert(&widget.kind) {
                return Err(PluginManifestError::DuplicateWidgetType(
                    widget.kind.clone(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginManifestError {
    Parse(String),
    MissingIdentity,
    ManifestVersionMismatch { expected: u32, actual: u32 },
    AbiMismatch { expected: u32, actual: u32 },
    NoWidgets,
    InvalidWidgetType(String),
    DuplicateWidgetType(String),
}

impl std::fmt::Display for PluginManifestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(message) => write!(formatter, "plugin manifest parse error: {message}"),
            Self::MissingIdentity => formatter.write_str("plugin manifest needs name and version"),
            Self::ManifestVersionMismatch { expected, actual } => {
                write!(
                    formatter,
                    "plugin manifest version mismatch: expected {expected}, got {actual}"
                )
            }
            Self::AbiMismatch { expected, actual } => {
                write!(
                    formatter,
                    "plugin manifest ABI mismatch: expected {expected}, got {actual}"
                )
            }
            Self::NoWidgets => formatter.write_str("plugin manifest declares no widgets"),
            Self::InvalidWidgetType(kind) => {
                write!(formatter, "invalid plugin widget type {kind:?}")
            }
            Self::DuplicateWidgetType(kind) => {
                write!(formatter, "duplicate plugin widget type {kind:?}")
            }
        }
    }
}

impl std::error::Error for PluginManifestError {}

pub mod capabilities {
    pub const RENDER_SCENE: u64 = 1 << 0;
    pub const UPDATE: u64 = 1 << 1;
    pub const INPUT: u64 = 1 << 2;
    pub const OVERLAYS: u64 = 1 << 3;
    /// Request access to host-owned bounded animation progress.
    pub const ANIMATION: u64 = 1 << 4;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginHostV1 {
    pub abi_version: u32,
    pub capability_bits: u64,
}

impl PluginHostV1 {
    pub const fn dashboard_defaults() -> Self {
        Self {
            abi_version: PLUGIN_ABI_VERSION,
            capability_bits: capabilities::RENDER_SCENE
                | capabilities::UPDATE
                | capabilities::INPUT
                | capabilities::OVERLAYS
                | capabilities::ANIMATION,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginDescriptorV1 {
    pub abi_version: u32,
    pub required_host_abi: u32,
    pub capability_bits: u64,
    pub widget_type: [u8; PLUGIN_WIDGET_TYPE_MAX],
    pub widget_type_len: u8,
}

impl PluginDescriptorV1 {
    pub fn new(widget_type: &str, capability_bits: u64) -> Result<Self, PluginError> {
        if widget_type.is_empty() || widget_type.len() > PLUGIN_WIDGET_TYPE_MAX {
            return Err(PluginError::InvalidWidgetType(widget_type.to_owned()));
        }
        let mut name = [0; PLUGIN_WIDGET_TYPE_MAX];
        name[..widget_type.len()].copy_from_slice(widget_type.as_bytes());
        Ok(Self {
            abi_version: PLUGIN_ABI_VERSION,
            required_host_abi: PLUGIN_ABI_VERSION,
            capability_bits,
            widget_type: name,
            widget_type_len: widget_type.len() as u8,
        })
    }

    pub fn widget_type(&self) -> Result<&str, PluginError> {
        if self.widget_type_len as usize > PLUGIN_WIDGET_TYPE_MAX {
            return Err(PluginError::InvalidDescriptor);
        }
        std::str::from_utf8(&self.widget_type[..self.widget_type_len as usize])
            .map_err(|_| PluginError::InvalidDescriptor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginError {
    AbiMismatch { expected: u32, actual: u32 },
    UnsupportedHostAbi(u32),
    UnsupportedCapabilities { requested: u64, available: u64 },
    InvalidWidgetType(String),
    InvalidDescriptor,
    DuplicateWidgetType(String),
    UnknownWidgetType(String),
    Widget(WidgetError),
}

pub trait PluginModule: Send + Sync {
    fn descriptor(&self) -> PluginDescriptorV1;
    fn create(
        &self,
        config: &WidgetInstanceConfig,
        context: &WidgetRuntimeContext,
    ) -> Result<Box<dyn Widget>, PluginError>;
}

struct LoadedPlugin {
    module: Box<dyn PluginModule>,
    descriptor: PluginDescriptorV1,
}

pub struct PluginRegistry {
    host: PluginHostV1,
    modules: BTreeMap<String, LoadedPlugin>,
}

impl PluginRegistry {
    pub fn new(host: PluginHostV1) -> Self {
        Self {
            host,
            modules: BTreeMap::new(),
        }
    }

    pub fn host(&self) -> PluginHostV1 {
        self.host
    }

    pub fn validate_manifest(
        &self,
        manifest: &PluginManifestV1,
    ) -> Result<(), PluginManifestError> {
        manifest.validate()?;
        if manifest.capabilities & !self.host.capability_bits != 0 {
            return Err(PluginManifestError::InvalidWidgetType(format!(
                "manifest capabilities {:#x} exceed host capabilities {:#x}",
                manifest.capabilities, self.host.capability_bits
            )));
        }
        for widget in &manifest.widgets {
            if widget.capabilities & !self.host.capability_bits != 0 {
                return Err(PluginManifestError::InvalidWidgetType(format!(
                    "widget {} requests unsupported capabilities",
                    widget.kind
                )));
            }
        }
        Ok(())
    }

    pub fn load<M>(&mut self, module: M) -> Result<(), PluginError>
    where
        M: PluginModule + 'static,
    {
        let descriptor = module.descriptor();
        if descriptor.abi_version != PLUGIN_ABI_VERSION {
            return Err(PluginError::AbiMismatch {
                expected: PLUGIN_ABI_VERSION,
                actual: descriptor.abi_version,
            });
        }
        if descriptor.required_host_abi > self.host.abi_version {
            return Err(PluginError::UnsupportedHostAbi(
                descriptor.required_host_abi,
            ));
        }
        if descriptor.capability_bits & !self.host.capability_bits != 0 {
            return Err(PluginError::UnsupportedCapabilities {
                requested: descriptor.capability_bits,
                available: self.host.capability_bits,
            });
        }
        let kind = descriptor.widget_type()?.to_owned();
        if self.modules.contains_key(&kind) {
            return Err(PluginError::DuplicateWidgetType(kind));
        }
        self.modules.insert(
            kind,
            LoadedPlugin {
                module: Box::new(module),
                descriptor,
            },
        );
        Ok(())
    }

    pub fn contains(&self, kind: &str) -> bool {
        self.modules.contains_key(kind)
    }

    pub fn descriptors(&self) -> impl Iterator<Item = PluginDescriptorV1> + '_ {
        self.modules.values().map(|plugin| plugin.descriptor)
    }

    pub fn instantiate(
        &self,
        config: &WidgetInstanceConfig,
        context: &WidgetRuntimeContext,
    ) -> Result<Box<dyn Widget>, PluginError> {
        let plugin = self
            .modules
            .get(&config.kind)
            .ok_or_else(|| PluginError::UnknownWidgetType(config.kind.clone()))?;
        plugin.module.create(config, context)
    }
}

pub fn widget_error(error: PluginError) -> WidgetError {
    WidgetError::Plugin(error.to_string())
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AbiMismatch { expected, actual } => {
                write!(
                    formatter,
                    "plugin ABI mismatch: expected {expected}, got {actual}"
                )
            }
            Self::UnsupportedHostAbi(version) => {
                write!(formatter, "plugin requires unsupported host ABI {version}")
            }
            Self::UnsupportedCapabilities {
                requested,
                available,
            } => write!(
                formatter,
                "plugin capabilities {requested:#x} exceed host capabilities {available:#x}"
            ),
            Self::InvalidWidgetType(kind) => {
                write!(formatter, "invalid plugin widget type {kind:?}")
            }
            Self::InvalidDescriptor => formatter.write_str("invalid plugin descriptor"),
            Self::DuplicateWidgetType(kind) => {
                write!(formatter, "duplicate plugin widget type {kind:?}")
            }
            Self::UnknownWidgetType(kind) => {
                write!(formatter, "unknown plugin widget type {kind:?}")
            }
            Self::Widget(error) => write!(formatter, "plugin widget failed: {error}"),
        }
    }
}

impl std::error::Error for PluginError {}

pub struct ExternalTextPlugin;

impl PluginModule for ExternalTextPlugin {
    fn descriptor(&self) -> PluginDescriptorV1 {
        PluginDescriptorV1::new("external-text", capabilities::RENDER_SCENE)
            .expect("fixture descriptor is valid")
    }

    fn create(
        &self,
        config: &WidgetInstanceConfig,
        context: &WidgetRuntimeContext,
    ) -> Result<Box<dyn Widget>, PluginError> {
        Ok(Box::new(ExternalTextWidget {
            title: config
                .title
                .clone()
                .unwrap_or_else(|| " external ".to_owned()),
            label: config.label != crate::config::LabelPolicy::Never,
            text: config
                .text
                .clone()
                .unwrap_or_else(|| "plugin widget".to_owned()),
            appearance: WidgetAppearance::from_settings(&config.settings)
                .map_err(PluginError::Widget)?,
            theme: context
                .theme()
                .with_settings(&config.settings)
                .map_err(|error| {
                    PluginError::Widget(WidgetError::InvalidConfiguration(error.to_string()))
                })?,
        }))
    }
}

struct ExternalTextWidget {
    title: String,
    label: bool,
    text: String,
    appearance: WidgetAppearance,
    theme: crate::Theme,
}

impl Widget for ExternalTextWidget {
    fn kind(&self) -> &str {
        "external-text"
    }

    fn content_area(&self, area: Rect) -> Rect {
        self.appearance.content_area(area)
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = self.theme.overlay_background();
        let foreground = self.theme.overlay_foreground();
        let accent = if focused {
            self.theme.focus()
        } else {
            self.theme.border()
        };
        let mut scene = Scene::new(area);
        scene.fill(area, CellStyle::new(foreground, background));
        self.appearance.render_border(
            &mut scene,
            area,
            if self.label { &self.title } else { "" },
            CellStyle::new(accent, background),
        );
        let content_area = self.appearance.content_area(area);
        if content_area.width > 0 && content_area.height > 0 {
            let mut content = Scene::new(content_area);
            content.fill(content_area, CellStyle::new(foreground, background));
            content.text(
                content_area.x.saturating_add(1),
                content_area.y,
                &self.text,
                CellStyle::new(foreground, background),
            );
            scene.blit(&content, area);
        }
        scene
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppConfig, WidgetId, WidgetRegistry, WidgetRuntime};

    fn config() -> WidgetInstanceConfig {
        WidgetInstanceConfig {
            id: 7,
            kind: "external-text".to_owned(),
            title: Some(" plugin ".to_owned()),
            label: crate::config::LabelPolicy::Auto,
            text: Some("loaded".to_owned()),
            format: None,
            command: None,
            settings: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn host_loads_and_instantiates_the_minimal_external_widget() {
        let mut registry = PluginRegistry::new(PluginHostV1::dashboard_defaults());
        registry.load(ExternalTextPlugin).unwrap();
        let widget = registry
            .instantiate(&config(), &WidgetRuntimeContext::new())
            .unwrap();
        let scene = widget.render(Rect::new(0, 0, 20, 4), false);

        assert!(registry.contains("external-text"));
        assert_eq!(scene.cell_at(2, 1).unwrap().symbol, 'l');
        assert_eq!(registry.descriptors().count(), 1);
    }

    #[test]
    fn external_widget_text_stays_inside_its_outline() {
        let mut long_config = config();
        long_config.text = Some("012345678901234567890123".to_owned());
        let mut registry = PluginRegistry::new(PluginHostV1::dashboard_defaults());
        registry.load(ExternalTextPlugin).unwrap();
        let widget = registry
            .instantiate(&long_config, &WidgetRuntimeContext::new())
            .unwrap();
        let scene = widget.render(Rect::new(0, 0, 20, 4), false);

        assert_eq!(scene.cell_at(19, 1).unwrap().symbol, '│');
    }

    #[test]
    fn external_widget_uses_configured_content_appearance() {
        let mut styled_config = config();
        styled_config
            .settings
            .insert("padding".to_owned(), "2".to_owned());
        styled_config
            .settings
            .insert("border".to_owned(), "ascii".to_owned());
        let mut registry = PluginRegistry::new(PluginHostV1::dashboard_defaults());
        registry.load(ExternalTextPlugin).unwrap();
        let widget = registry
            .instantiate(&styled_config, &WidgetRuntimeContext::new())
            .unwrap();
        let area = Rect::new(0, 0, 16, 8);

        assert_eq!(widget.content_area(area), Rect::new(3, 3, 10, 2));
        let scene = widget.render(area, false);
        assert_eq!(scene.cell_at(0, 0).unwrap().symbol, '+');
        assert_eq!(scene.cell_at(15, 1).unwrap().symbol, '|');
    }

    #[test]
    fn external_widget_can_be_instantiated_by_the_widget_runtime() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 7
            type = "external-text"
            text = "runtime plugin"
            "#,
        )
        .unwrap();
        let mut plugins = PluginRegistry::new(PluginHostV1::dashboard_defaults());
        plugins.load(ExternalTextPlugin).unwrap();
        let runtime = WidgetRuntime::from_config_with_plugins(
            &WidgetRegistry::builtins(),
            Some(&plugins),
            &config,
        )
        .unwrap();
        let scenes = runtime.render(
            &BTreeMap::from([(WidgetId::new(7), Rect::new(0, 0, 24, 4))]),
            None,
        );

        assert_eq!(scenes[&WidgetId::new(7)].cell_at(2, 1).unwrap().symbol, 'r');
    }

    #[test]
    fn manifests_validate_abi_identity_and_capabilities() {
        let manifest = PluginManifestV1::parse(
            r#"
            manifest_version = 1
            name = "example"
            version = "1.0"
            abi_version = 1
            capabilities = 1
            [[widgets]]
            type = "example-widget"
            capabilities = 1
            "#,
        )
        .unwrap();
        let registry = PluginRegistry::new(PluginHostV1::dashboard_defaults());
        registry.validate_manifest(&manifest).unwrap();
        assert_eq!(manifest.widgets[0].kind, "example-widget");
    }

    #[test]
    fn malformed_plugin_manifests_are_rejected_before_loading_code() {
        assert!(matches!(
            PluginManifestV1::parse("name = \"missing\""),
            Err(PluginManifestError::Parse(_))
        ));
    }

    #[test]
    fn manifest_versions_are_rejected_before_code_loading() {
        let error = PluginManifestV1::parse(
            r#"
            manifest_version = 2
            name = "example"
            version = "1.0"
            abi_version = 1
            [[widgets]]
            type = "example-widget"
            "#,
        )
        .unwrap_err();
        assert_eq!(
            error,
            PluginManifestError::ManifestVersionMismatch {
                expected: PLUGIN_MANIFEST_VERSION,
                actual: 2,
            }
        );
    }

    #[test]
    fn incompatible_plugin_capabilities_are_rejected() {
        struct InputPlugin;
        impl PluginModule for InputPlugin {
            fn descriptor(&self) -> PluginDescriptorV1 {
                PluginDescriptorV1::new("input", capabilities::INPUT).unwrap()
            }

            fn create(
                &self,
                _config: &WidgetInstanceConfig,
                _context: &WidgetRuntimeContext,
            ) -> Result<Box<dyn Widget>, PluginError> {
                unreachable!()
            }
        }

        let mut registry = PluginRegistry::new(PluginHostV1 {
            abi_version: PLUGIN_ABI_VERSION,
            capability_bits: capabilities::RENDER_SCENE,
        });
        assert!(matches!(
            registry.load(InputPlugin),
            Err(PluginError::UnsupportedCapabilities { .. })
        ));
    }
}
