use std::collections::BTreeMap;

use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::{
    backend::OutputMetrics,
    scene::{CellStyle, Color, Scene},
    state::{FocusState, FocusTarget, SurfaceId},
};

const BACKGROUND: Color = Color::rgb(18, 22, 30);
const PANEL: Color = Color::rgb(27, 33, 44);
const TEXT: Color = Color::rgb(226, 232, 240);
const MUTED: Color = Color::rgb(148, 163, 184);
const ACCENT: Color = Color::rgb(125, 211, 252);
const FOCUS: Color = Color::rgb(250, 204, 21);
const SUCCESS: Color = Color::rgb(134, 239, 172);

struct Panel<'a> {
    id: SurfaceId,
    title: &'a str,
    first_line: &'a str,
    second_line: &'a str,
    accent: Color,
}

pub const WORKSPACE_SURFACE_ID: SurfaceId = SurfaceId::new(1);
pub const BACKEND_SURFACE_ID: SurfaceId = SurfaceId::new(2);

pub fn static_dashboard_surface_areas(area: Rect) -> [(SurfaceId, Rect); 2] {
    let sections = dashboard_sections(area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(sections[1]);

    [
        (WORKSPACE_SURFACE_ID, columns[0]),
        (BACKEND_SURFACE_ID, columns[1]),
    ]
}

pub fn configured_widget_surface_areas(
    area: Rect,
    surface_ids: &[SurfaceId],
) -> BTreeMap<SurfaceId, Rect> {
    let content = dashboard_sections(area)[1];
    if surface_ids.is_empty() {
        return BTreeMap::new();
    }

    let count = surface_ids.len() as u16;
    let base_width = content.width / count;
    let remainder = content.width % count;
    let mut x = content.x;
    surface_ids
        .iter()
        .enumerate()
        .map(|(index, &id)| {
            let width = base_width + u16::from(index < remainder as usize);
            let surface_area = Rect::new(x, content.y, width, content.height);
            x = x.saturating_add(width);
            (id, surface_area)
        })
        .collect()
}

pub fn render_static_dashboard(area: Rect) -> Scene {
    render_static_dashboard_with_focus(area, FocusState::default())
}

pub fn render_static_dashboard_with_focus(area: Rect, focus: FocusState) -> Scene {
    let mut scene = render_static_dashboard_shell(area);
    for surface_scene in render_static_dashboard_surface_scenes(area, focus).values() {
        scene.blit(surface_scene, surface_scene.area());
    }
    scene
}

pub fn render_static_dashboard_shell(area: Rect) -> Scene {
    render_static_dashboard_shell_with_metrics(area, OutputMetrics::default())
}

pub fn render_static_dashboard_shell_with_metrics(area: Rect, metrics: OutputMetrics) -> Scene {
    render_static_dashboard_shell_with_metrics_and_health(area, metrics, None)
}

pub fn render_static_dashboard_shell_with_metrics_and_health(
    area: Rect,
    metrics: OutputMetrics,
    widget_health: Option<&str>,
) -> Scene {
    let mut scene = Scene::new(area);
    scene.fill(area, CellStyle::new(TEXT, BACKGROUND));

    let sections = dashboard_sections(area);
    let header = sections[0];
    scene.fill(header, CellStyle::new(TEXT, PANEL));
    scene.border(header, " cmdash ", CellStyle::new(ACCENT, PANEL));
    scene.text(
        header.x.saturating_add(2),
        header.y.saturating_add(1),
        "A modular terminal dashboard",
        CellStyle::new(TEXT, PANEL).bold(),
    );

    scene.fill(sections[1], CellStyle::new(TEXT, BACKGROUND));
    let footer = sections[2];
    scene.fill(footer, CellStyle::new(MUTED, BACKGROUND));
    let footer_text = if metrics.bytes_saved > 0 {
        format!(
            "Tab / Shift+Tab  focus    q / Esc  quit    •    saved {} B    •    retained output",
            metrics.bytes_saved
        )
    } else {
        "Tab / Shift+Tab  focus    q / Esc  quit    •    retained frame".to_owned()
    };
    let footer_text = widget_health.map_or(footer_text.clone(), |health| {
        format!("{footer_text}    •    widgets: {health}")
    });
    scene.text(
        footer.x.saturating_add(1),
        footer.y,
        &footer_text,
        CellStyle::new(MUTED, BACKGROUND).dim(),
    );
    scene
}

pub fn render_static_dashboard_surface_scenes(
    area: Rect,
    focus: FocusState,
) -> BTreeMap<SurfaceId, Scene> {
    let [(workspace_id, workspace_area), (backend_id, backend_area)] =
        static_dashboard_surface_areas(area);
    let mut scenes = BTreeMap::new();
    scenes.insert(
        workspace_id,
        render_panel_scene(
            workspace_area,
            focus,
            Panel {
                id: workspace_id,
                title: " workspace ",
                first_line: "No terminal sessions are running.",
                second_line: "Dashboard widgets can work without a PTY.",
                accent: SUCCESS,
            },
        ),
    );
    scenes.insert(
        backend_id,
        render_panel_scene(
            backend_area,
            focus,
            Panel {
                id: backend_id,
                title: " backend ",
                first_line: "crossterm",
                second_line: "retained scene contract",
                accent: ACCENT,
            },
        ),
    );
    scenes
}

fn dashboard_sections(area: Rect) -> [Rect; 3] {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    [sections[0], sections[1], sections[2]]
}

fn render_panel_scene(area: Rect, focus: FocusState, panel: Panel<'_>) -> Scene {
    let mut scene = Scene::new(area);
    draw_panel(&mut scene, area, focus, panel);
    scene
}

fn draw_panel(scene: &mut Scene, area: Rect, focus: FocusState, panel: Panel<'_>) {
    let focused = focus.is_focused(FocusTarget::Surface(panel.id));
    let border_color = if focused { FOCUS } else { panel.accent };
    scene.fill(area, CellStyle::new(TEXT, PANEL));
    scene.border(area, panel.title, CellStyle::new(border_color, PANEL));
    if focused {
        scene.text(
            area.x.saturating_add(2),
            area.y.saturating_add(1),
            "focused",
            CellStyle::new(FOCUS, PANEL).bold(),
        );
    }
    scene.text(
        area.x.saturating_add(2),
        area.y.saturating_add(2),
        panel.first_line,
        CellStyle::new(TEXT, PANEL),
    );
    scene.text(
        area.x.saturating_add(2),
        area.y.saturating_add(3),
        panel.second_line,
        CellStyle::new(MUTED, PANEL).dim(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_dashboard_keeps_text_inside_the_requested_scene() {
        let scene = render_static_dashboard(Rect::new(0, 0, 80, 24));

        assert_eq!(scene.area(), Rect::new(0, 0, 80, 24));
        assert_eq!(scene.cell_at(2, 1).unwrap().symbol, 'A');
        assert_eq!(scene.cell_at(1, 23).unwrap().symbol, 'T');
    }

    #[test]
    fn static_dashboard_can_render_a_small_surface() {
        let scene = render_static_dashboard(Rect::new(0, 0, 8, 4));

        assert_eq!(scene.area(), Rect::new(0, 0, 8, 4));
        assert!(scene.cell_at(7, 3).is_some());
    }

    #[test]
    fn focused_surface_gets_a_focus_marker() {
        let mut state = crate::AppState::new(crate::BackendCapabilities {
            truecolor: true,
            mouse: true,
            bracketed_paste: true,
            kitty_graphics: false,
        });
        state
            .dispatch(crate::Command::Surface(crate::SurfaceCommand::Add(
                crate::Surface::new(WORKSPACE_SURFACE_ID, Rect::new(0, 0, 10, 4)),
            )))
            .unwrap();
        state
            .dispatch(crate::Command::Focus(crate::FocusCommand::Surface(
                WORKSPACE_SURFACE_ID,
            )))
            .unwrap();
        let focused = render_static_dashboard_with_focus(Rect::new(0, 0, 80, 24), state.focus());

        assert_eq!(focused.cell_at(2, 4).unwrap().symbol, 'f');
    }

    #[test]
    fn surface_layout_exposes_stable_ids() {
        let areas = static_dashboard_surface_areas(Rect::new(0, 0, 80, 24));
        assert_eq!(areas[0].0, WORKSPACE_SURFACE_ID);
        assert_eq!(areas[1].0, BACKEND_SURFACE_ID);
        assert!(areas[0].1.width > areas[1].1.width);
    }

    #[test]
    fn configured_widget_layout_distributes_the_content_area() {
        let ids = [SurfaceId::new(1), SurfaceId::new(2), SurfaceId::new(3)];
        let areas = configured_widget_surface_areas(Rect::new(0, 0, 80, 24), &ids);

        assert_eq!(areas.len(), 3);
        assert_eq!(areas[&ids[0]].x, 0);
        assert_eq!(areas[&ids[0]].y, 3);
        assert_eq!(areas[&ids[0]].height, 20);
        assert_eq!(areas[&ids[2]].x + areas[&ids[2]].width, 80);
    }
}
