//! The replay screen (depth 2): the `tv-arm --start … replay` report text,
//! scrollable with arrows / vim / page / home / end (see `keys.rs`).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Text;
use ratatui::widgets::Paragraph;

use crate::app::{App, ReportKind};
use crate::jobs::JobKind;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let (report, label) = current_report(app);
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
    // this one pane — the re-armed replay (`r`), the stored-plan raw replay
    // (`R`) and a re-bless (`f`) — and they answer different questions, so a
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

/// Which report the pane shows, and what to call it.
///
/// Read from [`crate::app::PlanData::shown_report`], set by whichever job
/// landed most recently — deliberately NOT a fixed precedence over the three
/// `Option`s. A precedence looks equivalent and is not: with one, a re-bless
/// would shadow every later `r` for the rest of the session, so the operator
/// would re-run a replay and be shown the old re-bless under a confident
/// title. Recording which one landed last is the only version that can be
/// right in both directions.
///
/// A selected report that has not run yet falls back to whatever HAS, so an
/// empty pane only ever means "nothing has run for this plan".
fn current_report(app: &App) -> (Option<&str>, &'static str) {
    let Some(d) = app.current_data() else {
        return (None, "Replay report");
    };
    let chosen = match d.shown_report {
        ReportKind::Rebless => (d.rebless_report.as_deref(), "Re-bless report"),
        ReportKind::RawReplay => (
            d.raw_replay_report.as_deref(),
            "Raw replay report (stored plan)",
        ),
        ReportKind::Replay => (d.replay_report.as_deref(), "Replay report (re-armed)"),
    };
    if chosen.0.is_some() {
        return chosen;
    }
    // Fall back to any report that exists, so the pane is never blank while
    // the plan holds something worth reading.
    for (body, label) in [
        (d.replay_report.as_deref(), "Replay report (re-armed)"),
        (
            d.raw_replay_report.as_deref(),
            "Raw replay report (stored plan)",
        ),
        (d.rebless_report.as_deref(), "Re-bless report"),
    ] {
        if body.is_some() {
            return (body, label);
        }
    }
    (None, "Replay report")
}
