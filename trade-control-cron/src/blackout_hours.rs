//! Daily market-hours blackout refresh.
//!
//! Once a day this resolves each actively-traded instrument's current-season
//! trading session into a set of UTC blackout [`NoEntryWindow`]s and writes
//! them to KV (`set_blackout_windows`). The `run_enter` reject gate and the
//! order sweep read those windows; this cron is the only writer.
//!
//! # Why daily, and why the window is recomputed not baked
//!
//! DST shifts the real close→open gap across the year. Re-reading TradeNation's
//! `market_info` each day gives the **current-season** London hours, which the
//! `tradenation-api` crate has already converted to Brisbane (the heavy
//! `chrono_tz Europe/London` math lives there, anchored to *today*). This cron
//! consumes those Brisbane strings and does pure modular arithmetic
//! ([`windows_from_session`]) to land UTC minute-of-day windows — so the WASM
//! worker links no timezone database. See the `market-hours-blackout-design`
//! memory and [`trade_control_core::intent::windows_from_session`].
//!
//! # Source of instruments, and TradeNation-only
//!
//! The instruments worth protecting are the ones we actually trade: the
//! distinct `(account, instrument)` pairs across the live `EntryAttempt` rows
//! **and** the registered `TradePlan`s (the engine's server-side setups). Only
//! TradeNation exposes `market_info`, so OANDA-scoped instruments are skipped
//! (OANDA has its own venue hours we don't model yet).
//!
//! # Fail-open, always
//!
//! A market with no close→open gap (24h), an unparseable session, a broker
//! error, or a resolve miss writes **no** windows for that instrument — never a
//! blackout invented from missing data. The ~26h TTL on each row means one
//! skipped daily run can't strand a stale window either.
//!
//! # Runtime-agnostic via the [`CronEnv`] seam
//!
//! Moved into `trade-control-cron` so both the wasm Cloudflare worker and the
//! native VM scheduler run the *same* refresh. The `&Env`-hidden broker
//! acquisition now travels through the [`CronEnv`] seam; the caller opens the
//! [`StateStore`] and passes it in. The `now.minute() < 15` once-per-hour gate
//! is the caller's job (it already wraps the NY-close apply in the same gate);
//! this fn only self-gates on the hour.

use std::collections::BTreeSet;

use chrono::{DateTime, Timelike, Utc};
use trade_control_core::intent::{Buffers, NoEntryWindow, windows_from_session};
use trade_control_core::state::StateStore;

use crate::broker_handle::BrokerHandle;
use crate::seam::CronEnv;

/// UTC hour at which the daily refresh runs (once per day). 06:00 UTC is a
/// quiet, fixed point well away from the NY-close-edge spread-blackout job
/// (21:00/22:00 UTC) so the two daily jobs don't contend for broker logins.
const REFRESH_HOUR_UTC: u32 = 6;

/// TTL for each written window set. Longer than a day (26h) so a single missed
/// daily run leaves yesterday's window in place rather than failing open
/// mid-session; two missed runs age it out (fail-open) as intended.
const WINDOW_TTL_SECONDS: u64 = 26 * 60 * 60;

/// Run the daily refresh iff `now` is the refresh hour's first tick. Mirrors
/// the NY-close-edge gating in `blackout_apply`: the caller's `now.minute() < 15`
/// check plus this hour check make it fire exactly once on the :00 tick of
/// [`REFRESH_HOUR_UTC`]. Safe to double-fire (idempotent overwrite), the gate
/// just avoids redundant broker calls.
pub async fn refresh_if_due<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    if now.hour() != REFRESH_HOUR_UTC {
        return;
    }
    refresh_market_hours(store, cron, now).await;
}

/// Enumerate the distinct `(account, instrument)` pairs we trade, resolve each
/// TradeNation instrument's session into UTC blackout windows, and write them.
/// Per-instrument failures log and continue — one bad market never blocks the
/// rest (the same per-row discipline the order sweep uses).
async fn refresh_market_hours<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    let pairs = match distinct_pairs(store).await {
        Ok(p) => p,
        Err(err) => {
            tracing::error!("blackout-hours: enumerate instruments: {err}");
            return;
        }
    };
    tracing::info!("blackout-hours: refreshing {} instrument(s)", pairs.len());

    for (account, instrument) in pairs {
        refresh_one(store, cron, account.as_deref(), &instrument, now).await;
    }
}

/// Distinct `(account, instrument)` pairs across live entry attempts and
/// registered trade plans. A `BTreeSet` dedups and gives stable ordering.
async fn distinct_pairs<S: StateStore>(
    store: &S,
) -> Result<BTreeSet<(Option<String>, String)>, trade_control_core::state::StateError> {
    let mut pairs = BTreeSet::new();
    for a in store.list_all_entry_attempts().await? {
        pairs.insert((a.account.clone(), a.instrument.clone()));
    }
    for p in store.list_all_trade_plans().await? {
        pairs.insert((p.account.clone(), p.plan.instrument.clone()));
    }
    Ok(pairs)
}

/// Resolve one instrument's session into windows and store them. TradeNation
/// only — an OANDA-scoped instrument is skipped (no `market_info` equivalent).
/// Any error path writes nothing for this instrument (fail-open).
async fn refresh_one<S, C>(
    store: &S,
    cron: &C,
    account: Option<&str>,
    instrument: &str,
    now: DateTime<Utc>,
) where
    S: StateStore,
    C: CronEnv,
{
    let broker = match cron.acquire_broker(account).await {
        Some(BrokerHandle::TradeNation(b)) => b,
        Some(BrokerHandle::Oanda(_)) => {
            tracing::info!("blackout-hours: {instrument} is OANDA-scoped; skipped (TN-only)");
            return;
        }
        None => {
            tracing::error!("blackout-hours: no broker for account={account:?} ({instrument})");
            return;
        }
    };

    let windows = match resolve_windows(&broker, instrument).await {
        Some(w) => w,
        None => return, // already logged; fail-open (no write)
    };

    match store
        .set_blackout_windows(instrument, &windows, now, WINDOW_TTL_SECONDS)
        .await
    {
        Ok(()) => tracing::info!(
            "blackout-hours: {instrument} → {} window(s) {windows:?}",
            windows.len()
        ),
        Err(err) => tracing::error!("blackout-hours: write {instrument}: {err}"),
    }
}

/// Resolve a TradeNation instrument's session into the merged UTC blackout
/// windows. Returns `None` (fail-open, logged) on any resolve / broker /
/// derivation miss. An empty `Vec` (24h market, no gaps) is a valid `Some`.
async fn resolve_windows(
    broker: &broker_tradenation_adapter::TradeNationAdapter,
    instrument: &str,
) -> Option<Vec<NoEntryWindow>> {
    let market =
        match tradenation_api::resolve_market(broker.0.client(), broker.0.session(), instrument)
            .await
        {
            Ok(m) => m,
            Err(err) => {
                tracing::error!("blackout-hours resolve_market({instrument}): {err:?}");
                return None;
            }
        };

    let info = match broker.0.market_info(market.market_id).await {
        Ok(i) => i,
        Err(err) => {
            tracing::error!("blackout-hours market_info({instrument}): {err:?}");
            return None;
        }
    };

    // The crate already converted the broker's London session to Brisbane
    // (DST-correct, anchored today). We hand the Brisbane (open, close) pairs to
    // the pure deriver, which lands UTC windows with no tz math in the worker.
    let ranges: Vec<(String, String)> = info
        .trade_session
        .ranges
        .iter()
        .map(|r| (r.open_brisbane.clone(), r.close_brisbane.clone()))
        .collect();

    Some(windows_from_session(&ranges, Buffers::default()))
}
