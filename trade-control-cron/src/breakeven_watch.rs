//! Break-even stop watcher (BUG-replay-no-breakeven-stop-at-50pct).
//!
//! Runs every 15-min cron tick alongside the order sweep and the spread
//! watcher. For each open position whose originating enter carried a
//! `breakeven` rule (snapshotted onto its [`EntryAttempt`] at placement), it
//! moves the broker-native stop-loss to **break-even** (the entry price) once a
//! candle has **closed** past 50% of the way from entry to take-profit.
//!
//! The worker has no per-position event loop, so this cron *is* the live
//! consumer of the break-even rule — the replay's counterpart is
//! `engine::simulate_fill`, which walks the candle path directly. Both resolve
//! the decision through the same pure core helper
//! ([`Breakeven::decide_move`](trade_control_core::intent::Breakeven::decide_move))
//! so the two can't drift (the standing "strategy changes go in BOTH replayer +
//! worker" rule).
//!
//! Mechanism (mirrors `blackout_apply`'s widen, but the *other direction* —
//! tightening to entry, not widening away):
//!
//! 1. List all [`EntryAttempt`] rows; the accounts with a tracked entry *are*
//!    the accounts with joinable open positions (self-scoping).
//! 2. For each open position, join back to its attempt and read the baked
//!    [`BreakevenSnapshot`] (entry / TP / threshold / granularity).
//! 3. Fetch a bounded candle window, keep only the bars that closed **at or
//!    after the fill** (the broker's own `opened_at` on the open position),
//!    find the one that ran furthest toward TP, and ask the pure decision for
//!    the new stop.
//! 4. If armed and not already at break-even, `amend_stop(fill_price)`.
//!
//! The decision itself — window bound, target price, noise floor — is pure and
//! lives in [`crate::breakeven_decision`]; this module is the broker wiring
//! around it. Both defects in `BUG-breakeven-arms-off-pre-fill-history.md` were
//! in the wiring's own inline logic, unreachable by any test, which is why the
//! decision was lifted out.
//!
//! Idempotency / one-way: the decision returns `None` when the stop is already
//! at (or past, in the trade's favour) break-even, so re-running every tick is
//! a no-op once armed — no need to persist an "armed" flag. The move never
//! widens a stop (the helper only ever returns the entry price, and suppresses
//! the move when `current_stop` is already there).
//!
//! **PRECONDITION (shared with `blackout_apply`):** `amend_stop` on an OPEN
//! position via TradeNation's `AmendCloseOrder` is demo-unverified. Every
//! intended amend is logged prominently first so a demo run can read it back
//! (SL moved to entry, TP unchanged) before this is trusted live.
//!
//! # Runtime-agnostic via the [`CronEnv`] seam
//!
//! Moved into `trade-control-cron` so both the wasm Cloudflare worker and the
//! native VM scheduler run the *same* break-even logic. The `&Env`-hidden broker
//! acquisition now travels through the [`CronEnv`] seam; the caller opens the
//! [`StateStore`] and passes it in, exactly as the engine tick does.

use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::{AmendError, Broker, Candle, Granularity, OpenPosition};
use trade_control_core::order_control::join_position_to_attempt;
use trade_control_core::state::{EntryAttempt, StateStore};

use crate::breakeven_decision::{
    BREAKEVEN_MIN_ATR_FRACTION, BreakevenBlock, BreakevenDecision, BreakevenInputs, decide,
};
use crate::broker_handle::BrokerHandle;
use crate::seam::CronEnv;

/// How far back the broker candle **pull** reaches. This bounds the FETCH only
/// — it is not, and must never again become, the window a break-even arms off.
/// [`crate::breakeven_decision`] narrows these bars to the ones that closed at
/// or after the fill; without that narrowing this constant is ~83 days of
/// pre-fill history on H4, which is exactly how
/// `BUG-breakeven-arms-off-pre-fill-history.md` happened. The pull stays
/// generous so a long-running position still has ATR warmup and its full
/// post-fill path in one request.
const BREAKEVEN_LOOKBACK_BARS: i64 = 500;

/// Walk every open position and move its stop to break-even when a candle has
/// closed past 50%-to-TP. Per-row errors are logged and skipped — one bad row
/// must never abort the loop (same discipline as the order sweep / blackout).
pub async fn watch<S, C>(store: &S, cron: &C, now: DateTime<Utc>)
where
    S: StateStore,
    C: CronEnv,
{
    let attempts = match store.list_all_entry_attempts().await {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("breakeven watch: list_all_entry_attempts: {err}");
            return;
        }
    };
    // Only trades that opted into break-even are interesting; if none did, skip
    // the broker round-trips entirely.
    if !attempts.iter().any(|a| a.breakeven.is_some()) {
        return;
    }
    // Distinct affected accounts (preserves insertion order, dedups).
    let mut accounts: Vec<Option<String>> = Vec::new();
    for a in &attempts {
        if a.breakeven.is_some() && !accounts.contains(&a.account) {
            accounts.push(a.account.clone());
        }
    }
    tracing::info!(
        "breakeven watch: {} attempt(s), {} BE account(s)",
        attempts.len(),
        accounts.len(),
    );
    for account in accounts {
        watch_account(cron, &attempts, account.as_deref(), now).await;
    }
}

/// Move-to-BE every eligible open position on one account. Logs + skips per
/// position.
async fn watch_account<C: CronEnv>(
    cron: &C,
    attempts: &[EntryAttempt],
    account: Option<&str>,
    now: DateTime<Utc>,
) {
    let Some(broker) = cron.acquire_broker(account).await else {
        tracing::error!(
            "breakeven watch[{}]: broker acquisition failed; skipping account",
            account.unwrap_or("<global>"),
        );
        return;
    };
    let account_id = account.unwrap_or("");
    let positions = match list_positions(&broker, account_id).await {
        Ok(p) => p,
        Err(err) => {
            tracing::error!(
                "breakeven watch[{}]: list_open_positions: {err}",
                account.unwrap_or("<global>"),
            );
            return;
        }
    };
    for position in positions {
        watch_one(&broker, account, attempts, &position, now).await;
    }
}

/// The two broker operations one position's break-even needs: read its candles
/// and move its stop.
///
/// A seam, not an abstraction for its own sake. [`watch_one`] is where both
/// live-money defects in `BUG-breakeven-arms-off-pre-fill-history.md` sat, and
/// with a concrete [`BrokerHandle`] in its signature it could not be driven by
/// a test at all — the pure decision could be proven correct while the wiring
/// around it quietly amended anyway. This trait is what lets a test assert the
/// thing that actually matters: *did the broker's stop move, and to what*.
#[allow(async_fn_in_trait)]
trait PositionBroker {
    async fn candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Option<Vec<Candle>>;

    async fn amend_stop(&self, account_id: &str, id: &str, new_stop: f64)
    -> Result<(), AmendError>;
}

impl PositionBroker for BrokerHandle {
    async fn candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Option<Vec<Candle>> {
        fetch_candles(self, instrument, granularity, since, now).await
    }

    async fn amend_stop(
        &self,
        account_id: &str,
        id: &str,
        new_stop: f64,
    ) -> Result<(), AmendError> {
        amend(self, account_id, id, new_stop).await
    }
}

/// Decide + (maybe) move one open position's stop to break-even.
async fn watch_one<B: PositionBroker>(
    broker: &B,
    account: Option<&str>,
    attempts: &[EntryAttempt],
    position: &OpenPosition,
    now: DateTime<Utc>,
) {
    // A position with no attached stop is left alone — break-even moves an
    // existing stop, it doesn't add one (same stance as the blackout widen).
    let Some(current_stop) = position.stop_loss else {
        return;
    };
    // Join → originating attempt → its break-even snapshot. Positions whose
    // attempt opted out (or that don't join) are skipped silently.
    let Some(attempt) = join_position_to_attempt(position, account, attempts) else {
        return;
    };
    let Some(snap) = attempt.breakeven else {
        return;
    };
    let trade_id = attempt.trade_id.as_str();
    let who = account.unwrap_or("<global>");

    // Bounded FETCH — the lookback bounds the broker pull, nothing else. Which
    // of these bars may *arm* is decided by `breakeven_decision`, against the
    // fill time.
    let since = now - Duration::seconds(snap.granularity.seconds() * BREAKEVEN_LOOKBACK_BARS);
    let Some(candles) = broker
        .candles(&position.instrument, snap.granularity, since, now)
        .await
    else {
        // The pull failed — logged inside `fetch_candles`; retry next tick.
        return;
    };

    let inputs = BreakevenInputs {
        snapshot: &snap,
        position,
        current_stop,
    };
    let (new_stop, armed_by, armed_at) = match decide(&inputs, candles, now) {
        BreakevenDecision::Hold => return,
        BreakevenDecision::Blocked(BreakevenBlock::NoFillTime) => {
            tracing::error!(
                "breakeven watch[{who}]: trade={trade_id} id={} instrument={} — broker reported \
                 no fill time for this position; REFUSING to arm break-even rather than arming \
                 off pre-fill history (see BUG-breakeven-arms-off-pre-fill-history.md). The \
                 original stop still protects the trade.",
                position.order_id,
                position.instrument,
            );
            return;
        }
        BreakevenDecision::Blocked(BreakevenBlock::InsideNoise {
            new_stop,
            reference_price,
            distance,
            floor,
        }) => {
            tracing::error!(
                "breakeven watch[{who}]: trade={trade_id} id={} instrument={} — REFUSING \
                 amend_stop to {new_stop}: it sits {distance} from the last close \
                 {reference_price}, inside the {floor} noise floor ({}× ATR). A break-even this \
                 close to market is not a scratch — something upstream produced a wrong target.",
                position.order_id,
                position.instrument,
                BREAKEVEN_MIN_ATR_FRACTION,
            );
            return;
        }
        BreakevenDecision::Amend {
            new_stop,
            armed_by,
            at,
        } => (new_stop, armed_by, at),
    };

    // PRECONDITION-guarded amend: log the intent prominently so a demo run can
    // confirm `AmendCloseOrder`-on-open-position moved the SL (and left TP)
    // before this is trusted live. `entry=` is the price the position actually
    // FILLED at where the broker reports one — never the order trigger.
    tracing::info!(
        "breakeven watch[{who}]: INTENT amend_stop trade={trade_id} id={} instrument={} \
         dir={:?} current_sl={current_stop} -> BE={new_stop} (entry={}, entry_source={}, \
         tp={}, armed_by={armed_by} at {armed_at}, filled_at={:?}) \
         (DEMO-CONFIRM AmendCloseOrder-on-open-position before trusting live)",
        position.order_id,
        position.instrument,
        position.direction,
        new_stop,
        if position.entry_price.is_some() {
            "broker-fill"
        } else {
            "placement-snapshot"
        },
        snap.take_profit,
        position.opened_at,
    );
    match broker
        .amend_stop(account.unwrap_or(""), &position.order_id, new_stop)
        .await
    {
        Ok(()) => tracing::info!(
            "breakeven watch[{who}]: amend_stop ok trade={trade_id} id={} -> {new_stop} \
             (break-even)",
            position.order_id,
        ),
        Err(AmendError::NotFound) => tracing::info!(
            "breakeven watch[{who}]: amend_stop id={} not found (position closed?) \
             trade={trade_id} — benign",
            position.order_id,
        ),
        Err(err) => tracing::error!(
            "breakeven watch[{who}]: amend_stop trade={trade_id} id={} -> {new_stop} FAILED \
             ({err}); will retry next tick",
            position.order_id,
        ),
    }
}

async fn fetch_candles(
    broker: &BrokerHandle,
    instrument: &str,
    granularity: Granularity,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<Vec<Candle>> {
    let res = match broker {
        BrokerHandle::Oanda(b) => b.get_candles(instrument, granularity, since, now).await,
        BrokerHandle::TradeNation(b) => b.get_candles(instrument, granularity, since, now).await,
        BrokerHandle::Ibkr(b) => b.get_candles(instrument, granularity, since, now).await,
    };
    match res {
        Ok(c) => Some(c),
        Err(err) => {
            tracing::error!("breakeven watch: get_candles({instrument}): {err}");
            None
        }
    }
}

async fn list_positions(
    broker: &BrokerHandle,
    account_id: &str,
) -> Result<Vec<OpenPosition>, String> {
    let res = match broker {
        BrokerHandle::Oanda(b) => b.list_open_positions(account_id).await,
        BrokerHandle::TradeNation(b) => b.list_open_positions(account_id).await,
        BrokerHandle::Ibkr(b) => b.list_open_positions(account_id).await,
    };
    res.map_err(|e| e.to_string())
}

async fn amend(
    broker: &BrokerHandle,
    account_id: &str,
    id: &str,
    new_stop: f64,
) -> Result<(), AmendError> {
    match broker {
        BrokerHandle::Oanda(b) => b.amend_stop(account_id, id, new_stop).await,
        BrokerHandle::TradeNation(b) => b.amend_stop(account_id, id, new_stop).await,
        BrokerHandle::Ibkr(b) => b.amend_stop(account_id, id, new_stop).await,
    }
}

/// Join an open position to the [`EntryAttempt`] that placed it — the source of
/// the baked [`BreakevenSnapshot`]. Same principled match + coarse fallback as
/// `blackout_apply::join_position_to_attempt`: exact on the snapshotted
/// `broker_trade_id == position_id`, else `instrument + direction + account`.
/// Pure & unit-testable.
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use trade_control_core::intent::{Breakeven, Direction};
    use trade_control_core::state::BreakevenSnapshot;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    fn attempt_with_be(
        instrument: &str,
        direction: Direction,
        account: Option<&str>,
        broker_trade_id: Option<&str>,
        snap: Option<BreakevenSnapshot>,
    ) -> EntryAttempt {
        EntryAttempt {
            trade_id: "t1".into(),
            account: account.map(|s| s.into()),
            instrument: instrument.into(),
            attempt_no: 1,
            broker_order_id: "ord-1".into(),
            broker_trade_id: broker_trade_id.map(|s| s.into()),
            direction,
            placed_at: ts("2026-06-24T00:00:00Z"),
            shell_time: ts("2026-06-24T00:00:00Z"),
            expires_at: ts("2026-06-30T00:00:00Z"),
            stop_loss_price: Some(1.1040),
            cancel_at: None,
            pip_size: Some(0.0001),
            blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
            breakeven: snap,
            order_control: None,
            superseded: false,
        }
    }

    fn position(instrument: &str, direction: Direction, position_id: &str) -> OpenPosition {
        OpenPosition {
            instrument: instrument.into(),
            direction,
            stop_loss: Some(1.1040),
            take_profit: None,
            position_id: position_id.into(),
            order_id: "ord-1".into(),
            stake: 1.0,
            entry_price: Some(1.1000),
            opened_at: Some(ts("2026-06-24T01:00:00Z")),
        }
    }

    /// A [`PositionBroker`] that records every amend instead of making one, so
    /// a test can assert on what the LIVE PATH would have sent the broker —
    /// not merely on what the pure decision returned.
    ///
    /// This exists because a mutation that made `watch_one` amend on a
    /// `Blocked` decision **survived** every test of the pure `decide`: the
    /// decision was right and the wiring ignored it. See
    /// `BUG-breakeven-arms-off-pre-fill-history.md`.
    struct SpyBroker {
        candles: Vec<Candle>,
        amends: RefCell<Vec<f64>>,
    }

    impl SpyBroker {
        fn with(candles: Vec<Candle>) -> Self {
            Self {
                candles,
                amends: RefCell::new(Vec::new()),
            }
        }
    }

    impl PositionBroker for SpyBroker {
        async fn candles(
            &self,
            _instrument: &str,
            _granularity: Granularity,
            _since: DateTime<Utc>,
            _now: DateTime<Utc>,
        ) -> Option<Vec<Candle>> {
            Some(self.candles.clone())
        }

        async fn amend_stop(
            &self,
            _account_id: &str,
            _id: &str,
            new_stop: f64,
        ) -> Result<(), AmendError> {
            self.amends.borrow_mut().push(new_stop);
            Ok(())
        }
    }

    fn bar(time: &str, close: f64) -> Candle {
        Candle {
            time: ts(time),
            o: close,
            h: close + 0.0010,
            l: close - 0.0010,
            c: close,
        }
    }

    /// The NZD_CAD incident geometry as the live cron would see it: an attempt
    /// carrying the trigger-priced snapshot, and an open position carrying the
    /// broker's real fill.
    fn incident_attempt() -> EntryAttempt {
        let mut a = attempt_with_be(
            "NZD_CAD",
            Direction::Short,
            None,
            Some("2321"),
            Some(BreakevenSnapshot {
                rule: Breakeven::at_half(),
                entry_price: 0.82046, // the TRIGGER, as placement snapshots it
                take_profit: 0.81531,
                granularity: Granularity::H4,
            }),
        );
        a.instrument = "NZD_CAD".into();
        a
    }

    fn incident_position() -> OpenPosition {
        OpenPosition {
            instrument: "NZD_CAD".into(),
            direction: Direction::Short,
            stop_loss: Some(0.82527),
            take_profit: Some(0.81531),
            position_id: "2321".into(),
            order_id: "2321".into(),
            stake: 876_934.0,
            entry_price: Some(0.82043), // the FILL
            opened_at: Some(ts("2026-08-10T23:46:00Z")),
        }
    }

    /// **The incident, driven through the live entry point.** July bars that
    /// close far past the arming level, a position that filled on 10 August,
    /// six minutes of life. The broker must receive NO amend at all.
    #[test]
    fn the_live_path_sends_no_amend_for_a_pre_fill_arm() {
        let attempts = vec![incident_attempt()];
        let pos = incident_position();
        // Every bar closes below the 0.81787 arming level, and every one of
        // them predates the fill.
        let candles: Vec<Candle> = (0..40)
            .map(|i| Candle {
                time: ts("2026-07-01T00:00:00Z") + Duration::hours(4 * i),
                o: 0.80500,
                h: 0.80600,
                l: 0.80400,
                c: 0.80500,
            })
            .collect();
        let broker = SpyBroker::with(candles);
        pollster::block_on(watch_one(
            &broker,
            None,
            &attempts,
            &pos,
            ts("2026-08-10T23:52:00Z"),
        ));
        assert!(
            broker.amends.borrow().is_empty(),
            "the live path amended off pre-fill history: {:?}",
            broker.amends.borrow(),
        );
    }

    /// The mirror, so the test above cannot pass merely because `watch_one`
    /// never amends anything: a genuine post-fill run must reach the broker —
    /// **at the fill price, not the trigger**.
    #[test]
    fn the_live_path_amends_to_the_fill_on_a_genuine_arm() {
        let attempts = vec![incident_attempt()];
        let pos = incident_position();
        let broker = SpyBroker::with(vec![
            bar("2026-08-11T00:00:00Z", 0.81900),
            bar("2026-08-11T04:00:00Z", 0.81700), // arms (< 0.81787)
        ]);
        pollster::block_on(watch_one(
            &broker,
            None,
            &attempts,
            &pos,
            ts("2026-08-11T08:00:00Z"),
        ));
        let amends = broker.amends.borrow().clone();
        assert_eq!(
            amends.len(),
            1,
            "expected exactly one amend, got {amends:?}"
        );
        assert!(
            (amends[0] - 0.82043).abs() < 1e-9,
            "the live path amended to {} — 0.82046 is the trigger, 0.82043 is the fill",
            amends[0],
        );
    }

    /// A `Blocked` decision must reach the broker as *nothing*. This is the
    /// wiring mutation that survived the pure-decision tests: the decision said
    /// "refuse" and the caller amended anyway.
    #[test]
    fn the_live_path_sends_no_amend_when_the_decision_is_blocked() {
        let attempts = vec![incident_attempt()];
        let mut pos = incident_position();
        // Broker reported no fill time → `BreakevenBlock::NoFillTime`.
        pos.opened_at = None;
        let broker = SpyBroker::with(vec![bar("2026-08-11T04:00:00Z", 0.81700)]);
        pollster::block_on(watch_one(
            &broker,
            None,
            &attempts,
            &pos,
            ts("2026-08-11T08:00:00Z"),
        ));
        assert!(
            broker.amends.borrow().is_empty(),
            "a Blocked decision must never reach the broker: {:?}",
            broker.amends.borrow(),
        );
    }

    /// The other `Blocked` variant, through the same entry point: a break-even
    /// that would land on top of market is refused at the broker boundary.
    #[test]
    fn the_live_path_sends_no_amend_for_a_target_inside_noise() {
        let mut attempt = incident_attempt();
        attempt.breakeven = Some(BreakevenSnapshot {
            rule: Breakeven::at_half(),
            entry_price: 0.81700,
            take_profit: 0.81000,
            granularity: Granularity::H4,
        });
        let mut pos = incident_position();
        pos.entry_price = Some(0.81700);
        pos.opened_at = Some(ts("2026-06-30T00:00:00Z"));
        pos.stop_loss = Some(0.82500);
        // 40 warm bars for a judgeable ATR, an arming bar, then price returns
        // right on top of the 0.81700 break-even target.
        let mut candles: Vec<Candle> = (0..40)
            .map(|i| Candle {
                time: ts("2026-07-01T00:00:00Z") + Duration::hours(4 * i),
                o: 0.81800,
                h: 0.81900,
                l: 0.81700,
                c: 0.81800,
            })
            .collect();
        candles.push(bar("2026-08-11T00:00:00Z", 0.81300));
        candles.push(bar("2026-08-11T04:00:00Z", 0.81700));
        let broker = SpyBroker::with(candles);
        pollster::block_on(watch_one(
            &broker,
            None,
            &[attempt],
            &pos,
            ts("2026-08-11T08:00:00Z"),
        ));
        assert!(
            broker.amends.borrow().is_empty(),
            "a break-even landing on market must never reach the broker: {:?}",
            broker.amends.borrow(),
        );
    }

    #[test]
    fn join_matches_on_broker_trade_id_first() {
        let snap = BreakevenSnapshot {
            rule: Breakeven::at_half(),
            entry_price: 1.1000,
            take_profit: 1.0900,
            granularity: Granularity::H1,
        };
        let attempts = vec![attempt_with_be(
            "EUR_USD",
            Direction::Short,
            Some("reversals"),
            Some("POS-9"),
            Some(snap),
        )];
        let pos = position("EUR_USD", Direction::Short, "POS-9");
        let hit = join_position_to_attempt(&pos, Some("reversals"), &attempts).unwrap();
        assert!(hit.breakeven.is_some());
    }

    #[test]
    fn join_misses_when_nothing_correlates() {
        let attempts = vec![attempt_with_be(
            "EUR_USD",
            Direction::Long,
            Some("reversals"),
            None,
            None,
        )];
        let pos = position("EUR_NZD", Direction::Short, "POS-X");
        assert!(join_position_to_attempt(&pos, Some("reversals"), &attempts).is_none());
    }
}
