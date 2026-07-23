//! Top-level render: frame layout (info bar / body / footer), screen dispatch,
//! and the popup/modal overlays.

mod compare;
mod infobar;
mod list;
mod popup;
mod replay;
mod timeline;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::App;
use crate::screen::Screen;

/// Render the whole frame.
pub fn render(f: &mut Frame, app: &App) {
    let show_infobar = app.screen != Screen::List;
    let constraints = if show_infobar {
        vec![
            Constraint::Length(3), // info bar
            Constraint::Min(1),    // body
            Constraint::Length(1), // footer
        ]
    } else {
        vec![Constraint::Min(1), Constraint::Length(1)]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    let (body, footer) = if show_infobar {
        infobar::render(f, app, chunks[0]);
        (chunks[1], chunks[2])
    } else {
        (chunks[0], chunks[1])
    };

    render_body(f, app, body);
    render_footer(f, app, footer);

    if app.show_popup {
        popup::render_detail(f, app);
    }
    if app.confirm.is_some() {
        popup::render_confirm(f, app);
    }
}

/// Dispatch the body area to the active screen's renderer.
fn render_body(f: &mut Frame, app: &App, area: Rect) {
    match app.screen {
        Screen::List => list::render(f, app, area),
        Screen::Timeline => timeline::render(f, app, area),
        Screen::Replay => replay::render(f, app, area),
        Screen::Compare => compare::render(f, app, area),
    }
}

/// The one-line footer: context hints on the left, status on the right.
fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let hints = match app.screen {
        Screen::List => "↑↓ move  →/n open  q quit",
        _ => "← back  →/n deeper  l load-TV  r replay  i detail  d/x delete  q quit",
    };
    let status_style = if app.status.is_error {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    f.render_widget(
        Paragraph::new(Line::from(hints)).style(Style::default().fg(Color::DarkGray)),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(app.status.text.clone())).style(status_style),
        chunks[1],
    );
}

/// A small helper: a bordered block with a title, used by several screens.
pub(crate) fn titled_block(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::DarkGray))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::{App, PlanData};
    use crate::plan::{parse_plan_export, parse_plan_list};
    use crate::screen::Screen;

    const LIST: &str = include_str!("../tests/fixtures/plan_list.yaml");
    const EXPORT: &str = include_str!("../tests/fixtures/plan_export.json");
    const TIMELINE: &str = include_str!("../tests/fixtures/plan_timeline.json");

    /// Flatten a rendered buffer to a string so we can assert on visible text.
    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn list_screen_renders_plans() {
        let rows = parse_plan_list(LIST).unwrap();
        let app = App::from_rows(rows);
        let mut term = Terminal::new(TestBackend::new(120, 40)).unwrap();
        term.draw(|f| super::render(f, &app)).unwrap();
        let text = buffer_text(&term);
        assert!(text.contains("Plans"));
        assert!(text.contains("ihs-gbp-usd-c0451533"));
    }

    #[test]
    fn timeline_screen_renders_infobar_and_events() {
        let rows = parse_plan_list(LIST).unwrap();
        // Point selection at the plan the fixtures are for.
        let mut app = App::from_rows(rows);
        app.select_to("hs-aud-cad-a07622da");
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: None,
            max_depth: 1,
        });
        app.set_screen(Screen::Timeline);

        let mut term = Terminal::new(TestBackend::new(160, 40)).unwrap();
        term.draw(|f| super::render(f, &app)).unwrap();
        let text = buffer_text(&term);
        // Info bar shows the entry mode; body shows the timeline frame.
        assert!(text.contains("normal"), "info bar should show entry mode");
        assert!(text.contains("Timeline"));
    }
}
