//! The timeline screen (depth 1): the ordered event trail for the open plan.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem};

use crate::app::App;
use crate::timeline::parse_events;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let json = app.current_data().and_then(|d| d.timeline_json.as_deref());

    let items: Vec<ListItem> = match json {
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

    let list = List::new(items).block(crate::ui::titled_block("Timeline"));
    f.render_widget(list, area);
}
