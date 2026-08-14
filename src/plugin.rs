use std::collections::BTreeMap;

use ratatui::layout::Rect;

use crate::{
    config::WidgetInstanceConfig,
    scene::{CellStyle, Color, Scene},
    widget::{Widget, WidgetError},
};

pub const PLUGIN_ABI_VERSION: u32 = 1;
pub const PLUGIN_WIDGET_TYPE_MAX: usize = 32;

pub mod capabilities {
    pub const RENDER_SCENE: u64 = 1 << 0;
    pub const UPDATE: u64 = 1 << 1;
    pub const INPUT: u64 = 1 << 2;
    pub const OVERLAYS: u64 = 1 << 3;
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
                | capabilities::OVERLAYS,
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
    fn create(&self, config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, PluginError>;
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
    ) -> Result<Box<dyn Widget>, PluginError> {
        let plugin = self
            .modules
            .get(&config.kind)
            .ok_or_else(|| PluginError::UnknownWidgetType(config.kind.clone()))?;
        plugin.module.create(config)
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

    fn create(&self, config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, PluginError> {
        Ok(Box::new(ExternalTextWidget {
            title: config
                .title
                .clone()
                .unwrap_or_else(|| " external ".to_owned()),
            text: config
                .text
                .clone()
                .unwrap_or_else(|| "plugin widget".to_owned()),
        }))
    }
}

struct ExternalTextWidget {
    title: String,
    text: String,
}

impl Widget for ExternalTextWidget {
    fn kind(&self) -> &str {
        "external-text"
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = Color::rgb(38, 28, 58);
        let foreground = Color::rgb(245, 232, 255);
        let accent = if focused {
            Color::rgb(250, 204, 21)
        } else {
            Color::rgb(216, 180, 254)
        };
        let mut scene = Scene::new(area);
        scene.fill(area, CellStyle::new(foreground, background));
        scene.border(area, &self.title, CellStyle::new(accent, background));
        if area.height > 2 {
            scene.text(
                area.x.saturating_add(2),
                area.y.saturating_add(1),
                &self.text,
                CellStyle::new(foreground, background),
            );
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
            text: Some("loaded".to_owned()),
            format: None,
            command: None,
        }
    }

    #[test]
    fn host_loads_and_instantiates_the_minimal_external_widget() {
        let mut registry = PluginRegistry::new(PluginHostV1::dashboard_defaults());
        registry.load(ExternalTextPlugin).unwrap();
        let widget = registry.instantiate(&config()).unwrap();
        let scene = widget.render(Rect::new(0, 0, 20, 4), false);

        assert!(registry.contains("external-text"));
        assert_eq!(scene.cell_at(2, 1).unwrap().symbol, 'l');
        assert_eq!(registry.descriptors().count(), 1);
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
    fn incompatible_plugin_capabilities_are_rejected() {
        struct InputPlugin;
        impl PluginModule for InputPlugin {
            fn descriptor(&self) -> PluginDescriptorV1 {
                PluginDescriptorV1::new("input", capabilities::INPUT).unwrap()
            }

            fn create(
                &self,
                _config: &WidgetInstanceConfig,
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
