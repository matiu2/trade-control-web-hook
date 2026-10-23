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
    let (report, label) = app.current_report();
    let loading = app.is_current_loading(JobKind::Replay)
        || app.is_current_loading(JobKind::RawReplay)
        || app.is_current_loading(JobKind::Rebless);

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
                "no replay yet — r re-arms from the chart, R replays the stored plan",
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

    // The title names WHICH report is on screen. Three different runs land in
    // this one pane — the re-armed replay (`R`), the stored-plan raw replay
    // (`r`) and a re-bless (`f`) — and they answer different questions, so a
    // generic "Replay report" would let an operator read a raw-replay number
    // as a re-armed one.
    let title = if max_scroll == 0 {
        format!(" {label} ")
    } else {
        format!(
            " {label} [{}/{}] — ↑↓/jk pgup/pgdn g/G ",
            scroll.saturating_add(1),
            max_scroll.saturating_add(1)
        )
    };

    let para = Paragraph::new(body)
        .block(crate::ui::titled_block(&title))
        .scroll((scroll, 0));
    f.render_widget(para, area);
}
