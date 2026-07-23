//! The persistent info bar: instrument · tf · broker │ entry-mode (order type) │
//! entry-ts │ outcome. Drawn on every non-list screen from the plan's cached
//! `PlanDetail` + timeline-derived facts.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::plan::PlanDetail;
use crate::timeline::{derive_entry_ts, derive_outcome};

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

    let title = format!(" {} ", plan.trade_id);
    let block = crate::ui::titled_block(&title);
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// Format the entry mode + per-leg order types, e.g.
/// `strategy-v2 (BCR stop + QM limit)`.
fn entry_mode_str(detail: &PlanDetail) -> String {
    let legs = detail
        .order_types
        .iter()
        .map(|(leg, ot)| format!("{leg} {}", ot.label()))
        .collect::<Vec<_>>()
        .join(" + ");
    if legs.is_empty() {
        detail.entry_mode.label().to_string()
    } else {
        format!("{} ({legs})", detail.entry_mode.label())
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

fn outcome_style(ok: bool) -> Style {
    if ok {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    }
}
