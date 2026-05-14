//! The trade intent (decrypted JSON) and the plaintext shell (TradingView-substituted
//! prices), plus the logic that merges the two into a `Resolved` intent ready for
//! risk-gating and order placement.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod resolution;

pub use resolution::{Resolved, ResolvedEntry};

/// Plaintext outer YAML — the part TradingView substitutes `{{...}}` into.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Shell {
    pub close: f64,
    pub high: f64,
    pub low: f64,
    /// ISO-8601 timestamp from TradingView. Used as an upper bound on the
    /// alert's freshness — alerts from yesterday should be obvious.
    pub time: DateTime<Utc>,
    /// Opaque encrypted blob.
    pub payload: String,
}

/// The fully-decrypted intent. `v` lets us reject future protocol versions cleanly.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Intent {
    /// Protocol version, must be `1`.
    pub v: u32,
    /// Unique id per intended trade, used for replay protection.
    pub id: String,
    /// Optional earliest time the alert is allowed to fire.
    #[serde(default)]
    pub not_before: Option<DateTime<Utc>>,
    /// Hard expiry — alerts that arrive after this are rejected.
    pub not_after: DateTime<Utc>,
    /// What to do.
    pub action: Action,
    /// OANDA instrument name, e.g. `EUR_USD`.
    pub instrument: String,
    /// Required for `enter`; ignored otherwise.
    #[serde(default)]
    pub direction: Option<Direction>,
    /// Required for `enter`.
    #[serde(default)]
    pub entry: Option<EntrySpec>,
    /// Required for `enter`.
    #[serde(default)]
    pub stop_loss: Option<PriceRef>,
    /// Required for `enter`.
    #[serde(default)]
    pub take_profit: Option<TakeProfit>,
    /// Required for `enter`. % of account equity. The server-side cap clamps it.
    #[serde(default)]
    pub risk_pct: Option<f64>,
    /// Required for `invalidate`.
    #[serde(default)]
    pub cooldown_hours: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Enter,
    Close,
    Invalidate,
    /// Read-only snapshot of cooldowns + recent seen ids. `instrument` is
    /// required by the schema but ignored — use any placeholder.
    Status,
    /// Clear a single instrument's cooldown.
    Unlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Long,
    Short,
}

/// Where in the plaintext shell to anchor a price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PriceAnchor {
    Close,
    High,
    Low,
}

/// Reference to a price derived from the plaintext shell, optionally offset.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PriceRef {
    pub from: PriceAnchor,
    /// Offset in pips. Sign matters: -2 means "low - 2 pips" regardless of direction.
    /// The "pip" here is the instrument's pip size; the caller supplies that.
    #[serde(default)]
    pub offset_pips: f64,
}

/// Take-profit can be specified either as a plaintext-anchored price (like SL)
/// or as an R-multiple of the stop distance.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum TakeProfit {
    /// `{ "from": "close", "offset_r": 2.0 }` — TP = entry + 2 × (entry - SL) for long.
    RMultiple {
        #[serde(default = "default_close_anchor")]
        from: PriceAnchor,
        offset_r: f64,
    },
    /// `{ "from": "high", "offset_pips": 10 }` — same shape as a SL price ref.
    Anchored(PriceRef),
}

fn default_close_anchor() -> PriceAnchor {
    PriceAnchor::Close
}

/// Entry order spec.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EntrySpec {
    /// Market order at current price (we use the plaintext `close` as the
    /// risk-math reference; OANDA fills at its own bid/ask).
    Market,
    /// Stop-entry pending order; price resolves against the plaintext shell.
    /// Triggers when price moves *through* the level — used for breakouts.
    /// Long stop sits *above* current price; short stop sits *below*.
    Stop {
        from: PriceAnchor,
        #[serde(default)]
        offset_pips: f64,
    },
    /// Limit pending order; price resolves against the plaintext shell.
    /// Fills when price comes *back* to the level — used for pullback entries.
    /// Long limit sits *below* current price; short limit sits *above*.
    Limit {
        from: PriceAnchor,
        #[serde(default)]
        offset_pips: f64,
    },
}

impl Shell {
    pub fn anchor_price(&self, anchor: PriceAnchor) -> f64 {
        match anchor {
            PriceAnchor::Close => self.close,
            PriceAnchor::High => self.high,
            PriceAnchor::Low => self.low,
        }
    }
}

impl PriceRef {
    pub fn resolve(&self, shell: &Shell, pip_size: f64) -> f64 {
        shell.anchor_price(self.from) + self.offset_pips * pip_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> Shell {
        Shell {
            close: 1.1000,
            high: 1.1020,
            low: 1.0980,
            time: "2026-05-13T12:00:00Z".parse().unwrap(),
            payload: "v1.dummy".into(),
        }
    }

    #[test]
    fn anchor_price_returns_correct_field() {
        let s = shell();
        assert_eq!(s.anchor_price(PriceAnchor::Close), 1.1000);
        assert_eq!(s.anchor_price(PriceAnchor::High), 1.1020);
        assert_eq!(s.anchor_price(PriceAnchor::Low), 1.0980);
    }

    #[test]
    fn price_ref_applies_pip_offset() {
        let s = shell();
        let sl = PriceRef {
            from: PriceAnchor::Low,
            offset_pips: -2.0,
        };
        // 1.0980 + (-2 * 0.0001) = 1.0978
        assert!((sl.resolve(&s, 0.0001) - 1.0978).abs() < 1e-9);
    }

    #[test]
    fn intent_round_trips_through_yaml() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.id, "abc");
        assert_eq!(intent.action, Action::Enter);
        assert_eq!(intent.direction, Some(Direction::Long));
    }

    #[test]
    fn intent_supports_stop_entry() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: stop, from: high, offset_pips: 2 }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: high, offset_pips: 50 }
            risk_pct: 0.5
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        match intent.entry {
            Some(EntrySpec::Stop { from, offset_pips }) => {
                assert_eq!(from, PriceAnchor::High);
                assert!((offset_pips - 2.0).abs() < 1e-9);
            }
            _ => panic!("expected stop entry"),
        }
    }

    #[test]
    fn intent_supports_limit_entry() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: limit, from: low, offset_pips: -5 }
            stop_loss: { from: low, offset_pips: -10 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        match intent.entry {
            Some(EntrySpec::Limit { from, offset_pips }) => {
                assert_eq!(from, PriceAnchor::Low);
                assert!((offset_pips - -5.0).abs() < 1e-9);
            }
            _ => panic!("expected limit entry"),
        }
    }

    #[test]
    fn status_intent_parses_without_extras() {
        let yaml = "
            v: 1
            id: status-a
            not_after: \"2026-05-14T03:30:00Z\"
            action: status
            instrument: ALL
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.action, Action::Status);
    }

    #[test]
    fn unlock_intent_parses_with_just_instrument() {
        let yaml = "
            v: 1
            id: unlock-a
            not_after: \"2026-05-14T03:30:00Z\"
            action: unlock
            instrument: EUR_USD
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.action, Action::Unlock);
        assert_eq!(intent.instrument, "EUR_USD");
    }

    #[test]
    fn invalidate_intent_parses_with_just_cooldown() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: invalidate
            instrument: EUR_USD
            cooldown_hours: 12
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.action, Action::Invalidate);
        assert_eq!(intent.cooldown_hours, Some(12));
    }
}
