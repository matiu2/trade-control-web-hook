//! Draw each replayed *filled* position onto the live TradingView chart.
//!
//! After a replay, every filled enter ([`report::resolve_fire`] →
//! [`FireResult`]) becomes two rectangles on the chart — a green box from
//! entry to take-profit and a red box from entry to stop-loss — spanning
//! the fill bar to the exit bar (or the window end, for a still-open
//! trade). This turns the text journal into the visual position zones the
//! operator studies on the chart.
//!
//! Why rectangles and not the native position tool: tv-mcp can't *create*
//! a `long_position`/`short_position` (TradingView's `createMultipointShape`
//! silently no-ops for it — it reports success but nothing lands), though
//! it reads them back fine. The rectangle lands cleanly, so a position is
//! drawn as two rectangles. See the `tvmcp_cannot_create_position_tool`
//! note.
//!
//! Re-run hygiene: every rectangle we draw is tagged with a `replay:`
//! text prefix. A later run clears only those (via `draw list` → `draw
//! get` per shape → match the prefix → `draw remove`), leaving the
//! operator's hand-drawn necklines / fibs / H&S anchors untouched.

use color_eyre::eyre::Result;
use trade_control_core::intent::Direction;
use trade_control_engine::TradePlan;
use trading_view::mcp::{Rect, TvMcp};

use super::brisbane::bne;
use super::replay::Replay;
use super::report::{self, FillKind, FireResult};

/// Text-prefix every annotation carries, so a later run finds and clears
/// only its own drawings.
const TAG_PREFIX: &str = "replay:";

/// Take-profit box colour (TradingView's default long-green).
const TP_COLOR: &str = "#26a69a";
/// Stop-loss box colour (TradingView's default short-red).
const SL_COLOR: &str = "#ef5350";
/// Muted box colour for a *not-taken* trade (never filled / declined) — grey,
/// so the operator can tell at a glance it never went on. Both legs share it.
const UNFILLED_COLOR: &str = "#787b86";
/// Fill transparency for taken-position boxes (0 opaque … 100 invisible).
/// Light tint so the candles underneath stay readable.
const BOX_TRANSPARENCY: u8 = 80;
/// Fainter still for a not-taken trade — it's only the *intended* bracket, so
/// it should sit visually behind the taken positions.
const UNFILLED_TRANSPARENCY: u8 = 90;

/// Clear prior replay annotations, then draw the replayed positions from
/// `replay` onto the chart `mcp` points at. Returns the number of positions
/// drawn (each is two rectangles). With `include_unfilled`, the not-taken
/// enters (never-filled pending orders, declined entries) are drawn too — as
/// muted boxes anchored at the fire bar — otherwise only the taken ones.
pub fn annotate(
    mcp: &TvMcp,
    plan: &TradePlan,
    replay: &Replay,
    include_unfilled: bool,
) -> Result<usize> {
    let cleared = clear_prior(mcp)?;
    if cleared > 0 {
        tracing::info!(cleared, "removed prior replay annotations");
    }

    let closes = report::collect_close_fires(replay);
    let resolve = |f: &_| {
        if include_unfilled {
            report::resolve_fire_any(plan, f, &closes)
        } else {
            report::resolve_fire(plan, f, &closes)
        }
    };
    let positions: Vec<FireResult> = replay.fires.iter().filter_map(resolve).collect();

    for pos in &positions {
        draw_position(mcp, plan, pos)?;
    }
    Ok(positions.len())
}

/// Remove every drawing whose text starts with [`TAG_PREFIX`]. Returns the
/// count removed. `draw list` only carries `{id, name=<kind>}` (not text),
/// so each shape is fetched with `draw get` to read its label.
fn clear_prior(mcp: &TvMcp) -> Result<usize> {
    let stubs = mcp.list_drawings()?;
    let mut removed = 0usize;
    for stub in stubs {
        let drawing = match mcp.get_drawing(&stub.id) {
            Ok(d) => d,
            // A shape that vanished between list and get is not our problem.
            Err(err) => {
                tracing::debug!(id = %stub.id, %err, "skipping undrawable shape");
                continue;
            }
        };
        if drawing.label().starts_with(TAG_PREFIX) {
            match mcp.remove_drawing(&stub.id) {
                Ok(r) if r.removed => removed += 1,
                Ok(_) => tracing::warn!(id = %stub.id, "remove reported not-removed"),
                Err(err) => tracing::warn!(id = %stub.id, %err, "remove failed"),
            }
        }
    }
    Ok(removed)
}

/// Draw one position as an entry→TP box and an entry→SL box, spanning the
/// fill bar to the exit (or window end). A *taken* trade gets the green/red
/// zones; a *not-taken* one (never-filled / declined) gets two muted grey
/// boxes at the fire bar, since it never went on. Both rectangles carry the
/// same `replay:` tag so the next run clears them.
fn draw_position(mcp: &TvMcp, plan: &TradePlan, pos: &FireResult) -> Result<()> {
    let from = pos.fill_at.timestamp();
    let to = pos.until.timestamp();
    let tag = annotation_tag(plan, pos);
    let taken = pos.kind.is_taken();
    let (tp_color, sl_color, transparency) = box_style(taken);

    mcp.draw_rectangle(&Rect {
        time1: from,
        price1: pos.entry_price,
        time2: to,
        price2: pos.take_profit,
        color: tp_color,
        transparency,
        text: &tag,
    })?;
    mcp.draw_rectangle(&Rect {
        time1: from,
        price1: pos.entry_price,
        time2: to,
        price2: pos.stop_loss,
        color: sl_color,
        transparency,
        text: &tag,
    })?;
    let shape = if taken {
        "entry→TP green, entry→SL red"
    } else {
        "intended bracket, muted (not taken)"
    };
    tracing::info!(
        tag = %tag,
        direction = ?pos.direction,
        from = %bne(pos.fill_at),
        "drew position ({shape})"
    );
    Ok(())
}

/// The (TP-box colour, SL-box colour, transparency) for a position. A *taken*
/// trade gets the green TP / red SL zones at the normal tint; a *not-taken*
/// one gets two muted grey boxes, fainter still, so it reads as "intended,
/// never went on".
fn box_style(taken: bool) -> (&'static str, &'static str, u8) {
    if taken {
        (TP_COLOR, SL_COLOR, BOX_TRANSPARENCY)
    } else {
        (UNFILLED_COLOR, UNFILLED_COLOR, UNFILLED_TRANSPARENCY)
    }
}

/// Short outcome label baked into the annotation tag so the operator can
/// read a position's fate straight off the drawing.
fn outcome_label(kind: FillKind) -> &'static str {
    match kind {
        FillKind::Open => "open",
        FillKind::StoppedOut => "SL",
        FillKind::TookProfit => "TP",
        FillKind::ClosedOnReversal => "reversal",
        FillKind::NeverFilled => "no-fill",
        FillKind::Declined => "declined",
    }
}

/// The `replay:<trade_id>:<side>:<outcome>:<fill-bar>` tag carried by a
/// position's two rectangles. Greppable prefix (for re-run clearing) +
/// unique per fill bar (so two fills in one plan don't collide), and the
/// side/outcome make the label self-describing on the chart.
fn annotation_tag(plan: &TradePlan, pos: &FireResult) -> String {
    let side = match pos.direction {
        Direction::Long => "long",
        Direction::Short => "short",
    };
    format!(
        "{TAG_PREFIX}{}:{side}:{}:{}",
        plan.trade_id,
        outcome_label(pos.kind),
        bne(pos.fill_at)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use trade_control_engine::{Granularity, TradePlan};

    fn empty_plan(trade_id: &str) -> TradePlan {
        TradePlan {
            trade_id: trade_id.to_string(),
            instrument: "NZD_CHF".to_string(),
            direction: Direction::Short,
            granularity: Granularity::M15,
            pip_size: 0.0001,
            rules: vec![],
            shadow: false,
        }
    }

    fn filled(fill_secs: i64, kind: FillKind) -> FireResult {
        FireResult {
            direction: Direction::Short,
            fill_at: Utc.timestamp_opt(fill_secs, 0).unwrap(),
            until: Utc.timestamp_opt(fill_secs + 1800, 0).unwrap(),
            entry_price: 0.46322,
            stop_loss: 0.46367,
            take_profit: 0.46171,
            kind,
        }
    }

    #[test]
    fn tag_is_prefixed_unique_and_self_describing() {
        let plan = empty_plan("trade-046");
        let a = annotation_tag(&plan, &filled(1_781_244_000, FillKind::StoppedOut));
        let b = annotation_tag(&plan, &filled(1_781_245_800, FillKind::StoppedOut));
        assert!(a.starts_with(TAG_PREFIX), "{a}");
        assert!(a.contains("trade-046"), "{a}");
        assert!(a.contains("short"), "side in tag: {a}");
        assert!(a.contains(":SL:"), "outcome in tag: {a}");
        assert_ne!(a, b, "different fill bars must produce different tags");
    }

    #[test]
    fn taken_positions_are_green_red_not_taken_are_muted() {
        let (tp, sl, t) = box_style(true);
        assert_eq!((tp, sl), (TP_COLOR, SL_COLOR), "taken = green TP / red SL");
        assert_eq!(t, BOX_TRANSPARENCY);

        let (tp, sl, t) = box_style(false);
        assert_eq!(tp, UNFILLED_COLOR, "not-taken TP muted");
        assert_eq!(sl, UNFILLED_COLOR, "not-taken SL muted");
        assert!(t > BOX_TRANSPARENCY, "not-taken is fainter than taken");
    }

    #[test]
    fn outcome_label_covers_every_kind() {
        assert_eq!(outcome_label(FillKind::Open), "open");
        assert_eq!(outcome_label(FillKind::StoppedOut), "SL");
        assert_eq!(outcome_label(FillKind::TookProfit), "TP");
        assert_eq!(outcome_label(FillKind::NeverFilled), "no-fill");
        assert_eq!(outcome_label(FillKind::Declined), "declined");
    }
}
