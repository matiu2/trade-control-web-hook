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
    println!("entered: trade_id={trade_id} — {}", resp.trim());
    Ok(0)
}
