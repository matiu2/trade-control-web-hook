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

use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::Broker;
use trade_control_core::state::{SpreadBlackoutRecord, StateStore};
use worker::{Env, console_error, console_log};

use super::constants::BLACKOUT_BACKSTOP_SECONDS;
use super::sweep::{BrokerHandle, acquire_broker_for_account, open_store};
use crate::state::KvStateStore;

/// Walk every per-trade spread-blackout record. For each `applied`
/// record, clear it when the spread has recovered or the backstop has
/// fired. Per-row errors are logged and skipped — one bad row must
/// never abort the loop (same discipline as the order sweep).
pub async fn watch_recovery(env: &Env, now: DateTime<Utc>) {
    let Some(store) = open_store(env) else {
        return;
    };
    let records = match store.list_all_spread_blackout_records().await {
        Ok(v) => v,
        Err(err) => {
            console_error!("blackout watch: list failed: {err}");
            return;
        }
    };
    console_log!("blackout watch: {} records", records.len());
    for record in records {
        if let Err(err) = watch_one(env, &store, &record, now).await {
            console_error!(
                "blackout watch[{}/{}]: {err}",
                record.trade_id,
                record.instrument
            );
        }
    }
}

/// Per-record recovery decision + clear. Returns an error string so the
/// caller can log with row context.
async fn watch_one(
    env: &Env,
    store: &KvStateStore,
    record: &SpreadBlackoutRecord,
    now: DateTime<Utc>,
) -> Result<(), String> {
    // SAFETY RULE 3 — never-touch-what-you-didn't-apply.
    if !record.applied {
        return Ok(());
    }

    // SAFETY RULE 2 — backstop timeout. Clear regardless of spread so a
    // stuck record never pins a trade forever.
    if backstop_due(record.opened_at, now) {
        // Sub-plans 4/5: restore stops/orders here before clearing.
        clear(store, record, "backstop").await?;
        console_log!(
            "blackout watch[{}]: backstop fired, cleared",
            record.trade_id
        );
        return Ok(());
    }

    // SAFETY RULE 1 — hard restore floor. Act whenever applied &&
    // spread-normal, REGARDLESS of the clock; we don't gate on
    // is_ny_close_edge.
    let spread = sample_spread(env, record).await?;
    if spread_recovered(spread, &record.instrument) {
        // Sub-plans 4/5: restore stops/orders here before clearing.
        clear(store, record, "recovery").await?;
        console_log!(
            "blackout watch[{}]: spread {spread} recovered, cleared",
            record.trade_id
        );
    }
    Ok(())
}

/// Acquire the record's-account broker and read the current spread via
/// the Sub-plan-1 `get_quote`. The per-broker match mirrors the order
/// sweep's price-fetch dispatch.
async fn sample_spread(env: &Env, record: &SpreadBlackoutRecord) -> Result<f64, String> {
    let broker = acquire_broker_for_account(env, record.account.as_deref())
        .await
        .ok_or_else(|| "broker acquisition failed".to_string())?;
    let quote = match broker {
        BrokerHandle::Oanda(b) => b.get_quote(&record.instrument).await,
        BrokerHandle::TradeNation(b) => b.get_quote(&record.instrument).await,
    }
    .map_err(|err| format!("get_quote: {err}"))?;
    Ok(quote.spread())
}

async fn clear(
    store: &KvStateStore,
    record: &SpreadBlackoutRecord,
    reason: &str,
) -> Result<(), String> {
    store
        .clear_spread_blackout_record(&record.trade_id)
        .await
        .map_err(|e| format!("{reason} clear: {e}"))
}

/// Pure backstop predicate: true once `now` is at/after
/// `opened_at + BLACKOUT_BACKSTOP_SECONDS`. Unit-testable without
/// KV/broker. Mirrors `sweep::bar_expiry_due`.
pub fn backstop_due(opened_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    now >= opened_at + Duration::seconds(BLACKOUT_BACKSTOP_SECONDS as i64)
}

/// Pure recovery predicate — unit-testable without KV/broker. True when
/// the sampled spread has dropped back to/under the recovered cutoff.
///
/// OPEN QUESTION (do NOT resolve in Sub-plan 2): where the cutoff comes
/// from for a cron-SAMPLED instrument. The watcher iterates KV records
/// and has **no intent in hand**, so it can't read the baked
/// `Intent.pip_size`. See [`recovered_cutoff`].
pub fn spread_recovered(spread: f64, instrument: &str) -> bool {
    spread <= recovered_cutoff(instrument)
}

/// PLACEHOLDER cutoff — needs operator tuning + a pip-source decision.
///
/// TODO(open-question, spread-blackout sub-plan 2): the recovered/elevated
/// spread thresholds are not yet calibrated, and the cron has no
/// `pip_size` for an arbitrary instrument (the baked `Intent.pip_size`
/// lives on the enter intent, which the watcher doesn't see). Candidate
/// fixes, strongest first:
///   1. Bake the absolute recovered cutoff (or `pip_size`) onto the
///      `SpreadBlackoutRecord` at apply time — Cron 1 in Sub-plan 4/5
///      *does* have the trade context — then read it off the record here.
///   2. A small worker-side constants table keyed by instrument string
///      (no `instrument-lookup` in WASM).
///   3. A `SPREAD_CUTOFF_<instrument>` secret (mirrors the old
///      `PIP_SIZE_*` pattern).
///
/// Until then this returns a single coarse absolute-price placeholder so
/// the lifecycle is compilable and testable. Sub-plan 3 inherits the
/// same open question for the entry-reject `elevated_cutoff`.
fn recovered_cutoff(_instrument: &str) -> f64 {
    // 0.0010 absolute price ≈ 10 pips on a 5-dp FX cross. Deliberately
    // generous so the placeholder errs toward clearing, not pinning;
    // NOT a tuned value — see the TODO above.
    0.0010
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    #[test]
    fn spread_recovered_below_cutoff() {
        assert!(spread_recovered(0.0005, "EUR_NZD"));
        assert!(
            spread_recovered(0.0010, "EUR_NZD"),
            "at cutoff counts as recovered"
        );
    }

    #[test]
    fn spread_not_recovered_above_cutoff() {
        // ~20 pips during the trough — still elevated.
        assert!(!spread_recovered(0.0020, "EUR_NZD"));
    }

    #[test]
    fn backstop_due_at_or_after_three_hours() {
        let opened = ts("2026-03-12T21:05:00Z");
        // exactly 3h later → due.
        assert!(backstop_due(opened, ts("2026-03-13T00:05:00Z")));
        // 3h + 1s → due.
        assert!(backstop_due(opened, ts("2026-03-13T00:05:01Z")));
    }

    #[test]
    fn backstop_not_due_before_three_hours() {
        let opened = ts("2026-03-12T21:05:00Z");
        // 1s short of 3h → not yet.
        assert!(!backstop_due(opened, ts("2026-03-13T00:04:59Z")));
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
            original_stops: Vec::new(),
            cancelled_orders: Vec::new(),
        };
        // The watcher returns early on this; even a long-past backstop
        // must not matter, because `applied` is checked first.
        assert!(!record.applied);
    }
}
