//! Plain-text content of each screen, for the clipboard copy (`c`).
//!
//! The renderers build ratatui `Line`s with styling; the clipboard wants the
//! **full** text (not just the visible viewport, and no colour). Rather than
//! scrape the rendered buffer, we re-derive the text from the same underlying
//! data + parsers the renderers use, so a copy always reflects the whole
//! content. Each function returns one `String` with `\n`-separated lines.

use serde_json::Value;

use crate::app::App;
use crate::divergence::{diff, live_fires, parse_replay_fires};
use crate::plan::PlanRow;
use crate::screen::Screen;
use crate::timeline::parse_events;

/// The plain text to copy for whatever is currently on screen. The detail popup
/// (`i`), if open, wins over the screen behind it.
pub fn current(app: &App) -> String {
    if app.show_popup {
        return detail(app);
    }
    match app.screen {
        Screen::List => list(app),
        Screen::Timeline => timeline(app),
        Screen::Replay => replay(app),
        Screen::Compare => compare(app),
    }
}

/// The `i` detail popup: the full pretty-printed `plan export` JSON.
fn detail(app: &App) -> String {
    app.current_data()
        .and_then(|d| d.export_json.as_deref())
        .map(pretty_json)
        .unwrap_or_else(|| "(no plan detail loaded)".to_string())
}

/// The plan list: every row (not just the on-screen ones), tab-aligned.
fn list(app: &App) -> String {
    let mut out = format!("Plans ({}) — oldest event first\n", app.plans.len());
    for p in &app.plans {
        out.push_str(&list_row(p));
        out.push('\n');
    }
    out
}

/// One list row as plain text — same columns the picker shows.
fn list_row(p: &PlanRow) -> String {
    let phase = p.phase.as_deref().unwrap_or("-");
    let archived = if p.is_archived() { "  ARCHIVED" } else { "" };
    let last_event = p.last_event().unwrap_or("—");
    format!(
        "{last_event}\t{}\t{}\t{}\t{phase}{archived}",
        p.trade_id, p.instrument, p.granularity
    )
}

/// The timeline: every parsed event as `ts marker text`.
fn timeline(app: &App) -> String {
    let Some(json) = app.current_data().and_then(|d| d.timeline_json.as_deref()) else {
        return "(timeline not loaded)".to_string();
    };
    let events = parse_events(json);
    if events.is_empty() {
        return "(no recorded events)".to_string();
    }
    let mut out = String::from("Timeline\n");
    for e in &events {
        out.push_str(&format!("{} {} {}\n", e.ts, e.marker, e.text));
    }
    out
}

/// The replay report — already plain text (ANSI-stripped at capture).
fn replay(app: &App) -> String {
    app.current_data()
        .and_then(|d| d.replay_report.clone())
        .unwrap_or_else(|| "(no replay report)".to_string())
}

/// The compare view: the divergence summary + detail, then the raw report and
/// live events side by side (as two stacked sections — the copy is linear text,
/// so we list them one after the other rather than in columns).
fn compare(app: &App) -> String {
    let data = app.current_data();
    let replay_txt = data.and_then(|d| d.replay_report.as_deref());
    let timeline_json = data.and_then(|d| d.timeline_json.as_deref());
    let (Some(replay_txt), Some(timeline_json)) = (replay_txt, timeline_json) else {
        return "(compare needs both replay + timeline)".to_string();
    };

    let live = live_fires(timeline_json);
    let replay_fires = parse_replay_fires(replay_txt);
    let div = diff(&live, &replay_fires);

    let mut out = String::new();
    out.push_str(&format!(
        "Divergence: {} matched · {} live-only · {} replay-only · {} timing\n\n",
        div.matches.len(),
        div.live_only.len(),
        div.replay_only.len(),
        div.timing.len(),
    ));
    if div.is_clean() {
        out.push_str("no divergence — replay matches live\n");
    } else {
        for f in &div.live_only {
            out.push_str(&format!(
                "←live only: {} {} @ {}\n",
                f.rule_id,
                action(f),
                f.ts
            ));
        }
        for f in &div.replay_only {
            out.push_str(&format!(
                "replay→ only: {} {} @ {}\n",
                f.rule_id,
                action(f),
                f.ts
            ));
        }
        for (rule_id, live_ts, replay_ts) in &div.timing {
            out.push_str(&format!(
                "Δ timing {rule_id}: live {live_ts} vs replay {replay_ts}\n"
            ));
        }
    }

    out.push_str("\n--- Live events (recorded) ---\n");
    out.push_str(&timeline(app));
    out.push_str("\n--- Replay report ---\n");
    out.push_str(replay_txt);
    out
}

/// The action of a fire fact, or an empty string.
fn action(f: &crate::divergence::FireFact) -> &str {
    f.action.as_deref().unwrap_or("")
}

/// Pretty-print single-line JSON; echo the input on parse failure. (Mirrors the
/// popup's own `pretty_json` — kept here so the copy text matches the view.)
fn pretty_json(s: &str) -> String {
    serde_json::from_str::<Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, PlanData};
    use crate::plan::{parse_plan_export, parse_plan_list};

    const LIST: &str = include_str!("../tests/fixtures/plan_list.yaml");
    const EXPORT: &str = include_str!("../tests/fixtures/plan_export.json");
    const TIMELINE: &str = include_str!("../tests/fixtures/plan_timeline.json");
    const REPLAY: &str = include_str!("../tests/fixtures/replay_report.txt");

    fn seeded_app(screen: Screen) -> App {
        let rows = parse_plan_list(LIST).unwrap();
        let mut app = App::from_rows(rows);
        app.select_to("hs-aud-cad-a07622da");
        app.seed_current(PlanData {
            detail: parse_plan_export(EXPORT).ok(),
            export_json: Some(EXPORT.to_string()),
            timeline_json: Some(TIMELINE.to_string()),
            replay_report: Some(REPLAY.to_string()),
            tv_loaded: true,
            max_depth: 3,
        });
        app.set_screen(screen);
        app
    }

    #[test]
    fn list_copies_every_row() {
        let app = seeded_app(Screen::List);
        let text = current(&app);
        assert!(text.starts_with("Plans ("));
        // Every plan id in the fixture appears — the whole list, not a viewport.
        for p in &app.plans {
            assert!(text.contains(&p.trade_id), "missing {}", p.trade_id);
        }
    }

    #[test]
    fn timeline_copies_all_events() {
        let app = seeded_app(Screen::Timeline);
        let text = current(&app);
        let events = parse_events(TIMELINE);
        assert!(!events.is_empty());
        // One line per event (plus the "Timeline" header).
        assert_eq!(text.lines().count(), events.len() + 1, "{text}");
    }

    #[test]
    fn replay_copies_full_report() {
        let app = seeded_app(Screen::Replay);
        let text = current(&app);
        assert!(text.contains("Done:"), "full report copied:\n{text}");
    }

    #[test]
    fn detail_popup_copies_pretty_json() {
        let mut app = seeded_app(Screen::Timeline);
        app.toggle_popup();
        let text = current(&app);
        // Pretty (multi-line) JSON of the export, not the single-line source.
        assert!(text.contains("trade_id"), "{text}");
        assert!(text.lines().count() > 5, "pretty-printed:\n{text}");
    }

    #[test]
    fn compare_copies_diff_and_both_sides() {
        let app = seeded_app(Screen::Compare);
        let text = current(&app);
        assert!(text.contains("Divergence:"), "{text}");
        assert!(text.contains("Live events"), "{text}");
        assert!(text.contains("Replay report"), "{text}");
    }
}
