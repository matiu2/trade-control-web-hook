//! Merge an `Intent` with its plaintext `Shell` into a `Resolved` trade ready
//! for risk-gating and OANDA order placement.

use chrono::{DateTime, Utc};

use super::{Action, Direction, EntrySpec, Intent, Shell, TakeProfit};

#[cfg(test)]
use super::PriceAnchor;

/// Resolved entry order with concrete prices.
#[derive(Debug, Clone)]
pub enum ResolvedEntry {
    /// Market order; `reference_price` is the price we use for the risk math.
    Market { reference_price: f64 },
    /// Stop-entry pending order at `trigger_price`.
    Stop { trigger_price: f64 },
    /// Limit-entry pending order at `trigger_price`.
    Limit { trigger_price: f64 },
}

/// Fully-resolved trade intent, with all prices computed.
#[derive(Debug, Clone)]
pub struct Resolved {
    // `id` and `not_after` aren't read by current dispatch (it uses the raw
    // `Intent` for those) but carrying them keeps `Resolved` self-contained.
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub not_after: DateTime<Utc>,
    pub instrument: String,
    pub direction: Direction,
    pub entry: ResolvedEntry,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub risk_pct: f64,
}

#[derive(Debug)]
pub enum ResolveError {
    /// Field required for `enter` is missing.
    MissingField(&'static str),
    /// Direction and SL placement disagree (e.g. long with SL above entry).
    InvalidGeometry,
    /// Action is not `enter`.
    NotAnEntry,
}

impl core::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingField(name) => write!(f, "missing field for entry: {name}"),
            Self::InvalidGeometry => f.write_str("stop/entry geometry inconsistent with direction"),
            Self::NotAnEntry => f.write_str("intent is not an entry action"),
        }
    }
}

impl std::error::Error for ResolveError {}

impl Resolved {
    /// Build a `Resolved` from an entry intent + its plaintext shell.
    /// `pip_size` is the instrument's pip size (e.g. 0.0001 for EUR_USD).
    pub fn from_intent(
        intent: &Intent,
        shell: &Shell,
        pip_size: f64,
    ) -> Result<Self, ResolveError> {
        if intent.action != Action::Enter {
            return Err(ResolveError::NotAnEntry);
        }
        let direction = intent
            .direction
            .ok_or(ResolveError::MissingField("direction"))?;
        let entry_spec = intent
            .entry
            .as_ref()
            .ok_or(ResolveError::MissingField("entry"))?;
        let sl_ref = intent
            .stop_loss
            .as_ref()
            .ok_or(ResolveError::MissingField("stop_loss"))?;
        let tp_spec = intent
            .take_profit
            .as_ref()
            .ok_or(ResolveError::MissingField("take_profit"))?;
        let risk_pct = intent
            .risk_pct
            .ok_or(ResolveError::MissingField("risk_pct"))?;

        let stop_loss = sl_ref.resolve(shell, pip_size);

        let (entry, reference_price) = match entry_spec {
            EntrySpec::Market => (
                ResolvedEntry::Market {
                    reference_price: shell.close,
                },
                shell.close,
            ),
            EntrySpec::Stop { from, offset_pips } => {
                let trigger = shell.anchor_price(*from) + offset_pips * pip_size;
                // Stop sits on the *far* side of current price for the direction:
                // long stops above close, short stops below.
                match direction {
                    Direction::Long if trigger <= shell.close => {
                        return Err(ResolveError::InvalidGeometry);
                    }
                    Direction::Short if trigger >= shell.close => {
                        return Err(ResolveError::InvalidGeometry);
                    }
                    _ => {}
                }
                (
                    ResolvedEntry::Stop {
                        trigger_price: trigger,
                    },
                    trigger,
                )
            }
            EntrySpec::Limit { from, offset_pips } => {
                let trigger = shell.anchor_price(*from) + offset_pips * pip_size;
                // Limit sits on the *near* side of current price for the direction:
                // long limits below close, short limits above. If it's the wrong
                // side, OANDA would fill instantly (turning the limit into a
                // market order at a worse price) — reject as a typo.
                match direction {
                    Direction::Long if trigger >= shell.close => {
                        return Err(ResolveError::InvalidGeometry);
                    }
                    Direction::Short if trigger <= shell.close => {
                        return Err(ResolveError::InvalidGeometry);
                    }
                    _ => {}
                }
                (
                    ResolvedEntry::Limit {
                        trigger_price: trigger,
                    },
                    trigger,
                )
            }
        };

        // SL must be on the correct side of the entry for the direction.
        match direction {
            Direction::Long if stop_loss >= reference_price => {
                return Err(ResolveError::InvalidGeometry);
            }
            Direction::Short if stop_loss <= reference_price => {
                return Err(ResolveError::InvalidGeometry);
            }
            _ => {}
        }

        let take_profit = resolve_tp(
            tp_spec,
            shell,
            pip_size,
            direction,
            reference_price,
            stop_loss,
        );

        // TP must be on the opposite side of entry from SL.
        match direction {
            Direction::Long if take_profit <= reference_price => {
                return Err(ResolveError::InvalidGeometry);
            }
            Direction::Short if take_profit >= reference_price => {
                return Err(ResolveError::InvalidGeometry);
            }
            _ => {}
        }

        Ok(Self {
            id: intent.id.clone(),
            not_after: intent.not_after,
            instrument: intent.instrument.clone(),
            direction,
            entry,
            stop_loss,
            take_profit,
            risk_pct,
        })
    }
}

fn resolve_tp(
    spec: &TakeProfit,
    shell: &Shell,
    pip_size: f64,
    direction: Direction,
    entry: f64,
    stop_loss: f64,
) -> f64 {
    match spec {
        TakeProfit::Anchored(price_ref) => price_ref.resolve(shell, pip_size),
        TakeProfit::RMultiple { from: _, offset_r } => {
            // R is the stop-loss distance in price units, always positive.
            let r = (entry - stop_loss).abs();
            match direction {
                Direction::Long => entry + offset_r * r,
                Direction::Short => entry - offset_r * r,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::PriceRef;

    fn shell() -> Shell {
        Shell {
            close: 1.1000,
            high: 1.1020,
            low: 1.0980,
            time: "2026-05-13T12:00:00Z".parse().unwrap(),
            payload: "v1.dummy".into(),
        }
    }

    fn long_market_intent() -> Intent {
        Intent {
            v: 1,
            id: "t1".into(),
            not_before: None,
            not_after: "2026-05-13T20:00:00Z".parse().unwrap(),
            action: Action::Enter,
            instrument: "EUR_USD".into(),
            direction: Some(Direction::Long),
            entry: Some(EntrySpec::Market),
            stop_loss: Some(PriceRef {
                from: PriceAnchor::Low,
                offset_pips: -2.0,
            }),
            take_profit: Some(TakeProfit::RMultiple {
                from: PriceAnchor::Close,
                offset_r: 2.0,
            }),
            risk_pct: Some(0.5),
            cooldown_hours: None,
        }
    }

    #[test]
    fn long_market_resolves_r_multiple_tp() {
        let r = Resolved::from_intent(&long_market_intent(), &shell(), 0.0001).unwrap();
        // SL = 1.0980 - 2*0.0001 = 1.0978
        assert!((r.stop_loss - 1.0978).abs() < 1e-9);
        // entry = 1.1000, R = 0.0022, TP = 1.1000 + 2*0.0022 = 1.1044
        assert!((r.take_profit - 1.1044).abs() < 1e-9);
    }

    #[test]
    fn short_market_resolves_anchored_tp() {
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Short);
        intent.stop_loss = Some(PriceRef {
            from: PriceAnchor::High,
            offset_pips: 2.0,
        });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef {
            from: PriceAnchor::Low,
            offset_pips: -10.0,
        }));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        // SL = 1.1020 + 2*0.0001 = 1.1022
        assert!((r.stop_loss - 1.1022).abs() < 1e-9);
        // TP = 1.0980 - 10*0.0001 = 1.0970
        assert!((r.take_profit - 1.0970).abs() < 1e-9);
    }

    #[test]
    fn stop_entry_uses_trigger_price_as_reference() {
        let mut intent = long_market_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::High,
            offset_pips: 2.0,
        });
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        // trigger = 1.1020 + 2*0.0001 = 1.1022
        match r.entry {
            ResolvedEntry::Stop { trigger_price } => assert!((trigger_price - 1.1022).abs() < 1e-9),
            _ => panic!("expected stop entry"),
        }
        // R is computed from the trigger, not the close.
        // SL = 1.0978; trigger = 1.1022; R = 0.0044; TP = 1.1022 + 2*0.0044 = 1.1110
        assert!((r.take_profit - 1.1110).abs() < 1e-9);
    }

    #[test]
    fn long_limit_below_close_resolves() {
        let mut intent = long_market_intent();
        // Long limit at 1.0985 — below the 1.1000 close.
        intent.entry = Some(EntrySpec::Limit {
            from: PriceAnchor::Low,
            offset_pips: 5.0,
        });
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.entry {
            ResolvedEntry::Limit { trigger_price } => {
                // 1.0980 + 5*0.0001 = 1.0985
                assert!((trigger_price - 1.0985).abs() < 1e-9);
            }
            _ => panic!("expected limit entry"),
        }
    }

    #[test]
    fn long_limit_at_or_above_close_rejected() {
        let mut intent = long_market_intent();
        // Long limit at 1.1010 — above the 1.1000 close; would fill instantly.
        intent.entry = Some(EntrySpec::Limit {
            from: PriceAnchor::High,
            offset_pips: -10.0,
        });
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidGeometry)
        ));
    }

    #[test]
    fn short_limit_above_close_resolves() {
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Short);
        intent.stop_loss = Some(PriceRef {
            from: PriceAnchor::High,
            offset_pips: 10.0,
        });
        // Short limit at 1.1015 — above the 1.1000 close.
        intent.entry = Some(EntrySpec::Limit {
            from: PriceAnchor::High,
            offset_pips: -5.0,
        });
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.entry {
            ResolvedEntry::Limit { trigger_price } => {
                // 1.1020 - 5*0.0001 = 1.1015
                assert!((trigger_price - 1.1015).abs() < 1e-9);
            }
            _ => panic!("expected limit entry"),
        }
    }

    #[test]
    fn long_stop_at_or_below_close_rejected() {
        let mut intent = long_market_intent();
        // Long stop at 1.0990 — below the 1.1000 close.
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Low,
            offset_pips: 10.0,
        });
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidGeometry)
        ));
    }

    #[test]
    fn long_with_sl_above_entry_rejected() {
        let mut intent = long_market_intent();
        intent.stop_loss = Some(PriceRef {
            from: PriceAnchor::High,
            offset_pips: 2.0,
        });
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidGeometry)
        ));
    }

    #[test]
    fn missing_field_rejected() {
        let mut intent = long_market_intent();
        intent.risk_pct = None;
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::MissingField("risk_pct"))
        ));
    }

    #[test]
    fn non_entry_action_rejected() {
        let mut intent = long_market_intent();
        intent.action = Action::Close;
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::NotAnEntry)
        ));
    }
}
