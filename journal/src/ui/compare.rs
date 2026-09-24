//! The compare screen (depth 3): the **replay-vs-live divergence diff** — the
//! bug-hunting headline — over the raw side-by-side.
//!
//! Layout (top → bottom):
//! 1. a one-line **divergence summary** band (`✓ matched · ⚠ live-only · ⚠
//!    replay-only · Δ timing`), green when clean, red/yellow when divergent;
//! 2. the **divergence detail**: live-only fires (`←live`), replay-only fires
//!    (`replay→`), and timing diffs (`Δ rule: live … vs replay …`), or a green
//!    "no divergence" line when everything agrees;
//! 3. the raw **replay ‖ live** side-by-side, kept for context.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use crate::app::App;
use crate::divergence::{Divergences, diff, live_fires, parse_replay_fires, parse_replay_outcome};
use crate::timeline::parse_events;

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // summary band
            Constraint::Min(6),     // divergence detail
            Constraint::Length(12), // raw side-by-side
        ])
        .split(area);

    let data = app.current_data();
    let replay = data.and_then(|d| d.replay_report.as_deref());
    let timeline = data.and_then(|d| d.timeline_json.as_deref());

    match (replay, timeline) {
        (Some(replay), Some(timeline)) => {
            let live = live_fires(timeline);
            let replay_fires = parse_replay_fires(replay);
            let div = diff(&live, &replay_fires);
            let outcome = parse_replay_outcome(replay);
            render_summary(f, rows[0], &div, live.len(), replay_fires.len(), &outcome);
            render_detail(f, rows[1], &div);
        }
        _ => {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "diff needs both replay + timeline (drill through Replay first)",
                    Style::default().fg(Color::DarkGray),
                )),
                rows[0],
            );
        }
    }

    render_side_by_side(f, app, rows[2]);
}

/// The one-line summary band: the four divergence counts, coloured by health.
fn render_summary(
    f: &mut Frame,
    area: Rect,
    div: &Divergences,
    live_n: usize,
    replay_n: usize,
    outcome: &crate::divergence::ReplayOutcome,
) {
    let clean = div.is_clean();
    let matched = div.matches.len();
    let counts_agree = live_n == replay_n;
    let all_good = clean && counts_agree;

    let base = if all_good { Color::Green } else { Color::Red };
    let warn = if all_good {
        Color::Green
    } else {
        Color::Yellow
    };

    let mut spans = vec![
        Span::styled(
            format!("✓ {matched} matched"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  ·  "),
        Span::styled(
            format!("⚠ {} live-only", div.live_only.len()),
            Style::default().fg(if div.live_only.is_empty() {
                Color::DarkGray
            } else {
                base
            }),
        ),
        Span::raw("  ·  "),
        Span::styled(
            format!("⚠ {} replay-only", div.replay_only.len()),
            Style::default().fg(if div.replay_only.is_empty() {
                Color::DarkGray
            } else {
                base
            }),
        ),
        Span::raw("  ·  "),
        Span::styled(
            format!("Δ {} timing", div.timing.len()),
            Style::default().fg(if div.timing.is_empty() {
                Color::DarkGray
            } else {
                warn
            }),
        ),
    ];
    // Outcome-level facts as a trailing note (fires count + final phase).
    if let Some(fires) = outcome.fires {
        let phase = outcome.final_phase.as_deref().unwrap_or("?");
        spans.push(Span::styled(
            format!("   │  replay: {fires} fires, phase {phase}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The divergence detail list: one line per live-only / replay-only / timing
/// divergence, or a single green "no divergence" line when clean.
fn render_detail(f: &mut Frame, area: Rect, div: &Divergences) {
    let mut items: Vec<ListItem> = Vec::new();

    for ff in &div.live_only {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                "←live    ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                fire_label(&ff.rule_id, &ff.action),
                Style::default().fg(Color::Red),
            ),
            Span::styled(
                format!("  @ {}", ff.ts),
                Style::default().fg(Color::DarkGray),
            ),
        ])));
    }
    for ff in &div.replay_only {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                "replay→  ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                fire_label(&ff.rule_id, &ff.action),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("  @ {}", ff.ts),
                Style::default().fg(Color::DarkGray),
            ),
        ])));
    }
    for delta in &div.timing {
        let (rule_id, live_ts, replay_ts) = (&delta.rule_id, &delta.live_ts, &delta.replay_ts);
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                "Δ timing ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(rule_id.clone()),
            Span::styled(
                format!("  live {live_ts}{}", observed(delta.lateness_secs)),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("  vs replay {replay_ts}"),
                Style::default().fg(Color::Magenta),
            ),
        ])));
    }

    if items.is_empty() {
        items.push(ListItem::new(Span::styled(
            "✓ no divergence — replay and live fired the same rules on the same bars",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )));
    }

    let list = List::new(items).block(crate::ui::titled_block("Divergence (replay vs live)"));
    f.render_widget(list, area);
}

/// How the cron's observation of the bar is annotated next to the live bar:
/// `+14s` for ordinary jitter, `+3h30m` for a rule that fired deep inside its
/// bar, and `-59m` when the tick landed BEFORE the bar closed — the
/// forming-bar signature, the one case a same-bar match is still a divergence.
/// Empty when the bundle carries no granularity to measure against.
fn observed(lateness_secs: Option<i64>) -> String {
    match lateness_secs {
        None => String::new(),
        Some(secs) => format!(" ({})", signed_duration(secs)),
    }
}

/// `-59m39s`, `+3h30m`, `+14s` — sign always shown, units trimmed to the two
/// most significant so the line stays narrow.
fn signed_duration(secs: i64) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.unsigned_abs();
    let (h, m, s) = (a / 3600, (a % 3600) / 60, a % 60);
    match (h, m) {
        (0, 0) => format!("{sign}{s}s"),
        (0, _) => format!("{sign}{m}m{s}s"),
        _ => format!("{sign}{h}h{m}m"),
    }
}

/// A compact `rule_id (action)` label for a fire.
fn fire_label(rule_id: &str, action: &Option<String>) -> String {
    match action {
        Some(a) => format!("{rule_id} ({a})"),
        None => rule_id.to_string(),
    }
}

/// The raw replay report ‖ live timeline, kept below the diff for context.
fn render_side_by_side(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    render_replay(f, app, cols[0]);
    render_timeline(f, app, cols[1]);
}

fn render_replay(f: &mut Frame, app: &App, area: Rect) {
    let report = app.current_data().and_then(|d| d.replay_report.as_deref());
    let body = match report {
        None => Text::styled("(no replay yet)", Style::default().fg(Color::DarkGray)),
        Some(text) => Text::raw(text),
    };
    let para = Paragraph::new(body)
        .block(crate::ui::titled_block("Replay (simulated)"))
        .wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_timeline(f: &mut Frame, app: &App, area: Rect) {
    let json = app.current_data().and_then(|d| d.timeline_json.as_deref());
    let items: Vec<ListItem> = match json {
        None => vec![ListItem::new("(no timeline)")],
        Some(json) => parse_events(json)
            .iter()
            .map(|e| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", e.ts), Style::default().fg(Color::DarkGray)),
                    Span::raw(format!("{} {}", e.marker, e.text)),
                ]))
            })
            .collect(),
    };
    let list = List::new(items).block(crate::ui::titled_block("Live (recorded)"));
    f.render_widget(list, area);
}

#[cfg(test)]
mod compare_tests {
    use super::*;

    #[test]
    fn the_wall_clock_is_shown_next_to_the_live_bar() {
        // The point of the annotation: you can always see WHEN the cron hit,
        // even though the comparison itself is on bars.
        assert_eq!(observed(Some(14)), " (+14s)");
        assert_eq!(observed(Some(0)), " (+0s)");
    }

    #[test]
    fn a_fire_before_its_bar_closed_reads_as_negative() {
        // The forming-bar signature, as recorded on AUD/NZD: the cron ticked
        // 21s into an H1 bar, i.e. 59m39s BEFORE it closed.
        assert_eq!(observed(Some(-3579)), " (-59m39s)");
    }

    #[test]
    fn a_long_lateness_is_trimmed_to_two_units() {
        // A news rule firing 3.5h into its H4 bar must not widen the line.
        assert_eq!(observed(Some(12633)), " (+3h30m)");
    }

    #[test]
    fn nothing_is_shown_when_the_lateness_is_unmeasurable() {
        assert_eq!(observed(None), "");
    }
}
