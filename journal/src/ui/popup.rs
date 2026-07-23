//! Overlays drawn on top of any screen: the `i` full-plan-detail dump and the
//! delete confirm modal.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;

/// The `i` popup: the full `plan export` JSON for the open plan, scrollable with
/// arrows / vim keys / page / home / end. Wrapping is off so line counts are
/// exact and the End clamp is correct; horizontal overflow is rare in the
/// structured JSON.
pub fn render_detail(f: &mut Frame, app: &App) {
    let area = centered(80, 80, f.area());
    let dump = app
        .current_data()
        .and_then(|d| d.export_json.as_deref())
        .map(pretty_json)
        .unwrap_or_else(|| "(no plan detail loaded)".to_string());

    // Clamp the scroll so End (u16::MAX) pins to the last page and you can't
    // scroll into empty space. Inner height excludes the two border rows.
    let total_lines = dump.lines().count() as u16;
    let inner_height = area.height.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(inner_height);
    let scroll = app.popup_scroll.min(max_scroll);

    let position = if max_scroll == 0 {
        String::new()
    } else {
        format!(
            " [{}/{}] ",
            scroll.saturating_add(1),
            max_scroll.saturating_add(1)
        )
    };
    let title = format!(" Plan detail{position}— ↑↓/jk pgup/pgdn g/G, i/esc close ");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Magenta));

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(Text::raw(dump))
            .block(block)
            .scroll((scroll, 0)),
        area,
    );
}

/// The delete confirm modal.
pub fn render_confirm(f: &mut Frame, app: &App) {
    let Some(confirm) = app.confirm.as_ref() else {
        return;
    };
    let area = centered(50, 20, f.area());

    let lines = vec![
        Line::from(Span::styled(
            confirm.prompt.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "y",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" delete    "),
            Span::styled(
                "n",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("/esc cancel"),
        ]),
    ];

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirm delete ")
        .border_style(Style::default().fg(Color::Red));

    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Pretty-print single-line JSON; echo the input on parse failure.
fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_string())
}

/// A centred rectangle sized as a percentage of `area`.
fn centered(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}
