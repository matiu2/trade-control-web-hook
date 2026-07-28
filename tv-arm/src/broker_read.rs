//! Live broker reads performed at arm time.
//!
//! Both are deliberately **blocking** and both **hard-error** rather than
//! falling back to a guess, because a guessed value here is silently wrong in a
//! way nothing downstream can detect:
//!
//! - the **spread** is baked onto the M/W enter so the worker can mid→bid/ask
//!   correct entry/SL/TP at fill time. A stale or defaulted spread mis-sizes
//!   every one of those.
//! - the **mid** is the `--pull-back` anchor — it *is* "price at arm time" by
//!   definition, so there is no sensible default at all.
//!
//! There is no operator override for either. See `frozen_setup` for why these
//! are re-read on every arm rather than frozen into a spec.

use color_eyre::eyre::{Context, Result};
use trade_control_conventions::Broker;

use crate::resolve_error::ResolveError;

/// Read the live broker spread (in pips) on a short-lived runtime.
///
/// `resolve_mw_trade` is sync (it's called from the sync `run`), but the
/// broker reads are async — so we spin a throwaway tokio runtime here,
/// the same bridge `auto_draw_calendar_lines` uses for its calendar
/// fetch. Any read failure (no token, network error, market closed,
/// degenerate spread) surfaces as a `Fatal` resolve error carrying the
/// actionable message from `spread::read_spread_pips`.
pub fn read_spread_blocking(
    broker: Broker,
    instrument: &str,
    pip_size: f64,
) -> std::result::Result<f64, ResolveError> {
    let runtime = tokio::runtime::Runtime::new()
        .context("starting tokio runtime for live spread read")
        .map_err(ResolveError::Fatal)?;
    runtime
        .block_on(crate::spread::read_spread_pips(
            broker, instrument, pip_size,
        ))
        .map_err(ResolveError::Fatal)
}

/// Blocking live **mid** read — the pullback prep's arm-time anchor. Same
/// runtime-bridge shape as [`read_spread_blocking`]; hard-errors on a
/// stale/degenerate quote so a bad anchor can't silently mis-fire the pullback.
pub fn read_mid_blocking(broker: Broker, instrument: &str) -> Result<f64> {
    let runtime =
        tokio::runtime::Runtime::new().context("starting tokio runtime for live mid read")?;
    runtime.block_on(crate::spread::read_mid(broker, instrument))
}
