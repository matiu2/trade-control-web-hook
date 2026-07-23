//! The compare screen (depth 3): replay report ‖ live timeline, side by side.
//! v1 is a visual side-by-side; the computed replay-vs-live divergence diff is
//! v2 (see TODO-journaller-tui.md).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use crate::app::App;
use crate::timeline::parse_events;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    render_replay(f, app, cols[0]);
    render_timeline(f, app, cols[1]);
}

fn render_replay(f: &mut Frame, app: &App, area: Rect) {
    let report = app.current_data().and_then(|d| d.replay_report.as_deref());
    let body = match report {
        None => Text::styled("(no replay yet)", Style::default().fg(Color::DarkGray)),
        Some(text) => Text::raw(text),
    };
    let para = Paragraph::new(body)
        .block(crate::ui::titled_block("Replay (simulated)"))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_timeline(f: &mut Frame, app: &App, area: Rect) {
    let json = app.current_data().and_then(|d| d.timeline_json.as_deref());
    let items: Vec<ListItem> = match json {
        None => vec![ListItem::new("(no timeline)")],
        Some(json) => parse_events(json)
            .iter()
            .map(|e| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", e.ts), Style::default().fg(Color::DarkGray)),
                    Span::raw(format!("{} {}", e.marker, e.text)),
                ]))
            })
            .collect(),
    };
    let list = List::new(items).block(crate::ui::titled_block("Live (recorded)"));
    f.render_widget(list, area);
}
