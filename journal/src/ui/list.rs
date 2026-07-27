//! The plan-picker list (depth 0).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::app::App;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    // While the `/` prompt is open (or a filter is applied), a one-line search
    // bar sits under the list so the operator can see what they're typing.
    let (list_area, search_area) = if app.search.active || app.search.is_filtering() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    // A visited plan (max_depth ≥ 1) gets a subtle marker so you can see what
    // you've already worked through.
    let visited = |trade_id: &str| {
        app.data
            .get(trade_id)
            .map(|d| d.max_depth >= 1)
            .unwrap_or(false)
    };

    // Only the rows the `/` filter lets through; `app.selected` indexes these.
    let rows = app.visible_plans();
    let items: Vec<ListItem> = rows
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

    // With a filter on, the title shows matched-of-total so it's obvious the
    // list is a subset (and not that plans went missing).
    let title = if app.search.is_filtering() {
        format!(
            "Plans ({}/{} matching) — oldest event first",
            rows.len(),
            app.plans.len()
        )
    } else {
        format!("Plans ({}) — oldest event first", app.plans.len())
    };
    let list = List::new(items)
        .block(crate::ui::titled_block(&title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(app.selected));
    }
    f.render_stateful_widget(list, list_area, &mut state);

    if let Some(search_area) = search_area {
        render_search_bar(f, app, search_area, rows.is_empty());
    }
}

/// The `/` search bar: a live-editing prompt while open, a dimmed reminder of
/// the applied filter once closed. A query matching nothing says so explicitly
/// rather than leaving an unexplained empty list.
fn render_search_bar(f: &mut Frame, app: &App, area: Rect, no_matches: bool) {
    let line = if app.search.active {
        // A block cursor marks the insertion point — there's no real terminal
        // cursor in the alternate screen here.
        let style = if no_matches {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::Yellow)
        };
        Line::from(vec![
            Span::styled("/", style.add_modifier(Modifier::BOLD)),
            Span::styled(app.search.query.clone(), style),
            Span::styled("█", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if no_matches {
                    "  no matches".to_string()
                } else {
                    String::new()
                },
                Style::default().fg(Color::Red),
            ),
            Span::styled(
                "   Enter keep · Esc clear",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                app.search.query.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "   / edit · Esc clear",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    };
    f.render_widget(Paragraph::new(line), area);
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
