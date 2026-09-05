//! The persistent info bar: instrument · tf · broker │ entry-mode (order type) │
//! entry-ts │ outcome │ fixture, plus the arm-time screenshot link. Drawn on
//! every non-list screen from the plan's cached `PlanDetail` +
//! timeline-derived facts, plus the scanned fixture corpus
//! (see [`crate::fixtures`]).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use trade_control_core::screenshot::ScreenshotUrl;

use crate::app::App;
use crate::plan::{EntryMode, PlanDetail};
use crate::timeline::{derive_entry_ts, derive_outcome};

/// Total height the info bar needs: two borders + the facts row, plus one more
/// row when the current plan carries a screenshot URL.
///
/// Shares [`screenshot_url`] with [`render`] so the reserved height and the
/// rendered content can't disagree — a mismatch would either clip the link or
/// leave a blank row.
pub fn height(app: &App) -> u16 {
    if screenshot_url(app).is_some() { 4 } else { 3 }
}

/// The current plan's arm-time screenshot URL, if it has one.
fn screenshot_url(app: &App) -> Option<&ScreenshotUrl> {
    let plan = app.current_plan()?;
    app.data
        .get(&plan.trade_id)?
        .detail
        .as_ref()?
        .screenshot_url
        .as_ref()
}

pub fn render(f: &mut Frame, app: &App, area: Rect) {
    let Some(plan) = app.current_plan() else {
        return;
    };
    let data = app.data.get(&plan.trade_id);

    // Instrument display name via instrument-lookup, falling back to the raw id.
    let instrument = display_instrument(&plan.instrument);

    let mut spans = vec![
        Span::styled(instrument, Style::default().fg(Color::Cyan)),
        Span::raw(" · "),
        Span::styled(plan.granularity.clone(), Style::default().fg(Color::Blue)),
    ];

    if let Some(detail) = data.and_then(|d| d.detail.as_ref()) {
        // Broker — the one that loads the right TradingView exchange. Shown so a
        // TradeNation vs OANDA plan is obvious at a glance.
        if !detail.broker.is_empty() {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                detail.broker.clone(),
                Style::default().fg(Color::LightMagenta),
            ));
        }
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            detail.direction.clone(),
            dir_style(&detail.direction),
        ));
        spans.push(Span::raw("  │  "));
        spans.push(Span::styled(
            entry_mode_str(detail),
            Style::default().fg(Color::Magenta),
        ));
    }

    // Entry timestamp + outcome from the timeline.
    if let Some(tl) = data.and_then(|d| d.timeline_json.as_deref()) {
        if let Some(ts) = derive_entry_ts(tl) {
            spans.push(Span::raw("  │  entry "));
            spans.push(Span::styled(ts, Style::default().fg(Color::Green)));
        }
        let (outcome, ok) = derive_outcome(tl);
        spans.push(Span::raw("  │  "));
        spans.push(Span::styled(outcome, outcome_style(ok)));
    }

    // Whether this setup is already in the replay-fixtures corpus — the answer
    // to "have I saved this one?" without leaving the TUI to `ls`. Dimmed when
    // absent so a missing fixture reads as a quiet gap rather than an error.
    let fixture = app.current_fixture_status();
    spans.push(Span::raw("  │  "));
    spans.push(Span::styled(fixture.label(), fixture_style(&fixture)));

    let mut lines = vec![Line::from(spans)];
    // Second line: the arm-time TradingView screenshot, when the plan carries
    // one. Shown as plain text and opened with `o` — deliberately *not* an
    // OSC 8 terminal hyperlink. ratatui measures a span's width with
    // unicode-width over the raw string, so the escape bytes would be counted
    // as ~30 visible cells and smeared one-per-cell through the buffer; the
    // diff then splices cursor moves into the middle of the URL and truncation
    // leaves an unterminated sequence. See ratatui#902 and its own
    // `examples/hyperlink.rs`, whose workaround only fits a bespoke
    // single-line widget. `o` is the reliable mechanism.
    if let Some(url) = screenshot_url(app) {
        lines.push(Line::from(vec![
            Span::raw("screenshot "),
            Span::styled(
                url.to_string(),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            Span::styled("  (o to open)", Style::default().fg(Color::DarkGray)),
        ]));
    }

    let title = format!(" {} ", plan.trade_id);
    let block = crate::ui::titled_block(&title);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Format the entry mode + per-leg order types, e.g.
/// `strategy-v2 (BCR stop + QM limit)`. When a BCR-family plan is missing a prep
/// gate (`--skip-bcr` et al) the skip is flagged, e.g.
/// `normal [skip-bcr] (BCR stop)` — so a "normal" enter that skipped
/// break-and-close-then-retest isn't mislabelled as the full setup.
fn entry_mode_str(detail: &PlanDetail) -> String {
    let legs = detail
        .order_types
        .iter()
        .map(|(leg, ot)| format!("{leg} {}", ot.label()))
        .collect::<Vec<_>>()
        .join(" + ");
    // The skip flag only applies to families that carry a BCR leg.
    let has_bcr_leg = matches!(detail.entry_mode, EntryMode::Normal | EntryMode::StrategyV2);
    let skip = if has_bcr_leg {
        detail
            .bcr_preps
            .skip_slug()
            .map(|s| format!(" [{s}]"))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let base = detail.entry_mode.label();
    if legs.is_empty() {
        format!("{base}{skip}")
    } else {
        format!("{base}{skip} ({legs})")
    }
}

/// Resolve the operator-facing display name for an instrument id via
/// instrument-lookup; fall back to the raw id if unknown or the catalog errors
/// (a malformed user overlay). Plans carry OANDA-style (`AUD_CAD`) or
/// TradeNation-style (`GBP/USD`) ids, so try both broker views.
fn display_instrument(raw: &str) -> String {
    use instrument_lookup::{Broker, by_broker_symbol};
    for broker in [Broker::Oanda, Broker::TradeNation] {
        if let Ok(Some(asset)) = by_broker_symbol(broker, raw) {
            return asset.display_name.clone();
        }
    }
    raw.to_string()
}

fn dir_style(direction: &str) -> Style {
    match direction {
        "long" => Style::default().fg(Color::Green),
        "short" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::Gray),
    }
}

/// Green when a capture covers this setup, dim grey when none does. "No
/// fixture" is a normal state for most plans — it's a prompt to press `s`, not
/// a problem — so it must not compete with the outcome for attention.
fn fixture_style(status: &crate::fixtures::Status) -> Style {
    if status.is_saved() {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn outcome_style(ok: bool) -> Style {
    if ok {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PlanData;
    use crate::plan::{BcrPreps, PlanRow};

    fn app_with(screenshot_url: Option<&str>) -> App {
        let row = PlanRow {
            trade_id: "t-1".into(),
            account: "demo".into(),
            instrument: "EUR_USD".into(),
            granularity: "h1".into(),
            phase: None,
            shadow: false,
            archived_at: None,
            watermark: None,
        };
        let mut app = App::from_rows(vec![row]);
        let detail = PlanDetail {
            trade_id: "t-1".into(),
            instrument: "EUR_USD".into(),
            direction: "short".into(),
            granularity: "h1".into(),
            armed_at: None,
            entry_mode: EntryMode::Normal,
            order_types: Vec::new(),
            bcr_preps: BcrPreps {
                break_and_close: true,
                retest: true,
            },
            broker: "oanda".into(),
            screenshot_url: screenshot_url.and_then(ScreenshotUrl::parse),
        };
        app.data.insert(
            "t-1".to_string(),
            PlanData {
                detail: Some(detail),
                ..Default::default()
            },
        );
        app
    }

    /// The reserved height must track the screenshot line, or the link is
    /// clipped (too short) / a blank row appears (too tall).
    #[test]
    fn height_grows_by_one_row_for_a_screenshot() {
        assert_eq!(height(&app_with(None)), 3, "no URL keeps the old height");
        assert_eq!(
            height(&app_with(Some("https://www.tradingview.com/x/pM2uDdC2/"))),
            4,
            "a URL adds exactly one row"
        );
    }

    /// The height and the rendered content read the *same* accessor, so they
    /// can't disagree about whether there's a line to show.
    #[test]
    fn height_and_content_agree_on_presence() {
        let with = app_with(Some("https://www.tradingview.com/x/pM2uDdC2/"));
        assert!(screenshot_url(&with).is_some());
        assert_eq!(height(&with), 4);

        // A junk URL is dropped at parse time, so neither the line nor the row
        // appears — the two stay in step.
        let junk = app_with(Some("https://evil.example.com/pwn"));
        assert!(screenshot_url(&junk).is_none());
        assert_eq!(height(&junk), 3);
    }
}
