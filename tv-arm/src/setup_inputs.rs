//! Everything an arm needs that came **from the chart**, in one value.
//!
//! `run` has two halves. The first reads TradingView and the economic calendar;
//! the second builds, signs, and registers a plan. This type is the seam: it is
//! the *complete* output of the first half, so the second half can run against a
//! `SetupInputs` that came from a live chart **or** from a frozen file, with no
//! way to tell the difference.
//!
//! ## Why "complete" is the load-bearing word
//!
//! A `SetupInputs` that is missing a field doesn't fail — it arms a **different
//! trade**, quietly. That has already happened three times inside
//! [`PlanGeometry`] alone (`MwPath.runup_start`, `sr_levels`, `MwPath.anchors`),
//! every time because the field fed a *gate* rather than a *trigger*, so
//! dropping it produced plausible numbers and green tests. The same risk applies
//! at this level, one layer up.
//!
//! Two things keep it honest:
//!
//! - **`granularity` is here, not on [`PlanGeometry`].** A chart resolution is
//!   not geometry, but it feeds `TrendlineCross.bar_seconds`, and trendline
//!   prices interpolate in **bar-index** space — so the same neckline read at H1
//!   vs H4 gives *different prices at the same instant* (measured: 1.116667 vs
//!   1.123333 on identical anchors, ~67 pips, no error raised). A frozen setup
//!   that forgot it would reprice the whole neckline on rebuild.
//! - **`chart_symbol` is broker-qualified** (`TRADENATION:EURUSD`, never a bare
//!   `EURUSD`), because a bare TradingView symbol silently resolves to the OANDA
//!   feed. A capture off the wrong feed is plausible and invisible.
//!
//! ## What is deliberately NOT here
//!
//! `Roles` — the raw `Drawing`s. Only the position-tool path still reads them
//! (`--market-entry`/`--stop-entry`/`--limit-entry`), and that path is
//! *inherently* live-chart: a position tool's SL/TP are TradingView drawing
//! properties with no frozen equivalent. Keeping `Roles` out of this struct is
//! what makes "a frozen arm cannot use the position tools" a **type** fact
//! rather than a documented promise.

use trade_control_conventions::Broker;

use crate::control_windows::{AsOf, ControlWindows};
use crate::instrument_resolution::ResolvedInstrument;
use crate::plan_geometry::PlanGeometry;
use crate::precision::EffectivePrecision;

/// The chart-derived half of an arm.
///
/// Produced by reading TradingView (live) or a frozen spec file; consumed by the
/// plan-building half, which cannot tell which. See the module doc.
#[derive(Debug, Clone)]
pub struct SetupInputs {
    /// The drawn setup, as plain data.
    pub geom: PlanGeometry,
    /// Calendar-derived pause/news windows, already pruned against
    /// [`Self::prune_as_of`].
    pub control: ControlWindows,
    /// Catalog resolution of the chart symbol — broker symbol, asset id, news
    /// currencies, precision.
    pub resolved: ResolvedInstrument,
    /// Broker-canonical instrument (`EUR/USD` for TN, `EUR_USD` for OANDA).
    pub instrument: String,
    /// Which broker's feed and account this arms against.
    pub broker: Broker,
    /// Live-TV-preferred pip/tick, falling back to the catalog.
    pub effective: EffectivePrecision,
    /// The chart resolution string (`60`, `240`, `D`). **Load-bearing** — see
    /// the module doc on bar-index interpolation.
    pub resolution: String,
    /// Broker-**qualified** TradingView symbol (`TRADENATION:EURUSD`). Recorded
    /// qualified on purpose — see the module doc.
    pub chart_symbol: String,
    /// Bare symbol, used only to name the on-disk output directory.
    pub raw_symbol: String,
    /// `--start` (journaling cursor) as an epoch, when given.
    pub start: Option<i64>,
    /// The as-of instant elapsed control windows were pruned against.
    pub prune_as_of: AsOf,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::plan_geometry::{Anchor, Line};

    /// Resolves through the REAL catalog rather than a hand-built stub —
    /// `ResolvedInstrument` holds a `&'static Asset`, so there is no way to fake
    /// one, and going through `resolve_for_broker` means this helper exercises
    /// the same path `run` does.
    pub(crate) fn sample() -> SetupInputs {
        let resolved =
            crate::instrument_resolution::resolve_for_broker("OANDA:EURUSD", Broker::Oanda)
                .expect("EURUSD is in the baseline catalog");
        SetupInputs {
            geom: PlanGeometry {
                neckline: Some(Line {
                    a: Anchor::new(1000, 1.10),
                    b: Anchor::new(2000, 1.12),
                }),
                ..Default::default()
            },
            control: ControlWindows::empty(),
            instrument: resolved.broker_symbol.clone(),
            resolved,
            broker: Broker::Oanda,
            effective: EffectivePrecision {
                pip_size: 0.0001,
                tick_size: 0.00001,
                tick_from_tv: true,
            },
            resolution: "60".into(),
            chart_symbol: "OANDA:EURUSD".into(),
            raw_symbol: "EURUSD".into(),
            start: None,
            prune_as_of: AsOf::wallclock(chrono::Utc::now()),
        }
    }

    /// The chart symbol must stay broker-qualified all the way through.
    ///
    /// A bare `EURUSD` silently resolves to the OANDA feed on TradingView, so a
    /// TradeNation capture that dropped the prefix would be scored against the
    /// wrong price data — plausible numbers, wrong answer, no error anywhere.
    #[test]
    fn the_chart_symbol_is_broker_qualified() {
        let s = sample();
        assert!(
            s.chart_symbol.contains(':'),
            "chart_symbol must keep its exchange prefix: {}",
            s.chart_symbol
        );
    }

    /// The resolution is carried, not inferred. It feeds
    /// `TrendlineCross.bar_seconds`, and trendline prices interpolate in
    /// bar-index space — so an arm that re-read it from a live chart left on
    /// another timeframe would reprice the whole neckline.
    #[test]
    fn the_resolution_is_carried_not_inferred() {
        let s = sample();
        assert_eq!(s.resolution, "60");
        // Nothing in `geom` can supply it — that's exactly why it lives here.
        assert!(s.geom.neckline.is_some(), "geometry has anchors …");
        // …but anchors are (epoch, price); the BAR SIZE is not recoverable from
        // them, so a frozen setup must carry it explicitly.
    }
}
