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

/// Hard server-side floor on `min_r`. Overrides below this are rejected
/// at both the encoder and the server. Mirrored by `validate_min_r` in the
/// CLI so typos fail before encryption.
pub const MIN_R_FLOOR: f64 = 1.0;

#[derive(Debug)]
pub enum ResolveError {
    /// Field required for `enter` is missing.
    MissingField(&'static str),
    /// Direction and SL placement disagree (e.g. long with SL above entry).
    InvalidGeometry,
    /// Action is not `enter`.
    NotAnEntry,
    /// `min_r` override is below the hard floor (1.0).
    MinRBelowFloor { requested: f64 },
    /// Implicit R is below `min_r` (default 1.0 if not overridden).
    BelowMinR { actual: f64, min: f64 },
    /// Entry price is outside the SL..TP range — would fill instantly
    /// against the stop or take-profit. Happens when the trigger candle
    /// gaps past one of the levels.
    EntryOutsideRange {
        entry: f64,
        stop_loss: f64,
        take_profit: f64,
    },
}

impl core::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingField(name) => write!(f, "missing field for entry: {name}"),
            Self::InvalidGeometry => f.write_str("stop/entry geometry inconsistent with direction"),
            Self::NotAnEntry => f.write_str("intent is not an entry action"),
            Self::MinRBelowFloor { requested } => write!(
                f,
                "min_r={requested} is below the hard floor of {MIN_R_FLOOR}"
            ),
            Self::BelowMinR { actual, min } => write!(
                f,
                "trade R={actual:.3} is below the required minimum of {min:.3}"
            ),
            Self::EntryOutsideRange {
                entry,
                stop_loss,
                take_profit,
            } => write!(
                f,
                "entry {entry} is outside the SL..TP range ({stop_loss}..{take_profit})"
            ),
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

        let take_profit = resolve_tp(
            tp_spec,
            shell,
            pip_size,
            direction,
            reference_price,
            stop_loss,
        );

        // Entry must sit strictly between SL and TP for the direction:
        //   long  → SL < entry < TP
        //   short → TP < entry < SL
        // A trigger candle that gaps past one of the levels would otherwise
        // fill straight into the stop or take-profit.
        let in_range = match direction {
            Direction::Long => stop_loss < reference_price && reference_price < take_profit,
            Direction::Short => take_profit < reference_price && reference_price < stop_loss,
        };
        if !in_range {
            return Err(ResolveError::EntryOutsideRange {
                entry: reference_price,
                stop_loss,
                take_profit,
            });
        }

        // Server-enforced floor: an `min_r` override cannot weaken the
        // default 1.0R minimum. Defense in depth against a custom encryptor
        // or stale CLI.
        let min_r = intent.min_r.unwrap_or(MIN_R_FLOOR);
        if min_r < MIN_R_FLOOR {
            return Err(ResolveError::MinRBelowFloor { requested: min_r });
        }
        // Implicit R = (TP - entry) / (entry - SL), absolute values for the
        // short case (direction-agnostic — geometry already checked above).
        let r_distance = (reference_price - stop_loss).abs();
        let tp_distance = (take_profit - reference_price).abs();
        let actual_r = tp_distance / r_distance;
        if actual_r < min_r {
            return Err(ResolveError::BelowMinR {
                actual: actual_r,
                min: min_r,
            });
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
            stop_loss: Some(PriceRef::Anchored {
                from: PriceAnchor::Low,
                offset_pips: -2.0,
            }),
            take_profit: Some(TakeProfit::RMultiple {
                from: PriceAnchor::Close,
                offset_r: 2.0,
            }),
            risk_pct: Some(0.5),
            cooldown_hours: None,
            min_r: None,
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
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::High,
            offset_pips: 2.0,
        });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Anchored {
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
        intent.stop_loss = Some(PriceRef::Anchored {
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
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::High,
            offset_pips: 2.0,
        });
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::EntryOutsideRange { .. })
        ));
    }

    #[test]
    fn long_absolute_sl_tp_resolves() {
        // Inverted H&S: SL=1.86236 absolute, TP=1.86899 absolute, market entry.
        // Shell.close = 1.1000 from the fixture — totally unrelated to SL/TP —
        // but we override it so the entry sits inside the range.
        let mut intent = long_market_intent();
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.86236 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.86899,
        }));
        let mut s = shell();
        s.close = 1.86500; // simulates a trigger candle inside the SL..TP range
        let r = Resolved::from_intent(&intent, &s, 0.0001).unwrap();
        assert!((r.stop_loss - 1.86236).abs() < 1e-9);
        assert!((r.take_profit - 1.86899).abs() < 1e-9);
    }

    #[test]
    fn long_absolute_sl_tp_entry_above_tp_rejected() {
        let mut intent = long_market_intent();
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.86236 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.86899,
        }));
        let mut s = shell();
        s.close = 1.87000; // gapped past TP
        match Resolved::from_intent(&intent, &s, 0.0001) {
            Err(ResolveError::EntryOutsideRange { entry, .. }) => {
                assert!((entry - 1.87000).abs() < 1e-9);
            }
            other => panic!("expected EntryOutsideRange, got {other:?}"),
        }
    }

    #[test]
    fn long_absolute_sl_tp_entry_below_sl_rejected() {
        let mut intent = long_market_intent();
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.86236 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.86899,
        }));
        let mut s = shell();
        s.close = 1.86000; // gapped below SL
        assert!(matches!(
            Resolved::from_intent(&intent, &s, 0.0001),
            Err(ResolveError::EntryOutsideRange { .. })
        ));
    }

    #[test]
    fn short_absolute_sl_tp_entry_below_tp_rejected() {
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Short);
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.87100 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.86200,
        }));
        let mut s = shell();
        s.close = 1.86100; // below the TP — invalid for short
        assert!(matches!(
            Resolved::from_intent(&intent, &s, 0.0001),
            Err(ResolveError::EntryOutsideRange { .. })
        ));
    }

    #[test]
    fn price_ref_absolute_round_trips_yaml() {
        let yaml = r#"{ absolute: 1.86236 }"#;
        let pr: PriceRef = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(pr, PriceRef::Absolute { absolute } if (absolute - 1.86236).abs() < 1e-9));
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
    fn default_min_r_passes_when_r_above_one() {
        // R = 2.0 trade (R-multiple TP at 2.0), no min_r override → passes.
        let r = Resolved::from_intent(&long_market_intent(), &shell(), 0.0001).unwrap();
        // entry=1.10, SL=1.0978, TP=1.1044 → R = 22/22 = 2.0
        let actual = (r.take_profit - 1.10) / (1.10 - r.stop_loss);
        assert!((actual - 2.0).abs() < 1e-9);
    }

    #[test]
    fn default_min_r_rejects_when_below_one() {
        // Anchored TP only 1 pip above entry but SL 22 pips below → 0.045R.
        let mut intent = long_market_intent();
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: 1.0,
        }));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::BelowMinR { actual, min }) => {
                assert!((min - 1.0).abs() < 1e-9);
                assert!(actual < 1.0, "actual R was {actual}");
            }
            other => panic!("expected BelowMinR, got {other:?}"),
        }
    }

    #[test]
    fn min_r_override_above_floor_enforced() {
        // R = 2.0 trade but we demand 3.0 → rejected.
        let mut intent = long_market_intent();
        intent.min_r = Some(3.0);
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::BelowMinR { .. })
        ));
    }

    #[test]
    fn min_r_override_above_floor_passes_when_met() {
        let mut intent = long_market_intent();
        intent.min_r = Some(2.0); // exactly meets the actual R
        assert!(Resolved::from_intent(&intent, &shell(), 0.0001).is_ok());
    }

    #[test]
    fn min_r_below_floor_rejected_at_server() {
        // Defense in depth: even if encoder somehow allowed it.
        let mut intent = long_market_intent();
        intent.min_r = Some(0.5);
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::MinRBelowFloor { requested }) => {
                assert!((requested - 0.5).abs() < 1e-9);
            }
            other => panic!("expected MinRBelowFloor, got {other:?}"),
        }
    }

    #[test]
    fn min_r_zero_rejected_at_server() {
        let mut intent = long_market_intent();
        intent.min_r = Some(0.0);
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::MinRBelowFloor { .. })
        ));
    }

    #[test]
    fn min_r_negative_rejected_at_server() {
        let mut intent = long_market_intent();
        intent.min_r = Some(-1.0);
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::MinRBelowFloor { .. })
        ));
    }

    #[test]
    fn short_default_min_r_rejects_when_below_one() {
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Short);
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::High,
            offset_pips: 10.0,
        });
        // Anchored TP 1 pip below entry — way under 1R for a 30-pip SL.
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: -1.0,
        }));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::BelowMinR { .. })
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
