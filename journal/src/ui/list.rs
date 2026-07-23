//! The plan-picker list (depth 0).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::app::App;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    // A visited plan (max_depth ≥ 1) gets a subtle marker so you can see what
    // you've already worked through.
    let visited = |trade_id: &str| {
        app.data
            .get(trade_id)
            .map(|d| d.max_depth >= 1)
            .unwrap_or(false)
    };

    let items: Vec<ListItem> = app
        .plans
        .iter()
        .map(|p| {
            let marker = if visited(&p.trade_id) { "· " } else { "  " };
            let phase = p.phase.as_deref().unwrap_or("-");
            let archived = if p.is_archived() { "  ARCHIVED" } else { "" };
            // Last-event time (Brisbane, compact) — the list's sort key, shown
            // so the oldest-first ordering is visible.
            let last_event = p
                .last_event()
                .map(short_bne)
                .unwrap_or_else(|| "  —".to_string());
            let line = Line::from(vec![
                Span::styled(marker, Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{last_event:11} "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:32} ", p.trade_id)),
                Span::styled(
                    format!("{:16} ", p.instrument),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("{:5} ", p.granularity),
                    Style::default().fg(Color::Blue),
                ),
                Span::styled(format!("{phase:22}"), Style::default().fg(Color::Yellow)),
                Span::styled(archived, Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let title = format!("Plans ({}) — oldest event first", app.plans.len());
    let list = List::new(items)
        .block(crate::ui::titled_block(&title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    if !app.plans.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, area, &mut state);
}

/// A compact Brisbane `MM-DD HH:MM` for the last-event column. Echoes the raw
/// string (truncated) if it isn't a parseable RFC3339 instant.
fn short_bne(raw: &str) -> String {
    use chrono::{DateTime, FixedOffset};
    let brisbane = FixedOffset::east_opt(10 * 3600)
        .unwrap_or_else(|| FixedOffset::east_opt(0).expect("UTC is a valid fixed offset"));
    match DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => dt
            .with_timezone(&brisbane)
            .format("%m-%d %H:%M")
            .to_string(),
        Err(_) => raw.chars().take(11).collect(),
    }
}
