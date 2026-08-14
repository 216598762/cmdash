use std::collections::BTreeMap;

use ratatui::layout::Rect;

use crate::{
    config::{AppConfig, WidgetInstanceConfig},
    scene::{CellStyle, Color, Scene},
    state::WidgetId,
};

pub trait Widget: Send {
    fn kind(&self) -> &str;
    fn render(&self, area: Rect, focused: bool) -> Scene;
}

pub type WidgetFactory = fn(&WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WidgetError {
    DuplicateWidgetType(String),
    UnknownWidgetType(String),
    DuplicateWidgetId(WidgetId),
}

#[derive(Default)]
pub struct WidgetRegistry {
    factories: BTreeMap<String, WidgetFactory>,
}

impl WidgetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn builtins() -> Self {
        let mut registry = Self::new();
        registry
            .register("text", text_widget_factory)
            .expect("built-in widget types are unique");
        registry
    }

    pub fn register(
        &mut self,
        kind: impl Into<String>,
        factory: WidgetFactory,
    ) -> Result<(), WidgetError> {
        let kind = kind.into();
        if self.factories.contains_key(&kind) {
            return Err(WidgetError::DuplicateWidgetType(kind));
        }
        self.factories.insert(kind, factory);
        Ok(())
    }

    pub fn contains(&self, kind: &str) -> bool {
        self.factories.contains_key(kind)
    }

    fn instantiate(&self, config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError> {
        let factory = self
            .factories
            .get(&config.kind)
            .ok_or_else(|| WidgetError::UnknownWidgetType(config.kind.clone()))?;
        factory(config)
    }
}

pub struct WidgetRuntime {
    instances: BTreeMap<WidgetId, Box<dyn Widget>>,
}
impl WidgetRuntime {
    pub fn empty() -> Self {
        Self {
            instances: BTreeMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    pub fn from_config(registry: &WidgetRegistry, config: &AppConfig) -> Result<Self, WidgetError> {
        let mut instances = BTreeMap::new();
        for widget_config in &config.workspace.widgets {
            let id = WidgetId::new(widget_config.id);
            if instances.contains_key(&id) {
                return Err(WidgetError::DuplicateWidgetId(id));
            }
            instances.insert(id, registry.instantiate(widget_config)?);
        }
        Ok(Self { instances })
    }

    pub fn widget_ids(&self) -> impl Iterator<Item = WidgetId> + '_ {
        self.instances.keys().copied()
    }

    pub fn widget_kind(&self, id: WidgetId) -> Option<&str> {
        self.instances.get(&id).map(|widget| widget.kind())
    }

    pub fn render(
        &self,
        areas: &BTreeMap<WidgetId, Rect>,
        focused: Option<WidgetId>,
    ) -> BTreeMap<WidgetId, Scene> {
        self.instances
            .iter()
            .filter_map(|(&id, widget)| {
                areas
                    .get(&id)
                    .map(|&area| (id, widget.render(area, focused == Some(id))))
            })
            .collect()
    }
}

struct TextWidget {
    title: String,
    text: String,
}

impl Widget for TextWidget {
    fn kind(&self) -> &str {
        "text"
    }

    fn render(&self, area: Rect, focused: bool) -> Scene {
        let background = Color::rgb(27, 33, 44);
        let foreground = Color::rgb(226, 232, 240);
        let accent = if focused {
            Color::rgb(250, 204, 21)
        } else {
            Color::rgb(125, 211, 252)
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

fn text_widget_factory(config: &WidgetInstanceConfig) -> Result<Box<dyn Widget>, WidgetError> {
    Ok(Box::new(TextWidget {
        title: config.title.clone().unwrap_or_else(|| " text ".to_owned()),
        text: config.text.clone().unwrap_or_default(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_register_and_render_configured_text_widgets() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 4
            type = "text"
            title = " greeting "
            text = "hello"
            "#,
        )
        .unwrap();
        let registry = WidgetRegistry::builtins();
        let runtime = WidgetRuntime::from_config(&registry, &config).unwrap();
        let areas = BTreeMap::from([(WidgetId::new(4), Rect::new(0, 0, 16, 4))]);

        assert_eq!(runtime.widget_ids().collect::<Vec<_>>(), [WidgetId::new(4)]);
        assert_eq!(runtime.widget_kind(WidgetId::new(4)), Some("text"));
        let scenes = runtime.render(&areas, Some(WidgetId::new(4)));
        assert_eq!(scenes[&WidgetId::new(4)].cell_at(2, 1).unwrap().symbol, 'h');
    }

    #[test]
    fn unknown_widget_types_are_rejected_by_the_registry() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "missing"
            "#,
        )
        .unwrap();

        assert_eq!(
            WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).err(),
            Some(WidgetError::UnknownWidgetType("missing".to_owned()))
        );
    }

    #[test]
    fn rendering_omits_instances_without_a_layout_area() {
        let config = AppConfig::parse(
            r#"
            version = 1
            [[workspace.widgets]]
            id = 1
            type = "text"
            "#,
        )
        .unwrap();
        let runtime = WidgetRuntime::from_config(&WidgetRegistry::builtins(), &config).unwrap();

        assert!(runtime.render(&BTreeMap::new(), None).is_empty());
    }
}
