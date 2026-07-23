//! The replay screen (depth 2): the `tv-arm --start … replay` report text,
//! scrollable with arrows / vim / page / home / end (see `keys.rs`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::jobs::JobKind;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let report = app.current_data().and_then(|d| d.replay_report.as_deref());
    let loading = app.is_current_loading(JobKind::Replay);

    // Only a loaded report scrolls; the loading / empty placeholders are one
    // line, so their clamp is trivially 0.
    let (body, total_lines) = match report {
        None if loading => (
            Text::styled(
                format!("{} running replay…", app.spinner()),
                Style::default().fg(Color::Yellow),
            ),
            0,
        ),
        None => (
            Text::styled(
                "no replay yet — press r to run",
                Style::default().fg(Color::DarkGray),
            ),
            0,
        ),
        Some(text) => (Text::raw(text), text.lines().count() as u16),
    };

    // Clamp the scroll so End (u16::MAX) pins to the last page — inner height
    // excludes the two border rows. Wrap is off so line counts are exact (the
    // report's long summary line can overflow horizontally; that's acceptable
    // for a monospaced report and keeps End correct — same as the detail popup).
    let inner_height = area.height.saturating_sub(2);
    let max_scroll = total_lines.saturating_sub(inner_height);
    let scroll = app.replay_scroll.min(max_scroll);

    let title = if max_scroll == 0 {
        " Replay report ".to_string()
    } else {
        format!(
            " Replay report [{}/{}] — ↑↓/jk pgup/pgdn g/G ",
            scroll.saturating_add(1),
            max_scroll.saturating_add(1)
        )
    };

    let para = Paragraph::new(body)
        .block(crate::ui::titled_block(&title))
        .scroll((scroll, 0));
    f.render_widget(para, area);
}
