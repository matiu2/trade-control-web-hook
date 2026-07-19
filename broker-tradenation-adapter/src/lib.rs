//! Adapt `broker-tradenation`'s inherent methods to `core::broker::Broker`.
//!
//! Upstream owns its own copies of `EntryRequest` / `Direction` / `ResolvedEntry`
//! / `EntryError` / `RiskBudget`, structurally identical to ours. We translate
//! between them at the boundary so the worker dispatch can stay generic over
//! [`Broker`].

use broker_tradenation::TradeNationBroker;
use candle_model::Granularity as CmGranularity;
use chrono::{DateTime, Duration, Utc};
use trade_control_core::broker::{
    AmendError, AttemptState, BidAskCandle, Broker, CancelError, Candle, CandleError, EntryError,
    EntryRequest, Granularity, LookupError, OpenPosition, PendingOrder, Quote,
};
use trade_control_core::intent::{Direction, ResolvedEntry, RiskBudget};
use tradenation_api::ohlcv::PriceType;
use tradenation_api::{OpeningOrder, Position, TransactionRecord};

/// Closed-trade scan window. Plan §3 recommends ~50; one TN page
/// returns ~50 records so we fetch a single page. **Caveat: TN's
/// `TransactionRecord` exposes only `RefID`, not the originating
/// `OrderID` or `PositionID` — so step 3 of the algorithm cannot
/// match on TradeNation in v1. Closed attempts fall through to
/// `Cancelled` instead. See report-back note in the 1b commit.
const CLOSED_TRADE_HISTORY_DAYS: u32 = 90;

pub struct TradeNationAdapter(pub TradeNationBroker);

impl Broker for TradeNationAdapter {
    async fn place_entry(
        &self,
        max_risk_pct: f64,
        max_open_positions: u32,
        req: &EntryRequest<'_>,
    ) -> Result<String, EntryError> {
        if req.dry_run {
            // Upstream `place_entry` doesn't yet support a no-op mode,
            // so we can't run the full sizing path (FX, market resolve,
            // stake calc) without risking an actual order. Log the
            // inputs and bail. Once upstream gains a `dry_run` flag,
            // switch this to the same pattern as OANDA.
            tracing::info!(
                "DRY-RUN tradenation: instrument={} direction={:?} entry={:?} sl={} tp={} risk={:?} (stake not computed — upstream lacks dry-run support)",
                req.instrument,
                req.direction,
                req.entry,
                req.stop_loss,
                req.take_profit,
                req.risk,
            );
            return Ok(format!("dry-run-{}", req.instrument));
        }
        let upstream_req = broker_tradenation::EntryRequest {
            instrument: req.instrument,
            direction: to_upstream_direction(req.direction),
            entry: to_upstream_entry(&req.entry),
            stop_loss: req.stop_loss,
            take_profit: req.take_profit,
            risk: to_upstream_risk(req.risk),
        };
        self.0
            .place_entry(max_risk_pct, max_open_positions, &upstream_req)
            .await
            .map_err(from_upstream_error)
    }

    async fn close_positions(&self, instrument: &str) -> bool {
        self.0.close_positions(instrument).await
    }

    async fn cancel_pending_for_instrument(&self, instrument: &str) -> usize {
        self.0.cancel_pending_for_instrument(instrument).await
    }

    async fn lookup_attempt_state(
        &self,
        instrument: &str,
        broker_order_id: &str,
        broker_trade_id: Option<&str>,
    ) -> Result<AttemptState, LookupError> {
        // `get_account_details` returns BOTH pending orders and open
        // positions in one round-trip, so steps 1 and 2 share a fetch.
        let details = tradenation_api::get_account_details(self.0.session())
            .await
            .map_err(|err| {
                tracing::error!("tn lookup get_account_details: {err:?}");
                LookupError::Transient
            })?;

        // Step 3's fetch is only needed if we already snapshotted a
        // broker_trade_id — same optimisation as the OANDA path.
        let closed: Option<Vec<TransactionRecord>> = if broker_trade_id.is_some() {
            let recs = tradenation_api::get_transaction_history(
                self.0.client(),
                self.0.session(),
                CLOSED_TRADE_HISTORY_DAYS,
                0,
            )
            .await
            .map_err(|err| {
                tracing::error!("tn lookup get_transaction_history: {err:?}");
                LookupError::Transient
            })?;
            Some(recs)
        } else {
            None
        };

        Ok(compute_attempt_state(
            instrument,
            broker_order_id,
            broker_trade_id,
            &details.opening_orders.records,
            &details.positions.records,
            closed.as_deref(),
        ))
    }

    async fn cancel_order(
        &self,
        _account_id: &str,
        broker_order_id: &str,
    ) -> Result<(), CancelError> {
        // TradeNation picks the account from the session, so the
        // trait-level `account_id` is intentionally ignored.
        tradenation_api::cancel_order(self.0.client(), self.0.session(), broker_order_id)
            .await
            .map_err(|err| {
                tracing::error!("tn cancel_order({broker_order_id}): {err:?}");
                CancelError::Transient
            })
    }

    async fn get_quote(&self, instrument: &str) -> Result<Quote, LookupError> {
        // Two hops: name → market_id, then market_id → (bid, ask).
        // Upstream `latest_bid_ask` returns the most recent 1m bid/ask
        // candle close as a (f64, f64) tuple. The trait's default
        // `get_current_price` takes the mid; the spread-blackout systems
        // read `spread()`.
        let market = tradenation_api::resolve_market(self.0.client(), self.0.session(), instrument)
            .await
            .map_err(|err| {
                tracing::error!("tn resolve_market({instrument}): {err:?}");
                LookupError::Transient
            })?;
        let (bid, ask) = tradenation_api::latest_bid_ask(self.0.client(), market.market_id)
            .await
            .map_err(|err| {
                tracing::error!(
                    "tn latest_bid_ask({instrument}, market_id={}): {err:?}",
                    market.market_id
                );
                LookupError::Transient
            })?;
        Ok(Quote { bid, ask })
    }

    async fn list_open_positions(
        &self,
        _account_id: &str,
    ) -> Result<Vec<OpenPosition>, LookupError> {
        // TradeNation binds the account via the session, so the
        // trait-level `account_id` is intentionally ignored.
        let details = tradenation_api::get_account_details(self.0.session())
            .await
            .map_err(|err| {
                tracing::error!("tn list_open_positions get_account_details: {err:?}");
                LookupError::Transient
            })?;
        Ok(details
            .positions
            .records
            .iter()
            .map(tn_position_to_open)
            .collect())
    }

    async fn list_pending_orders(
        &self,
        _account_id: &str,
    ) -> Result<Vec<PendingOrder>, LookupError> {
        let details = tradenation_api::get_account_details(self.0.session())
            .await
            .map_err(|err| {
                tracing::error!("tn list_pending_orders get_account_details: {err:?}");
                LookupError::Transient
            })?;
        Ok(details
            .opening_orders
            .records
            .iter()
            .filter_map(|o| {
                let mapped = tn_order_to_pending(o);
                if mapped.is_none() {
                    tracing::error!(
                        "tn list_pending_orders: skipping malformed order_id={} market={} (no stop/limit trigger)",
                        o.order_id,
                        o.market,
                    );
                }
                mapped
            })
            .collect())
    }

    async fn get_candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<Candle>, CandleError> {
        if since >= now {
            return Err(CandleError::BadRange);
        }
        // TN's chart OHLCV is count-back-from-`end_time`, not a true range,
        // so fetch enough candles to cover the window then trim to `> since`
        // with `filter_new_candles`. `native` from `to_cm_granularity` is whether
        // TN's raw endpoint serves this TF directly (min/quarter/hour/day). H4/M5
        // have no native endpoint (they need adapter-side aggregation, not yet
        // built) so they're rejected loudly here.
        let (cm_gran, native) = to_cm_granularity(granularity);
        if !native {
            tracing::error!(
                "tn get_candles({instrument}): granularity {granularity:?} not TN-native"
            );
            // Structural, not a degenerate window — the engine must surface this
            // loudly, never treat it as an empty no-op (bug ②: silent brick).
            return Err(CandleError::UnsupportedGranularity);
        }
        let count = candle_count_for_window(granularity, since, now);

        // name → market_id (same first hop as `get_quote`).
        let market = tradenation_api::resolve_market(self.0.client(), self.0.session(), instrument)
            .await
            .map_err(|err| {
                tracing::error!("tn get_candles resolve_market({instrument}): {err:?}");
                CandleError::Transient
            })?;

        // `get_candles_range_aggregated` is the drop-in for `get_candles_range`
        // that serves non-native TFs (H4/M5) by fetching the native base (H1/M1)
        // and rolling it up on 00/04/08/12/16/20 UTC buckets (`tradenation-api`
        // v0.4.0). Native TFs pass straight through unchanged.
        let raw = tradenation_api::aggregation::get_candles_range_aggregated(
            self.0.client(),
            market.market_id,
            cm_gran,
            PriceType::Mid,
            count,
            now,
        )
        .await
        .map_err(|err| {
            tracing::error!(
                "tn get_candles({instrument}, market_id={}, count={count}): {err:?}",
                market.market_id
            );
            CandleError::Transient
        })?;

        let candles = raw
            .iter()
            .map(|c| Candle {
                time: c.timestamp.with_timezone(&Utc),
                o: c.open,
                h: c.high,
                l: c.low,
                c: c.close,
            })
            .collect();

        Ok(trade_control_core::broker::filter_new_candles(
            candles, since,
        ))
    }

    async fn get_bidask_candles(
        &self,
        instrument: &str,
        granularity: Granularity,
        since: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Vec<BidAskCandle>, CandleError> {
        if since >= now {
            return Err(CandleError::BadRange);
        }
        // Same native-TF + count-back windowing as `get_candles`. TN serves one
        // OHLCV series per `PriceType`, so a two-sided read is THREE fetches
        // (mid for the OHLC the engine convention uses, bid + ask for the
        // books) zipped by timestamp. NOTE for the H4 switch-over: this path
        // fetches H4 for ALL THREE PriceTypes — the aggregating `tradenation-api`
        // must serve H4 for Mid, Bid AND Ask, not just Mid.
        let (cm_gran, native) = to_cm_granularity(granularity);
        if !native {
            tracing::error!(
                "tn get_bidask_candles({instrument}): granularity {granularity:?} not TN-native"
            );
            // Structural, not a degenerate window — see `get_candles` above.
            return Err(CandleError::UnsupportedGranularity);
        }
        let market = tradenation_api::resolve_market(self.0.client(), self.0.session(), instrument)
            .await
            .map_err(|err| {
                tracing::error!("tn get_bidask_candles resolve_market({instrument}): {err:?}");
                CandleError::Transient
            })?;

        let map_err = |err| {
            tracing::error!(
                "tn get_bidask_candles({instrument}, market_id={}): {err:?}",
                market.market_id
            );
            CandleError::Transient
        };

        // A window wider than TN's per-request ceiling (e.g. many days of M1) is
        // fetched in several count-back chunks walking backward from `now`; a
        // window that fits is a single chunk. Each chunk is three series (mid +
        // bid + ask) zipped by timestamp, then all chunks union-dedup by
        // timestamp so the +slack seam overlap collapses to one bar.
        let mut by_ts: std::collections::HashMap<DateTime<Utc>, BidAskCandle> =
            std::collections::HashMap::new();
        for (count, end) in chunk_windows(granularity, since, now) {
            // Aggregating fetch: H4/M5 built from the native base per PriceType,
            // each side rolled up on the same UTC buckets (see `get_candles`).
            let fetch = |price: PriceType| {
                tradenation_api::aggregation::get_candles_range_aggregated(
                    self.0.client(),
                    market.market_id,
                    cm_gran,
                    price,
                    count,
                    end,
                )
            };
            // Sequential (the session client is `!Send`).
            let mid = fetch(PriceType::Mid).await.map_err(map_err)?;
            let bid = fetch(PriceType::Bid).await.map_err(map_err)?;
            let ask = fetch(PriceType::Ask).await.map_err(map_err)?;

            // Index bid/ask by timestamp so a length/gap mismatch between the
            // series drops that bar rather than mis-aligning the whole window.
            let bid_by_ts: std::collections::HashMap<_, _> =
                bid.iter().map(|c| (c.timestamp, c)).collect();
            let ask_by_ts: std::collections::HashMap<_, _> =
                ask.iter().map(|c| (c.timestamp, c)).collect();

            for m in &mid {
                let (Some(b), Some(a)) = (bid_by_ts.get(&m.timestamp), ask_by_ts.get(&m.timestamp))
                else {
                    continue;
                };
                let time = m.timestamp.with_timezone(&Utc);
                if time <= since {
                    continue; // strictly after the watermark
                }
                by_ts.entry(time).or_insert(BidAskCandle {
                    time,
                    o: m.open,
                    h: m.high,
                    l: m.low,
                    c: m.close,
                    bid_o: b.open,
                    bid_h: b.high,
                    bid_l: b.low,
                    bid_c: b.close,
                    ask_o: a.open,
                    ask_h: a.high,
                    ask_l: a.low,
                    ask_c: a.close,
                });
            }
        }

        let mut candles: Vec<BidAskCandle> = by_ts.into_values().collect();
        candles.sort_by_key(|c| c.time);
        Ok(candles)
    }

    async fn amend_stop(
        &self,
        _account_id: &str,
        position_or_order_id: &str,
        new_stop: f64,
    ) -> Result<(), AmendError> {
        // `amend_order` needs the originating order id, market name, stake
        // and the new stop. The trait-level id alone doesn't carry that, so
        // re-fetch and locate the record. Positions first (the cron's primary
        // target), then pending orders.
        //
        // VERIFIED on the experimental demo 2026-06-30: `amend_order` does
        // amend an OPEN position's SL keyed by its originating order id, and
        // the with-TP path preserves the take-profit. The no-TP path (now a
        // stop-only mode-2 amend, see below) moves the SL with no phantom TP.
        let details = tradenation_api::get_account_details(self.0.session())
            .await
            .map_err(|err| {
                tracing::error!("tn amend_stop get_account_details: {err:?}");
                AmendError::Transient
            })?;

        let target = find_amend_target(
            position_or_order_id,
            &details.positions.records,
            &details.opening_orders.records,
        )
        .ok_or(AmendError::NotFound)?;

        // Move the stop, leaving the take-profit untouched. `amend_order`
        // takes the TP as `Option<f64>`:
        //   Some(tp) → amend both legs (orderModeID 3), re-sending the
        //              existing TP so it stays put.
        //   None     → stop-only amend (orderModeID 2). Required for a
        //              position with NO take-profit: passing 0.0 used to be
        //              rejected by TradeNation as a TP at price 0
        //              (`#5-9 "too close to market"`), silently failing to
        //              move the stop. VERIFIED on the experimental demo
        //              2026-06-30: with-TP amend preserves the TP; no-TP amend
        //              now moves the SL and leaves the TP absent.
        tradenation_api::amend_order(
            self.0.client(),
            self.0.session(),
            target.order_id,
            &target.market,
            target.stake,
            new_stop,
            target.existing_take_profit,
        )
        .await
        .map(|_| ())
        .map_err(|err| {
            tracing::error!(
                "tn amend_stop(order_id={}, market={}, new_stop={new_stop}): {err:?}",
                target.order_id,
                target.market,
            );
            AmendError::Transient
        })
    }
}

/// The bits of a position / opening order `amend_stop` needs to call the
/// upstream `amend_order`. Split out so the lookup is unit-testable.
struct AmendTarget {
    order_id: u64,
    market: String,
    stake: f64,
    existing_take_profit: Option<f64>,
}

/// Locate the amend target for `id` among open positions (matched on
/// `position_id` OR originating `order_id`), then pending orders (matched on
/// `order_id`). Returns `None` if nothing matches. Pure — unit-tested.
fn find_amend_target(
    id: &str,
    positions: &[Position],
    pending: &[OpeningOrder],
) -> Option<AmendTarget> {
    if let Some(p) = positions
        .iter()
        .find(|p| p.position_id.to_string() == id || p.order_id.to_string() == id)
    {
        return Some(AmendTarget {
            order_id: p.order_id,
            market: p.market_name.clone(),
            stake: p.stake,
            existing_take_profit: p.limit_order_price,
        });
    }
    pending
        .iter()
        .find(|o| o.order_id.to_string() == id)
        .map(|o| AmendTarget {
            order_id: o.order_id,
            market: o.market.clone(),
            stake: o.stake,
            // For a pending entry order the parsed stop/limit prices are the
            // ENTRY TRIGGER, not the SL/TP (see `PendingOrder` doc + the IDO*
            // note). We have no parsed SL/TP to preserve, so leave TP as None.
            existing_take_profit: None,
        })
}

/// Map a TradeNation [`Position`] to a broker-agnostic [`OpenPosition`].
/// Pure — unit-tested for the Buy/Sell → direction and SL/TP optionality
/// branches. `direction` is `"Buy"` / `"Sell"` in upstream fixtures.
fn tn_position_to_open(p: &Position) -> OpenPosition {
    OpenPosition {
        instrument: p.market_name.clone(),
        direction: if p.direction == "Sell" {
            Direction::Short
        } else {
            Direction::Long
        },
        stop_loss: p.stop_order_price,
        take_profit: p.limit_order_price,
        position_id: p.position_id.to_string(),
        order_id: p.order_id.to_string(),
        stake: p.stake,
    }
}

/// Map a TradeNation [`OpeningOrder`] to a broker-agnostic [`PendingOrder`].
/// Returns `None` for a malformed order with neither stop nor limit trigger
/// (caller skips it rather than failing the whole list).
///
/// **Trigger semantics:** on a pending entry order, whichever of
/// `stop_order_price` / `limit_order_price` is set is the ENTRY TRIGGER —
/// stop-entry if `stop_order_price` is set, else limit-entry. This is the
/// trigger, NOT the attached SL/TP (those live in unparsed `IDO*` fields).
fn tn_order_to_pending(o: &OpeningOrder) -> Option<PendingOrder> {
    let (trigger, is_stop) = match (o.stop_order_price, o.limit_order_price) {
        (Some(s), _) => (s, true),
        (None, Some(l)) => (l, false),
        (None, None) => return None,
    };
    Some(PendingOrder {
        order_id: o.order_id.to_string(),
        instrument: o.market.clone(),
        direction: if o.direction == "Sell" {
            Direction::Short
        } else {
            Direction::Long
        },
        trigger,
        is_stop,
        stake: o.stake,
    })
}

fn to_upstream_risk(r: RiskBudget) -> broker_tradenation::RiskBudget {
    match r {
        RiskBudget::Percent(p) => broker_tradenation::RiskBudget::Percent(p),
        RiskBudget::Amount(a) => broker_tradenation::RiskBudget::Amount(a),
        RiskBudget::Units(s) => broker_tradenation::RiskBudget::Units(s),
    }
}

fn to_upstream_direction(d: Direction) -> broker_tradenation::Direction {
    match d {
        Direction::Long => broker_tradenation::Direction::Long,
        Direction::Short => broker_tradenation::Direction::Short,
    }
}

fn to_upstream_entry(e: &ResolvedEntry) -> broker_tradenation::ResolvedEntry {
    match e {
        // Upstream re-fetches the live bid/ask when placing a market order, so
        // `reference_price` is not threaded through — it only matters to the
        // OANDA risk math on the other side.
        ResolvedEntry::Market { .. } => broker_tradenation::ResolvedEntry::Market,
        ResolvedEntry::Stop { trigger_price } => broker_tradenation::ResolvedEntry::Stop {
            price: *trigger_price,
        },
        ResolvedEntry::Limit { trigger_price } => broker_tradenation::ResolvedEntry::Limit {
            price: *trigger_price,
        },
    }
}

/// Whether `tradenation-api` serves this TF (natively OR by aggregation).
///
/// As of `tradenation-api` v0.4.0 the adapter fetches candles through
/// `aggregation::get_candles_range_aggregated`, which serves H4/M5 by fetching
/// the native base (H1/M1) and rolling it up on 00/04/08/12/16/20 UTC buckets
/// (matches OANDA + TradingView). So H4 and M5 are now **served** — these are
/// `true`. TN's raw endpoint still has no H4/M5 path of its own, but the adapter
/// no longer calls it directly for candles. A TF that the aggregator genuinely
/// cannot build would still be rejected (`UnsupportedGranularity`); today every
/// engine `Granularity` is covered (min/15m/hour/day native; H4=4×H1, M5=5×M1).
const TN_SERVES_H4: bool = true;
const TN_SERVES_M5: bool = true;

/// Engine [`Granularity`] → `candle_model::Granularity` + whether
/// `tradenation-api` serves it (native or aggregated). Native TFs are
/// minute / quarter(15m) / hour / day; H4/M5 are served by aggregation
/// ([`TN_SERVES_H4`]/[`TN_SERVES_M5`] = `true`, see there). Pure.
fn to_cm_granularity(g: Granularity) -> (CmGranularity, bool) {
    match g {
        Granularity::M1 => (CmGranularity::OneMinute, true),
        Granularity::M15 => (CmGranularity::FifteenMinutes, true),
        Granularity::H1 => (CmGranularity::OneHour, true),
        Granularity::D1 => (CmGranularity::OneDay, true),
        Granularity::M5 => (CmGranularity::FiveMinutes, TN_SERVES_M5),
        Granularity::H4 => (CmGranularity::FourHours, TN_SERVES_H4),
    }
}

/// How many candles to fetch ending at `now` to be sure of covering
/// `(since, now]`. TN's OHLCV is count-back-from-end, so we ask for one bar per
/// `granularity` step in the window plus a small slack for boundary alignment,
/// clamped to TN's per-request ceiling. `filter_new_candles` trims any extra.
/// Pure — unit-tested.
fn candle_count_for_window(g: Granularity, since: DateTime<Utc>, now: DateTime<Utc>) -> usize {
    const SLACK: i64 = 3;
    let span = (now - since).num_seconds().max(0);
    let bars = span / g.seconds() + 1 + SLACK;
    bars.clamp(1, TN_MAX_CANDLES_PER_REQUEST) as usize
}

/// TN's chart endpoint caps a single request; keep well under it. A window
/// wider than this many bars must be fetched in multiple count-back requests
/// (see [`chunk_windows`]).
const TN_MAX_CANDLES_PER_REQUEST: i64 = 1000;

/// Split `(since, now]` into count-back request windows, each fetching at most
/// [`TN_MAX_CANDLES_PER_REQUEST`] bars, walking **backward** from `now`.
///
/// TN's OHLCV endpoint is count-back-from-an-`end`, capped per request. A wide
/// window (e.g. 30 days of M1 ≈ 43k bars) therefore needs several requests, each
/// ending one granularity-step before the previous chunk's earliest bar. Returns
/// `(count, end)` pairs newest-first; the caller fetches each, unions, and trims
/// to `(since, now]`. Each `count` carries the same `+1+SLACK` boundary slack as
/// [`candle_count_for_window`] so adjacent chunks overlap by a bar rather than
/// leaving a gap (the union dedups by timestamp).
///
/// Pure — unit-tested. A degenerate (`since >= now`) span yields one minimal
/// window (the caller guards `BadRange` earlier anyway).
fn chunk_windows(
    g: Granularity,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Vec<(usize, DateTime<Utc>)> {
    const SLACK: i64 = 3;
    let step = g.seconds().max(1);
    // Time span one full request covers, in seconds.
    let chunk_span = TN_MAX_CANDLES_PER_REQUEST * step;

    let mut windows = Vec::new();
    let mut end = now;
    loop {
        // Bars from `since` up to this chunk's `end`, capped at the per-request
        // ceiling (+slack), never zero.
        let span = (end - since).num_seconds().max(0);
        let bars = (span / step + 1 + SLACK).clamp(1, TN_MAX_CANDLES_PER_REQUEST) as usize;
        windows.push((bars, end));

        // Earliest bar this chunk reaches back to (approx). Once it's at/below
        // `since` we've covered the whole window.
        let reached = end - Duration::seconds(chunk_span);
        if reached <= since {
            break;
        }
        // Next chunk ends one step before this chunk's earliest bar; the +slack
        // overlap means the union has no gap at the seam.
        end = reached + Duration::seconds(step);
        // Safety: never loop unboundedly (a year of M1 is ~525 chunks; cap far
        // above any real request).
        if windows.len() >= 1024 {
            break;
        }
    }
    windows
}

fn from_upstream_error(e: broker_tradenation::EntryError) -> EntryError {
    use broker_tradenation::EntryError as U;
    match e {
        U::AccountFetch => EntryError::AccountFetch,
        U::EquityParse => EntryError::EquityParse,
        U::RiskCapExceeded { requested, cap } => EntryError::RiskCapExceeded { requested, cap },
        U::OpenPositionsCapExceeded => EntryError::OpenPositionsCapExceeded,
        U::UnitsBelowMinimum => EntryError::UnitsBelowMinimum,
        U::EntryTooCloseToMarket => EntryError::EntryTooCloseToMarket,
        U::OrderRejected => EntryError::OrderRejected,
    }
}

/// Pure helper running plan §3's four-step algorithm against
/// pre-fetched TradeNation payloads. Split out from the trait impl
/// so unit tests can cover every branch without hitting the network.
///
/// - Step 1 matches `OpeningOrder.order_id` against `broker_order_id`.
/// - Step 2 matches `Position.order_id` (the originating order id
///   correlation) — **not** `Position.position_id`. On TN those
///   differ; the worker stored the order id on `EntryAttempt` so
///   that's what we look up by, and we return `position_id` as the
///   `broker_trade_id` so future closed-trade lookups can correlate.
/// - Step 3 matches `TransactionRecord.ref_id` against the stored
///   `broker_trade_id`. **Will never match in v1** — TN's transaction
///   history exposes `RefID`, not `PositionID`. Closed TN trades
///   fall through to `Cancelled`. Live with it for v1.
/// - Step 4 returns `Cancelled` when we had a snapshot,
///   `Unknown` when we didn't.
fn compute_attempt_state(
    instrument: &str,
    broker_order_id: &str,
    broker_trade_id: Option<&str>,
    pending: &[OpeningOrder],
    positions: &[Position],
    closed: Option<&[TransactionRecord]>,
) -> AttemptState {
    // TN market names are case-sensitive in places; normalise on the
    // way in to match the upstream broker-trait impl.
    let inst_key = instrument.to_lowercase();

    // 1. Pending: opening order on this instrument with matching id.
    let is_pending = pending
        .iter()
        .filter(|o| o.market.to_lowercase() == inst_key)
        .any(|o| o.order_id.to_string() == broker_order_id);
    if is_pending {
        return AttemptState::Pending;
    }

    // 2. Open position belonging to this attempt. TN populates
    //    `Position.order_id` as the originating OrderID and
    //    `Position.position_id` as the distinct PositionID.
    //
    //    We match on EITHER:
    //      - `Position.order_id == broker_order_id` (the originating
    //        entry order id we placed), OR
    //      - `Position.position_id == broker_trade_id` (the PositionID
    //        snapshotted onto the attempt on a prior lookup).
    //
    //    The second correlation is load-bearing: on a bracketed entry,
    //    TN executes the entry order and then attaches a *fresh* SL
    //    child order with a NEW id, and the live `Position.order_id`
    //    can report that child id rather than the original entry id we
    //    stored. Matching only on the entry order id then misses the
    //    still-open position and falls through to `Unknown`, which is
    //    exactly the Bug #11 duplicate-entry hole. See
    //    `bug-011-reentry-while-position-open.md`. We still return the
    //    PositionID as broker_trade_id so the row gets snapshotted for
    //    subsequent lookups.
    let open_match = positions
        .iter()
        .filter(|p| p.market_name.to_lowercase() == inst_key)
        .find(|p| {
            p.order_id.to_string() == broker_order_id
                || broker_trade_id.is_some_and(|btid| p.position_id.to_string() == btid)
        });
    if let Some(p) = open_match {
        return AttemptState::OpenPosition {
            broker_trade_id: p.position_id.to_string(),
        };
    }

    // 3. Closed trade match by broker_trade_id (against RefID — see
    //    helper doc for why this currently won't match on TN).
    if let (Some(btid), Some(history)) = (broker_trade_id, closed)
        && let Some(rec) = history
            .iter()
            .filter(|r| r.is_trade())
            .filter(|r| r.description.to_lowercase().contains(&inst_key))
            .find(|r| r.ref_id == btid)
    {
        let pl = rec.profit_loss_f64();
        return if pl > 0.0 {
            AttemptState::ClosedWin { realized_pl: pl }
        } else {
            AttemptState::ClosedLossOrBreakeven { realized_pl: pl }
        };
    }

    // 4. Distinguish lost-snapshot from never-snapshotted.
    if broker_trade_id.is_some() {
        AttemptState::Cancelled
    } else {
        AttemptState::Unknown
    }
}

#[cfg(test)]
mod attempt_state_tests {
    use super::*;

    fn opening_order(order_id: u64, market: &str) -> OpeningOrder {
        OpeningOrder {
            order_id,
            market_id: 0,
            market: market.into(),
            direction: "Buy".into(),
            stake: 1.0,
            stop_order_price: Some(1.1),
            limit_order_price: None,
            current_price: None,
            currency_symbol: String::new(),
            period: None,
            period_original: String::new(),
            creation_time_utc: String::new(),
            quote_id: 0,
        }
    }

    fn position(position_id: u64, order_id: u64, market_name: &str) -> Position {
        Position {
            position_id,
            order_id,
            market_id: 0,
            market_name: market_name.into(),
            direction: "Buy".into(),
            stake: 1.0,
            opening_price: 1.1,
            current_price: 1.1,
            open_pl: 0.0,
            stop_order_price: None,
            limit_order_price: None,
            imr: 0.0,
            currency_symbol: String::new(),
            creation_time: None,
            creation_time_original: String::new(),
            quote_id: 0,
            tradable: true,
        }
    }

    fn closed_trade(ref_id: &str, description: &str, profit_loss: &str) -> TransactionRecord {
        TransactionRecord {
            description: description.into(),
            ref_id: ref_id.into(),
            action: "Trade Receivable".into(),
            // `is_trade()` requires TransactionType=2 and non-empty open/close prices.
            transaction_type: "2".into(),
            transaction_date: None,
            transaction_date_original: String::new(),
            open_period: None,
            open_period_original: String::new(),
            open_price: "1.0".into(),
            close_price: "1.1".into(),
            profit_loss: profit_loss.into(),
            amount: "1.0".into(),
            currency: "USD".into(),
        }
    }

    #[test]
    fn pending_when_order_id_in_opening_orders() {
        let pending = vec![opening_order(101, "EUR/USD"), opening_order(102, "EUR/USD")];
        let s = compute_attempt_state("EUR/USD", "101", None, &pending, &[], None);
        assert_eq!(s, AttemptState::Pending);
    }

    #[test]
    fn pending_filter_respects_instrument() {
        // Same order id but different instrument — should NOT match
        // (defensive: TN order ids are globally unique in practice,
        // but the algorithm filters anyway and the test pins it).
        let pending = vec![opening_order(101, "AUD/USD")];
        let s = compute_attempt_state("EUR/USD", "101", None, &pending, &[], None);
        assert_eq!(s, AttemptState::Unknown);
    }

    #[test]
    fn open_position_matches_on_originating_order_id_not_position_id() {
        // Critical TN-specific behaviour: position.order_id is the
        // originating OrderID (correlates to broker_order_id);
        // position.position_id is the distinct PositionID and is
        // returned as broker_trade_id.
        let positions = vec![position(
            /*PositionID*/ 9999, /*OrderID*/ 101, "EUR/USD",
        )];
        let s = compute_attempt_state("EUR/USD", "101", None, &[], &positions, None);
        assert_eq!(
            s,
            AttemptState::OpenPosition {
                broker_trade_id: "9999".into()
            }
        );
        // And verify we do NOT match if we look up by the position id
        // (which would be the wrong correlation key).
        let s_wrong = compute_attempt_state("EUR/USD", "9999", None, &[], &positions, None);
        assert_eq!(s_wrong, AttemptState::Unknown);
    }

    #[test]
    fn open_position_matches_on_position_id_when_order_id_drifted() {
        // Bug #11 regression: a bracketed TN entry executes and the live
        // position can report the SL *child* order id (here 26815021),
        // NOT the original entry order id we placed and stored
        // (26815011). Looking up by the stored entry order id alone
        // would miss → Unknown → duplicate entry. But once we've
        // snapshotted the PositionID as broker_trade_id, matching on it
        // must still resolve the position as Open.
        let positions = vec![position(
            /*PositionID*/ 27205376, /*live OrderID (SL child)*/ 26815021, "EUR/CAD",
        )];
        // Stored entry order id 26815011 no longer matches any
        // Position.order_id, but the snapshotted PositionID does.
        let s = compute_attempt_state(
            "EUR/CAD",
            "26815011",
            Some("27205376"),
            &[],
            &positions,
            None,
        );
        assert_eq!(
            s,
            AttemptState::OpenPosition {
                broker_trade_id: "27205376".into()
            }
        );
    }

    #[test]
    fn closed_win_when_ref_id_matches_and_pl_positive() {
        let closed = vec![closed_trade("ref-7", "EUR/USD", "12.5")];
        let s = compute_attempt_state("EUR/USD", "101", Some("ref-7"), &[], &[], Some(&closed));
        assert_eq!(s, AttemptState::ClosedWin { realized_pl: 12.5 });
    }

    #[test]
    fn closed_loss_or_breakeven_when_pl_non_positive() {
        let closed = vec![
            closed_trade("ref-loss", "EUR/USD", "-7.0"),
            closed_trade("ref-be", "EUR/USD", "0.0"),
        ];
        let loss =
            compute_attempt_state("EUR/USD", "101", Some("ref-loss"), &[], &[], Some(&closed));
        assert_eq!(
            loss,
            AttemptState::ClosedLossOrBreakeven { realized_pl: -7.0 }
        );
        let be = compute_attempt_state("EUR/USD", "101", Some("ref-be"), &[], &[], Some(&closed));
        assert_eq!(be, AttemptState::ClosedLossOrBreakeven { realized_pl: 0.0 });
    }

    #[test]
    fn cancelled_when_snapshotted_but_history_does_not_match() {
        // The realistic v1 TN case: trade_id was snapshotted (so we
        // know the attempt filled at some point), but the closed
        // history scan can't find it because RefID != PositionID.
        // Algorithm should report Cancelled.
        let closed = vec![closed_trade("ref-other", "EUR/USD", "5.0")];
        let s = compute_attempt_state("EUR/USD", "101", Some("9999"), &[], &[], Some(&closed));
        assert_eq!(s, AttemptState::Cancelled);
    }

    #[test]
    fn unknown_when_no_snapshot_and_nothing_to_match() {
        // No broker_trade_id snapshot, not pending, not open.
        let s = compute_attempt_state("EUR/USD", "101", None, &[], &[], None);
        assert_eq!(s, AttemptState::Unknown);
    }

    #[test]
    fn pending_takes_priority_over_open_branch() {
        // Defensive ordering check.
        let pending = vec![opening_order(101, "EUR/USD")];
        let positions = vec![position(9999, 101, "EUR/USD")];
        let s = compute_attempt_state("EUR/USD", "101", None, &pending, &positions, None);
        assert_eq!(s, AttemptState::Pending);
    }

    #[test]
    fn closed_skips_non_trade_records() {
        // is_trade() requires TransactionType="2" + non-empty
        // open/close prices. A funding/conversion record with a
        // matching ref_id must NOT be classified as a closed trade.
        let mut funding = closed_trade("ref-7", "EUR/USD", "5.0");
        funding.transaction_type = "1".into();
        let s = compute_attempt_state("EUR/USD", "101", Some("ref-7"), &[], &[], Some(&[funding]));
        assert_eq!(s, AttemptState::Cancelled);
    }
}

#[cfg(test)]
mod candle_fetch_tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn baseline_native_granularities_map_and_flag() {
        // Minute / quarter / hour / day are always TN-native, independent of the
        // H4/M5 switch.
        assert_eq!(
            to_cm_granularity(Granularity::M1),
            (CmGranularity::OneMinute, true)
        );
        assert_eq!(
            to_cm_granularity(Granularity::M15),
            (CmGranularity::FifteenMinutes, true)
        );
        assert_eq!(
            to_cm_granularity(Granularity::H1),
            (CmGranularity::OneHour, true)
        );
        assert_eq!(
            to_cm_granularity(Granularity::D1),
            (CmGranularity::OneDay, true)
        );
    }

    #[test]
    fn h4_m5_map_to_the_right_timeframe_and_are_served() {
        // H4/M5 map to the correct `candle_model` timeframe and are now SERVED
        // (`true`): as of tradenation-api v0.4.0 the adapter fetches through
        // `get_candles_range_aggregated`, which builds H4=4×H1 / M5=5×M1 on
        // 00/04/08/12/16/20 UTC buckets. So they pass the fetch guard rather than
        // being rejected. See `TN_SERVES_H4`.
        let (h4_cm, h4_native) = to_cm_granularity(Granularity::H4);
        assert_eq!(h4_cm, CmGranularity::FourHours);
        assert_eq!(h4_native, TN_SERVES_H4);
        assert!(h4_native, "H4 is served via aggregation");

        let (m5_cm, m5_native) = to_cm_granularity(Granularity::M5);
        assert_eq!(m5_cm, CmGranularity::FiveMinutes);
        assert_eq!(m5_native, TN_SERVES_M5);
        assert!(m5_native, "M5 is served via aggregation");
    }

    #[test]
    fn count_covers_window_plus_slack() {
        // 5 H1 bars of span → 5 + 1 + 3 slack = 9.
        let since = ts("2026-06-16T00:00:00Z");
        let now = ts("2026-06-16T05:00:00Z");
        assert_eq!(candle_count_for_window(Granularity::H1, since, now), 9);
    }

    #[test]
    fn count_is_at_least_one_for_zero_span() {
        let t = ts("2026-06-16T00:00:00Z");
        // Degenerate span is guarded earlier (BadRange) but the helper must
        // still never return 0.
        assert!(candle_count_for_window(Granularity::H1, t, t) >= 1);
    }

    #[test]
    fn count_clamps_to_ceiling() {
        // A year of M1 bars would be ~525k; must clamp to the 1000 cap.
        let since = ts("2025-06-16T00:00:00Z");
        let now = ts("2026-06-16T00:00:00Z");
        assert_eq!(candle_count_for_window(Granularity::M1, since, now), 1000);
    }

    #[test]
    fn chunk_windows_single_for_small_range() {
        // 5 H1 bars fit in one request → one window, ending at `now`.
        let since = ts("2026-06-16T00:00:00Z");
        let now = ts("2026-06-16T05:00:00Z");
        let w = chunk_windows(Granularity::H1, since, now);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].1, now);
        assert_eq!(w[0].0, 9); // 5 + 1 + 3 slack, same as candle_count_for_window
    }

    #[test]
    fn chunk_windows_pages_wide_m1_range() {
        // 3 days of M1 = 4320 bars; at 1000/request that's ≥ 5 chunks.
        let since = ts("2026-06-13T00:00:00Z");
        let now = ts("2026-06-16T00:00:00Z");
        let w = chunk_windows(Granularity::M1, since, now);
        assert!(
            w.len() >= 5,
            "expected ≥5 chunks for 3d of M1, got {}",
            w.len()
        );
        // Newest chunk ends at `now`.
        assert_eq!(w[0].1, now);
        // Every chunk stays under the per-request ceiling.
        assert!(w.iter().all(|(c, _)| *c <= 1000));
        // Chunks walk strictly backward in time.
        assert!(w.windows(2).all(|p| p[1].1 < p[0].1));
        // The oldest chunk reaches back to at least `since` (its count-back span
        // covers the remaining window).
        let oldest_end = w.last().unwrap().1;
        assert!(
            oldest_end - Duration::seconds(1000 * 60) <= since,
            "oldest chunk must cover back to `since`"
        );
    }

    #[test]
    fn chunk_windows_seams_overlap_not_gap() {
        // Adjacent chunks must overlap (slack) so the union has no missing bar.
        let since = ts("2026-06-13T00:00:00Z");
        let now = ts("2026-06-16T00:00:00Z");
        let w = chunk_windows(Granularity::M1, since, now);
        let step = 60i64;
        for pair in w.windows(2) {
            let newer_end = pair[0].1;
            let newer_earliest = newer_end - Duration::seconds(1000 * step);
            let older_end = pair[1].1;
            // The older chunk's end is at/after the newer chunk's earliest bar
            // (minus a step) → they meet or overlap, never leave a gap.
            assert!(
                older_end >= newer_earliest - Duration::seconds(step),
                "seam gap between chunks"
            );
        }
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::*;

    fn position_with(
        position_id: u64,
        order_id: u64,
        market_name: &str,
        direction: &str,
        sl: Option<f64>,
        tp: Option<f64>,
    ) -> Position {
        Position {
            position_id,
            order_id,
            market_id: 0,
            market_name: market_name.into(),
            direction: direction.into(),
            stake: 2.5,
            opening_price: 1.1,
            current_price: 1.1,
            open_pl: 0.0,
            stop_order_price: sl,
            limit_order_price: tp,
            imr: 0.0,
            currency_symbol: String::new(),
            creation_time: None,
            creation_time_original: String::new(),
            quote_id: 0,
            tradable: true,
        }
    }

    fn opening_order_with(
        order_id: u64,
        market: &str,
        direction: &str,
        stop: Option<f64>,
        limit: Option<f64>,
    ) -> OpeningOrder {
        OpeningOrder {
            order_id,
            market_id: 0,
            market: market.into(),
            direction: direction.into(),
            stake: 3.0,
            stop_order_price: stop,
            limit_order_price: limit,
            current_price: None,
            currency_symbol: String::new(),
            period: None,
            period_original: String::new(),
            creation_time_utc: String::new(),
            quote_id: 0,
        }
    }

    #[test]
    fn position_maps_buy_to_long_and_keeps_sl_tp() {
        let p = position_with(9999, 101, "EUR/USD", "Buy", Some(1.05), Some(1.20));
        let o = tn_position_to_open(&p);
        assert_eq!(
            o,
            OpenPosition {
                instrument: "EUR/USD".into(),
                direction: Direction::Long,
                stop_loss: Some(1.05),
                take_profit: Some(1.20),
                position_id: "9999".into(),
                order_id: "101".into(),
                stake: 2.5,
            }
        );
    }

    #[test]
    fn position_maps_sell_to_short_and_optional_sl_tp() {
        let p = position_with(1, 2, "Spot Gold", "Sell", None, None);
        let o = tn_position_to_open(&p);
        assert_eq!(o.direction, Direction::Short);
        assert_eq!(o.stop_loss, None);
        assert_eq!(o.take_profit, None);
        assert_eq!(o.position_id, "1");
        assert_eq!(o.order_id, "2");
    }

    #[test]
    fn pending_stop_entry_sets_is_stop_true_and_uses_trigger() {
        // stop_order_price set → stop-entry, trigger = that price.
        let ord = opening_order_with(55, "EUR/USD", "Buy", Some(1.1234), None);
        let p = tn_order_to_pending(&ord).expect("mapped");
        assert_eq!(
            p,
            PendingOrder {
                order_id: "55".into(),
                instrument: "EUR/USD".into(),
                direction: Direction::Long,
                trigger: 1.1234,
                is_stop: true,
                stake: 3.0,
            }
        );
    }

    #[test]
    fn pending_limit_entry_sets_is_stop_false() {
        let ord = opening_order_with(56, "AUD/USD", "Sell", None, Some(0.6500));
        let p = tn_order_to_pending(&ord).expect("mapped");
        assert!(!p.is_stop);
        assert_eq!(p.trigger, 0.6500);
        assert_eq!(p.direction, Direction::Short);
    }

    #[test]
    fn pending_prefers_stop_when_both_present() {
        // Defensive: if both are somehow set, the stop side wins (matches
        // the "whichever of stop/limit is set" rule, stop-first).
        let ord = opening_order_with(57, "EUR/USD", "Buy", Some(1.10), Some(1.20));
        let p = tn_order_to_pending(&ord).expect("mapped");
        assert!(p.is_stop);
        assert_eq!(p.trigger, 1.10);
    }

    #[test]
    fn pending_with_neither_trigger_is_skipped() {
        let ord = opening_order_with(58, "EUR/USD", "Buy", None, None);
        assert_eq!(tn_order_to_pending(&ord), None);
    }

    #[test]
    fn amend_target_matches_position_by_position_id() {
        let positions = vec![position_with(
            9999,
            101,
            "EUR/USD",
            "Buy",
            Some(1.05),
            Some(1.20),
        )];
        let t = find_amend_target("9999", &positions, &[]).expect("found");
        // amend key is the ORIGINATING order id, not the position id.
        assert_eq!(t.order_id, 101);
        assert_eq!(t.market, "EUR/USD");
        assert_eq!(t.stake, 2.5);
        assert_eq!(t.existing_take_profit, Some(1.20));
    }

    #[test]
    fn amend_target_matches_position_by_order_id() {
        let positions = vec![position_with(9999, 101, "EUR/USD", "Buy", None, None)];
        let t = find_amend_target("101", &positions, &[]).expect("found");
        assert_eq!(t.order_id, 101);
        assert_eq!(t.existing_take_profit, None);
    }

    #[test]
    fn amend_target_falls_back_to_pending_order() {
        let pending = vec![opening_order_with(77, "EUR/USD", "Buy", Some(1.10), None)];
        let t = find_amend_target("77", &[], &pending).expect("found");
        assert_eq!(t.order_id, 77);
        // Pending orders carry no parsed SL/TP — TP left None.
        assert_eq!(t.existing_take_profit, None);
    }

    #[test]
    fn amend_target_none_when_id_absent() {
        assert!(find_amend_target("404", &[], &[]).is_none());
    }
}
