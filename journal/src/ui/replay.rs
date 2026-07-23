//! The replay screen (depth 2): the `replay-candles --plan` report text.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::App;
use crate::jobs::JobKind;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let report = app.current_data().and_then(|d| d.replay_report.as_deref());
    let loading = app.is_current_loading(JobKind::Replay);
    let body = match report {
        None if loading => Text::styled(
            format!("{} running replay…", app.spinner()),
            Style::default().fg(Color::Yellow),
        ),
        None => Text::styled(
            "no replay yet — press r to run",
            Style::default().fg(Color::DarkGray),
        ),
        Some(text) => Text::raw(text),
    };
    let para = Paragraph::new(body)
        .block(crate::ui::titled_block("Replay report"))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}
