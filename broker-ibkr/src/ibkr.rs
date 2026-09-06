//! IBKR broker operations, behind the `Broker` trait surface in [`crate`].
//!
//! # What is implemented, and what is deliberately not
//!
//! This lands the **sizing** half of the IBKR broker: [`place_entry`] resolves
//! equity, applies the risk caps, and converts a risk budget into whole
//! contracts using the multiplier baked onto the signed intent. That is the
//! part that can be verified offline, and it is the part the rest of the system
//! has been waiting on since the multiplier was baked.
//!
//! The **order-transmitting** half is not wired yet. Every operation that would
//! transmit to, or read live state from, the Gateway returns a loud
//! "unimplemented" rather than a plausible-looking empty answer:
//!
//! | operation | why not yet |
//! |---|---|
//! | order submission | the spike has never placed an order; the paper Gateway path is unexercised |
//! | [`get_quote`] | needs a market-data entitlement decision — see below |
//! | positions / pending orders | needs the subscription-draining pattern the order path establishes |
//!
//! **Why refuse rather than stub.** An empty `Vec` from `list_open_positions`
//! does not read as "not implemented" — it reads as "the account is flat", and
//! the cron treats it as such: the breakeven watch, the pending sweep and the
//! blackout apply all act on that answer. A stub here would not fail, it would
//! quietly mismanage positions. `no_silent_degrade_prefer_loud_failure` applies
//! with unusual force to a broker adapter, because the caller cannot tell a
//! polite lie from the truth.
//!
//! # Market data is not optional
//!
//! [`get_quote`] feeds `sl_spread_floor`, a **hard entry gate**: an entry is
//! refused unless its stop distance clears `SL_MIN_SPREAD_MULTIPLE` (10x) the
//! live spread. Delayed COMEX/CME data would make that gate read a stale
//! spread and either block every entry or wave through a trade whose stop sits
//! inside the real spread. So this must fail closed until a live entitlement is
//! confirmed — the plan flags entitlements as the most likely thing to stop the
//! project, and a silently-delayed quote is exactly how that would go unnoticed.

use core::future::Future;

use chrono::{DateTime, Utc};
use ibapi::prelude::Client;
use trade_control_core::broker::{
    AmendError, AttemptState, BidAskCandle, CancelError, Candle, CandleError, CloseOutcome,
    EntryError, EntryRequest, Granularity, LookupError, OpenPosition, PendingOrder, Placement,
    Quote,
};
use trade_control_core::intent::{ResolvedEntry, RiskBudget};

use crate::risk;

/// Default address of a paper-trading IB Gateway, re-exported so callers need
/// not depend on `ibkr-client` directly.
pub use ibkr_client::PAPER_GATEWAY;

/// Why an IBKR broker handle could not be built or used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IbkrError {
    /// The Gateway socket could not be reached, or the handshake failed.
    ///
    /// A frequent real cause is the Gateway reporting a timezone abbreviation
    /// `ibapi` does not know, which is a hard failure rather than a degrade —
    /// hence `ibkr-client::register_timezone_aliases`.
    Connect(String),
}

impl std::fmt::Display for IbkrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Deliberately unprefixed: every caller wraps this in its own
            // "connect failed" context, and prefixing here stutters
            // ("connect failed: connect failed: …") in the operator's error.
            Self::Connect(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for IbkrError {}

/// Risk-gate and size an entry, then place it.
///
/// The sizing path is complete and mirrors OANDA's order of operations: cap
/// check, equity, stop-distance sanity, FX, size, minimum check. The
/// difference is the quantum — whole contracts scaled by the contract
/// multiplier, see [`crate::risk`].
///
/// Transmission is not yet wired, so a non-dry-run placement refuses rather
/// than returning a fabricated order id (which downstream would record as a
/// real live order).
pub async fn place_entry(
    client: &Client,
    account_id: &str,
    max_risk_pct: f64,
    max_open_positions: u32,
    req: &EntryRequest<'_>,
) -> Result<Placement, EntryError> {
    place_entry_with(
        &GatewayAccounts { client },
        account_id,
        max_risk_pct,
        max_open_positions,
        req,
    )
    .await
}

/// Where an account snapshot comes from.
///
/// The Gateway read is the part of this broker that is genuinely unimplemented,
/// so it is named as a seam rather than inlined. That keeps the sizing logic —
/// which is complete — separable from the I/O that is not, and lets the real
/// implementation drop in without touching any of the money math.
trait AccountSource {
    fn snapshot(
        &self,
        account_id: &str,
        instrument: &str,
    ) -> impl Future<Output = Result<AccountSnapshot, EntryError>>;
}

/// The real source: a connected IB Gateway.
struct GatewayAccounts<'a> {
    #[allow(dead_code)] // Held for the unimplemented Gateway read below.
    client: &'a Client,
}

impl AccountSource for GatewayAccounts<'_> {
    async fn snapshot(
        &self,
        account_id: &str,
        instrument: &str,
    ) -> Result<AccountSnapshot, EntryError> {
        account_snapshot(account_id, instrument).await
    }
}

/// Risk-gate and size an entry against any [`AccountSource`].
///
/// `place_entry` is this with the Gateway wired in.
async fn place_entry_with<A: AccountSource>(
    accounts: &A,
    account_id: &str,
    max_risk_pct: f64,
    max_open_positions: u32,
    req: &EntryRequest<'_>,
) -> Result<Placement, EntryError> {
    // Order matters. A missing multiplier is a defect in how the trade was
    // ARMED, not a market condition, so it is refused before any account
    // round-trip — otherwise a connection failure would mask it and the
    // operator would chase the wrong problem during an incident.
    let multiplier = usable_multiplier(req.contract_multiplier)?;
    check_percent_cap(req.risk, max_risk_pct)?;

    let account = accounts.snapshot(account_id, req.instrument).await?;
    if account.open_position_count >= max_open_positions {
        return Err(EntryError::OpenPositionsCapExceeded);
    }

    let reference_price = req.entry.reference_price();
    let stop_distance = (reference_price - req.stop_loss).abs();
    if stop_distance <= 0.0 || !stop_distance.is_finite() {
        return Err(EntryError::OrderRejected);
    }

    let fx_rate = account.fx_quote_to_account;

    let (raw_contracts, effective_pct) = size_entry(
        req.risk,
        account.equity,
        max_risk_pct,
        stop_distance,
        multiplier,
        fx_rate,
    )?;

    // Fit onto the exchange's real order-size grid. IBKR reports `min_size` and
    // `size_increment` per contract, so this is the broker's own minimum rather
    // than a bare `== 0` check — the distinction OANDA's `units == 0` misses.
    // `None` means the size does not clear the exchange minimum, which is the
    // same operator-visible outcome as sizing to zero.
    let contracts = risk::fit_to_size_grid(
        raw_contracts,
        account.limits.min_size,
        account.limits.size_increment,
    )
    .unwrap_or(0);

    let dry = if req.dry_run { "DRY-RUN " } else { "" };
    tracing::info!(
        "{dry}ibkr sizing: instrument={} mode={:?} equity={} multiplier={multiplier} \
         fx_quote_to_account={fx_rate} effective_pct={effective_pct:.4} \
         entry_ref={reference_price} sl={} contracts={contracts}",
        req.instrument,
        req.risk,
        account.equity,
        req.stop_loss,
    );

    if contracts == 0 {
        // Sized honestly, and the answer was "not one contract". Since v137
        // this parks the setup and re-checks it once a bar, rather than
        // retrying forever — which matters far more on futures than on spot,
        // where one contract is 100% of the granularity.
        return Err(EntryError::UnitsBelowMinimum);
    }

    if req.dry_run {
        // The sizing path genuinely ran, so report the size — a dry-run exists
        // precisely to show what would have been placed. Same contract as
        // OANDA's dry-run (and unlike TradeNation's, which cannot size).
        return Ok(Placement {
            order_id: format!("dry-run-{}", req.instrument),
            size: Some(f64::from(contracts)),
            price: Some(reference_price),
        });
    }

    // Transmission is not implemented. Refusing keeps the failure visible and
    // retryable; inventing an order id would have the worker record a live
    // order that does not exist, and every later reconciliation would chase it.
    tracing::error!(
        "ibkr order transmission not implemented: refusing to place instrument={} \
         contracts={contracts} (sizing succeeded; entry={:?})",
        req.instrument,
        req.entry,
    );
    Err(EntryError::OrderRejected)
}

/// The contract multiplier this order will be sized on, or a refusal.
///
/// Checked **before** any account round-trip, because a missing multiplier is a
/// defect in how the trade was armed rather than a market condition — there is
/// nothing to learn by asking the broker, and surfacing it as `AccountFetch`
/// would send the operator chasing a connection problem.
///
/// There is deliberately no `unwrap_or(1.0)`: `1.0` is a *valid-looking*
/// multiplier that silently places an ES position 50x larger than authorised.
fn usable_multiplier(multiplier: Option<f64>) -> Result<f64, EntryError> {
    match multiplier {
        Some(m) if m.is_finite() && m > 0.0 => Ok(m),
        _ => Err(EntryError::ContractSizeUnavailable),
    }
}

/// Cheap ceiling check for `Percent` risk, exactly as OANDA does. `Amount` and
/// `Units` are checked against the equity-derived percent inside
/// [`size_entry`], once equity is known.
fn check_percent_cap(risk: RiskBudget, max_risk_pct: f64) -> Result<(), EntryError> {
    if let RiskBudget::Percent(pct) = risk
        && pct > max_risk_pct
    {
        return Err(EntryError::RiskCapExceeded {
            requested: pct,
            cap: max_risk_pct,
        });
    }
    Ok(())
}

/// Resolve the risk budget into whole contracts, plus the effective percent of
/// equity for the cap check and the operator's log line.
///
/// Split out from [`place_entry`] so the money math is testable without a
/// Gateway: everything above this is I/O, everything inside is arithmetic.
fn size_entry(
    risk: RiskBudget,
    equity: f64,
    max_risk_pct: f64,
    stop_distance: f64,
    multiplier: f64,
    fx_rate: f64,
) -> Result<(u32, f64), EntryError> {
    match risk {
        RiskBudget::Percent(pct) => {
            if equity <= 0.0 || !equity.is_finite() {
                return Err(EntryError::EquityParse);
            }
            let budget = equity * pct / 100.0;
            Ok((
                risk::contracts_for_budget(budget, stop_distance, multiplier, fx_rate),
                pct,
            ))
        }
        RiskBudget::Amount(amount) => {
            if equity <= 0.0 || !equity.is_finite() {
                return Err(EntryError::EquityParse);
            }
            let pct = amount / equity * 100.0;
            if pct > max_risk_pct {
                return Err(EntryError::RiskCapExceeded {
                    requested: pct,
                    cap: max_risk_pct,
                });
            }
            Ok((
                risk::contracts_for_budget(amount, stop_distance, multiplier, fx_rate),
                pct,
            ))
        }
        RiskBudget::Units(literal) => {
            if equity <= 0.0 || !equity.is_finite() {
                return Err(EntryError::EquityParse);
            }
            // A literal size on futures means CONTRACTS, so its implied money
            // risk must go through the multiplier too — otherwise the cap check
            // under-counts the risk by exactly the multiplier and waves through
            // a position 50x (ES) over the cap.
            let implied_amount = literal * stop_distance * multiplier * fx_rate;
            let pct = implied_amount / equity * 100.0;
            if pct > max_risk_pct {
                return Err(EntryError::RiskCapExceeded {
                    requested: pct,
                    cap: max_risk_pct,
                });
            }
            let contracts = if !literal.is_finite() || literal <= 0.0 {
                0
            } else {
                literal.floor().min(f64::from(u32::MAX)) as u32
            };
            Ok((contracts, pct))
        }
    }
}

/// The account facts sizing needs: equity, how many positions are open, the FX
/// rate from the contract's currency into the account's, and the exchange's
/// order-size limits for the contract being traded.
struct AccountSnapshot {
    equity: f64,
    open_position_count: u32,
    /// Value of one unit of the contract's quote currency in account currency.
    fx_quote_to_account: f64,
    /// The traded contract's exchange order-size limits.
    limits: SizeLimits,
}

/// An exchange's order-size rules for one contract, as IBKR reports them on
/// `ContractDetails`.
///
/// A named pair rather than two loose `f64` arguments: `min_size` and
/// `size_increment` are adjacent, same-typed and easily transposed, and
/// transposing them silently changes the order size rather than failing. Same
/// reasoning as `InstrumentSizing` in `instrument-lookup`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SizeLimits {
    /// Smallest order the exchange accepts, in contracts.
    pub min_size: f64,
    /// Order-size granularity, in contracts.
    pub size_increment: f64,
}

/// Read equity, open-position count and the FX rate from the Gateway.
///
/// Not implemented yet: it needs the same subscription-draining pattern the
/// order path will establish, and getting FX wrong mis-sizes the trade in
/// exactly the way `fx_quote_to_account` exists to prevent. Failing here keeps
/// [`place_entry`] honest — it cannot size against an invented equity.
async fn account_snapshot(
    account_id: &str,
    instrument: &str,
) -> Result<AccountSnapshot, EntryError> {
    tracing::error!(
        "ibkr account snapshot not implemented (account={account_id} instrument={instrument})"
    );
    Err(EntryError::AccountFetch)
}

/// Close every open position on `instrument`.
///
/// Returns [`CloseOutcome::Errored`] until implemented — **not**
/// [`CloseOutcome::NothingOpen`], which since v135 is a *successful* no-op that
/// consumes the intent id. Reporting an unimplemented close as "nothing was
/// open" would mark the close fulfilled and leave a real position running,
/// which is precisely the incident `CloseOutcome`'s three-way split exists to
/// prevent.
pub async fn close_positions(_client: &Client, account_id: &str, instrument: &str) -> CloseOutcome {
    tracing::error!("ibkr close_positions not implemented ({account_id} {instrument})");
    CloseOutcome::Errored
}

/// Cancel resting orders on `instrument`; returns the number cancelled.
///
/// Returns `0` and logs, which is honest here in a way it is not elsewhere: the
/// count is "how many we cancelled", and we cancelled none. The caller does not
/// read `0` as "the book is clean".
pub async fn cancel_pending_for_instrument(
    _client: &Client,
    account_id: &str,
    instrument: &str,
) -> usize {
    tracing::error!(
        "ibkr cancel_pending_for_instrument not implemented ({account_id} {instrument})"
    );
    0
}

/// Look up a previously-placed attempt.
///
/// `Transient` rather than [`AttemptState::Unknown`]: `Unknown` tells the retry
/// gate "this attempt is dead, a new entry may be placed", which would let the
/// gate re-place over an order it cannot see. `Transient` makes the caller
/// reject this fire and try again.
pub async fn lookup_attempt_state(
    _client: &Client,
    _account_id: &str,
    instrument: &str,
    broker_order_id: &str,
    _broker_trade_id: Option<&str>,
) -> Result<AttemptState, LookupError> {
    tracing::error!("ibkr lookup_attempt_state not implemented ({instrument} {broker_order_id})");
    Err(LookupError::Transient)
}

/// Cancel one resting order by broker id.
pub async fn cancel_order(_client: &Client, broker_order_id: &str) -> Result<(), CancelError> {
    tracing::error!("ibkr cancel_order not implemented (order={broker_order_id})");
    Err(CancelError::Transient)
}

/// Fetch a live two-sided quote.
///
/// **Fails closed on purpose.** See the module docs: this feeds the SL-spread
/// floor, a hard entry gate, and a delayed quote there is worse than no quote.
pub async fn get_quote(_client: &Client, instrument: &str) -> Result<Quote, LookupError> {
    tracing::error!(
        "ibkr get_quote not implemented ({instrument}) — market-data entitlement unconfirmed"
    );
    Err(LookupError::Transient)
}

/// All open positions on the account.
pub async fn list_open_positions(
    _client: &Client,
    account_id: &str,
) -> Result<Vec<OpenPosition>, LookupError> {
    tracing::error!("ibkr list_open_positions not implemented (account={account_id})");
    Err(LookupError::Transient)
}

/// Move an open position's stop.
pub async fn amend_stop(
    _client: &Client,
    position_or_order_id: &str,
    new_stop: f64,
) -> Result<(), AmendError> {
    tracing::error!("ibkr amend_stop not implemented ({position_or_order_id} -> {new_stop})");
    Err(AmendError::Transient)
}

/// All resting entry orders on the account.
pub async fn list_pending_orders(
    _client: &Client,
    account_id: &str,
) -> Result<Vec<PendingOrder>, LookupError> {
    tracing::error!("ibkr list_pending_orders not implemented (account={account_id})");
    Err(LookupError::Transient)
}

/// Historical mid candles.
pub async fn get_candles(
    _client: &Client,
    instrument: &str,
    granularity: Granularity,
    _since: DateTime<Utc>,
    _now: DateTime<Utc>,
) -> Result<Vec<Candle>, CandleError> {
    tracing::error!("ibkr get_candles not implemented ({instrument} {granularity:?})");
    Err(CandleError::Transient)
}

/// Historical bid/ask candles, for the fill simulator.
pub async fn get_bidask_candles(
    _client: &Client,
    instrument: &str,
    granularity: Granularity,
    _since: DateTime<Utc>,
    _now: DateTime<Utc>,
) -> Result<Vec<BidAskCandle>, CandleError> {
    tracing::error!("ibkr get_bidask_candles not implemented ({instrument} {granularity:?})");
    Err(CandleError::Transient)
}

/// Whether an entry is a market order — the only kind IBKR's bracket builder
/// can attach a stop-loss and take-profit to alongside a *stop* entry.
///
/// `ibapi`'s `BracketOrderBuilder` offers `entry_market()` and `entry_limit()`
/// and **no `entry_stop()`**, while this system's primary entry mode is a
/// stop-entry above/below the neckline. So a stop entry cannot be placed as a
/// single bracket and needs a parent stop order with children attached by
/// `parent_id` instead.
///
/// Recorded as a named predicate now, with its reasoning, so the order path
/// starts from the constraint rather than rediscovering it against a live
/// Gateway.
#[allow(dead_code)]
pub(crate) fn bracket_can_carry(entry: &ResolvedEntry) -> bool {
    matches!(
        entry,
        ResolvedEntry::Market { .. } | ResolvedEntry::Limit { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ES_MULTIPLIER: f64 = 50.0;

    /// A futures instrument arriving with no multiplier must be refused with
    /// its OWN error, not sized as though the multiplier were 1.0.
    ///
    /// This is the single most consequential test in the crate: silently
    /// defaulting to 1.0 places an ES position 50x larger than authorised, and
    /// nothing downstream would flag it — the order is valid, just enormous.
    #[test]
    fn a_futures_entry_without_a_multiplier_is_refused_not_defaulted() {
        let err = usable_multiplier(None).expect_err("None must be refused");
        assert!(
            matches!(err, EntryError::ContractSizeUnavailable),
            "expected ContractSizeUnavailable, got {err:?}"
        );
    }

    /// A present-but-unusable multiplier is the same refusal. `0.0` is the
    /// dangerous value: it makes risk-per-contract zero, so an unguarded
    /// division yields infinity rather than a refusal.
    #[test]
    fn an_unusable_multiplier_is_refused_too() {
        for bad in [0.0, -50.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let err = usable_multiplier(Some(bad)).expect_err("must be refused");
            assert!(
                matches!(err, EntryError::ContractSizeUnavailable),
                "multiplier {bad} must be refused, got {err:?}"
            );
        }
    }

    /// An account source that answers with fixed facts, and records whether it
    /// was consulted at all.
    struct FakeAccounts {
        snapshot: AccountSnapshot,
        consulted: std::cell::Cell<bool>,
    }

    impl FakeAccounts {
        fn with_equity(equity: f64) -> Self {
            Self {
                snapshot: AccountSnapshot {
                    equity,
                    open_position_count: 0,
                    fx_quote_to_account: 1.0,
                    limits: SizeLimits {
                        min_size: 1.0,
                        size_increment: 1.0,
                    },
                },
                consulted: std::cell::Cell::new(false),
            }
        }
    }

    impl AccountSource for FakeAccounts {
        async fn snapshot(
            &self,
            _account_id: &str,
            _instrument: &str,
        ) -> Result<AccountSnapshot, EntryError> {
            self.consulted.set(true);
            Ok(AccountSnapshot { ..self.snapshot })
        }
    }

    fn request<'a>(risk: RiskBudget, multiplier: Option<f64>) -> EntryRequest<'a> {
        EntryRequest {
            instrument: "ES",
            direction: trade_control_core::intent::Direction::Long,
            entry: ResolvedEntry::Market {
                reference_price: 5_800.0,
            },
            stop_loss: 5_780.0,
            take_profit: 5_860.0,
            risk,
            dry_run: true,
            contract_multiplier: multiplier,
        }
    }

    /// A missing multiplier must be refused **without consulting the account**.
    ///
    /// Ordering is the point, not just the error: if the account were fetched
    /// first, a Gateway that is merely down would mask the real defect and the
    /// operator would see `AccountFetch` — chasing a connection problem during
    /// an incident while the actual fault is an intent armed without a
    /// multiplier. Asserting only the error value would let the two be swapped
    /// silently, which is exactly what happened when this test was missing.
    #[tokio::test]
    async fn a_missing_multiplier_is_refused_before_the_account_is_consulted() {
        let accounts = FakeAccounts::with_equity(100_000.0);
        let err = place_entry_with(
            &accounts,
            "DU1",
            2.0,
            5,
            &request(RiskBudget::Percent(1.0), None),
        )
        .await
        .expect_err("no multiplier must refuse");

        assert!(
            matches!(err, EntryError::ContractSizeUnavailable),
            "got {err:?}"
        );
        assert!(
            !accounts.consulted.get(),
            "the account must NOT be fetched before the multiplier is validated"
        );
    }

    /// The mirror: a well-formed futures request DOES reach the account. Without
    /// this, moving the check later would satisfy the test above trivially (by
    /// never fetching at all).
    #[tokio::test]
    async fn a_well_formed_request_does_reach_the_account() {
        let accounts = FakeAccounts::with_equity(100_000.0);
        place_entry_with(
            &accounts,
            "DU1",
            2.0,
            5,
            &request(RiskBudget::Percent(1.0), Some(ES_MULTIPLIER)),
        )
        .await
        .expect("a complete request sizes");
        assert!(accounts.consulted.get(), "the account must be consulted");
    }

    /// End to end through `place_entry_with`: 1% of $100,000 is $1,000, one ES
    /// contract at a 20-point stop risks exactly that, so a dry run reports
    /// one contract. This is the whole chain — multiplier, sizing, size grid.
    #[tokio::test]
    async fn a_dry_run_reports_the_size_it_would_have_placed() {
        let accounts = FakeAccounts::with_equity(100_000.0);
        let placement = place_entry_with(
            &accounts,
            "DU1",
            2.0,
            5,
            &request(RiskBudget::Percent(1.0), Some(ES_MULTIPLIER)),
        )
        .await
        .expect("sizes");
        assert_eq!(placement.size, Some(1.0));
        assert_eq!(placement.price, Some(5_800.0));
    }

    /// A budget too small for one contract must surface as `UnitsBelowMinimum`
    /// — the variant Stage 7 parks and re-checks once a bar — and NOT as
    /// `ContractSizeUnavailable`, which would be a permanent-looking refusal
    /// for a condition that a change in equity genuinely fixes.
    #[tokio::test]
    async fn a_sub_contract_budget_is_below_minimum_not_unavailable() {
        // 1% of $10,000 = $100, against a $1,000-risk ES contract.
        let accounts = FakeAccounts::with_equity(10_000.0);
        let err = place_entry_with(
            &accounts,
            "DU1",
            2.0,
            5,
            &request(RiskBudget::Percent(1.0), Some(ES_MULTIPLIER)),
        )
        .await
        .expect_err("cannot afford a contract");
        assert!(matches!(err, EntryError::UnitsBelowMinimum), "got {err:?}");
    }

    /// The open-positions cap is enforced, mirroring OANDA.
    #[tokio::test]
    async fn the_open_positions_cap_is_enforced() {
        let mut accounts = FakeAccounts::with_equity(100_000.0);
        accounts.snapshot.open_position_count = 5;
        let err = place_entry_with(
            &accounts,
            "DU1",
            2.0,
            5,
            &request(RiskBudget::Percent(1.0), Some(ES_MULTIPLIER)),
        )
        .await
        .expect_err("at the cap must refuse");
        assert!(
            matches!(err, EntryError::OpenPositionsCapExceeded),
            "{err:?}"
        );
    }

    /// An exchange minimum above the sized quantity must refuse rather than
    /// rounding up to it — rounding up would place more risk than authorised.
    #[tokio::test]
    async fn a_size_under_the_exchange_minimum_refuses_rather_than_rounding_up() {
        let mut accounts = FakeAccounts::with_equity(100_000.0);
        accounts.snapshot.limits.min_size = 2.0;
        // Sizes to exactly 1 contract, which is below a min_size of 2.
        let err = place_entry_with(
            &accounts,
            "DU1",
            2.0,
            5,
            &request(RiskBudget::Percent(1.0), Some(ES_MULTIPLIER)),
        )
        .await
        .expect_err("1 contract must not clear a 2-contract minimum");
        assert!(matches!(err, EntryError::UnitsBelowMinimum), "got {err:?}");
    }

    /// A real multiplier passes through unchanged — the guard must not be so
    /// strict that it refuses the values we actually trade.
    #[test]
    fn every_traded_multiplier_is_accepted() {
        // Live values read off the paper Gateway 2026-09-06.
        for good in [100.0, 10.0, ES_MULTIPLIER, 5.0] {
            assert_eq!(
                usable_multiplier(Some(good)).expect("must be accepted"),
                good
            );
        }
    }

    /// The percent cap is refused up-front, before equity is fetched.
    #[test]
    fn a_percent_over_the_cap_is_refused_before_the_account_is_touched() {
        let err = check_percent_cap(RiskBudget::Percent(3.0), 2.0)
            .expect_err("3% must not pass a 2% cap");
        assert!(matches!(err, EntryError::RiskCapExceeded { .. }), "{err:?}");

        check_percent_cap(RiskBudget::Percent(2.0), 2.0).expect("at the cap is allowed");
        // `Amount` / `Units` cannot be judged without equity, so they pass this
        // gate and are capped inside `size_entry` instead.
        check_percent_cap(RiskBudget::Amount(1e9), 2.0).expect("amount deferred to size_entry");
        check_percent_cap(RiskBudget::Units(1e9), 2.0).expect("units deferred to size_entry");
    }

    /// The risk cap must be applied on the *contract* risk, i.e. through the
    /// multiplier. Without it a `Units` budget under-reports its own risk by
    /// exactly the multiplier and slips past the cap.
    #[test]
    fn a_literal_contract_count_reports_risk_through_the_multiplier() {
        // 2 ES contracts, 20-point stop ⇒ 2 × 20 × 50 = $2,000 risk.
        // On $100,000 equity that is 2.0%.
        let (contracts, pct) = size_entry(
            RiskBudget::Units(2.0),
            100_000.0,
            2.0,
            20.0,
            ES_MULTIPLIER,
            1.0,
        )
        .expect("2.0% is exactly at the cap");
        assert_eq!(contracts, 2);
        assert!((pct - 2.0).abs() < 1e-9, "got {pct}");

        // The same order against a 1.0% cap must be refused. Drop the
        // multiplier from the implied-risk math and this reads as 0.04% —
        // comfortably "under" a cap it exceeds fifty-fold.
        let err = size_entry(
            RiskBudget::Units(2.0),
            100_000.0,
            1.0,
            20.0,
            ES_MULTIPLIER,
            1.0,
        )
        .expect_err("2% risk must not pass a 1% cap");
        assert!(matches!(err, EntryError::RiskCapExceeded { .. }), "{err:?}");
    }

    /// Percent sizing goes through the multiplier as well.
    #[test]
    fn percent_sizing_uses_the_multiplier() {
        // 1% of $100,000 = $1,000. One ES contract at a 20-point stop risks
        // $1,000, so exactly 1 contract.
        let (contracts, pct) = size_entry(
            RiskBudget::Percent(1.0),
            100_000.0,
            2.0,
            20.0,
            ES_MULTIPLIER,
            1.0,
        )
        .expect("sizes");
        assert_eq!(contracts, 1);
        assert!((pct - 1.0).abs() < 1e-9);
    }

    /// A budget too small for one contract sizes to zero rather than erroring
    /// here — `place_entry` turns that into `UnitsBelowMinimum`, which parks.
    #[test]
    fn a_budget_below_one_contract_sizes_to_zero() {
        let (contracts, _) = size_entry(
            RiskBudget::Percent(0.5),
            100_000.0,
            2.0,
            20.0,
            ES_MULTIPLIER,
            1.0,
        )
        .expect("sizes");
        assert_eq!(contracts, 0, "$500 does not buy a $1,000-risk contract");
    }

    /// Zero or nonsensical equity must be an error, never a division that
    /// yields a nonsense percentage.
    #[test]
    fn unusable_equity_is_an_error() {
        for bad in [0.0, -1.0, f64::NAN] {
            for risk in [
                RiskBudget::Percent(1.0),
                RiskBudget::Amount(1_000.0),
                RiskBudget::Units(1.0),
            ] {
                let err = size_entry(risk, bad, 2.0, 20.0, ES_MULTIPLIER, 1.0)
                    .expect_err("equity {bad} must fail");
                assert!(matches!(err, EntryError::EquityParse), "{err:?}");
            }
        }
    }

    /// A stop entry cannot ride IBKR's bracket builder — it offers market and
    /// limit entries only. Pinned so the order path starts from the real
    /// constraint instead of discovering it against a live Gateway.
    #[test]
    fn only_market_and_limit_entries_fit_a_bracket() {
        assert!(bracket_can_carry(&ResolvedEntry::Market {
            reference_price: 5_800.0
        }));
        assert!(bracket_can_carry(&ResolvedEntry::Limit {
            trigger_price: 5_800.0
        }));
        assert!(
            !bracket_can_carry(&ResolvedEntry::Stop {
                trigger_price: 5_800.0
            }),
            "a stop entry needs a parent order with children attached by \
             parent_id — ibapi's BracketOrderBuilder has no entry_stop()"
        );
    }
}
