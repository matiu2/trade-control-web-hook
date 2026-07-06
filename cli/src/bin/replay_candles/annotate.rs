//! Draw each replayed *filled* position onto the live TradingView chart.
//!
//! After a replay, every filled enter ([`report::resolve_fire`] →
//! [`FireResult`]) becomes a native TradingView **position tool** — a
//! long/short risk-reward bracket from the fill price to its stop and
//! take-profit — spanning the fill bar to the exit bar (or the window end,
//! for a still-open trade). This turns the text journal into the visual
//! position zones the operator studies on the chart. The tool's own built-in
//! stats (R:R, P&L) and the green/red zones convey the outcome; the text
//! journal carries the detail, so no on-chart label is drawn.
//!
//! Why the native position tool (this replaced two rectangles): the tool
//! *can* be created via tv-mcp after all — `createShape` returns a Promise
//! that must be **awaited** (the old fire-and-forget path saw `null` and
//! wrongly concluded it no-ops), and its stop/profit are set as tick
//! offsets, which the bridge derives from the live series mintick.
//!
//! Re-run hygiene: the position tool has no text to tag, so every drawing
//! this makes is tracked by **entity-id in a sidecar manifest**
//! ([`manifest_path`]). A later run reads the manifest, removes exactly those
//! ids, then rewrites it — leaving the operator's hand-drawn necklines / fibs
//! / H&S anchors untouched.

use std::fs;
use std::path::PathBuf;

use color_eyre::eyre::{Result, WrapErr};
use trade_control_core::intent::Direction;
use trade_control_engine::TradePlan;
use trading_view::mcp::{Position, PositionSide, TvMcp};

use super::brisbane::bne;
use super::replay::Replay;
use super::report::{self, FillKind, FireResult};

/// Take-profit / long tint (TradingView's default long-green).
const LONG_COLOR: &str = "#26a69a";
/// Stop-loss / short tint (TradingView's default short-red).
const SHORT_COLOR: &str = "#ef5350";
/// Muted tint for a *not-taken* trade (never filled / declined) — grey, so
/// the operator can tell at a glance it never went on.
const UNFILLED_COLOR: &str = "#787b86";
/// Zone transparency for taken positions (0 opaque … 100 invisible). Light
/// tint so the candles underneath stay readable.
const ZONE_TRANSPARENCY: u8 = 80;
/// Fainter still for a not-taken trade — it's only the *intended* bracket.
const UNFILLED_TRANSPARENCY: u8 = 90;

/// Where the entity-ids of the drawings this run makes are recorded, so the
/// next run can remove exactly those (the position tool has no text field to
/// tag). Under `~/.config/trade-control/` alongside the worker configs.
fn manifest_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").wrap_err("HOME not set — can't locate config dir")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("trade-control")
        .join("replay-annotations.json"))
}

/// Read the entity-ids recorded by a prior run. A missing/unreadable/empty
/// manifest yields an empty list — nothing to clear.
fn read_manifest() -> Vec<String> {
    let path = match manifest_path() {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    match fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Persist the entity-ids this run drew so the next run can clear them.
fn write_manifest(ids: &[String]) -> Result<()> {
    let path = manifest_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).wrap_err("creating config dir for annotation manifest")?;
    }
    let json = serde_json::to_string_pretty(ids)?;
    fs::write(&path, json).wrap_err("writing annotation manifest")?;
    Ok(())
}

/// Clear prior replay annotations, then draw the replayed positions from
/// `replay` onto the chart `mcp` points at. Returns the number of positions
/// drawn. With `include_unfilled`, the not-taken enters (never-filled pending
/// orders, declined entries) are drawn too — as muted brackets anchored at the
/// fire bar — otherwise only the taken ones.
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

    let mut drawn_ids = Vec::new();
    for pos in &positions {
        draw_position(mcp, pos, &mut drawn_ids)?;
    }
    write_manifest(&drawn_ids)?;
    Ok(positions.len())
}

/// Remove every drawing whose entity-id the prior run recorded in the sidecar
/// manifest. Returns the count removed. Ids that already vanished (operator
/// deleted them by hand) are skipped without error.
fn clear_prior(mcp: &TvMcp) -> Result<usize> {
    let mut removed = 0usize;
    for id in read_manifest() {
        match mcp.remove_drawing(&id) {
            Ok(r) if r.removed => removed += 1,
            Ok(_) => tracing::debug!(id = %id, "prior annotation already gone"),
            Err(err) => tracing::warn!(id = %id, %err, "remove failed"),
        }
    }
    Ok(removed)
}

/// Draw one position as a native position tool spanning the fill bar to the
/// exit (or window end). A *taken* trade gets the green/red bracket; a
/// *not-taken* one (never-filled / declined) gets a muted-grey bracket at the
/// fire bar, since it never went on. The tool's entity-id is pushed into `ids`
/// for the sidecar manifest.
fn draw_position(mcp: &TvMcp, pos: &FireResult, ids: &mut Vec<String>) -> Result<()> {
    let taken = pos.kind.is_taken();
    let (color, transparency) = box_style(taken, pos.direction);
    let side = match pos.direction {
        Direction::Long => PositionSide::Long,
        Direction::Short => PositionSide::Short,
    };

    let position = mcp.draw_position_tool(&Position {
        time1: pos.fill_at.timestamp(),
        time2: pos.until.timestamp(),
        entry: pos.entry_price,
        stop_loss: pos.stop_loss,
        take_profit: pos.take_profit,
        direction: side,
        color,
        transparency,
    })?;
    if let Some(id) = position.entity_id {
        ids.push(id);
    } else {
        tracing::warn!(direction = ?pos.direction, "position tool did not land");
    }

    let shape = if taken {
        "position tool"
    } else {
        "muted (not taken)"
    };
    tracing::info!(
        outcome = outcome_label(pos.kind),
        direction = ?pos.direction,
        from = %bne(pos.fill_at),
        "drew position ({shape})"
    );
    Ok(())
}

/// The (tint colour, zone transparency) for a position. A *taken* trade gets
/// its directional green/red tint at the normal transparency; a *not-taken*
/// one gets a muted-grey bracket, fainter still, so it reads as "intended,
/// never went on".
fn box_style(taken: bool, direction: Direction) -> (&'static str, u8) {
    if !taken {
        return (UNFILLED_COLOR, UNFILLED_TRANSPARENCY);
    }
    let color = match direction {
        Direction::Long => LONG_COLOR,
        Direction::Short => SHORT_COLOR,
    };
    (color, ZONE_TRANSPARENCY)
}

/// Short outcome label stamped next to a position so the operator can read a
/// position's fate straight off the chart.
fn outcome_label(kind: FillKind) -> &'static str {
    match kind {
        FillKind::Open => "open",
        FillKind::StoppedOut => "SL",
        FillKind::TookProfit => "TP",
        FillKind::ClosedOnReversal => "reversal",
        FillKind::NeverFilled => "no-fill",
        FillKind::Declined => "declined",
        FillKind::SpreadBlackout => "spread",
        FillKind::GateBlocked => "gate-blocked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taken_positions_use_directional_tint_not_taken_are_muted() {
        let (c, t) = box_style(true, Direction::Long);
        assert_eq!(
            (c, t),
            (LONG_COLOR, ZONE_TRANSPARENCY),
            "taken long = green"
        );
        let (c, t) = box_style(true, Direction::Short);
        assert_eq!(
            (c, t),
            (SHORT_COLOR, ZONE_TRANSPARENCY),
            "taken short = red"
        );

        let (c, t) = box_style(false, Direction::Long);
        assert_eq!(c, UNFILLED_COLOR, "not-taken muted");
        assert!(t > ZONE_TRANSPARENCY, "not-taken is fainter than taken");
    }

    #[test]
    fn outcome_label_covers_every_kind() {
        assert_eq!(outcome_label(FillKind::Open), "open");
        assert_eq!(outcome_label(FillKind::StoppedOut), "SL");
        assert_eq!(outcome_label(FillKind::TookProfit), "TP");
        assert_eq!(outcome_label(FillKind::NeverFilled), "no-fill");
        assert_eq!(outcome_label(FillKind::Declined), "declined");
        assert_eq!(outcome_label(FillKind::ClosedOnReversal), "reversal");
        assert_eq!(outcome_label(FillKind::SpreadBlackout), "spread");
        assert_eq!(outcome_label(FillKind::GateBlocked), "gate-blocked");
    }

    #[test]
    fn manifest_path_is_under_config_trade_control() {
        // SAFETY: single-threaded test; sets HOME only for this assertion.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
        }
        let p = manifest_path().expect("HOME set");
        assert_eq!(
            p,
            PathBuf::from("/home/tester/.config/trade-control/replay-annotations.json")
        );
    }

    #[test]
    fn read_manifest_missing_file_is_empty() {
        // SAFETY: single-threaded test.
        unsafe {
            std::env::set_var("HOME", "/nonexistent-home-for-replay-test");
        }
        assert!(read_manifest().is_empty(), "missing manifest → no ids");
    }
}
