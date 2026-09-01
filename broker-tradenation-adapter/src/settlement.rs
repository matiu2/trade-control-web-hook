//! Build a [`Settlement`] from TradeNation's two history endpoints.
//!
//! TradeNation exposes the account's past as two separate streams, and they
//! answer different questions:
//!
//! - **Activity** (`get_all_activity`) — the order/position *event* log: every
//!   placement, execution, and close, each with its fill `price` and `stake`.
//!   This is where the real entry and exit prices live.
//! - **Transactions** (`get_all_transactions`) — the settled *cash* ledger:
//!   realised P&L per closed trade, plus funding and conversions. This is
//!   where the money lives.
//!
//! Neither alone answers "what happened to my trade": activity knows the
//! prices but not the P&L, transactions know the P&L but arrive keyed by a
//! `RefID` that is not the order id. So both are fetched, the summary is built
//! from what each is authoritative for, and **both raw streams are retained**
//! so a human can audit the summary.
//!
//! # Attribution is best-effort, and says so
//!
//! TradeNation's `TransactionRecord` carries only a `RefID` — **not** the
//! originating `OrderID` or `PositionID` (the same gap that already stops
//! `lookup_attempt_state` resolving closed TN trades). The activity log's
//! `result` field does carry ids, as free text like
//! `"Execute Order:26793941"` / `"Close Position:27187050"`, so that is what
//! we match our order ids against.
//!
//! Where attribution fails we keep the rows and record a warning rather than
//! dropping evidence or guessing — an archived plan is never re-derivable.

use chrono::{DateTime, Utc};
use trade_control_core::settlement::{LedgerEntry, LedgerSource, SettledTrade, Settlement};

/// The instrument-agnostic prefix TradeNation uses in an activity `result`
/// when a position is closed, e.g. `"Close Position:27187050"`.
const CLOSE_POSITION_PREFIX: &str = "Close Position:";
/// …and when an order executes, e.g. `"Execute Order:26793941"`.
const EXECUTE_ORDER_PREFIX: &str = "Execute Order:";

/// Convert a broker-local timestamp to UTC. Upstream already resolved the
/// broker's London time to a fixed Brisbane offset, so this is a pure offset
/// shift with no zone guessing.
fn to_utc(ts: Option<DateTime<chrono::FixedOffset>>) -> Option<DateTime<Utc>> {
    ts.map(|t| t.with_timezone(&Utc))
}

/// Parse one of TradeNation's stringly-typed numerics.
///
/// Returns `None` for anything unparseable — including the `""` and `"-"` the
/// platform uses for "not applicable". Deliberately **not**
/// `TransactionRecord::profit_loss_f64`, which folds a parse failure into
/// `0.0`: that turns an unknown P&L into a breakeven trade, the exact
/// conflation [`trade_control_core::settlement`] exists to prevent.
fn parse_num(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return None;
    }
    trimmed.parse().ok()
}

/// Pull the numeric id out of an activity `result` like
/// `"Close Position:27187050"`. `None` when the row isn't of that shape.
fn id_after(result: &str, prefix: &str) -> Option<String> {
    result
        .strip_prefix(prefix)
        .map(|rest| rest.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// Does this activity row refer to one of the orders we placed?
///
/// Matches the ids embedded in `result` (both the execute and close shapes)
/// against the worker's own `EntryAttempt` order ids. A row that mentions none
/// of them belongs to some other trade on the account.
fn mentions_our_order(result: &str, our_ids: &[String]) -> bool {
    let referenced =
        id_after(result, EXECUTE_ORDER_PREFIX).or_else(|| id_after(result, CLOSE_POSITION_PREFIX));
    match referenced {
        Some(id) => our_ids.contains(&id),
        // No id in the text: fall back to a substring probe so a differently
        // worded row still attributes rather than being silently dropped.
        None => our_ids.iter().any(|ours| result.contains(ours.as_str())),
    }
}

/// Map one activity record to a raw ledger row, preserving the broker's own
/// wording in `description` (`result` is the row's most informative field).
fn activity_to_entry(rec: &tradenation_api::ActivityRecord) -> LedgerEntry {
    LedgerEntry {
        source: LedgerSource::Activity,
        reference: id_after(&rec.result, EXECUTE_ORDER_PREFIX)
            .or_else(|| id_after(&rec.result, CLOSE_POSITION_PREFIX)),
        occurred_at: to_utc(rec.transaction_date),
        description: rec.result.clone(),
        instrument: Some(rec.market.clone()),
        price: rec.price,
        size: parse_num(&rec.stake),
        // An activity row is an event, not a cash movement — the money lives
        // in the cash ledger. Leaving this `None` keeps the two streams
        // honest rather than duplicating a price into an amount column.
        amount: None,
        currency: Some(rec.currency.clone()),
    }
}

/// Map one cash-ledger record to a raw ledger row.
fn transaction_to_entry(rec: &tradenation_api::TransactionRecord) -> LedgerEntry {
    LedgerEntry {
        source: LedgerSource::Cash,
        reference: Some(rec.ref_id.clone()),
        occurred_at: to_utc(rec.transaction_date),
        // `action` is the ledger's own wording ("Trade Receivable"); the
        // market name lives in `description`, so both are kept and joined
        // rather than one being dropped.
        description: format!("{} ({})", rec.action, rec.description),
        instrument: Some(rec.description.clone()),
        price: parse_num(&rec.close_price),
        size: parse_num(&rec.amount),
        amount: parse_num(&rec.profit_loss),
        currency: Some(rec.currency.clone()),
    }
}

/// Build a [`SettledTrade`] from a cash-ledger trade row.
///
/// The cash ledger is authoritative for money and for the open/close prices of
/// a *settled* trade; it carries no order id, so `broker_order_id` is left
/// `None` and the `ref_id` stands as the trade's identity.
fn settled_from_transaction(rec: &tradenation_api::TransactionRecord) -> SettledTrade {
    SettledTrade {
        broker_trade_id: rec.ref_id.clone(),
        broker_order_id: None,
        instrument: Some(rec.description.clone()),
        entry_price: parse_num(&rec.open_price),
        exit_price: parse_num(&rec.close_price),
        size: parse_num(&rec.amount),
        opened_at: to_utc(rec.open_period),
        closed_at: to_utc(rec.transaction_date),
        realized_pl: parse_num(&rec.profit_loss),
        // TN folds financing into the trade's P&L rather than reporting it
        // separately on the trade row, so there is nothing to split out.
        financing: None,
        currency: Some(rec.currency.clone()),
    }
}

/// Assemble the settlement from both streams.
///
/// Pure so every attribution branch is unit-testable without a TradeNation
/// session — the fetching half lives in the adapter's trait impl.
///
/// `instrument` is TradeNation's market name (`"EUR/USD"`); `our_order_ids`
/// are the ids this worker placed. Rows for other instruments are dropped;
/// rows for this instrument that can't be tied to our orders are **kept in the
/// ledger** (with a warning) but do not become summaries — they are evidence,
/// not conclusions.
pub(crate) fn build_settlement(
    instrument: &str,
    our_order_ids: &[String],
    activity: &[tradenation_api::ActivityRecord],
    transactions: &[tradenation_api::TransactionRecord],
    since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Settlement {
    let mut warnings = Vec::new();

    // Activity rows for this instrument inside the window.
    let ours: Vec<&tradenation_api::ActivityRecord> = activity
        .iter()
        .filter(|r| r.market == instrument)
        .filter(|r| to_utc(r.transaction_date).is_none_or(|t| t >= since))
        .collect();

    let attributed: Vec<&&tradenation_api::ActivityRecord> = ours
        .iter()
        .filter(|r| mentions_our_order(&r.result, our_order_ids))
        .collect();

    if !ours.is_empty() && attributed.is_empty() && !our_order_ids.is_empty() {
        warnings.push(format!(
            "no activity row referenced our order ids ({}) — rows kept unattributed",
            our_order_ids.join(",")
        ));
    }

    // Cash rows: TN gives us no order id here (only `RefID`), so we can filter
    // by instrument + window and by "is a closed trade", but not by *our*
    // trade. Say so rather than implying the P&L is definitely ours.
    let cash: Vec<&tradenation_api::TransactionRecord> = transactions
        .iter()
        .filter(|r| r.description == instrument)
        .filter(|r| to_utc(r.transaction_date).is_none_or(|t| t >= since))
        .collect();

    let trades: Vec<SettledTrade> = cash
        .iter()
        .filter(|r| r.is_trade())
        .map(|r| settled_from_transaction(r))
        .collect();

    if !trades.is_empty() {
        warnings.push(
            "cash-ledger rows carry only a RefID, not an OrderID — trades are matched by \
             instrument and time window, so a concurrent manual trade on the same market \
             could be included"
                .to_string(),
        );
    }

    let ledger: Vec<LedgerEntry> = ours
        .iter()
        .map(|r| activity_to_entry(r))
        .chain(cash.iter().map(|r| transaction_to_entry(r)))
        .collect();

    Settlement {
        broker: "tradenation".to_string(),
        fetched_at: now,
        trades,
        ledger,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid stamp")
    }

    fn bne(secs: i64) -> Option<DateTime<chrono::FixedOffset>> {
        let off = chrono::FixedOffset::east_opt(10 * 3600).expect("10h is a valid offset");
        Some(utc(secs).with_timezone(&off))
    }

    fn activity(market: &str, result: &str, price: Option<f64>) -> tradenation_api::ActivityRecord {
        tradenation_api::ActivityRecord {
            market: market.into(),
            transaction_date: bne(1_000),
            transaction_date_original: String::new(),
            expiry_date: None,
            expiry_date_original: String::new(),
            channel: "System".into(),
            direction: "Sell".into(),
            direction_id: "True".into(),
            stake: "2.75".into(),
            price,
            order_type: String::new(),
            stop_order_price: None,
            limit_order_price: None,
            quote_mode: String::new(),
            good_till: String::new(),
            result: result.into(),
            currency: "AUD".into(),
            is_rolling_market: true,
        }
    }

    fn transaction(
        market: &str,
        open: &str,
        close: &str,
        pl: &str,
    ) -> tradenation_api::TransactionRecord {
        tradenation_api::TransactionRecord {
            description: market.into(),
            ref_id: "27187050".into(),
            action: "Trade Receivable".into(),
            transaction_type: "2".into(),
            transaction_date: bne(2_000),
            transaction_date_original: String::new(),
            open_period: bne(1_000),
            open_period_original: String::new(),
            open_price: open.into(),
            close_price: close.into(),
            profit_loss: pl.into(),
            amount: "2.75".into(),
            currency: "AUD".into(),
        }
    }

    #[test]
    fn a_settled_trade_carries_the_real_fills_and_pl() {
        let s = build_settlement(
            "EUR/USD",
            &["26793941".into()],
            &[activity("EUR/USD", "Execute Order:26793941", Some(1.1000))],
            &[transaction("EUR/USD", "1.1000", "1.1050", "1.25")],
            utc(0),
            utc(9_999),
        );
        let t = &s.trades[0];
        assert_eq!(
            t.entry_price,
            Some(1.1000),
            "the real fill, not the trigger"
        );
        assert_eq!(t.exit_price, Some(1.1050));
        assert_eq!(t.realized_pl, Some(1.25));
        assert_eq!(s.total_realized_pl(), Some(1.25));
        assert!(!t.is_open(), "a settled cash row means the trade closed");
    }

    #[test]
    fn an_unparseable_pl_is_none_not_zero() {
        // Upstream's own `profit_loss_f64()` returns 0.0 here, which would read
        // as a breakeven trade. We must not inherit that.
        let s = build_settlement(
            "EUR/USD",
            &["26793941".into()],
            &[],
            &[transaction("EUR/USD", "1.1000", "1.1050", "n/a")],
            utc(0),
            utc(9_999),
        );
        assert_eq!(
            s.trades[0].realized_pl, None,
            "an unparseable P&L must be unknown, never a breakeven 0.0"
        );
        assert_eq!(s.total_realized_pl(), None);
    }

    #[test]
    fn rows_for_other_instruments_are_excluded() {
        let s = build_settlement(
            "EUR/USD",
            &["26793941".into()],
            &[activity(
                "Spot Gold",
                "Execute Order:99999999",
                Some(2000.0),
            )],
            &[transaction("Spot Gold", "2000", "2010", "5.0")],
            utc(0),
            utc(9_999),
        );
        assert!(s.trades.is_empty(), "another market's trade is not ours");
        assert!(s.ledger.is_empty());
    }

    #[test]
    fn unattributable_activity_is_kept_as_evidence_with_a_warning() {
        // The row is for our instrument but names an order we didn't place.
        // Dropping it would destroy the only record of what happened.
        let s = build_settlement(
            "EUR/USD",
            &["26793941".into()],
            &[activity("EUR/USD", "Execute Order:11111111", Some(1.1))],
            &[],
            utc(0),
            utc(9_999),
        );
        assert_eq!(s.ledger.len(), 1, "the row is retained, not discarded");
        assert!(
            s.warnings.iter().any(|w| w.contains("no activity row")),
            "and the gap is stated, got {:?}",
            s.warnings
        );
    }

    #[test]
    fn the_refid_attribution_limit_is_declared_not_hidden() {
        // TN's cash ledger has no OrderID, so a same-market manual trade could
        // be swept in. That must be visible to whoever reads the number.
        let s = build_settlement(
            "EUR/USD",
            &["26793941".into()],
            &[],
            &[transaction("EUR/USD", "1.1000", "1.1050", "1.25")],
            utc(0),
            utc(9_999),
        );
        assert!(
            s.warnings.iter().any(|w| w.contains("RefID")),
            "the attribution limit must be recorded, got {:?}",
            s.warnings
        );
    }

    #[test]
    fn rows_before_the_window_are_excluded() {
        // A 90-day fetch spans far more than one plan's life; only the plan's
        // own window is relevant.
        let s = build_settlement(
            "EUR/USD",
            &["26793941".into()],
            &[activity("EUR/USD", "Execute Order:26793941", Some(1.1))],
            &[transaction("EUR/USD", "1.1000", "1.1050", "1.25")],
            utc(5_000), // after both fixtures' timestamps
            utc(9_999),
        );
        assert!(s.trades.is_empty(), "pre-window trade excluded");
        assert!(s.ledger.is_empty(), "pre-window activity excluded");
    }

    #[test]
    fn a_non_trade_cash_row_is_ledgered_but_not_summarised() {
        // Funding / conversion rows are real ledger entries but not trades;
        // summarising them would invent a trade that never happened.
        let mut funding = transaction("EUR/USD", "", "", "-0.12");
        funding.transaction_type = "3".into();
        funding.action = "Funding".into();
        let s = build_settlement("EUR/USD", &[], &[], &[funding], utc(0), utc(9_999));
        assert!(s.trades.is_empty(), "funding is not a trade");
        assert_eq!(s.ledger.len(), 1, "but it is still ledgered");
        assert_eq!(s.ledger[0].amount, Some(-0.12));
    }

    #[test]
    fn both_streams_are_tagged_by_source() {
        // Activity and cash answer different questions; a reader must be able
        // to tell which stream a row came from.
        let s = build_settlement(
            "EUR/USD",
            &["26793941".into()],
            &[activity("EUR/USD", "Close Position:26793941", Some(1.105))],
            &[transaction("EUR/USD", "1.1000", "1.1050", "1.25")],
            utc(0),
            utc(9_999),
        );
        assert!(s.ledger.iter().any(|e| e.source == LedgerSource::Activity));
        assert!(s.ledger.iter().any(|e| e.source == LedgerSource::Cash));
    }

    #[test]
    fn a_close_position_row_attributes_by_its_embedded_id() {
        // The two `result` shapes ("Execute Order:" / "Close Position:") must
        // both resolve, or the exit row goes unattributed.
        assert!(mentions_our_order(
            "Close Position:27187050",
            &["27187050".into()]
        ));
        assert!(mentions_our_order(
            "Execute Order:26793941",
            &["26793941".into()]
        ));
        assert!(!mentions_our_order(
            "Close Position:27187050",
            &["99999999".into()]
        ));
    }

    #[test]
    fn parse_num_treats_the_platforms_blanks_as_unknown() {
        // TN renders "not applicable" as "" or "-"; neither is a zero.
        assert_eq!(parse_num(""), None);
        assert_eq!(parse_num("-"), None);
        assert_eq!(parse_num("  "), None);
        assert_eq!(parse_num("0"), Some(0.0), "an explicit zero IS a value");
        assert_eq!(parse_num("1.25"), Some(1.25));
    }
}
