//! Position-tool direct entry — the `--market-entry` / `--stop-entry` /
//! `--limit-entry` path.
//!
//! This is the *other* way to place a trade with tv-arm, and it shares almost
//! nothing with the pattern-arming flow. There's no plan, no engine rules, no
//! preps or vetos: the operator draws a long/short position tool, and its
//! entry/SL/TP go straight to the worker as a single signed enter that's
//! placed on receipt.
//!
//! Because it reads a *drawing property* (the position tool's tick distances)
//! rather than geometry a frozen spec could carry, this path is refused under
//! `--spec-in` — see the guard in [`crate::pipeline`].

use std::fs;

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context, Result};
use tracing::info;
use trade_control_cli as cli;
use trade_control_conventions::Broker;
use trade_control_core::sig::KEY_LEN;

use crate::args::{Args, PositionEntry};
use crate::broker_kind::broker_to_kind;
use crate::calendar::read_trade_expiry;
use crate::instrument_resolution::ResolvedInstrument;
use crate::pipeline::arm_out_dir;
use crate::plan_geometry::PlanGeometry;
use crate::position_trade::{core_direction, resolve_levels};
use crate::register_post::post_intent_blocking;
use crate::roles::Roles;

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
    roles: &Roles,
    resolved: &ResolvedInstrument,
    instrument: &str,
    account: &str,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<i32> {
    let Some(pos) = roles.position.as_ref() else {
        eprintln!(
            "ERROR: --{}-entry was set but no long/short position tool is drawn on the chart.",
            match mode {
                PositionEntry::Market => "market",
                PositionEntry::Stop => "stop",
                PositionEntry::Limit => "limit",
            }
        );
        return Ok(1);
    };

    // Tick-distance SL/TP → absolute prices. tick_size is the per-broker
    // catalog value (NOT pip_size — see position_trade docs).
    let levels = resolve_levels(pos, resolved.precision.tick_size)?;

    // Expiry: a drawn trade-expiry line wins; otherwise now + flag hours.
    let trade_expiry = match read_trade_expiry(&PlanGeometry::from_roles(roles)) {
        Ok(t) => t,
        Err(_) => now + chrono::Duration::hours(i64::from(args.expiry_hours)),
    };

    let kind = match mode {
        PositionEntry::Market => cli::PositionEntryKind::Market,
        PositionEntry::Stop => cli::PositionEntryKind::Stop,
        PositionEntry::Limit => cli::PositionEntryKind::Limit,
    };
    let direction = core_direction(pos.direction);

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
