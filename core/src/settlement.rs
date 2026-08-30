//! What the broker says actually happened to a trade, fetched once the plan
//! has finished and stored alongside it.
//!
//! # Why this exists
//!
//! Everything else in this codebase records what the system *decided*: the
//! plan, the fired rules, the intents dispatched, the order ids returned. None
//! of it records what the **broker** did — the real fill price, the real exit,
//! the realised P&L, the financing. The operator's trade log could show that a
//! `05-enter` fired and an order id came back, but not what the trade was
//! actually worth.
//!
//! [`Settlement`] is that missing half: a broker-reported account of one
//! finished trade, fetched at archive time and persisted on the archived plan.
//!
//! # Two layers, deliberately kept separate
//!
//! - [`SettledTrade`] — the **normalised** summary every broker can answer:
//!   entry, exit, size, realised P&L. This is what a report reads.
//! - [`LedgerEntry`] — the **raw** activity / transaction rows behind it, kept
//!   close to each broker's own shape. This is what you read when the summary
//!   looks wrong and you need to see the fees, the partial closes, the
//!   financing, and the order of events.
//!
//! Normalising the ledger away would lose exactly the detail it exists to
//! provide, so it is carried as text-ish rows rather than forced into a
//! common schema. Normalising *nothing* would push per-broker parsing into
//! every consumer. Hence both.
//!
//! # Everything is optional, and that is not laziness
//!
//! Brokers disagree about what they report, and a fetch can partly fail. A
//! `None` here means **the broker did not tell us**, which is a different
//! fact from zero — a `realized_pl` of `Some(0.0)` is a breakeven trade, while
//! `None` is an unknown one. A consumer must never render `None` as `0`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A broker-reported account of one finished trade, captured when its plan was
/// archived. See the module docs for why the summary and the raw ledger are
/// both kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settlement {
    /// Which broker answered — `"oanda"` / `"tradenation"`. Kept because the
    /// [`LedgerEntry`] rows are only interpretable against their source.
    pub broker: String,
    /// When this settlement was fetched. Not when the trade closed — see
    /// [`SettledTrade::closed_at`] for that.
    pub fetched_at: DateTime<Utc>,
    /// The normalised per-trade summaries. Usually one; more when a plan
    /// re-entered (multi-shot places several trades under one `trade_id`).
    pub trades: Vec<SettledTrade>,
    /// The raw broker rows behind the summaries, newest-first as the broker
    /// returned them. May be non-empty even when `trades` is empty — a fetch
    /// that finds activity it cannot confidently attribute still keeps it
    /// rather than discarding evidence.
    pub ledger: Vec<LedgerEntry>,
    /// Why this settlement is incomplete, when it is. Empty on a clean fetch.
    /// A partial answer is kept and explained rather than thrown away: an
    /// archived plan is never re-derivable, so a half-answer beats nothing.
    pub warnings: Vec<String>,
}

impl Settlement {
    /// Realised P&L across every settled trade, in account currency. `None`
    /// when no trade reported one — never `Some(0.0)` as a stand-in for
    /// unknown, which would read as a breakeven trade.
    pub fn total_realized_pl(&self) -> Option<f64> {
        let mut seen = false;
        let total = self
            .trades
            .iter()
            .filter_map(|t| t.realized_pl)
            .inspect(|_| seen = true)
            .sum();
        seen.then_some(total)
    }

    /// True when the broker answered nothing at all — no summary and no raw
    /// rows. Distinguishes "we asked and there was nothing" from "we never
    /// asked", which is the absence of a `Settlement` entirely.
    pub fn is_empty(&self) -> bool {
        self.trades.is_empty() && self.ledger.is_empty()
    }
}

/// One finished trade as the broker reports it, normalised across brokers.
///
/// Every field past the ids is `Option` because brokers differ in what they
/// return and a partial fetch is kept rather than dropped. `None` means the
/// broker did not report it — see the module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettledTrade {
    /// The broker's own id for this trade / position. OANDA trade id,
    /// TradeNation `PositionID`.
    pub broker_trade_id: String,
    /// The originating order id, when the broker distinguishes it. On
    /// TradeNation `OrderID` differs from `PositionID`; on OANDA they match.
    pub broker_order_id: Option<String>,
    pub instrument: Option<String>,
    /// The price the trade actually opened at — the **fill**, unlike the
    /// requested rate on [`crate::broker::Placement`].
    pub entry_price: Option<f64>,
    /// The price it actually closed at. `None` while still open.
    pub exit_price: Option<f64>,
    /// Position size: OANDA units, TradeNation stake.
    pub size: Option<f64>,
    pub opened_at: Option<DateTime<Utc>>,
    /// `None` means the broker reports it as still open. That is a real state
    /// at archive time, not an error — see the `is_open` note below.
    pub closed_at: Option<DateTime<Utc>>,
    /// Realised P&L in account currency. `Some(0.0)` is a genuine breakeven;
    /// `None` is unknown.
    pub realized_pl: Option<f64>,
    /// Financing / swap / carry charged over the life of the trade, when the
    /// broker separates it from `realized_pl`.
    pub financing: Option<f64>,
    /// Account currency the money figures are in (`"AUD"`, `"USD"`).
    pub currency: Option<String>,
}

impl SettledTrade {
    /// Whether the broker still reports this trade as open.
    ///
    /// A plan can archive while a position is open — the engine retires on a
    /// terminal veto or the trade-expiry clock, not on the position closing —
    /// so this is a legitimate outcome of an archive-time fetch, not a fault.
    pub fn is_open(&self) -> bool {
        self.closed_at.is_none()
    }
}

/// One raw row from a broker's activity log or cash ledger, kept close to the
/// broker's own shape.
///
/// This is deliberately *not* normalised into a common schema — see the module
/// docs. It exists so that when the summary looks wrong you can read what the
/// broker actually said, in its own words.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// Which stream this came from — the event log or the cash ledger. They
    /// answer different questions and must not be conflated.
    pub source: LedgerSource,
    /// The broker's reference for this row (TradeNation `RefID`, OANDA
    /// transaction id).
    pub reference: Option<String>,
    /// When it happened, as the broker reported it.
    pub occurred_at: Option<DateTime<Utc>>,
    /// The broker's own description of the event — TradeNation's
    /// `"Close Position:27187050"` / `"Trade Receivable"`, OANDA's transaction
    /// type. Kept verbatim: it is the row's most useful field and any
    /// re-wording would lose information.
    pub description: String,
    pub instrument: Option<String>,
    /// Price on the row, when it carries one (a fill price for an execution).
    pub price: Option<f64>,
    /// Size / stake on the row, when it carries one.
    pub size: Option<f64>,
    /// Money moved by this row, in `currency`.
    pub amount: Option<f64>,
    pub currency: Option<String>,
}

/// Which broker stream a [`LedgerEntry`] came from.
///
/// TradeNation exposes these as genuinely separate endpoints answering
/// different questions, and conflating them loses that: an execution is not a
/// cash movement, and a financing charge is not an order event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LedgerSource {
    /// The order/position event log — placements, fills, closes. TradeNation's
    /// Activity panel; OANDA's order/trade transactions.
    Activity,
    /// The settled cash ledger — realised P&L, funding, conversions.
    /// TradeNation's transaction history; OANDA's account transactions.
    Cash,
}

impl LedgerSource {
    /// Stable label for logs and rendered output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Cash => "cash",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(realized_pl: Option<f64>) -> SettledTrade {
        SettledTrade {
            broker_trade_id: "t1".into(),
            broker_order_id: None,
            instrument: None,
            entry_price: None,
            exit_price: None,
            size: None,
            opened_at: None,
            closed_at: None,
            realized_pl,
            financing: None,
            currency: None,
        }
    }

    fn settlement(trades: Vec<SettledTrade>) -> Settlement {
        Settlement {
            broker: "tradenation".into(),
            fetched_at: DateTime::from_timestamp(0, 0).expect("epoch is a valid timestamp"),
            trades,
            ledger: Vec::new(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn total_realized_pl_sums_every_reporting_trade() {
        // A multi-shot plan places several trades under one trade_id; the log
        // wants the trade's total, not just the last leg.
        let s = settlement(vec![trade(Some(1.5)), trade(Some(-0.5))]);
        assert_eq!(s.total_realized_pl(), Some(1.0));
    }

    #[test]
    fn total_realized_pl_is_none_when_nothing_reported_not_zero() {
        // The distinction this type exists to preserve: an unknown P&L must
        // not render as a breakeven trade.
        let s = settlement(vec![trade(None), trade(None)]);
        assert_eq!(
            s.total_realized_pl(),
            None,
            "unknown must stay None, never Some(0.0)"
        );
    }

    #[test]
    fn a_genuine_breakeven_is_some_zero_not_none() {
        // The mirror of the case above — Some(0.0) is a real answer.
        let s = settlement(vec![trade(Some(0.0))]);
        assert_eq!(s.total_realized_pl(), Some(0.0));
    }

    #[test]
    fn a_partly_reporting_set_sums_only_what_was_reported() {
        // One leg answered, one didn't: report the known part rather than
        // discarding it, and don't count the unknown as zero.
        let s = settlement(vec![trade(Some(2.0)), trade(None)]);
        assert_eq!(s.total_realized_pl(), Some(2.0));
    }

    #[test]
    fn a_trade_the_broker_still_reports_open_is_flagged() {
        // A plan archives on a terminal veto / expiry, which can precede the
        // position closing — so this is a real outcome, not a fault.
        assert!(trade(None).is_open());
        let mut closed = trade(Some(1.0));
        closed.closed_at = DateTime::from_timestamp(1, 0);
        assert!(!closed.is_open());
    }

    #[test]
    fn is_empty_separates_no_answer_from_an_unasked_question() {
        assert!(settlement(Vec::new()).is_empty());
        let mut with_ledger = settlement(Vec::new());
        with_ledger.ledger.push(LedgerEntry {
            source: LedgerSource::Cash,
            reference: None,
            occurred_at: None,
            description: "Trade Receivable".into(),
            instrument: None,
            price: None,
            size: None,
            amount: None,
            currency: None,
        });
        assert!(
            !with_ledger.is_empty(),
            "raw rows with no summary still count as an answer"
        );
    }

    #[test]
    fn settlement_round_trips_through_json() {
        // It is persisted as a jsonb body on the archived plan, so the shape
        // must survive a store round-trip.
        let mut s = settlement(vec![trade(Some(1.25))]);
        s.ledger.push(LedgerEntry {
            source: LedgerSource::Activity,
            reference: Some("27187050".into()),
            occurred_at: DateTime::from_timestamp(1_700_000_000, 0),
            description: "Close Position:27187050".into(),
            instrument: Some("EUR/USD".into()),
            price: Some(1.1025),
            size: Some(2.75),
            amount: Some(1.25),
            currency: Some("AUD".into()),
        });
        s.warnings.push("cash ledger unavailable".into());
        let json = serde_json::to_string(&s).expect("serialises");
        let back: Settlement = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, s);
    }

    #[test]
    fn ledger_source_serialises_as_a_stable_kebab_label() {
        // Stored rows outlive the code that wrote them; the discriminant must
        // not be a bare integer that renumbers when a variant is added.
        let json = serde_json::to_string(&LedgerSource::Activity).expect("serialises");
        assert_eq!(json, "\"activity\"");
        assert_eq!(LedgerSource::Cash.as_str(), "cash");
    }
}
