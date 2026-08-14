use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use ratatui::layout::Rect;

use crate::{
    config::LayoutConfig,
    state::{OverlayId, WidgetId},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutNode {
    Leaf(WidgetId),
    Columns(Vec<LayoutNode>),
    Tabs {
        active: usize,
        children: Vec<LayoutNode>,
    },
    Stack(Vec<LayoutNode>),
    Overlay(OverlayId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutTree {
    root: LayoutNode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutError {
    MissingWidget(WidgetId),
    MissingOverlay(OverlayId),
    EmptyChildren,
    InvalidActiveTab(usize),
}

impl fmt::Display for LayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingWidget(id) => {
                write!(formatter, "layout references missing widget {}", id.get())
            }
            Self::MissingOverlay(id) => {
                write!(formatter, "layout references missing overlay {}", id.get())
            }
            Self::EmptyChildren => formatter.write_str("layout nodes must have children"),
            Self::InvalidActiveTab(index) => {
                write!(formatter, "active tab index {index} is out of range")
            }
        }
    }
}

impl std::error::Error for LayoutError {}

impl LayoutTree {
    pub fn from_config(
        config: Option<&LayoutConfig>,
        widget_ids: impl IntoIterator<Item = WidgetId>,
        overlay_ids: impl IntoIterator<Item = OverlayId>,
    ) -> Result<Self, LayoutError> {
        let widgets: BTreeSet<_> = widget_ids.into_iter().collect();
        let overlays: BTreeSet<_> = overlay_ids.into_iter().collect();
        let root = match config {
            Some(config) => convert_node(config, &widgets, &overlays)?,
            None => LayoutNode::Columns(widgets.into_iter().map(LayoutNode::Leaf).collect()),
        };
        Ok(Self { root })
    }

    pub fn root(&self) -> &LayoutNode {
        &self.root
    }

    pub fn visible_widget_ids(&self) -> Vec<WidgetId> {
        let mut ids = Vec::new();
        collect_visible_widgets(&self.root, &mut ids);
        ids
    }

    pub fn visible_overlay_ids(&self) -> Vec<OverlayId> {
        let mut ids = Vec::new();
        collect_visible_overlays(&self.root, &mut ids);
        ids
    }

    pub fn widget_areas(&self, area: Rect) -> BTreeMap<WidgetId, Rect> {
        let mut placements = BTreeMap::new();
        place_widgets(&self.root, area, &mut placements);
        placements
    }
}

fn convert_node(
    config: &LayoutConfig,
    widgets: &BTreeSet<WidgetId>,
    overlays: &BTreeSet<OverlayId>,
) -> Result<LayoutNode, LayoutError> {
    match config {
        LayoutConfig::Leaf { widget } => {
            let id = WidgetId::new(*widget);
            if !widgets.contains(&id) {
                return Err(LayoutError::MissingWidget(id));
            }
            Ok(LayoutNode::Leaf(id))
        }
        LayoutConfig::Columns { children } => {
            if children.is_empty() {
                return Err(LayoutError::EmptyChildren);
            }
            Ok(LayoutNode::Columns(
                children
                    .iter()
                    .map(|child| convert_node(child, widgets, overlays))
                    .collect::<Result<_, _>>()?,
            ))
        }
        LayoutConfig::Tabs { active, children } => {
            if children.is_empty() {
                return Err(LayoutError::EmptyChildren);
            }
            if *active >= children.len() {
                return Err(LayoutError::InvalidActiveTab(*active));
            }
            Ok(LayoutNode::Tabs {
                active: *active,
                children: children
                    .iter()
                    .map(|child| convert_node(child, widgets, overlays))
                    .collect::<Result<_, _>>()?,
            })
        }
        LayoutConfig::Stack { children } => {
            if children.is_empty() {
                return Err(LayoutError::EmptyChildren);
            }
            Ok(LayoutNode::Stack(
                children
                    .iter()
                    .map(|child| convert_node(child, widgets, overlays))
                    .collect::<Result<_, _>>()?,
            ))
        }
        LayoutConfig::Overlay { overlay } => {
            let id = OverlayId::new(*overlay);
            if !overlays.contains(&id) {
                return Err(LayoutError::MissingOverlay(id));
            }
            Ok(LayoutNode::Overlay(id))
        }
    }
}

fn collect_visible_widgets(node: &LayoutNode, ids: &mut Vec<WidgetId>) {
    match node {
        LayoutNode::Leaf(id) => {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        LayoutNode::Columns(children) | LayoutNode::Stack(children) => {
            for child in children {
                collect_visible_widgets(child, ids);
            }
        }
        LayoutNode::Tabs { active, children } => {
            collect_visible_widgets(&children[*active], ids);
        }
        LayoutNode::Overlay(_) => {}
    }
}

fn collect_visible_overlays(node: &LayoutNode, ids: &mut Vec<OverlayId>) {
    match node {
        LayoutNode::Overlay(id) => {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        LayoutNode::Columns(children) | LayoutNode::Stack(children) => {
            for child in children {
                collect_visible_overlays(child, ids);
            }
        }
        LayoutNode::Tabs { active, children } => {
            collect_visible_overlays(&children[*active], ids);
        }
        LayoutNode::Leaf(_) => {}
    }
}

fn place_widgets(node: &LayoutNode, area: Rect, placements: &mut BTreeMap<WidgetId, Rect>) {
    match node {
        LayoutNode::Leaf(id) => {
            placements.insert(*id, area);
        }
        LayoutNode::Columns(children) => {
            for (child, child_area) in children.iter().zip(split_columns(area, children.len())) {
                place_widgets(child, child_area, placements);
            }
        }
        LayoutNode::Tabs { active, children } => {
            place_widgets(&children[*active], area, placements);
        }
        LayoutNode::Stack(children) => {
            for child in children {
                place_widgets(child, area, placements);
            }
        }
        LayoutNode::Overlay(_) => {}
    }
}

fn split_columns(area: Rect, count: usize) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let count = count as u16;
    let base_width = area.width / count;
    let remainder = area.width % count;
    let mut x = area.x;
    (0..count)
        .map(|index| {
            let width = base_width + u16::from(index < remainder);
            let child = Rect::new(x, area.y, width, area.height);
            x = x.saturating_add(width);
            child
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widgets() -> [WidgetId; 3] {
        [WidgetId::new(1), WidgetId::new(2), WidgetId::new(3)]
    }

    #[test]
    fn default_layout_places_all_widgets_in_columns() {
        let tree = LayoutTree::from_config(None, widgets(), []).unwrap();
        let areas = tree.widget_areas(Rect::new(0, 0, 30, 6));

        assert_eq!(tree.visible_widget_ids(), widgets());
        assert_eq!(areas[&WidgetId::new(1)].width, 10);
        assert_eq!(areas[&WidgetId::new(3)].x, 20);
    }

    #[test]
    fn tabs_only_place_the_active_branch() {
        let config = LayoutConfig::Tabs {
            active: 1,
            children: vec![
                LayoutConfig::Leaf { widget: 1 },
                LayoutConfig::Columns {
                    children: vec![
                        LayoutConfig::Leaf { widget: 2 },
                        LayoutConfig::Leaf { widget: 3 },
                    ],
                },
            ],
        };
        let tree = LayoutTree::from_config(Some(&config), widgets(), []).unwrap();
        let areas = tree.widget_areas(Rect::new(0, 0, 20, 4));

        assert_eq!(
            tree.visible_widget_ids(),
            [WidgetId::new(2), WidgetId::new(3)]
        );
        assert!(!areas.contains_key(&WidgetId::new(1)));
        assert_eq!(areas[&WidgetId::new(2)].width, 10);
    }

    #[test]
    fn stack_exposes_overlay_nodes_without_treating_them_as_widgets() {
        let config = LayoutConfig::Stack {
            children: vec![
                LayoutConfig::Leaf { widget: 1 },
                LayoutConfig::Overlay { overlay: 9 },
            ],
        };
        let tree = LayoutTree {
            root: convert_node(
                &config,
                &BTreeSet::from([WidgetId::new(1)]),
                &BTreeSet::from([OverlayId::new(9)]),
            )
            .unwrap(),
        };

        assert_eq!(tree.visible_widget_ids(), [WidgetId::new(1)]);
        assert_eq!(tree.visible_overlay_ids(), [OverlayId::new(9)]);
    }
}
