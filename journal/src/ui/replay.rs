//! The replay screen (depth 2): the `replay-candles --plan` report text.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::App;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let report = app.current_data().and_then(|d| d.replay_report.as_deref());
    let body = match report {
        None => Text::styled(
            "running replay… (press r to re-run)",
            Style::default().fg(Color::DarkGray),
        ),
        Some(text) => Text::raw(text),
    };
    let para = Paragraph::new(body)
        .block(crate::ui::titled_block("Replay report"))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}
