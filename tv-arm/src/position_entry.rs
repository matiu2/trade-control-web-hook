//! Position-tool direct entry — the `--market-entry` / `--stop-entry` /
//! `--limit-entry` path.
//!
//! This is the *other* way to place a trade with tv-arm, and it shares almost
//! nothing with the pattern-arming flow. There's no plan, no engine rules, no
//! preps or vetos: the operator draws a long/short position tool, and its
//! entry/SL/TP go straight to the worker as a single signed enter, dispatched
//! on receipt.
//!
//! # `Market` is fire-and-forget; `Stop`/`Limit` are cron-MANAGED
//!
//! `--market-entry` fills on receipt, so there is no resting order to look
//! after and its broker bracket covers it from the fill. It stays entirely
//! unmanaged, by design.
//!
//! `--stop-entry` / `--limit-entry` **rest unfilled**, which is a materially
//! different situation: price can trade through the drawn stop *before* the
//! order fills, killing the setup while the order sits there still able to fill
//! later on the way back. So `build_position_enter` gives those two kinds
//! `EntryDedup::GateOwned` + `max_retries: Static(1)`, which routes them
//! through the retry gate and writes the `EntryAttempt` row that every
//! attempt-keyed cron enumerates. See CLAUDE.md, "Manual entries".
//!
//! # Two sources, one trade
//!
//! This path used to be live-chart only, because it read a *drawing property*
//! (TradingView's tick distances) that no frozen spec could carry. Since
//! 2026-09-24 a spec CAN carry the trade, as absolute prices — see
//! [`crate::frozen_position`] for why local-chart's three-anchor tool has a
//! frozen equivalent when TradingView's does not.
//!
//! [`PositionSource`] resolves either source to the same triple — levels,
//! direction, trade-expiry — so everything below it is identical whichever
//! the operator armed from. The refusal in [`crate::pipeline`] narrowed
//! accordingly: a spec with no `position` still cannot use these flags.

use std::fs;

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use tracing::info;
use trade_control_cli as cli;
use trade_control_conventions::Broker;
use trade_control_core::sig::KEY_LEN;

use crate::args::{Args, PositionEntry};
use crate::broker_kind::broker_to_kind;
use crate::calendar::read_trade_expiry;
use crate::frozen_position::FrozenPosition;
use crate::instrument_resolution::ResolvedInstrument;
use crate::pipeline::arm_out_dir;
use crate::plan_geometry::PlanGeometry;
use crate::position_trade::{PositionLevels, core_direction, resolve_levels};
use crate::register_post::post_intent_blocking;
use crate::roles::{PositionDirection, Roles};

/// Where a position entry's numbers come from — a live TradingView drawing,
/// or a frozen spec.
///
/// Both are resolved to the SAME triple up front ([`Self::pick`]), so no
/// code below this point branches on the source. That is deliberate: the two
/// differ only in how the prices are *recovered* (tick offsets × `tick_size`
/// versus already-absolute), and letting that distinction travel any further
/// would mean every later step had to know about it.
///
/// ⚠️ The conversion asymmetry is the whole hazard. Running
/// [`resolve_levels`] on already-absolute prices — the "simplification" of
/// treating both sources alike — turns the operator's 19670.6 ESPIX entry
/// into 196.706, a plausible-looking number for a different instrument
/// entirely. [`FrozenPosition::levels`] takes no `tick_size` argument
/// precisely so that mistake cannot be written.
#[derive(Debug)]
pub(crate) struct PositionSource {
    pub levels: PositionLevels,
    pub direction: PositionDirection,
    /// The `PlanGeometry` the trade-expiry is read from. A live arm derives
    /// it from the drawings; a frozen arm already has it in the spec.
    pub geom: PlanGeometry,
}

impl PositionSource {
    /// Resolve whichever source is present.
    ///
    /// Prefers `roles` when both exist, which in practice never happens —
    /// `roles` is `Some` only on the live-chart path and `frozen` only on a
    /// spec path, and `SetupInputs` is built by exactly one of the two. The
    /// preference is stated rather than left to chance so that if the two
    /// ever DO meet, the live chart (the thing the operator is looking at)
    /// wins rather than a file that may be stale — the hazard in
    /// `[[arm-export-first-match-wins-is-a-hazard]]`.
    ///
    /// Having neither is the rejection, and it names both ways to fix it.
    /// `geom` is the frozen spec's own geometry, used only for its
    /// `trade_expiry_epoch` on the frozen branch — a static trade's spec
    /// carries the drawn trade-expiry there, exactly as a pattern spec does.
    /// Passing `PlanGeometry::default()` instead would drop it and make
    /// every frozen position entry fail the expiry check that follows.
    pub(crate) fn pick(
        roles: Option<&Roles>,
        frozen: Option<&FrozenPosition>,
        geom: &PlanGeometry,
        tick_size: f64,
    ) -> Result<Self> {
        if let Some(roles) = roles {
            let pos = roles.position.as_ref().ok_or_else(|| {
                eyre!(
                    "--market-entry / --stop-entry / --limit-entry need a long/short \
                     position tool drawn on the chart, and there is none"
                )
            })?;
            return Ok(Self {
                // Tick-distance SL/TP → absolute prices. `tick_size` is the
                // per-broker catalog value (NOT pip_size — see
                // `position_trade` docs).
                levels: resolve_levels(pos, tick_size)?,
                direction: pos.direction,
                geom: PlanGeometry::from_roles(roles),
            });
        }
        let frozen = frozen.ok_or_else(|| {
            eyre!(
                "--market-entry / --stop-entry / --limit-entry need a position to place, \
                 from either a live chart with a position tool drawn on it or a frozen \
                 setup carrying one"
            )
        })?;
        // Already absolute — see the type doc. No tick conversion.
        Ok(Self {
            levels: frozen.levels(),
            direction: frozen.direction(),
            geom: geom.clone(),
        })
    }
}

/// Position-tool direct entry. Read the drawn long/short position tool,
/// convert its tick-distance SL/TP to absolute prices via the catalog
/// `tick_size`, build + sign a naked enter, and POST it straight to the
/// worker (placed on receipt). Returns the process exit code: `1` for a
/// clean operator-facing rejection (no position drawn, stop/limit not
/// supported yet), propagated `Err` for a real failure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_position_entry(
    args: &Args,
    mode: PositionEntry,
    broker: Broker,
    source: PositionSource,
    resolved: &ResolvedInstrument,
    instrument: &str,
    account: &str,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<i32> {
    // Already resolved to absolute prices by `PositionSource::pick`,
    // whichever source they came from. See that type's doc for why the
    // tick conversion lives there and not here.
    let levels = source.levels;

    // Expiry: the drawn trade-expiry line, REQUIRED — no fallback.
    //
    // # Why there is no default here (v146)
    //
    // This used to fall back to `now + args.expiry_hours` (default 48) on ANY
    // read failure. That was safe only while the expiry was inert for a manual
    // entry: it bounded the enter's `not_after` (how long the *alert* stays
    // valid) and nothing swept the resting order.
    //
    // v145 made manual resting entries cron-managed, so the **sweep now
    // enforces this timestamp** — it cancels the resting order when the expiry
    // passes. A silently-defaulted expiry therefore silently cancels a live
    // order, and the operator never chose the number that did it. Worse, the
    // old `Err(_)` swallowed *every* failure mode identically: a chart with no
    // expiry line, a malformed one, and an out-of-range timestamp all became
    // "48 hours" with nothing logged.
    //
    // The three pattern call sites — `hs_resolve.rs`, `mw_resolve.rs` and the
    // `pipeline.rs` hint — all use `read_trade_expiry(geom)?` and REFUSE TO ARM
    // without a drawn line. This site was the only one that substituted a
    // guess, so requiring it makes manual entries consistent with every other
    // way of arming a trade rather than adding a new restriction.
    //
    // `--expiry-hours` is consequently dead for this path; it is retained on
    // `Args` only so an existing invocation carrying it still parses.
    let trade_expiry = read_trade_expiry(&source.geom).wrap_err(
        "draw a trade-expiry vertical on the chart before arming a manual entry. It is not \
         optional: the order sweep cancels this resting order when the expiry passes, so \
         guessing a default would silently cancel a live order at a time you never chose. \
         Pattern arming (H&S / M&W) already requires it.",
    )?;

    let kind = match mode {
        PositionEntry::Market => cli::PositionEntryKind::Market,
        PositionEntry::Stop => cli::PositionEntryKind::Stop,
        PositionEntry::Limit => cli::PositionEntryKind::Limit,
    };
    let direction = core_direction(source.direction);

    info!(
        instrument,
        direction = ?direction,
        mode = ?mode,
        entry = levels.entry,
        stop_loss = levels.stop_loss,
        take_profit = levels.take_profit,
        tick_size = resolved.precision.tick_size,
        trade_expiry = %trade_expiry.to_rfc3339(),
        "position-tool direct entry"
    );

    let spec = cli::PositionEnterSpec {
        instrument: instrument.to_string(),
        account: account.to_string(),
        broker: broker_to_kind(broker),
        direction,
        kind,
        entry_price: levels.entry,
        stop_loss: levels.stop_loss,
        take_profit: levels.take_profit,
        trade_expiry,
        risk_amount: args.risk_amount,
        pip_size: args.pip_size.or(Some(resolved.precision.pip_size)),
        tick_size: args.tick_size.or(Some(resolved.precision.tick_size)),
        // Futures only, and catalog-only — there is deliberately no
        // `--contract-multiplier` flag to override it with.
        contract_multiplier: resolved.precision.contract_multiplier,
        dry_run: args.broker_dry_run,
    };

    let (trade_id, signed_body) = match cli::build_position_enter(&spec, key, now) {
        Ok(v) => v,
        // Build/validation failure (bad geometry, sign error) — clean rejection.
        Err(e) => {
            eprintln!("ERROR: {e}");
            return Ok(1);
        }
    };

    // Persist the signed body for audit (same place pattern bundles land).
    let out_dir = arm_out_dir(instrument)?;
    let body_path = out_dir.join(format!("{trade_id}-enter.yaml"));
    fs::write(&body_path, &signed_body)
        .with_context(|| format!("writing {}", body_path.display()))?;

    // The whole point of the position path: POST straight to the worker,
    // which places the order on receipt.
    let resp = post_intent_blocking(signed_body).wrap_err("POST position enter to worker")?;
    info!(trade_id = %trade_id, worker_response = %resp.trim(), "position enter POSTed");
    for line in entry_confirmation(&trade_id, resp.trim(), args.broker_dry_run) {
        println!("{line}");
    }
    Ok(0)
}

/// Build the operator-facing confirmation lines for a placed position entry.
///
/// This path has no plan, no engine rules and no preps or vetos, so this line
/// is the *only* thing the operator sees at placement time — which is exactly
/// how `BUG-market-entry-no-broker-confirmation-trail.md` happened. There, an
/// entry the operator believed was open and managed had in fact never reached
/// the broker, and the discrepancy only surfaced nine days later via a raw
/// broker activity export.
///
/// Two things this line must not do:
///
/// * **Claim more than the worker said.** The worker answers a successful
///   dispatch with a flat `ok` (`worker/src/http.rs`'s `action_to_parts`) —
///   the broker order id lives in the persisted request record, not the
///   response body. So "accepted" is the honest word: the worker took the
///   order. Printing `entered:` overstated it.
/// * **Read identically for a dry run.** `--broker-dry-run` also returns a
///   2xx `ok`, so the previous line was byte-identical for a dry run and a
///   live placement.
///
/// It also names the `trade_id` and the command that resolves the remaining
/// question ("did it actually fill?") — `plan timeline` reads the request
/// records keyed by this `trade_id`, which is where the broker order id landed.
fn entry_confirmation(trade_id: &str, worker_response: &str, dry_run: bool) -> Vec<String> {
    if dry_run {
        return vec![format!(
            "DRY RUN — no order placed at the broker: trade_id={trade_id}"
        )];
    }
    vec![
        format!("accepted by worker: trade_id={trade_id} — {worker_response}"),
        // `plan timeline` takes the trade_id positionally. The binary is
        // installed per-environment under a suffixed name
        // (`trade-control-staging`, …), so name the suffix rather than a bare
        // `trade-control`, which no longer exists.
        format!("  confirm the fill with: trade-control-<env> plan timeline {trade_id}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frozen_position::{FrozenDirection, FrozenPosition};

    /// The operator's real ESPIX_EUR static trade, and the TradeNation tick
    /// size for it (0.1, from the instrument-lookup catalog).
    fn espix() -> FrozenPosition {
        FrozenPosition {
            direction: FrozenDirection::Long,
            entry: 19670.6,
            stop_loss: 19637.3,
            take_profit: 19910.3,
        }
    }
    const ESPIX_TICK: f64 = 0.1;

    /// **The money test.** A frozen position's prices are ABSOLUTE and must
    /// reach the order untouched.
    ///
    /// Found by mutation: making this branch multiply by `tick_size` — the
    /// "simplification" of treating both sources alike, and the single most
    /// likely future edit here — left all 523 tests green while turning a
    /// 19670.6 entry into 1967.06. Plausible number, wrong instrument's
    /// scale, no error anywhere.
    ///
    /// A non-1.0 tick is essential: with `tick_size == 1.0` the mutation is
    /// invisible.
    #[test]
    fn a_frozen_position_is_never_tick_converted() {
        assert_ne!(ESPIX_TICK, 1.0, "a 1.0 tick would hide the bug under test");
        let source =
            PositionSource::pick(None, Some(&espix()), &PlanGeometry::default(), ESPIX_TICK)
                .expect("a frozen position is a valid source");
        assert_eq!(source.levels.entry, 19670.6);
        assert_eq!(source.levels.stop_loss, 19637.3);
        assert_eq!(source.levels.take_profit, 19910.3);
        assert_eq!(source.direction, PositionDirection::Long);
    }

    /// The spec's own geometry travels through, because its
    /// `trade_expiry_epoch` is what the (mandatory, no-fallback) expiry
    /// check reads. Passing a default `PlanGeometry` here would make every
    /// frozen position entry fail that check.
    #[test]
    fn the_frozen_specs_trade_expiry_reaches_the_source() {
        let geom = PlanGeometry {
            trade_expiry_epoch: Some(1_790_424_000),
            ..PlanGeometry::default()
        };
        let source =
            PositionSource::pick(None, Some(&espix()), &geom, ESPIX_TICK).expect("valid source");
        assert_eq!(source.geom.trade_expiry_epoch, Some(1_790_424_000));
    }

    /// Neither source is the rejection, and it must name both ways to fix it
    /// rather than only the chart (which is how a spec-armed operator gets
    /// sent hunting for TradingView).
    #[test]
    fn no_source_at_all_is_refused_naming_both_doors() {
        let err = PositionSource::pick(None, None, &PlanGeometry::default(), ESPIX_TICK)
            .expect_err("nothing to place")
            .to_string();
        assert!(err.contains("live chart"), "{err}");
        assert!(err.contains("frozen setup"), "{err}");
    }

    use super::entry_confirmation;

    /// The line the operator reads at placement time must not claim the order
    /// reached the broker — the worker answers a successful dispatch with a
    /// flat `ok`, and the broker order id is not in that body. The old wording
    /// (`entered: …`) asserted a fill the CLI had no evidence for, which is
    /// how `BUG-market-entry-no-broker-confirmation-trail.md` began.
    #[test]
    fn live_entry_reports_acceptance_not_a_confirmed_fill() {
        let lines = entry_confirmation("pos-nzd-cad-37926360", "ok", false);
        let joined = lines.join("\n");
        assert!(
            !joined.contains("entered:"),
            "must not claim a fill the worker never confirmed: {joined:?}"
        );
        assert!(
            joined.contains("accepted by worker"),
            "should say what actually happened: {joined:?}"
        );
        // The trade_id is the key into `plan timeline`, where the broker order
        // id was recorded — the operator needs both to answer "did it fill?".
        assert!(joined.contains("pos-nzd-cad-37926360"));
        assert!(
            joined.contains("plan timeline"),
            "must point at the command that resolves the fill: {joined:?}"
        );
    }

    /// A dry run returns the same 2xx `ok` as a live placement, so before this
    /// the two printed byte-identical lines. They must be distinguishable.
    #[test]
    fn dry_run_is_visibly_different_from_a_live_placement() {
        let dry = entry_confirmation("pos-nzd-cad-37926360", "ok", true).join("\n");
        let live = entry_confirmation("pos-nzd-cad-37926360", "ok", false).join("\n");

        assert_ne!(dry, live, "a dry run must not read like a live placement");
        assert!(
            dry.contains("DRY RUN"),
            "the dry run must say so plainly: {dry:?}"
        );
        assert!(
            !dry.contains("accepted by worker"),
            "a dry run placed nothing, so it must not claim acceptance: {dry:?}"
        );
    }
}
