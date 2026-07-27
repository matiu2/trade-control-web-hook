//! Resolving an **M/W** (double-top / double-bottom) setup into a signed trade
//! spec.
//!
//! Extracted from `pipeline.rs` unchanged. M/W and H&S diverge completely at
//! resolution: M/W has no invalidation line, no TP fib, and no prep drawings —
//! direction and geometry come from the path anchors alone, and the worker
//! computes entry/SL/TP from the baked `MwSpec` params.
//!
//! The gates here are the interesting part, and they are all *rejections of the
//! operator's drawing* rather than internal errors:
//!
//! - **anchor count** must be 3 or 4. An over-long path is refused rather than
//!   truncated — reading the first four anchors of a 5-anchor drawing arms a
//!   different pattern than the one on screen (see `MwPath::anchors`).
//! - **structure**: the runup leg must exceed the retrace leg, else it isn't an
//!   M/W shape.
//! - **neckline retracement depth**, capped at 40% by default (50% behind
//!   `--allow-50-pct-m-trades`).
//! - **right shoulder** (the optional 4th anchor) must be on the correct side of
//!   the neckline and satisfy the 1.3 alignment of the shorter shoulder.
//!
//! Everything reads [`PlanGeometry`], never `Roles`, so a frozen-spec re-arm
//! takes the identical path.

use chrono::{DateTime, Utc};
use color_eyre::eyre::eyre;
use tracing::info;
use trade_control_cli as cli;
use trade_control_conventions::{Broker, Direction};

use crate::args::Args;
use crate::broker_kind::broker_to_kind;
use crate::broker_read::read_spread_blocking;
use crate::calendar::read_trade_expiry;
use crate::hs_resolve::round5;
use crate::mw_geometry;
use crate::plan_geometry::PlanGeometry;
use crate::resolve_error::ResolveError;

/// M/W path: direction and geometry come from the 3-anchor path drawing
/// (`mw_path`), not from the H&S drawing constellation. Required
/// drawings are just the path + the trade-expiry line.
///
/// This is the live wrapper: it runs the cheap chart guards first (so an
/// operator chart mistake fails fast without a network round-trip), then
/// reads the broker spread live and delegates to
/// [`resolve_mw_trade_with_spread`] for the geometry gates + baking. The
/// pure inner fn is what the unit tests drive.
pub fn resolve_mw_trade(
    args: &Args,
    geom: &PlanGeometry,
    instrument: &str,
    account: &str,
    broker: Broker,
    catalog_pip: f64,
    catalog_tick: f64,
) -> std::result::Result<(Direction, cli::TradeSpec), ResolveError> {
    // --pip-size / --tick-size override the canonical catalog values when set.
    let pip_size = args.pip_size.unwrap_or(catalog_pip);
    let tick_size = args.tick_size.unwrap_or(catalog_tick);
    check_mw_required(geom)?;
    // The arm-time broker spread is read live (OANDA /pricing or the
    // TradeNation chart endpoint) and baked into the enter intent so the
    // worker can mid→bid/ask correct entry/SL/TP at fill time. There is
    // no operator override — a failed read hard-errors rather than bake a
    // guessed spread.
    let spread_pips = read_spread_blocking(broker, instrument, pip_size)?;
    resolve_mw_trade_with_spread(
        args,
        geom,
        instrument,
        account,
        broker,
        pip_size,
        tick_size,
        spread_pips,
    )
}

/// Cheap, offline guards every M/W arm needs before the live spread
/// read: exactly 3 path anchors and a trade-expiry line. Run first so a
/// fat-fingered chart fails without a network round-trip.
pub fn check_mw_required(geom: &PlanGeometry) -> std::result::Result<(), ResolveError> {
    let path = geom
        .mw_path
        .as_ref()
        .ok_or_else(|| eyre!("resolve_mw_trade called without an mw_path"))?;
    if !matches!(path.anchors, 3 | 4) {
        return Err(ResolveError::Reject(format!(
            "M/W path must have 3 anchors [A runup-start, B first-point, C neckline] or 4 \
             [+ D right-shoulder]; found {}",
            path.anchors
        )));
    }
    if geom.trade_expiry_epoch.is_none() {
        return Err(ResolveError::Reject(
            "missing required drawing for M/W:\n  - vertical_line labeled 'trade-expiry'\n".into(),
        ));
    }
    Ok(())
}

/// Pure M/W resolution given an already-read `spread_pips`: direction
/// from the anchors, structure + neckline-depth gates, then bakes the
/// static `MwSpec`. No I/O — the live spread read happens in the
/// [`resolve_mw_trade`] wrapper. Unit-tested directly.
#[allow(clippy::too_many_arguments)]
pub fn resolve_mw_trade_with_spread(
    args: &Args,
    geom: &PlanGeometry,
    instrument: &str,
    account: &str,
    broker: Broker,
    pip_size: f64,
    tick_size: f64,
    spread_pips: f64,
) -> std::result::Result<(Direction, cli::TradeSpec), ResolveError> {
    check_mw_required(geom)?;
    // The anchors come straight off `PlanGeometry` — the same plain-data path a
    // spec-driven arm uses — so the direction decision and the structure/retrace
    // gates below cannot diverge between a live arm and a rebuild. (This is why
    // `MwPath` must carry `runup_start`: it feeds direction and both gates,
    // though no *trigger* reads it.)
    let path = geom
        .mw_path
        .as_ref()
        .ok_or_else(|| eyre!("resolve_mw_trade_with_spread called without an mw_path"))?;
    let runup_start = path.runup_start;
    let first_point = path.first_point;
    let neckline = path.neckline;
    // Optional 4th anchor: the drawn right shoulder (arms immediately).
    let right_shoulder = path.right_shoulder;

    let direction = mw_geometry::mw_direction_from_anchors(runup_start, first_point)
        .ok_or_else(|| ResolveError::Reject(mw_flat_first_leg_msg(runup_start, first_point)))?;
    // Coarse "is this even an M/W shape" gate (runup leg > retrace leg).
    if let Err(e) = mw_geometry::check_mw_structure(runup_start, first_point, neckline) {
        return Err(ResolveError::Reject(format!("{e}\n")));
    }
    // Neckline-retracement depth gate.
    let pct = mw_geometry::neckline_retrace_pct(runup_start, first_point, neckline);
    if let Err(msg) = gate_neckline_pct(pct, args.allow_50_pct_m_trades) {
        return Err(ResolveError::Reject(msg));
    }
    // 4-point path: reject a drawing whose right shoulder is on the wrong
    // side of the neckline or breaks the 1.3 alignment of the shorter
    // shoulder. Drawing-level validity, so it fails arm here rather than
    // silently baking a bad geometry.
    if let Some(rs) = right_shoulder
        && let Err(e) = mw_geometry::validate_right_shoulder(first_point, neckline, rs)
    {
        return Err(ResolveError::Reject(format!("{e}\n")));
    }

    // The SL-vs-spread floor (hard limit) is enforced at build time in the
    // shared `cli::build_mw_pattern` chokepoint that this resolve feeds into,
    // and again at fire time in the worker against the live spread. Not
    // duplicated here — see `cli/src/trade_patterns.rs::build_mw_pattern`.

    let expiry = read_trade_expiry(geom)?;
    let pattern = match direction {
        Direction::Short => cli::TradePattern::M,
        Direction::Long => cli::TradePattern::W,
    };
    info!(
        direction = direction.as_str(),
        pattern = ?pattern,
        runup_start, first_point, neckline,
        right_shoulder = ?right_shoulder,
        retrace_pct = %format!("{:.1}%", pct * 100.0),
        spread_pips,
        pip_size,
        "M/W path resolved",
    );
    let spec = build_mw_trade_spec(
        args,
        instrument,
        account,
        broker,
        pattern,
        expiry,
        MwSpecAnchors {
            runup_start,
            first_point,
            neckline,
            right_shoulder,
            spread_pips,
            pip_size,
            tick_size,
        },
    );
    Ok((direction, spec))
}

/// The static M/W geometry baked into the signed enter intent — a
/// complete mirror of `cli::MwSpec`. `pip_size` is the canonical catalog
/// value (or the `--pip-size` override); `spread_pips` the arm-time
/// broker spread.
pub struct MwSpecAnchors {
    pub runup_start: f64,
    pub first_point: f64,
    pub neckline: f64,
    /// `D` — the optional drawn right shoulder (4-point path).
    pub right_shoulder: Option<f64>,
    pub spread_pips: f64,
    pub pip_size: f64,
    /// Canonical instrument tick size (or `--tick-size`), baked onto the enter
    /// so the worker snaps the mid-correct M/W prices onto the broker's grid.
    pub tick_size: f64,
}

/// Gate the neckline-retracement percentage. Default ceiling is
/// `< 40%`; `--allow-50-pct-m-trades` raises it to `<= 50%`; `> 50%` is
/// always rejected. A `NaN` pct (degenerate zero-runup path) is
/// rejected too.
pub fn gate_neckline_pct(pct: f64, allow_50: bool) -> std::result::Result<(), String> {
    if pct.is_nan() {
        return Err("M/W neckline retracement is undefined (zero-length runup leg)\n".into());
    }
    if pct > 0.50 {
        return Err(format!(
            "M/W neckline retracement {:.1}% exceeds the hard 50% ceiling — not a valid \
             reversal\n",
            pct * 100.0
        ));
    }
    if pct >= 0.40 && !allow_50 {
        return Err(format!(
            "M/W neckline retracement {:.1}% is >= 40% — pass --allow-50-pct-m-trades to arm a \
             marginal setup up to 50%\n",
            pct * 100.0
        ));
    }
    Ok(())
}

pub fn mw_flat_first_leg_msg(runup_start: f64, first_point: f64) -> String {
    format!(
        "M/W path has a flat first leg (A == B): runup_start={runup_start}, \
         first_point={first_point} — cannot infer direction\n"
    )
}

/// Build the M/W trade spec: no preps, single-shot, baked `MwSpec`. The
/// worker derives entry/SL/TP from the path geometry, so `tp_price` is a
/// placeholder the M/W build path ignores (it's `None` on the enter
/// intent).
pub fn build_mw_trade_spec(
    args: &Args,
    instrument: &str,
    account: &str,
    broker: Broker,
    pattern: cli::TradePattern,
    expiry: DateTime<Utc>,
    anchors: MwSpecAnchors,
) -> cli::TradeSpec {
    cli::TradeSpec {
        pattern,
        instrument: instrument.to_string(),
        account: account.to_string(),
        broker: broker_to_kind(broker),
        trade_expiry: expiry,
        risk_pct: args.risk_pct.unwrap_or(1.0),
        risk_amount: args.risk_amount,
        dry_run: args.broker_dry_run,
        // M/W is single-shot: a broker rejection of a placed order is
        // terminal (no re-entry).
        max_retries: 0,
        // Order expiry is governed by trade_expiry + the cancel/abort
        // vetos; the bar-count menu is an H&S feature.
        expiry_bars: None,
        skip_preps: Vec::new(),
        // Pullback is an H&S retest alternative; the M/W path has its own
        // geometry (cancel/abort/overshoot) and no retest, so no pullback.
        pull_back: None,
        entry_offset_pips: None,
        sl_offset_pips: None,
        // Both offset forms None → the shared builder applies the ATR-pct
        // default (DEFAULT_BUFFER_ATR_PCT). Unused on the M/W path (worker
        // computes geometry); the H&S enter inherits the volatility-scaled buffer.
        entry_offset_atr_pct: None,
        sl_offset_atr_pct: None,
        sl_anchor: None,
        // Worker computes the real TP (hard 1R); this field is unused on
        // the M/W build path. Set to the neckline as a harmless,
        // non-zero placeholder so any accidental serialization is sane.
        tp_price: round5(anchors.neckline),
        // M/W anchors SL via the worker-computed geometry, not an
        // absolute drawn stop.
        sl_price: None,
        entry_deadline_pct: 80,
        allow_entry: args.entry_filter_script.clone(),
        // M/W entry is always a stop order at the worker-computed level;
        // --entry-market is an H&S flag and is ignored here.
        entry_mode: cli::EntryMode::Stop,
        // Golden is a Head-and-Shoulders signal-candle concept; M/W entry is a
        // geometry-driven stop the worker resolves, so it never gates on golden.
        // (`--skip-golden` is an H&S-only lever and is irrelevant here.)
        needs_golden: false,
        needs_confirmed: args.require_confirmation,
        // No close-on-reversal for M/W (TP is a hard 1R), so news/SR
        // close coverage is not wired.
        close_on_news: false,
        sr_reversal_ranges: Vec::new(),
        veto_on_reversal: false,
        needs_confirmed_close: false,
        prep_expiries: Vec::new(),
        mw: Some(cli::MwSpec {
            neckline: anchors.neckline,
            first_point: anchors.first_point,
            runup_start: anchors.runup_start,
            right_shoulder: anchors.right_shoulder,
            spread_pips: anchors.spread_pips,
            pip_size: anchors.pip_size,
            tick_size: Some(anchors.tick_size),
        }),
        // Mirror the M/W pip onto the top-level field (the cli M/W builder
        // also does this); keeps the worker's sizing tail on the baked pip.
        pip_size: Some(anchors.pip_size),
        // Baked tick so the worker snaps the mid-correct M/W prices onto grid.
        tick_size: Some(anchors.tick_size),
        blackout_close: args.blackout_close.into_core(),
        // M/W has no fib / invalidation drawing — its abort/cancel/overshoot
        // vetos cover the level guards, so no continuous entry-level vetos.
        entry_level_vetos: Vec::new(),
        // M/W is out of scope for wrong-side stop recovery (it has no
        // EntrySpec — resolves via intent.mw). Keep today's behaviour.
        recover_entry: trade_control_core::intent::RecoverEntryAction::Skip,
        // strategy-v2 (dual stop + QM enter) is H&S-only.
        strategy_v2: false,
        // No QM leg on this path; default keeps the spec yaml byte-identical.
        qm_entry_mode: cli::EntryMode::Stop,
        // Break-even on at 50% by default; `--no-breakeven` opts out,
        // `--breakeven-pct` overrides. M/W honours it exactly like H&S — the
        // worker resolves the M/W geometry at fill, so the cron's snapshot has
        // a concrete entry/TP for the 50% level.
        breakeven_pct: if args.no_breakeven {
            None
        } else {
            Some(args.breakeven_pct.unwrap_or(0.5))
        },
        // Entry SL-spread floor window baked onto the enter; `None` → worker default (5).
        spread_window: args.spread_window,
    }
}

#[cfg(test)]
mod tests {
    //! Tests for M/W trade-spec resolution: the neckline-% gate, anchor-count
    //! validation, direction, and the geometry baked onto the spec.
    //!
    //! These lived in `pipeline`'s test module in two clusters separated by the
    //! plan-emission tests, both under a `// ===== M / W trade-spec resolution`
    //! marker that also covered H&S and news cases.

    use super::*;
    use crate::args::Args;
    use crate::plan_geometry::PlanGeometry;
    use crate::roles::Roles;
    use crate::test_drawings::{SPREAD, now, path, path_n, path4, vline};
    use clap::Parser;
    use trading_view::drawings::Drawing;

    /// Parse `Args` straight from an argv slice, as the binary would.
    fn mw_args(extra: &[&str]) -> Args {
        let mut argv = vec!["tv-arm"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).expect("parse mw args")
    }

    #[test]
    fn gate_neckline_pct_default_ceiling_is_40() {
        // < 40% passes without the flag.
        assert!(gate_neckline_pct(0.399, false).is_ok());
        // >= 40% needs the flag.
        assert!(gate_neckline_pct(0.40, false).is_err());
        assert!(gate_neckline_pct(0.499, false).is_err());
    }

    #[test]
    fn gate_neckline_pct_flag_raises_ceiling_to_50() {
        assert!(gate_neckline_pct(0.40, true).is_ok());
        assert!(gate_neckline_pct(0.499, true).is_ok());
        assert!(gate_neckline_pct(0.50, true).is_ok());
    }

    #[test]
    fn gate_neckline_pct_above_50_always_errors() {
        assert!(gate_neckline_pct(0.501, true).is_err());
        assert!(gate_neckline_pct(0.501, false).is_err());
    }

    #[test]
    fn gate_neckline_pct_nan_errors() {
        assert!(gate_neckline_pct(f64::NAN, true).is_err());
    }
    fn mw_roles(p: Drawing) -> Roles {
        Roles {
            mw_path: Some(p),
            trade_expiry: Some(vline("exp", now().timestamp() + 86_400)),
            ..Default::default()
        }
    }

    /// Drive the pure resolver with an injected spread — what the tests
    /// use in place of `resolve_mw_trade` (which now reads the spread
    /// live over the network).
    fn resolve(
        args: &Args,
        roles: &Roles,
        instrument: &str,
        broker: Broker,
        catalog_pip: f64,
    ) -> std::result::Result<(Direction, cli::TradeSpec), ResolveError> {
        let pip_size = args.pip_size.unwrap_or(catalog_pip);
        // Tests bake tick == pip (no separate catalog tick threaded here).
        resolve_mw_trade_with_spread(
            args,
            &PlanGeometry::from_roles(roles),
            instrument,
            "ms-tn-1",
            broker,
            pip_size,
            pip_size,
            SPREAD,
        )
    }

    #[test]
    fn resolve_mw_m_is_short_and_bakes_geometry() {
        // Worked M: A=1.1000, B=1.1200, C=1.1120 → pct 0.40 (needs flag).
        // No --pip-size, so the catalog pip (passed here) is baked.
        let roles = mw_roles(path("p", [1.1000, 1.1200, 1.1120]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        let (dir, spec) = match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Ok(v) => v,
            Err(_) => panic!("expected Ok"),
        };
        assert_eq!(dir, Direction::Short);
        assert_eq!(spec.pattern, cli::TradePattern::M);
        assert_eq!(spec.max_retries, 0);
        assert!(spec.prep_expiries.is_empty());
        let mw = spec.mw.expect("mw baked");
        assert!((mw.neckline - 1.1120).abs() < 1e-9);
        assert!((mw.first_point - 1.1200).abs() < 1e-9);
        assert!((mw.runup_start - 1.1000).abs() < 1e-9);
        // The injected live spread flows through to the baked intent.
        assert!((mw.spread_pips - SPREAD).abs() < 1e-9);
        // Catalog pip flows through unchanged.
        assert!((mw.pip_size - 0.0001).abs() < 1e-12);
        // ...and is mirrored onto the top-level spec field.
        assert_eq!(spec.pip_size, Some(0.0001));
    }

    #[test]
    fn resolve_mw_4point_bakes_right_shoulder() {
        // 4-point M: A=1.1000, B=1.1200, C=1.1120, D=1.1190 (valid: inside
        // the 1.3 ceiling of the shorter shoulder, same side as B).
        let roles = mw_roles(path4("p", [1.1000, 1.1200, 1.1120, 1.1190]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        let (dir, spec) = resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001)
            .expect("valid 4-point M resolves");
        assert_eq!(dir, Direction::Short);
        let mw = spec.mw.expect("mw baked");
        assert_eq!(mw.right_shoulder, Some(1.1190));
    }

    #[test]
    fn resolve_mw_4point_rejects_misaligned_right_shoulder() {
        // D=1.1300 breaks the 1.3 alignment (taller shoulder past the
        // ceiling of the shorter) → the drawing is rejected at arm.
        let roles = mw_roles(path4("p", [1.1000, 1.1200, 1.1120, 1.1300]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Err(ResolveError::Reject(msg)) => {
                assert!(msg.contains("1.3 alignment"), "msg = {msg}")
            }
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn resolve_mw_4point_rejects_wrong_side_right_shoulder() {
        // D=1.1100 sits below the neckline (wrong side for an M) → rejected.
        let roles = mw_roles(path4("p", [1.1000, 1.1200, 1.1120, 1.1100]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Err(ResolveError::Reject(msg)) => assert!(msg.contains("wrong side"), "msg = {msg}"),
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }
    #[test]
    fn resolve_mw_bakes_catalog_pip_when_no_override() {
        // A JPY-like catalog pip of 0.01 is baked when --pip-size is absent.
        let roles = mw_roles(path("p", [1.1000, 1.1200, 1.1180])); // pct 0.10
        let args = mw_args(&[]);
        let (_dir, spec) =
            resolve(&args, &roles, "USD_JPY", Broker::TradeNation, 0.01).expect("ok");
        assert!((spec.mw.expect("mw").pip_size - 0.01).abs() < 1e-12);
    }

    #[test]
    fn resolve_mw_pip_size_flag_overrides_catalog() {
        // --pip-size beats the catalog value passed in.
        let roles = mw_roles(path("p", [1.1000, 1.1200, 1.1180])); // pct 0.10
        let args = mw_args(&["--pip-size", "0.25"]);
        let (_dir, spec) =
            resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001).expect("ok");
        assert!((spec.mw.expect("mw").pip_size - 0.25).abs() < 1e-12);
    }

    #[test]
    fn resolve_mw_w_is_long() {
        // Worked W: A=1.1200, B=1.1000, C=1.1080 → pct 0.40 (needs flag).
        let roles = mw_roles(path("p", [1.1200, 1.1000, 1.1080]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        let (dir, spec) =
            resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001).expect("ok");
        assert_eq!(dir, Direction::Long);
        assert_eq!(spec.pattern, cli::TradePattern::W);
    }

    #[test]
    fn resolve_mw_rejects_40_pct_without_flag() {
        let roles = mw_roles(path("p", [1.1000, 1.1200, 1.1120])); // pct 0.40
        let args = mw_args(&[]); // no --allow-50-pct-m-trades
        match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Err(ResolveError::Reject(msg)) => {
                assert!(msg.contains("40%"), "msg = {msg}");
                assert!(msg.contains("--allow-50-pct-m-trades"), "msg = {msg}");
            }
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn check_mw_required_rejects_wrong_anchor_count() {
        // A 2-anchor path fails the cheap guard before any spread read.
        let roles = mw_roles(path_n("p", &[1.1, 1.12]));
        match check_mw_required(&PlanGeometry::from_roles(&roles)) {
            Err(ResolveError::Reject(msg)) => {
                assert!(
                    msg.contains("3 anchors") && msg.contains("found 2"),
                    "msg = {msg}"
                )
            }
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    /// A too-SHORT M/W path must still look like an M/W to the dispatcher.
    ///
    /// `geom.mw_path.is_some()` is the pattern discriminant in `run`. If a
    /// 2-anchor path extracted to `None` (the `?`-on-missing shape every other
    /// role in `from_roles` uses), a half-drawn M/W would fall through to the
    /// **H&S** branch and be reported as missing `fib_retracement (TP)` and
    /// `trend_line labeled 'neckline'` — drawings the operator was never making.
    /// They'd go looking for an H&S bug in an M/W setup.
    ///
    /// Caught for real: an earlier version of this refactor used `?` here, and
    /// `check_mw_required_rejects_wrong_anchor_count` failed with the internal
    /// `Fatal("called without an mw_path")` instead of the operator-facing
    /// "found 2".
    #[test]
    fn a_short_mw_path_still_dispatches_as_mw_not_hs() {
        let roles = Roles {
            mw_path: Some(path_n("p", &[1.1000, 1.1200])),
            trade_expiry: Some(vline("exp", now().timestamp() + 86_400)),
            ..Default::default()
        };
        let geom = PlanGeometry::from_roles(&roles);
        assert!(
            geom.mw_path.is_some(),
            "a short path must NOT vanish — that reroutes it to the H&S branch"
        );
        assert_eq!(geom.mw_path.as_ref().map(|p| p.anchors), Some(2));
        // …and the gate turns that count into an operator-facing rejection.
        match check_mw_required(&geom) {
            Err(ResolveError::Reject(msg)) => assert!(msg.contains("found 2"), "msg = {msg}"),
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    /// An over-long path is rejected too — and it is the ONLY case the four named
    /// anchor fields cannot represent, since extraction truncates to the first
    /// four. Without `MwPath::anchors` a 5-anchor drawing arms silently as if the
    /// operator had drawn a 4-anchor one: a different pattern than what's on
    /// screen, with no error.
    #[test]
    fn an_over_long_mw_path_is_rejected_not_truncated() {
        let roles = Roles {
            mw_path: Some(path_n("p", &[1.1000, 1.1200, 1.1120, 1.1190, 1.1250])),
            trade_expiry: Some(vline("exp", now().timestamp() + 86_400)),
            ..Default::default()
        };
        let geom = PlanGeometry::from_roles(&roles);
        assert_eq!(geom.mw_path.as_ref().map(|p| p.anchors), Some(5));
        match check_mw_required(&geom) {
            Err(ResolveError::Reject(msg)) => assert!(msg.contains("found 5"), "msg = {msg}"),
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn check_mw_required_accepts_four_anchor_path() {
        // A 4-anchor path (right shoulder drawn) passes the count guard.
        let roles = Roles {
            mw_path: Some(path4("p", [1.1000, 1.1200, 1.1120, 1.1190])),
            trade_expiry: Some(vline("exp", now().timestamp() + 86_400)),
            ..Default::default()
        };
        assert!(check_mw_required(&PlanGeometry::from_roles(&roles)).is_ok());
    }

    #[test]
    fn check_mw_required_rejects_missing_trade_expiry() {
        let roles = Roles {
            mw_path: Some(path("p", [1.1000, 1.1200, 1.1180])),
            // no trade_expiry
            ..Default::default()
        };
        match check_mw_required(&PlanGeometry::from_roles(&roles)) {
            Err(ResolveError::Reject(msg)) => assert!(msg.contains("trade-expiry"), "msg = {msg}"),
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn resolve_mw_rejects_bad_structure() {
        // retrace deeper than runup: A=1.1120, B=1.1200, C=1.1000.
        let roles = mw_roles(path("p", [1.1120, 1.1200, 1.1000]));
        let args = mw_args(&["--allow-50-pct-m-trades"]);
        match resolve(&args, &roles, "EUR_USD", Broker::TradeNation, 0.0001) {
            Err(ResolveError::Reject(msg)) => assert!(msg.contains("runup leg"), "msg = {msg}"),
            other => panic!("expected Reject, got {:?}", other.map(|_| ())),
        }
    }
}
