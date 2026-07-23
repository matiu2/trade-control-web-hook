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
use ratatui::text::{Line, Span};
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
    // While a background job runs for the open plan, prefix the status with an
    // animated spinner so it's obviously live, not frozen.
    let status_line = match app.current_busy() {
        Some(kind) => Line::from(vec![
            Span::styled(
                format!("{} ", app.spinner()),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("{}…", kind.verb()),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        None => Line::from(Span::styled(app.status.text.clone(), status_style)),
    };
    f.render_widget(Paragraph::new(status_line), chunks[1]);
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
    const REPLAY: &str = include_str!("../tests/fixtures/replay_report.txt");

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
        // Oldest last-event first: the oldest-watermark plan is at the top and
        // therefore on-screen; a top-of-fixture (newest) plan sorts to the
        // bottom, off a 40-row screen.
        assert!(
            text.contains("hs-aud-chf-648e83cd"),
            "oldest plan should be at the top:\n{text}"
        );
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
        // Info bar shows entry mode + broker; body shows the timeline frame.
        assert!(text.contains("normal"), "info bar should show entry mode");
        assert!(text.contains("oanda"), "info bar should show the broker");
        assert!(text.contains("Timeline"));
    }

    #[test]
    fn detail_popup_scrolls() {
        let rows = parse_plan_list(LIST).unwrap();
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
        app.toggle_popup(); // open the detail popup

        // A small viewport so the JSON overflows and scrolling is meaningful.
        let render = |app: &App| {
            let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
            term.draw(|f| super::render(f, app)).unwrap();
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };

        let top = render(&app);
        assert!(top.contains("Plan detail"), "popup titled:\n{top}");
        // At the top, the first JSON keys are visible.
        assert!(
            top.contains("trade_id") || top.contains('{'),
            "top of dump:\n{top}"
        );

        // Scroll to the end; the top-of-file content should no longer show.
        app.scroll_popup_end();
        let bottom = render(&app);
        assert_ne!(top, bottom, "scrolling should change the visible text");
    }

    #[test]
    fn compare_screen_shows_divergence_diff() {
        // The Compare screen's headline is the replay-vs-live divergence diff.
        // The AUD_CAD fixtures diverge purely on timing: live fires
        // pause/resume/news-start/news-end across 03:30–12:30, the replay fires
        // all four at 13:00 → 4 matched rule ids, 4 timing divergences.
        let rows = parse_plan_list(LIST).unwrap();
        let mut app = App::from_rows(rows);
        app.select_to("hs-aud-cad-a07622da");
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: Some(REPLAY.to_string()),
            max_depth: 3,
        });
        app.set_screen(Screen::Compare);

        let mut term = Terminal::new(TestBackend::new(200, 44)).unwrap();
        term.draw(|f| super::render(f, &app)).unwrap();
        let text = buffer_text(&term);

        // Summary band: 4 matched, 0 one-sided, 4 timing divergences.
        assert!(text.contains("4 matched"), "summary matched count:\n{text}");
        assert!(text.contains("0 live-only"), "no under-fire:\n{text}");
        assert!(text.contains("0 replay-only"), "no over-fire:\n{text}");
        assert!(
            text.contains("4 timing"),
            "timing divergence count:\n{text}"
        );
        // Detail lists the timing divergences with both bars.
        assert!(text.contains("timing"), "detail shows timing rows:\n{text}");
        assert!(
            text.contains("live 2026-07-23 03:30") || text.contains("03:30"),
            "detail shows the live pause bar:\n{text}"
        );
        assert!(
            text.contains("13:00"),
            "detail shows the replay bar:\n{text}"
        );
        // The diff is the headline — it is NOT the clean "no divergence" line.
        assert!(
            !text.contains("no divergence"),
            "the AUD_CAD fixtures DO diverge on timing:\n{text}"
        );
        // The raw side-by-side is still present below the diff.
        assert!(
            text.contains("Live (recorded)"),
            "side-by-side kept:\n{text}"
        );
    }
}
