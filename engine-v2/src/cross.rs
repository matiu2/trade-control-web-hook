//! Pure trigger / level-cross evaluation.
//!
//! A **faithful port** of the old engine's `eval_trigger`, `level_crossed`,
//! `line_price_at`, `bar_index_at`, and `trigger_uses_close`
//! (`engine/src/evaluate.rs`). Ported verbatim (logic-for-logic) so the
//! break-and-close rule sees byte-identical cross decisions to the old engine.
//! The behaviour and every subtle edge (the buffered zone, the ordinal
//! bar-index interpolation, the `bar_seconds` fallback) are documented at their
//! old-engine source; this port preserves them exactly.

use trade_control_core::broker::Candle;
use trade_control_core::trade_plan::{BarEvent, CrossDir, LinePoint, Trigger};

/// Whether a trigger's evaluation reads the prior close (so `last_close` must
/// be tracked for it). Port of the old engine's `trigger_uses_close`.
pub fn trigger_uses_close(trigger: &Trigger) -> bool {
    matches!(
        trigger,
        Trigger::HorizontalCross {
            bar: BarEvent::OnClose,
            ..
        } | Trigger::PriceValueCross {
            bar: BarEvent::OnClose,
            ..
        } | Trigger::TrendlineCross {
            bar: BarEvent::OnClose,
            ..
        }
    )
}

/// Pure trigger evaluation against a single candle. Port of the old engine's
/// `eval_trigger`.
///
/// `prev_close` is the rule's last processed close (for `OnClose` crosses);
/// `None` on the seed bar, which never fires an `OnClose` cross. `window` is the
/// ascending bar series used to resolve a `TrendlineCross`'s level in bar-index
/// space (ignored by every other trigger). `buffer_pct` is the plan-level
/// cross-depth buffer.
///
/// The `TimeReached` / `MwEveryBar` / `PinePattern` arms are preserved from the
/// old engine for totality, but a break-and-close rule never carries them — it
/// is a level/trendline cross.
pub fn eval_trigger(
    trigger: &Trigger,
    candle: &Candle,
    prev_close: Option<f64>,
    window: &[Candle],
    buffer_pct: f64,
) -> bool {
    match trigger {
        Trigger::HorizontalCross { level, dir, bar }
        | Trigger::PriceValueCross { level, dir, bar } => {
            level_crossed(*level, *dir, *bar, candle, prev_close, buffer_pct)
        }
        Trigger::TrendlineCross {
            a,
            b,
            extend_forward,
            bar_seconds,
            dir,
            bar,
        } => {
            let Some(level) = line_price_at(a, b, candle, *extend_forward, *bar_seconds, window)
            else {
                return false;
            };
            level_crossed(level, *dir, *bar, candle, prev_close, buffer_pct)
        }
        Trigger::TimeReached { at_epoch } => candle.time.timestamp() >= *at_epoch,
        Trigger::MwEveryBar => true,
        // A `PinePattern` never reaches here in the old engine either (the entry
        // path special-cases it); returning `false` keeps this total. No
        // break-and-close rule carries it.
        Trigger::PinePattern { .. } => false,
    }
}

/// Evaluate a **horizontal price-level** cross — the no-projection path for a
/// [`PriceLevel`](trade_control_types_v2::PriceLevel) cap (`too_high`/`too_low`).
///
/// A [`Line`](trade_control_types_v2::Line) resolves its crossed price by bar-index
/// projection ([`line_price_at`]); a level *is* the price, so there is nothing to
/// project. This assembles a [`Trigger::HorizontalCross`] — the v1 arm that
/// [`eval_trigger`] routes straight to [`level_crossed`] with **no**
/// `line_price_at` call — and evaluates it. The window is genuinely unused by that
/// arm, so we pass an empty slice: a level cross has no dependence on the bar
/// series at all (unlike a trendline, whose level depends on where the bar sits in
/// index space).
///
/// The `buffer_pct` cross-depth buffer still applies (via `level_crossed`), so a
/// cap inherits the same near-miss protection a neckline gets — a one-tick graze
/// inside `[level ± buffer]` does not count as a cross. This is deliberate: an
/// invalidation cap should not trip on a liquidity wick any more than a break
/// should. The retest's time-decaying *closeness* tolerance is retest-specific and
/// does **not** apply here — an invalidation cross is strict (a cross is a cross).
///
/// - `level` — the cap price.
/// - `dir` — which side counts (`Up` for a short's too-high, `Down` for a long's
///   too-low; `Either` for a symmetric pcl-exhausted cap).
/// - `bar` — intrabar straddle vs a confirmed close.
/// - `prev_close` — the rule's last close, for an `OnClose` cap (`None` on seed).
/// - `buffer_pct` — the plan-level cross-depth buffer.
pub fn eval_level(
    level: f64,
    dir: CrossDir,
    bar: BarEvent,
    candle: &Candle,
    prev_close: Option<f64>,
    buffer_pct: f64,
) -> bool {
    let trigger = Trigger::HorizontalCross { level, dir, bar };
    // The window is unused by the `HorizontalCross` arm (no projection) — an empty
    // slice is correct, not a placeholder.
    eval_trigger(&trigger, candle, prev_close, &[], buffer_pct)
}

/// Did `candle` cross `level` in direction `dir` under the bar-event mode?
/// Port of the old engine's `level_crossed`.
///
/// `buffer_pct` widens the raw line into a zone `[level - buffer, level +
/// buffer]`. `0.0` reproduces the bare line exactly. See the old engine's
/// `level_crossed` doc comment for the full semantics (intrabar straddle,
/// OnClose zone-edge comparison).
pub(crate) fn level_crossed(
    level: f64,
    dir: CrossDir,
    bar: BarEvent,
    candle: &Candle,
    prev_close: Option<f64>,
    buffer_pct: f64,
) -> bool {
    let buffer = level * buffer_pct / 100.0;
    match bar {
        BarEvent::Intrabar => {
            let straddles = candle.l <= level && level <= candle.h;
            if !straddles {
                return false;
            }
            match dir {
                CrossDir::Either => true,
                CrossDir::Up => candle.h >= level + buffer,
                CrossDir::Down => candle.l <= level - buffer,
            }
        }
        BarEvent::OnClose => {
            let Some(prev) = prev_close else {
                return false;
            };
            let upper = level + buffer;
            let lower = level - buffer;
            let up = prev < upper && candle.c >= upper;
            let down = prev > lower && candle.c <= lower;
            match dir {
                CrossDir::Up => up,
                CrossDir::Down => down,
                CrossDir::Either => up || down,
            }
        }
    }
}

/// Interpolate a trendline's price at candle `t`, in bar-index space. Port of
/// the old engine's `line_price_at`.
pub(crate) fn line_price_at(
    a: &LinePoint,
    b: &LinePoint,
    t: &Candle,
    extend_forward: bool,
    bar_seconds: i64,
    window: &[Candle],
) -> Option<f64> {
    let ia = bar_index_at(a.at_epoch, window, bar_seconds)?;
    let ib = bar_index_at(b.at_epoch, window, bar_seconds)?;
    let it = bar_index_at(t.time.timestamp(), window, bar_seconds)?;
    if it > ib && !extend_forward {
        return None;
    }
    let span = ib - ia;
    if span == 0.0 {
        return Some(a.price);
    }
    let frac = (it - ia) / span;
    Some(a.price + (b.price - a.price) * frac)
}

/// Resolve an epoch to a (possibly fractional) bar index within `window`. Port
/// of the old engine's `bar_index_at`.
fn bar_index_at(epoch: i64, window: &[Candle], bar_seconds: i64) -> Option<f64> {
    if window.is_empty() {
        return None;
    }
    if let Some(i) = window.iter().position(|c| c.time.timestamp() == epoch) {
        return Some(i as f64);
    }
    let first = window[0].time.timestamp();
    let last = window[window.len() - 1].time.timestamp();
    if epoch < first {
        if bar_seconds <= 0 {
            return None;
        }
        return Some(-((first - epoch) as f64 / bar_seconds as f64));
    }
    if epoch > last {
        if bar_seconds <= 0 {
            return None;
        }
        let last_idx = (window.len() - 1) as f64;
        return Some(last_idx + (epoch - last) as f64 / bar_seconds as f64);
    }
    let hi = window.iter().position(|c| c.time.timestamp() > epoch)?;
    let lo = hi - 1;
    let lo_t = window[lo].time.timestamp();
    let hi_t = window[hi].time.timestamp();
    let frac = (epoch - lo_t) as f64 / (hi_t - lo_t) as f64;
    Some(lo as f64 + frac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    /// A candle with the given OHLC at a fixed (irrelevant) time — a level cross
    /// does not read the time, which is the whole point of the no-projection path.
    fn candle(o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle {
            time: Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap(),
            o,
            h,
            l,
            c,
        }
    }

    /// [`eval_level`] fires with an **empty window** — proving it never touches the
    /// bar series (no `line_price_at`). An intrabar `Up` cap at 1.30 is crossed by
    /// a bar whose high straddles and pierces it.
    #[test]
    fn eval_level_crosses_without_projection() {
        // High reaches 1.305, straddling 1.30 from below — an Up cross, no window.
        let bar = candle(1.28, 1.305, 1.279, 1.30);
        assert!(
            eval_level(1.30, CrossDir::Up, BarEvent::Intrabar, &bar, None, 0.0),
            "an Up cap is crossed with zero window — no projection dependency",
        );
        // A bar entirely below the cap does not cross it.
        let below = candle(1.28, 1.295, 1.279, 1.29);
        assert!(
            !eval_level(1.30, CrossDir::Up, BarEvent::Intrabar, &below, None, 0.0),
            "a bar that never reaches the cap does not cross",
        );
    }

    /// The `cross_buffer_pct` near-miss protection applies to a level cap exactly
    /// as it does to a line: a graze that reaches the bare level but not past the
    /// buffer zone does **not** count. Buffer = 0.1% of 1.30 = 0.0013, so the Up
    /// cross needs high ≥ 1.3013.
    #[test]
    fn eval_level_honours_cross_buffer() {
        // Grazes to exactly the bare level (1.30) — inside the buffer, no cross.
        let graze = candle(1.28, 1.30, 1.279, 1.30);
        assert!(
            !eval_level(1.30, CrossDir::Up, BarEvent::Intrabar, &graze, None, 0.1),
            "a graze to the bare level is inside the buffer ⇒ not a cross",
        );
        // Pierces past the buffer (high 1.302 > 1.3013) — a genuine cross.
        let pierce = candle(1.28, 1.302, 1.279, 1.301);
        assert!(
            eval_level(1.30, CrossDir::Up, BarEvent::Intrabar, &pierce, None, 0.1),
            "piercing past the buffer zone is a genuine cross",
        );
    }
}
