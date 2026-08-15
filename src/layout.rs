use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use ratatui::layout::Rect;

use crate::{
    config::{LayoutConfig, SplitDirection},
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
    Split {
        direction: SplitDirection,
        ratios: Vec<u16>,
        children: Vec<LayoutNode>,
    },
    Overlay(OverlayId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutTree {
    root: LayoutNode,
    hidden_widgets: BTreeSet<WidgetId>,
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
        Ok(Self {
            root,
            hidden_widgets: BTreeSet::new(),
        })
    }

    pub fn root(&self) -> &LayoutNode {
        &self.root
    }

    pub fn visible_widget_ids(&self) -> Vec<WidgetId> {
        let mut ids = Vec::new();
        collect_visible_widgets(&self.root, &self.hidden_widgets, &mut ids);
        ids
    }

    pub fn visible_overlay_ids(&self) -> Vec<OverlayId> {
        let mut ids = Vec::new();
        collect_visible_overlays(&self.root, &mut ids);
        ids
    }

    pub fn widget_areas(&self, area: Rect) -> BTreeMap<WidgetId, Rect> {
        let mut placements = BTreeMap::new();
        place_widgets(&self.root, &self.hidden_widgets, area, &mut placements);
        placements
    }

    pub fn switch_tabs(&mut self, forward: bool) -> bool {
        switch_tabs_in_node(&mut self.root, forward)
    }

    pub fn adjust_split_for_widget(&mut self, widget: WidgetId, delta: i16) -> bool {
        adjust_split(&mut self.root, widget, delta)
    }

    pub fn hide_widget(&mut self, widget: WidgetId) -> bool {
        self.hidden_widgets.insert(widget)
    }

    pub fn is_hidden(&self, widget: WidgetId) -> bool {
        self.hidden_widgets.contains(&widget)
    }
}

fn switch_tabs_in_node(node: &mut LayoutNode, forward: bool) -> bool {
    match node {
        LayoutNode::Tabs { active, children } if children.len() > 1 => {
            if forward {
                *active = (*active + 1) % children.len();
            } else {
                *active = (*active + children.len() - 1) % children.len();
            }
            true
        }
        LayoutNode::Columns(children)
        | LayoutNode::Stack(children)
        | LayoutNode::Split { children, .. } => children
            .iter_mut()
            .any(|child| switch_tabs_in_node(child, forward)),
        LayoutNode::Tabs { children, .. } => children
            .iter_mut()
            .any(|child| switch_tabs_in_node(child, forward)),
        LayoutNode::Leaf(_) | LayoutNode::Overlay(_) => false,
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
        LayoutConfig::Split {
            direction,
            ratios,
            children,
        } => {
            if children.is_empty() {
                return Err(LayoutError::EmptyChildren);
            }
            Ok(LayoutNode::Split {
                direction: *direction,
                ratios: ratios.clone(),
                children: children
                    .iter()
                    .map(|child| convert_node(child, widgets, overlays))
                    .collect::<Result<_, _>>()?,
            })
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

fn collect_visible_widgets(
    node: &LayoutNode,
    hidden_widgets: &BTreeSet<WidgetId>,
    ids: &mut Vec<WidgetId>,
) {
    match node {
        LayoutNode::Leaf(id) => {
            if !hidden_widgets.contains(id) && !ids.contains(id) {
                ids.push(*id);
            }
        }
        LayoutNode::Columns(children)
        | LayoutNode::Stack(children)
        | LayoutNode::Split { children, .. } => {
            for child in children {
                collect_visible_widgets(child, hidden_widgets, ids);
            }
        }
        LayoutNode::Tabs { active, children } => {
            collect_visible_widgets(&children[*active], hidden_widgets, ids);
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
        LayoutNode::Columns(children)
        | LayoutNode::Stack(children)
        | LayoutNode::Split { children, .. } => {
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

fn place_widgets(
    node: &LayoutNode,
    hidden_widgets: &BTreeSet<WidgetId>,
    area: Rect,
    placements: &mut BTreeMap<WidgetId, Rect>,
) {
    match node {
        LayoutNode::Leaf(id) => {
            if !hidden_widgets.contains(id) {
                placements.insert(*id, area);
            }
        }
        LayoutNode::Columns(children) => {
            for (child, child_area) in children.iter().zip(split_area(
                area,
                children.len(),
                SplitDirection::Horizontal,
                &[],
            )) {
                place_widgets(child, hidden_widgets, child_area, placements);
            }
        }
        LayoutNode::Tabs { active, children } => {
            place_widgets(&children[*active], hidden_widgets, area, placements);
        }
        LayoutNode::Stack(children) => {
            for child in children {
                place_widgets(child, hidden_widgets, area, placements);
            }
        }
        LayoutNode::Split {
            direction,
            ratios,
            children,
        } => {
            for (child, child_area) in
                children
                    .iter()
                    .zip(split_area(area, children.len(), *direction, ratios))
            {
                place_widgets(child, hidden_widgets, child_area, placements);
            }
        }
        LayoutNode::Overlay(_) => {}
    }
}

fn adjust_split(node: &mut LayoutNode, widget: WidgetId, delta: i16) -> bool {
    match node {
        LayoutNode::Split {
            children, ratios, ..
        } => {
            let Some(index) = children
                .iter()
                .position(|child| contains_widget(child, widget))
            else {
                return children
                    .iter_mut()
                    .any(|child| adjust_split(child, widget, delta));
            };
            if children.len() < 2 || delta == 0 {
                return false;
            }
            if ratios.len() != children.len() || ratios.iter().all(|ratio| *ratio == 0) {
                *ratios = vec![100 / children.len() as u16; children.len()];
                let assigned = ratios.iter().sum::<u16>();
                if let Some(last) = ratios.last_mut() {
                    *last += 100 - assigned;
                }
            }
            let neighbor = if delta.is_positive() {
                (index + 1 < children.len()).then_some(index + 1)
            } else {
                (index > 0).then_some(index - 1)
            };
            let Some(neighbor) = neighbor else {
                return false;
            };
            let amount = delta.unsigned_abs().min(ratios[neighbor].saturating_sub(1));
            if amount == 0 {
                return false;
            }
            if delta.is_positive() {
                ratios[index] = ratios[index].saturating_add(amount);
                ratios[neighbor] = ratios[neighbor].saturating_sub(amount);
            } else {
                ratios[index] = ratios[index].saturating_sub(amount);
                ratios[neighbor] = ratios[neighbor].saturating_add(amount);
            }
            true
        }
        LayoutNode::Columns(children) | LayoutNode::Stack(children) => children
            .iter_mut()
            .any(|child| adjust_split(child, widget, delta)),
        LayoutNode::Tabs { children, .. } => children
            .iter_mut()
            .any(|child| adjust_split(child, widget, delta)),
        LayoutNode::Leaf(_) | LayoutNode::Overlay(_) => false,
    }
}

fn contains_widget(node: &LayoutNode, widget: WidgetId) -> bool {
    match node {
        LayoutNode::Leaf(id) => *id == widget,
        LayoutNode::Columns(children)
        | LayoutNode::Stack(children)
        | LayoutNode::Split { children, .. } => {
            children.iter().any(|child| contains_widget(child, widget))
        }
        LayoutNode::Tabs { children, .. } => {
            children.iter().any(|child| contains_widget(child, widget))
        }
        LayoutNode::Overlay(_) => false,
    }
}

fn split_area(area: Rect, count: usize, direction: SplitDirection, ratios: &[u16]) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let count = count as u16;
    let (total, mut offset) = match direction {
        SplitDirection::Horizontal => (area.width, area.x),
        SplitDirection::Vertical => (area.height, area.y),
    };
    let weights = if ratios.len() == count as usize && ratios.iter().any(|ratio| *ratio > 0) {
        ratios.to_vec()
    } else {
        vec![1; count as usize]
    };
    let total_weight: u32 = weights.iter().map(|weight| u32::from(*weight)).sum();
    let mut assigned = 0u16;
    (0..count)
        .map(|index| {
            let size = if index + 1 == count {
                total.saturating_sub(assigned)
            } else {
                ((u32::from(total) * u32::from(weights[index as usize])) / total_weight) as u16
            };
            assigned = assigned.saturating_add(size);
            let child = match direction {
                SplitDirection::Horizontal => Rect::new(offset, area.y, size, area.height),
                SplitDirection::Vertical => Rect::new(area.x, offset, area.width, size),
            };
            offset = offset.saturating_add(size);
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
    fn split_layouts_divide_rows_and_columns() {
        let horizontal = LayoutConfig::Split {
            direction: SplitDirection::Horizontal,
            ratios: Vec::new(),
            children: vec![
                LayoutConfig::Leaf { widget: 1 },
                LayoutConfig::Leaf { widget: 2 },
            ],
        };
        let tree = LayoutTree::from_config(Some(&horizontal), widgets(), []).unwrap();
        let areas = tree.widget_areas(Rect::new(0, 0, 20, 10));
        assert_eq!(areas[&WidgetId::new(1)], Rect::new(0, 0, 10, 10));

        let vertical = LayoutConfig::Split {
            direction: SplitDirection::Vertical,
            ratios: Vec::new(),
            children: vec![
                LayoutConfig::Leaf { widget: 1 },
                LayoutConfig::Leaf { widget: 2 },
            ],
        };
        let tree = LayoutTree::from_config(Some(&vertical), widgets(), []).unwrap();
        let areas = tree.widget_areas(Rect::new(0, 0, 20, 10));
        assert_eq!(areas[&WidgetId::new(2)], Rect::new(0, 5, 20, 5));
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
    fn switching_tabs_changes_visible_widgets_and_preserves_the_tree() {
        let config = LayoutConfig::Tabs {
            active: 0,
            children: vec![
                LayoutConfig::Leaf { widget: 1 },
                LayoutConfig::Leaf { widget: 2 },
            ],
        };
        let mut tree =
            LayoutTree::from_config(Some(&config), [WidgetId::new(1), WidgetId::new(2)], [])
                .unwrap();

        assert_eq!(tree.visible_widget_ids(), [WidgetId::new(1)]);
        assert!(tree.switch_tabs(true));
        assert_eq!(tree.visible_widget_ids(), [WidgetId::new(2)]);
        assert!(tree.switch_tabs(false));
        assert_eq!(tree.visible_widget_ids(), [WidgetId::new(1)]);
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
            hidden_widgets: BTreeSet::new(),
        };

        assert_eq!(tree.visible_widget_ids(), [WidgetId::new(1)]);
        assert_eq!(tree.visible_overlay_ids(), [OverlayId::new(9)]);
    }
}
