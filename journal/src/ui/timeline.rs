//! The timeline screen (depth 1): the ordered event trail for the open plan.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem};

use crate::app::App;
use crate::timeline::{parse_events, settlement_lines};

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let json = app.current_data().and_then(|d| d.timeline_json.as_deref());

    let mut items: Vec<ListItem> = match json {
        None => vec![ListItem::new(Line::from(Span::styled(
            "loading timeline…",
            Style::default().fg(Color::DarkGray),
        )))],
        Some(json) => {
            let events = parse_events(json);
            if events.is_empty() {
                vec![ListItem::new(Line::from("(no recorded events)"))]
            } else {
                events
                    .iter()
                    .map(|e| {
                        let marker_style = if e.marker == '•' {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::Cyan)
                        };
                        ListItem::new(Line::from(vec![
                            Span::styled(
                                format!("{} ", e.ts),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(format!("{} ", e.marker), marker_style),
                            Span::raw(e.text.clone()),
                        ]))
                    })
                    .collect()
            }
        }
    };

    // The broker's own account of the trade, once the plan has been archived
    // with one. Appended below the event trail: the timeline says what the
    // system decided, this says what the broker actually did.
    if let Some(export) = app.current_data().and_then(|d| d.export_json.as_deref()) {
        for line in settlement_lines(export) {
            let style = if line.starts_with("  !") {
                // A stated limit on what the numbers mean — must not read as
                // just more data.
                Style::default().fg(Color::Yellow)
            } else if line.starts_with("  ") {
                Style::default()
            } else {
                Style::default().fg(Color::Green)
            };
            items.push(ListItem::new(Line::from(Span::styled(line, style))));
        }
    }

    let list = List::new(items).block(crate::ui::titled_block("Timeline"));
    f.render_widget(list, area);
}
