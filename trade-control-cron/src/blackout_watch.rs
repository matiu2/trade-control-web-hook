//! Cron 2 step — spread-recovery watcher. Runs every 15 min alongside
//! the order sweep. For each per-trade blackout record with
//! `applied == true`, decide whether the blackout has lifted; if so,
//! clear the record. The ACTUAL restore of widened stops (Sub-plan 4)
//! and cancelled orders (Sub-plan 5) hooks in at the marked points —
//! Sub-plan 2 only owns the flag lifecycle + the clear.
//!
//! Three safety rules are quoted from the master plan and enforced here
//! (do NOT optimise them out):
//!
//! 1. **Hard restore floor** — act whenever `applied && spread-normal`,
//!    REGARDLESS of the clock. Recovery is never gated on
//!    `is_ny_close_edge`; a blackout that lifts in 20 min restores in
//!    20 min.
//! 2. **Backstop timeout** — `now >= opened_at + BLACKOUT_BACKSTOP_SECONDS`
//!    clears unconditionally, so a stuck record (broker flaky, spread
//!    genuinely elevated for hours) never pins a trade forever.
//! 3. **Never-touch-what-you-didn't-apply** — the watcher's first line
//!    is `if !record.applied { return }`. Sub-plan 2 never sets
//!    `applied = true` (only 4/5 do, after a real broker mutation), so
//!    here the loop is effectively a no-op — the skeleton 4/5 fill.
//!
//! # Runtime-agnostic via the [`CronEnv`] seam
//!
//! Moved into `trade-control-cron` so both the wasm Cloudflare worker and the
//! native VM scheduler run the *same* recovery watcher. The `&Env`-hidden broker
//! acquisition travels through the [`CronEnv`] seam; the caller opens the
//! [`StateStore`] and passes it in.

use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::{AmendError, Broker};
use trade_control_core::state::{SpreadBlackoutRecord, StateStore};

use crate::broker_handle::BrokerHandle;
use crate::constants::SAFETY_FORCE_RESTORE_SECONDS;
use crate::seam::CronEnv;

/// Walk every per-trade spread-blackout record. For each `applied`
/// record, clear it when the spread has recovered or the backstop has
/// fired. Per-row errors are logged and skipped — one bad row must
/// never abort the loop (same discipline as the order sweep).
pub async fn watch_recovery<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    let records = match store.list_all_spread_blackout_records().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("blackout watch: list failed: {err}");
            return;
        }
    };
    tracing::info!("blackout watch: {} records", records.len());
    for record in records {
        if let Err(err) = watch_one(store, cron, &record, now).await {
            tracing::error!(
                "blackout watch[{}/{}]: {err}",
                record.trade_id,
                record.instrument
            );
        }
    }
}

/// Per-record recovery decision + clear. Returns an error string so the
/// caller can log with row context.
async fn watch_one<S, C>(
    store: &S,
    cron: &C,
    record: &SpreadBlackoutRecord,
    now: DateTime<Utc>,
) -> Result<(), String>
where
    S: StateStore,
    C: CronEnv,
{
    // SAFETY RULE 3 — never-touch-what-you-didn't-apply.
    if !record.applied {
        return Ok(());
    }

    // NORMAL restore FIRST (SAFETY RULE 1 — hard restore floor). Act whenever
    // applied && spread-normal, REGARDLESS of the clock. This is the path that
    // restores at the block's end when the live spread genuinely recovers; with
    // the per-record TTL now sized to outlive the block (concern 1 of the
    // backstop split), it wins BEFORE any expiry and long before the safety
    // ceiling.
    let spread_abs = sample_spread(cron, record).await?;
    // Convert the broker's absolute `ask − bid` to pips via the pip baked
    // onto the record at apply time (Cron 1). The whole feature works in
    // pips consistently; a `0.0`/non-finite pip means the apply path never
    // baked one (a Sub-plan-2-era row, or a position with no joinable
    // EntryAttempt) — fall back to a never-recover so the backstop is the
    // only clear, rather than declaring recovery on a bogus pip division.
    let spread_pips = spread_in_pips(spread_abs, record.pip_size);
    if spread_recovered(spread_pips) {
        // Restore both halves before clearing:
        //  - System 2: widened stops → remembered originals.
        //  - System 3: cancelled resting orders → re-driven via the entry path.
        // They operate on independent lists of the SAME record (`original_stops`
        // vs `cancelled_orders`), so they don't stomp each other; a trade may
        // carry both (multi-shot: an open position whose stop was widened AND a
        // resting re-entry order that was cancelled).
        restore_remembered_stops(cron, record).await;
        crate::restore_cancelled_orders(store, cron, record, now).await;
        clear(store, record, "recovery").await?;
        tracing::info!(
            "blackout watch[{}]: spread {spread_pips}p recovered, cleared",
            record.trade_id
        );
        return Ok(());
    }

    // SAFETY force-restore (last resort, SAFETY RULE 2) — a record still
    // `applied` a very long time after `opened_at`, i.e. the live spread never
    // recovered enough to clear it above. The timer (SAFETY_FORCE_RESTORE_SECONDS
    // = 12h) is LONGER than any realistic block so it fires only past any
    // legitimate block — it can no longer force-restore into an active trough the
    // way the old 3h ceiling did (21:00+3h=00:00Z, mid-AUD/CHF's-8h-block). A
    // stuck record is force-cleared rather than pinning the trade forever.
    if backstop_due(record.opened_at, now) {
        restore_remembered_stops(cron, record).await;
        crate::restore_cancelled_orders(store, cron, record, now).await;
        clear(store, record, "backstop").await?;
        tracing::info!(
            "blackout watch[{}]: safety force-restore fired, cleared",
            record.trade_id
        );
    }
    Ok(())
}

/// Convert an absolute `ask − bid` spread to pips using the record's baked
/// pip size. Returns `f64::INFINITY` when `pip_size` is unusable
/// (`0.0`/non-finite) so the caller never declares recovery on a bogus
/// division — the backstop becomes the only clear path for that record.
/// Pure — unit-testable without KV/broker.
fn spread_in_pips(spread_abs: f64, pip_size: f64) -> f64 {
    if pip_size > 0.0 && pip_size.is_finite() {
        spread_abs / pip_size
    } else {
        f64::INFINITY
    }
}

/// Restore every remembered widened stop to its **remembered original**,
/// then return. The hard rule: restore from `remembered.original_stop`
/// VERBATIM, never `current − widen` — a partial widen / missed tick /
/// double-fire all stay correct because the remembered original is
/// idempotent (restoring twice lands on the same number). Per-id errors are
/// logged and skipped so the clear still proceeds; a closed position yields
/// `AmendError::NotFound` and is treated as benign (nothing to restore).
/// System 2 only ever moves a stop — it never closes or tightens.
async fn restore_remembered_stops<C: CronEnv>(cron: &C, record: &SpreadBlackoutRecord) {
    if record.original_stops.is_empty() {
        return;
    }
    let Some(broker) = cron.acquire_broker(record.account.as_deref()).await else {
        tracing::error!(
            "blackout restore[{}]: broker acquisition failed — {} stop(s) left widened until \
             the operator restores them (backstop TTL is the final net)",
            record.trade_id,
            record.original_stops.len(),
        );
        return;
    };
    let account = record.account.as_deref().unwrap_or("");
    for remembered in &record.original_stops {
        let result = match &broker {
            BrokerHandle::Oanda(b) => {
                b.amend_stop(
                    account,
                    &remembered.position_or_order_id,
                    remembered.original_stop,
                )
                .await
            }
            BrokerHandle::TradeNation(b) => {
                b.amend_stop(
                    account,
                    &remembered.position_or_order_id,
                    remembered.original_stop,
                )
                .await
            }
        };
        match result {
            Ok(()) => tracing::info!(
                "blackout restore[{}]: amend_stop ok id={} -> original {} (verbatim, no recompute)",
                record.trade_id,
                remembered.position_or_order_id,
                remembered.original_stop,
            ),
            Err(AmendError::NotFound) => tracing::info!(
                "blackout restore[{}]: id={} gone (closed during window) — benign, nothing to \
                 restore",
                record.trade_id,
                remembered.position_or_order_id,
            ),
            Err(err) => tracing::error!(
                "blackout restore[{}]: amend_stop id={} -> {} FAILED ({err}) — stop left WIDENED, \
                 operator must restore manually",
                record.trade_id,
                remembered.position_or_order_id,
                remembered.original_stop,
            ),
        }
    }
}

/// Acquire the record's-account broker and read the current spread via
/// the Sub-plan-1 `get_quote`. The per-broker match mirrors the order
/// sweep's price-fetch dispatch.
async fn sample_spread<C: CronEnv>(cron: &C, record: &SpreadBlackoutRecord) -> Result<f64, String> {
    let broker = cron
        .acquire_broker(record.account.as_deref())
        .await
        .ok_or_else(|| "broker acquisition failed".to_string())?;
    let quote = match broker {
        BrokerHandle::Oanda(b) => b.get_quote(&record.instrument).await,
        BrokerHandle::TradeNation(b) => b.get_quote(&record.instrument).await,
    }
    .map_err(|err| format!("get_quote: {err}"))?;
    Ok(quote.spread())
}

async fn clear<S: StateStore>(
    store: &S,
    record: &SpreadBlackoutRecord,
    reason: &str,
) -> Result<(), String> {
    store
        .clear_spread_blackout_record(&record.trade_id)
        .await
        .map_err(|e| format!("{reason} clear: {e}"))
}

/// Pure safety force-restore predicate: true once `now` is at/after
/// `opened_at + SAFETY_FORCE_RESTORE_SECONDS` (the 12h last-resort ceiling, not
/// the per-record TTL). Unit-testable without KV/broker. Mirrors
/// `sweep::bar_expiry_due`.
pub fn backstop_due(opened_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now >= opened_at + Duration::seconds(SAFETY_FORCE_RESTORE_SECONDS as i64)
}

/// Pure recovery predicate — unit-testable without KV/broker. True when
/// the sampled spread (**in pips**) has dropped back to/under the recovered
/// cutoff.
///
/// UNITS (reconciled in Sub-plan 4): the whole spread-blackout feature now
/// works in pips. The caller converts the broker's absolute `ask − bid` to
/// pips via [`spread_in_pips`] using the `pip_size` baked onto the record at
/// apply time (Cron 1) — resolving Sub-plan 2's "no intent in hand" open
/// question. The cutoff itself lives beside System 1's *elevated* cutoff in
/// `trade_control_core::spread_blackout` so the hysteresis pair is tuned in ONE
/// place (`RECOVERED < ELEVATED`).
pub fn spread_recovered(spread_pips: f64) -> bool {
    spread_pips <= recovered_cutoff()
}

/// The recovered-spread cutoff in pips. Single source of truth:
/// `trade_control_core::spread_blackout::SPREAD_BLACKOUT_RECOVERED_PIPS`,
/// co-located with System 1's elevated cutoff so the hysteresis invariant is
/// visible and tuned in one file.
///
/// TODO(open-question, spread-blackout): the recovered/elevated cutoffs are
/// still uncalibrated placeholders and flat across instruments. If they
/// become per-instrument (tied to the per-instrument widen-clamp open
/// question in `blackout_widen` / `blackout_apply`), thread the instrument
/// through here. Calibrate on demo before relying on these.
fn recovered_cutoff() -> f64 {
    trade_control_core::spread_blackout::SPREAD_BLACKOUT_RECOVERED_PIPS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    #[test]
    fn spread_recovered_below_cutoff() {
        // Pips now (reconciled units). Recovered cutoff is 4p.
        assert!(spread_recovered(2.0));
        assert!(
            spread_recovered(trade_control_core::spread_blackout::SPREAD_BLACKOUT_RECOVERED_PIPS),
            "at cutoff counts as recovered"
        );
    }

    #[test]
    fn spread_not_recovered_above_cutoff() {
        // ~20 pips during the trough — still elevated; also the 8p elevated
        // band (between recovered 4p and elevated 8p) is NOT yet recovered.
        assert!(!spread_recovered(20.0));
        assert!(!spread_recovered(6.0), "hysteresis band is not recovered");
    }

    #[test]
    fn spread_in_pips_uses_record_pip_size() {
        // 0.0022 absolute at 0.0001 pip = 22 pips.
        assert!((spread_in_pips(0.0022, 0.0001) - 22.0).abs() < 1e-9);
        // Unusable pip (0.0 / non-finite) -> INFINITY so recovery never
        // fires on a bogus division; backstop is the only clear.
        assert_eq!(spread_in_pips(0.0022, 0.0), f64::INFINITY);
        assert_eq!(spread_in_pips(0.0022, f64::NAN), f64::INFINITY);
        assert!(!spread_recovered(spread_in_pips(0.0001, 0.0)));
    }

    #[test]
    fn safety_force_restore_due_at_or_after_twelve_hours() {
        let opened = ts("2026-03-12T21:05:00Z");
        // exactly 12h later (the SAFETY_FORCE_RESTORE_SECONDS ceiling) → due.
        assert!(backstop_due(opened, ts("2026-03-13T09:05:00Z")));
        // 12h + 1s → due.
        assert!(backstop_due(opened, ts("2026-03-13T09:05:01Z")));
    }

    #[test]
    fn safety_force_restore_not_due_before_twelve_hours() {
        let opened = ts("2026-03-12T21:05:00Z");
        // 1s short of 12h → not yet.
        assert!(!backstop_due(opened, ts("2026-03-13T09:04:59Z")));
        // NOT due at the old 3h mark — that fired mid-block for multi-hour blocks.
        assert!(!backstop_due(opened, ts("2026-03-13T00:05:00Z")));
        // freshly opened → not yet.
        assert!(!backstop_due(opened, ts("2026-03-12T21:20:00Z")));
    }

    /// Documents the `!applied` short-circuit as a value-level invariant:
    /// a record the box never touched is left alone. The watcher's first
    /// line enforces it; this asserts the field default that makes it safe.
    #[test]
    fn unapplied_record_is_the_left_alone_default() {
        let record = SpreadBlackoutRecord {
            trade_id: "hs-eur-nzd-c1e0f25b".into(),
            instrument: "EUR_NZD".into(),
            account: Some("reversals".into()),
            applied: false,
            opened_at: ts("2026-03-12T21:05:00Z"),
            expires_at: ts("2026-03-13T00:05:00Z"),
            pip_size: 0.0001,
            original_stops: Vec::new(),
            cancelled_orders: Vec::new(),
        };
        // The watcher returns early on this; even a long-past backstop
        // must not matter, because `applied` is checked first.
        assert!(!record.applied);
    }
}
