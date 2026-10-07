//! Broker-confirmed exits, independent of whether the setup may enter again.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::intent::Direction;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
    TrailingStopLoss,
    Other,
    Unknown,
}

impl ExitReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::TakeProfit => "hit take profit",
            Self::StopLoss => "hit stop loss",
            Self::TrailingStopLoss => "hit trailing stop loss",
            Self::Other => "closed by broker",
            Self::Unknown => "closed (exit reason unavailable)",
        }
    }
}

/// Actual execution and the trigger of the exit order that filled. Never
/// infer the reason from the sign of P&L: a moved stop can close at a profit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerTradeExit {
    pub broker_trade_id: String,
    pub exit_order_id: Option<String>,
    pub transaction_id: Option<String>,
    /// None if only a closed status is available, without the broker's time.
    pub closed_at: Option<DateTime<Utc>>,
    pub reason: ExitReason,
    pub broker_reason: Option<String>,
    pub entry_price: Option<f64>,
    pub exit_price: Option<f64>,
    /// Broker's final TP/SL trigger, including amendments. None for a manual
    /// market close, or when the broker cannot report the trigger.
    pub expected_price: Option<f64>,
    pub realized_pl: Option<f64>,
    pub account_currency: Option<String>,
    /// OANDA units, for calculating cash slippage in the quote currency.
    /// Spread-bet stake must not be passed off as units.
    pub units: Option<f64>,
    pub quote_currency: Option<String>,
}

/// Persisted as a structured cron note. Keeping both order and trade IDs
/// makes the report usable as evidence with the broker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerExit {
    pub broker_order_id: String,
    pub attempt_no: u32,
    pub direction: Direction,
    pub pip_size: Option<f64>,
    pub execution: BrokerTradeExit,
}

impl BrokerExit {
    /// Positive = adverse (cost); negative = favourable (bonus), for both
    /// long and short exits. A trigger amendment is not execution slippage.
    pub fn slippage_price(&self) -> Option<f64> {
        let expected = self.execution.expected_price?;
        let actual = self.execution.exit_price?;
        let delta = match self.direction {
            Direction::Long => expected - actual,
            Direction::Short => actual - expected,
        };
        delta.is_finite().then_some(delta)
    }

    pub fn summary_lines(&self) -> Vec<String> {
        let e = &self.execution;
        let mut lines = vec![
            format!("{} — attempt #{}", e.reason.label(), self.attempt_no),
            format!(
                "order={} trade={} exit-order={}",
                self.broker_order_id,
                e.broker_trade_id,
                e.exit_order_id.as_deref().unwrap_or("unavailable")
            ),
        ];
        if let Some(actual) = e.exit_price {
            let expected = e
                .expected_price
                .map(|p| format!("{p:.5}"))
                .unwrap_or_else(|| "unavailable".into());
            lines.push(format!(
                "expected exit {expected} → broker fill {actual:.5}"
            ));
        }
        if let Some(delta) = self.slippage_price() {
            let label = if delta < -1e-10 {
                "favourable / bonus"
            } else if delta > 1e-10 {
                "adverse / cost"
            } else {
                "no slippage"
            };
            let pips = self
                .pip_size
                .filter(|p| p.is_finite() && *p > 0.0)
                .map(|p| format!("{:+.2} pips", delta / p))
                .unwrap_or_else(|| format!("{delta:+.5} price"));
            let cash = e
                .units
                .zip(e.quote_currency.as_deref())
                .filter(|(units, _)| units.is_finite())
                .map(|(units, currency)| format!(" ({:+.2} {currency})", delta * units.abs()))
                .unwrap_or_default();
            lines.push(format!("exit slippage {pips}{cash} — {label}"));
        }
        if let Some(pl) = e.realized_pl {
            let currency = e.account_currency.as_deref().unwrap_or("account currency");
            lines.push(format!("broker realized P&L {pl:+.2} {currency}"));
        }
        if let Some(reason) = &e.broker_reason {
            let txn = e.transaction_id.as_deref().unwrap_or("unavailable");
            lines.push(format!("broker reason={reason} transaction={txn}"));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exit(direction: Direction, expected: f64, actual: f64) -> BrokerExit {
        BrokerExit {
            broker_order_id: "2525".into(),
            attempt_no: 1,
            direction,
            pip_size: Some(0.0001),
            execution: BrokerTradeExit {
                broker_trade_id: "2526".into(),
                exit_order_id: Some("2528".into()),
                transaction_id: Some("2540".into()),
                closed_at: Some("2026-10-07T17:10:00Z".parse().unwrap()),
                reason: ExitReason::StopLoss,
                broker_reason: Some("STOP_LOSS_ORDER".into()),
                entry_price: Some(0.99078),
                exit_price: Some(actual),
                expected_price: Some(expected),
                realized_pl: Some(-6169.1689),
                account_currency: Some("AUD".into()),
                units: Some(100_000.0),
                quote_currency: Some("CAD".into()),
            },
        }
    }

    #[test]
    fn favourable_and_adverse_slippage_mirror_with_direction() {
        for (direction, expected, actual, pips) in [
            (Direction::Long, 1.0, 1.0001, -1.0),
            (Direction::Long, 1.0, 0.9999, 1.0),
            (Direction::Short, 1.0, 0.9999, -1.0),
            (Direction::Short, 1.0, 1.0001, 1.0),
        ] {
            let e = exit(direction, expected, actual);
            assert!((e.slippage_price().unwrap() / 0.0001 - pips).abs() < 1e-8);
            let text = e.summary_lines().join("\n");
            assert!(text.contains(if pips < 0.0 { "bonus" } else { "cost" }));
            assert!(text.contains(if pips < 0.0 {
                "-10.00 CAD"
            } else {
                "+10.00 CAD"
            }));
        }
    }

    #[test]
    fn missing_trigger_does_not_invent_slippage() {
        let mut e = exit(Direction::Short, 1.0, 1.01);
        e.execution.expected_price = None;
        assert_eq!(e.slippage_price(), None);
        assert!(!e.summary_lines().join("\n").contains("slippage"));
    }
}
