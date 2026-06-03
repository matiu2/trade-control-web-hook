//! Merge an `Intent` with its plaintext `Shell` into a `Resolved` trade ready
//! for risk-gating and OANDA order placement.

use chrono::{DateTime, Utc};

use super::{Action, Direction, EntrySpec, Intent, Shell, TakeProfit};
use crate::rules::{self, RhaiScope, RuleError};
use crate::tunable::Tunable;

#[cfg(test)]
use super::{BrokerKind, PriceAnchor};

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

/// How units are determined for this trade. Resolved from the
/// intent's `risk_pct` / `risk_amount` / `size_units` fields
/// (exactly one of them is required).
///
/// `Percent` and `Amount` feed the sizing math (`budget / stop_distance`).
/// `Units` bypasses it — the worker sends the literal count.
#[derive(Debug, Clone, Copy)]
pub enum RiskBudget {
    /// Risk this percent of account equity.
    Percent(f64),
    /// Risk this fixed amount in account currency. Worker translates
    /// to an effective `risk_pct` at fire time so the cap still applies.
    Amount(f64),
    /// Place this many units directly, bypassing sizing math. Cap
    /// check still runs via the implied money risk
    /// (`units * stop_distance` ÷ equity).
    Units(f64),
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
    pub risk: RiskBudget,
    /// Worker should compute the sizing as normal but skip placing the
    /// order — the inputs / calculations / output get logged instead.
    /// Defaults to false. See `Intent::dry_run`.
    pub dry_run: bool,
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
    /// More than one of `risk_pct` / `risk_amount` / `size_units`
    /// set on the same intent. They're mutually exclusive — pick one.
    BothRiskModesSet,
    /// `risk_amount` set to a non-positive / non-finite value.
    InvalidRiskAmount { value: f64 },
    /// `size_units` set to a non-positive / non-finite value.
    InvalidSizeUnits { value: f64 },
    /// A `Tunable::Script` on the intent failed to evaluate. Carries
    /// the field name and short error kind (`parse` / `eval` /
    /// `wrong-type`) so the worker can map it to a 412 with a
    /// telemetry-friendly outcome string.
    ScriptFailed {
        field: &'static str,
        kind: &'static str,
        message: String,
    },
    /// A `Tunable<f64>` field resolved to a non-positive / non-finite
    /// value (e.g. a `risk_pct` script that returned `0.0`).
    InvalidTunableValue { field: &'static str, value: f64 },
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
            Self::BothRiskModesSet => {
                f.write_str("risk_pct / risk_amount / size_units are mutually exclusive")
            }
            Self::InvalidRiskAmount { value } => {
                write!(f, "risk_amount must be positive and finite, got {value}")
            }
            Self::InvalidSizeUnits { value } => {
                write!(f, "size_units must be positive and finite, got {value}")
            }
            Self::ScriptFailed {
                field,
                kind,
                message,
            } => {
                write!(f, "{field} script ({kind}): {message}")
            }
            Self::InvalidTunableValue { field, value } => {
                write!(f, "{field} must be positive and finite, got {value}")
            }
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
        // Sizing-mode selection. `risk_pct` is always present (default
        // `Static(1.0)`), so it's the fallback. `risk_amount` and
        // `size_units` are mutually exclusive with each other and with
        // each other only — either overrides the risk_pct default. The
        // actual values — all can be a `Tunable<f64>` script — are
        // resolved at the bottom of this function, after the geometry
        // is finalised so scripts can reference `r_multiple` /
        // `tp_distance` / etc.
        enum SizingMode {
            Pct,
            Amount,
            Units,
        }
        let sizing_mode = match (intent.risk_amount.is_some(), intent.size_units.is_some()) {
            (false, false) => SizingMode::Pct,
            (true, false) => SizingMode::Amount,
            (false, true) => SizingMode::Units,
            (true, true) => return Err(ResolveError::BothRiskModesSet),
        };

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

        // Build the partial Resolved that Tunable scripts evaluate
        // against — Phase 2 bindings (`entry_price`, `r_multiple`,
        // etc.) come from this snapshot. We rebuild it from the
        // pieces we just computed rather than reading back from a
        // finished Resolved, so the script sees exactly the values
        // we're about to store. min_r / risk_pct / risk_amount /
        // size_units scripts all evaluate against this snapshot.
        let geometry_snapshot = Self {
            id: intent.id.clone(),
            not_after: intent.not_after,
            instrument: intent.instrument.clone(),
            direction,
            entry: entry.clone(),
            stop_loss,
            take_profit,
            // Placeholder — overwritten below once risk_pct (which
            // may be a script that reads `r_multiple` / `tp_distance`)
            // is resolved. Scripts never see `risk`.
            risk: RiskBudget::Percent(0.0),
            dry_run: intent.dry_run.unwrap_or(false),
        };

        // Server-enforced floor: an `min_r` override cannot weaken the
        // default 1.0R minimum. Defense in depth against a custom encryptor
        // or stale CLI. min_r may be a Tunable<f64> script — resolve it
        // against the geometry snapshot so scripts can reference
        // `r_multiple` / `tp_distance` etc.
        let min_r = match &intent.min_r {
            None => MIN_R_FLOOR,
            Some(tunable) => {
                resolve_f64_tunable("min_r", tunable, shell, &geometry_snapshot, pip_size)?
            }
        };
        if !min_r.is_finite() || min_r < MIN_R_FLOOR {
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

        let risk = match sizing_mode {
            SizingMode::Units => {
                let tunable = intent
                    .size_units
                    .as_ref()
                    .ok_or(ResolveError::MissingField("size_units"))?;
                let units = resolve_f64_tunable(
                    "size_units",
                    tunable,
                    shell,
                    &geometry_snapshot,
                    pip_size,
                )?;
                if !units.is_finite() || units <= 0.0 {
                    return Err(ResolveError::InvalidSizeUnits { value: units });
                }
                RiskBudget::Units(units)
            }
            SizingMode::Pct => {
                let pct = resolve_f64_tunable(
                    "risk_pct",
                    &intent.risk_pct,
                    shell,
                    &geometry_snapshot,
                    pip_size,
                )?;
                if !pct.is_finite() || pct <= 0.0 {
                    return Err(ResolveError::InvalidTunableValue {
                        field: "risk_pct",
                        value: pct,
                    });
                }
                RiskBudget::Percent(pct)
            }
            SizingMode::Amount => {
                let tunable = intent
                    .risk_amount
                    .as_ref()
                    .ok_or(ResolveError::MissingField("risk_amount"))?;
                let amount = resolve_f64_tunable(
                    "risk_amount",
                    tunable,
                    shell,
                    &geometry_snapshot,
                    pip_size,
                )?;
                if !amount.is_finite() || amount <= 0.0 {
                    return Err(ResolveError::InvalidRiskAmount { value: amount });
                }
                RiskBudget::Amount(amount)
            }
        };

        Ok(Self {
            risk,
            ..geometry_snapshot
        })
    }
}

/// Resolve a `Tunable<f64>` field against the standard three-phase
/// scope. Maps [`RuleError`] variants onto [`ResolveError::ScriptFailed`]
/// with a short `kind` label so the worker's outcome string matches the
/// CLI validator's vocabulary (`parse` / `eval` / `wrong-type`).
fn resolve_f64_tunable(
    field: &'static str,
    tunable: &Tunable<f64>,
    shell: &Shell,
    resolved: &Resolved,
    pip_size: f64,
) -> Result<f64, ResolveError> {
    let engine = rules::build_engine();
    let mut scope = RhaiScope::new();
    rules::bind_shell_anchors(&mut scope, shell);
    rules::bind_intent_derived(&mut scope, resolved, pip_size);
    rules::resolve_tunable::<f64>(&engine, &mut scope, tunable).map_err(|err| {
        let kind = match &err {
            RuleError::Parse(_) => "parse",
            RuleError::Eval(_) => "eval",
            RuleError::WrongType { .. } => "wrong-type",
        };
        ResolveError::ScriptFailed {
            field,
            kind,
            message: err.to_string(),
        }
    })
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
            signal_high: None,
            signal_low: None,
            signal_range: None,
            signal_start_time: None,
            signal_kind: None,
            golden: None,
            atr: None,
            signal_confirmed: None,
            recent_high: None,
            recent_low: None,
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
            risk_pct: Tunable::Static(0.5),
            risk_amount: None,
            size_units: None,
            dry_run: None,
            cooldown_hours: None,
            min_r: None,
            broker: BrokerKind::Oanda,
            step: None,
            name: None,
            ttl_hours: Tunable::Static(0),
            level: None,
            requires_preps: Vec::new(),
            vetos: Vec::new(),
            clears: Vec::new(),
            account: None,
            trade_id: None,
            max_retries: crate::tunable::Tunable::Static(0),
            allow_entry: None,
            needs_golden: false,
            blackout_id: None,
            news_id: None,
            require_news_window: None,
            require_price_in_ranges: None,
            needs_confirmed: false,
            inside_window: Vec::new(),
            price_bands: Vec::new(),
            reason: None,
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
    fn risk_amount_overrides_risk_pct_default() {
        // risk_pct is always present (default Static(1.0)). risk_amount,
        // when set, supersedes it. Sanity-pin that the resolver picks
        // Amount rather than failing or doubling up.
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::Static(1.0));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.risk {
            RiskBudget::Amount(a) => assert!((a - 1.0).abs() < 1e-9),
            other => panic!("expected Amount, got {other:?}"),
        }
    }

    #[test]
    fn risk_amount_zero_rejected() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::Static(0.0));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidRiskAmount { .. })
        ));
    }

    #[test]
    fn risk_amount_negative_rejected() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::Static(-1.0));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidRiskAmount { .. })
        ));
    }

    #[test]
    fn risk_amount_nan_rejected() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::Static(f64::NAN));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidRiskAmount { .. })
        ));
    }

    #[test]
    fn size_units_resolves_to_units_budget() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(0.01));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.risk {
            RiskBudget::Units(u) => assert!((u - 0.01).abs() < 1e-9),
            other => panic!("expected Units, got {other:?}"),
        }
    }

    #[test]
    fn size_units_overrides_risk_pct_default() {
        // risk_pct (default Static(1.0)) is silently overridden when
        // size_units is set — only risk_amount and size_units are
        // mutually exclusive post-flatten.
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(0.01));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.risk {
            RiskBudget::Units(u) => assert!((u - 0.01).abs() < 1e-9),
            other => panic!("expected Units, got {other:?}"),
        }
    }

    #[test]
    fn size_units_with_risk_amount_rejected() {
        // The remaining mutual-exclusion edge — risk_amount and
        // size_units cannot both be set since they're alternative
        // sizing modes (not overrides of a default).
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::Static(1.0));
        intent.size_units = Some(Tunable::Static(0.01));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::BothRiskModesSet)
        ));
    }

    #[test]
    fn size_units_zero_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(0.0));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidSizeUnits { .. })
        ));
    }

    #[test]
    fn size_units_negative_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(-0.01));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidSizeUnits { .. })
        ));
    }

    #[test]
    fn size_units_nan_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(f64::NAN));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidSizeUnits { .. })
        ));
    }

    #[test]
    fn dry_run_defaults_false() {
        let r = Resolved::from_intent(&long_market_intent(), &shell(), 0.0001).unwrap();
        assert!(!r.dry_run);
    }

    #[test]
    fn dry_run_propagates_to_resolved() {
        let mut intent = long_market_intent();
        intent.dry_run = Some(true);
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        assert!(r.dry_run);
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
        intent.min_r = Some(Tunable::Static(3.0));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::BelowMinR { .. })
        ));
    }

    #[test]
    fn min_r_override_above_floor_passes_when_met() {
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::Static(2.0)); // exactly meets the actual R
        assert!(Resolved::from_intent(&intent, &shell(), 0.0001).is_ok());
    }

    #[test]
    fn min_r_below_floor_rejected_at_server() {
        // Defense in depth: even if encoder somehow allowed it.
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::Static(0.5));
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
        intent.min_r = Some(Tunable::Static(0.0));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::MinRBelowFloor { .. })
        ));
    }

    #[test]
    fn min_r_negative_rejected_at_server() {
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::Static(-1.0));
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

    // ---- risk_pct as Tunable ----

    #[test]
    fn risk_pct_static_path_unchanged() {
        // Sanity: the existing tests already cover this via
        // long_market_intent(), but pin it explicitly.
        let intent = long_market_intent();
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.risk {
            RiskBudget::Percent(p) => assert!((p - 0.5).abs() < 1e-9),
            other => panic!("expected Percent(0.5), got {other:?}"),
        }
    }

    #[test]
    fn risk_pct_script_evaluates_against_geometry() {
        // R = 2.0 on the fixture; script picks 1.0% if R >= 2 else 0.5%.
        let mut intent = long_market_intent();
        intent.risk_pct = Tunable::from_script("if r_multiple >= 2.0 { 1.0 } else { 0.5 }");
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.risk {
            RiskBudget::Percent(p) => assert!((p - 1.0).abs() < 1e-9),
            other => panic!("expected Percent(1.0), got {other:?}"),
        }
    }

    #[test]
    fn risk_pct_script_can_read_shell_anchors() {
        // golden=Some(true) on a fixture-extended shell — go aggressive
        // (1.0%) on golden, conservative (0.25%) otherwise.
        let mut intent = long_market_intent();
        intent.risk_pct = Tunable::from_script("if golden == true { 1.0 } else { 0.25 }");
        let mut s = shell();
        s.golden = Some(true);
        let r = Resolved::from_intent(&intent, &s, 0.0001).unwrap();
        assert!(matches!(r.risk, RiskBudget::Percent(p) if (p - 1.0).abs() < 1e-9));
    }

    #[test]
    fn risk_pct_script_returning_zero_rejected() {
        let mut intent = long_market_intent();
        intent.risk_pct = Tunable::from_script("0.0");
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::InvalidTunableValue { field, value }) => {
                assert_eq!(field, "risk_pct");
                assert_eq!(value, 0.0);
            }
            other => panic!("expected InvalidTunableValue, got {other:?}"),
        }
    }

    #[test]
    fn risk_pct_script_returning_negative_rejected() {
        let mut intent = long_market_intent();
        intent.risk_pct = Tunable::from_script("-0.5");
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidTunableValue {
                field: "risk_pct",
                ..
            })
        ));
    }

    #[test]
    fn risk_pct_script_parse_error_surfaces() {
        let mut intent = long_market_intent();
        intent.risk_pct = Tunable::from_script("if if if");
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::ScriptFailed { field, kind, .. }) => {
                assert_eq!(field, "risk_pct");
                assert_eq!(kind, "parse");
            }
            other => panic!("expected ScriptFailed(parse), got {other:?}"),
        }
    }

    #[test]
    fn risk_pct_script_wrong_return_type_surfaces() {
        // Script returns bool, not f64.
        let mut intent = long_market_intent();
        intent.risk_pct = Tunable::from_script("true");
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::ScriptFailed { field, kind, .. }) => {
                assert_eq!(field, "risk_pct");
                assert_eq!(kind, "wrong-type");
            }
            other => panic!("expected ScriptFailed(wrong-type), got {other:?}"),
        }
    }

    #[test]
    fn risk_pct_yaml_omitted_defaults_to_one_percent() {
        // Wire form for an operator who omits risk_pct entirely. Post-
        // flatten, this lands as the default Static(1.0) — the
        // operator's standard setting — instead of being a deserialise
        // error. Same back-compat shape as max_retries / ttl_hours.
        let yaml = r#"
v: 1
id: msg-1
not_after: "2026-06-01T00:00:00Z"
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss: { from: low, offset_pips: -2.0 }
take_profit: { from: close, offset_r: 2.0 }
"#;
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(intent.risk_pct, Tunable::Static(p) if (p - 1.0).abs() < 1e-9));
        // And the serialised form drops the field — byte-identical wire
        // to pre-flatten intents that omitted it.
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(!back.contains("risk_pct"));
    }

    #[test]
    fn risk_pct_yaml_static_parses() {
        // Wire-form regression — `risk_pct: 0.5` (no tag) must still
        // deserialise as Static. This is the byte-identical-back-compat
        // claim that the whole Tunable design rests on.
        let yaml = r#"
v: 1
id: msg-1
not_after: "2026-06-01T00:00:00Z"
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss: { from: low, offset_pips: -2.0 }
take_profit: { from: close, offset_r: 2.0 }
risk_pct: 0.5
"#;
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(intent.risk_pct, Tunable::Static(p) if (p - 0.5).abs() < 1e-9));
    }

    #[test]
    fn risk_pct_yaml_script_parses() {
        let yaml = r#"
v: 1
id: msg-1
not_after: "2026-06-01T00:00:00Z"
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss: { from: low, offset_pips: -2.0 }
take_profit: { from: close, offset_r: 2.0 }
risk_pct: !script "if r_multiple >= 2.0 { 1.0 } else { 0.5 }"
"#;
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(intent.risk_pct, Tunable::Script(_)));
    }

    // ---- risk_amount as Tunable ----

    #[test]
    fn risk_amount_static_path_unchanged() {
        // Mirrors the risk_pct sanity pin — Static(amount) must resolve
        // to RiskBudget::Amount(amount) with the same arithmetic as before
        // the Tunable promotion.
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::Static(2.5));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.risk {
            RiskBudget::Amount(a) => assert!((a - 2.5).abs() < 1e-9),
            other => panic!("expected Amount(2.5), got {other:?}"),
        }
    }

    #[test]
    fn risk_amount_script_evaluates_against_geometry() {
        // R = 2.0 on the fixture; bet $2 if R >= 2 else $1. Proves
        // risk_amount scripts see Phase 2 bindings, same as risk_pct.
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::from_script(
            "if r_multiple >= 2.0 { 2.0 } else { 1.0 }",
        ));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.risk {
            RiskBudget::Amount(a) => assert!((a - 2.0).abs() < 1e-9),
            other => panic!("expected Amount(2.0), got {other:?}"),
        }
    }

    #[test]
    fn risk_amount_script_can_read_shell_anchors() {
        // Pump amount up to $5 when golden, otherwise $1.
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::from_script(
            "if golden == true { 5.0 } else { 1.0 }",
        ));
        let mut s = shell();
        s.golden = Some(true);
        let r = Resolved::from_intent(&intent, &s, 0.0001).unwrap();
        assert!(matches!(r.risk, RiskBudget::Amount(a) if (a - 5.0).abs() < 1e-9));
    }

    #[test]
    fn risk_amount_script_returning_zero_rejected() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::from_script("0.0"));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::InvalidRiskAmount { value }) => assert_eq!(value, 0.0),
            other => panic!("expected InvalidRiskAmount, got {other:?}"),
        }
    }

    #[test]
    fn risk_amount_script_returning_negative_rejected() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::from_script("-1.0"));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidRiskAmount { .. })
        ));
    }

    #[test]
    fn risk_amount_script_parse_error_surfaces() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::from_script("if if if"));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::ScriptFailed { field, kind, .. }) => {
                assert_eq!(field, "risk_amount");
                assert_eq!(kind, "parse");
            }
            other => panic!("expected ScriptFailed(parse), got {other:?}"),
        }
    }

    #[test]
    fn risk_amount_script_wrong_return_type_surfaces() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::from_script("true"));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::ScriptFailed { field, kind, .. }) => {
                assert_eq!(field, "risk_amount");
                assert_eq!(kind, "wrong-type");
            }
            other => panic!("expected ScriptFailed(wrong-type), got {other:?}"),
        }
    }

    #[test]
    fn risk_amount_yaml_static_parses() {
        // Wire-form regression — `risk_amount: 1.0` (no tag) must still
        // deserialise as Static. Matches the risk_pct back-compat claim.
        let yaml = r#"
v: 1
id: msg-1
not_after: "2026-06-01T00:00:00Z"
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss: { from: low, offset_pips: -2.0 }
take_profit: { from: close, offset_r: 2.0 }
risk_amount: 1.0
"#;
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(intent.risk_amount, Some(Tunable::Static(a)) if (a - 1.0).abs() < 1e-9));
    }

    #[test]
    fn risk_amount_yaml_script_parses() {
        let yaml = r#"
v: 1
id: msg-1
not_after: "2026-06-01T00:00:00Z"
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss: { from: low, offset_pips: -2.0 }
take_profit: { from: close, offset_r: 2.0 }
risk_amount: !script "if r_multiple >= 2.0 { 2.0 } else { 1.0 }"
"#;
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(intent.risk_amount, Some(Tunable::Script(_))));
    }

    // ---- size_units as Tunable ----

    #[test]
    fn size_units_static_path_unchanged() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(0.05));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.risk {
            RiskBudget::Units(u) => assert!((u - 0.05).abs() < 1e-9),
            other => panic!("expected Units(0.05), got {other:?}"),
        }
    }

    #[test]
    fn size_units_script_evaluates_against_geometry() {
        // R = 2.0 on the fixture; scale size with R.
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::from_script(
            "if r_multiple >= 2.0 { 0.02 } else { 0.01 }",
        ));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001).unwrap();
        match r.risk {
            RiskBudget::Units(u) => assert!((u - 0.02).abs() < 1e-9),
            other => panic!("expected Units(0.02), got {other:?}"),
        }
    }

    #[test]
    fn size_units_script_can_read_shell_anchors() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::from_script(
            "if golden == true { 0.05 } else { 0.01 }",
        ));
        let mut s = shell();
        s.golden = Some(true);
        let r = Resolved::from_intent(&intent, &s, 0.0001).unwrap();
        assert!(matches!(r.risk, RiskBudget::Units(u) if (u - 0.05).abs() < 1e-9));
    }

    #[test]
    fn size_units_script_returning_zero_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::from_script("0.0"));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::InvalidSizeUnits { value }) => assert_eq!(value, 0.0),
            other => panic!("expected InvalidSizeUnits, got {other:?}"),
        }
    }

    #[test]
    fn size_units_script_returning_negative_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::from_script("-0.01"));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::InvalidSizeUnits { .. })
        ));
    }

    #[test]
    fn size_units_script_parse_error_surfaces() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::from_script("if if if"));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::ScriptFailed { field, kind, .. }) => {
                assert_eq!(field, "size_units");
                assert_eq!(kind, "parse");
            }
            other => panic!("expected ScriptFailed(parse), got {other:?}"),
        }
    }

    #[test]
    fn size_units_script_wrong_return_type_surfaces() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::from_script("true"));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::ScriptFailed { field, kind, .. }) => {
                assert_eq!(field, "size_units");
                assert_eq!(kind, "wrong-type");
            }
            other => panic!("expected ScriptFailed(wrong-type), got {other:?}"),
        }
    }

    #[test]
    fn size_units_yaml_static_parses() {
        let yaml = r#"
v: 1
id: msg-1
not_after: "2026-06-01T00:00:00Z"
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss: { from: low, offset_pips: -2.0 }
take_profit: { from: close, offset_r: 2.0 }
size_units: 0.01
"#;
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(intent.size_units, Some(Tunable::Static(u)) if (u - 0.01).abs() < 1e-9));
    }

    #[test]
    fn size_units_yaml_script_parses() {
        let yaml = r#"
v: 1
id: msg-1
not_after: "2026-06-01T00:00:00Z"
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss: { from: low, offset_pips: -2.0 }
take_profit: { from: close, offset_r: 2.0 }
size_units: !script "if r_multiple >= 2.0 { 0.02 } else { 0.01 }"
"#;
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(intent.size_units, Some(Tunable::Script(_))));
    }

    // ---- min_r as Tunable ----

    #[test]
    fn min_r_static_path_unchanged() {
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::Static(1.5));
        // Fixture R = 2.0, override demands 1.5 → passes.
        assert!(Resolved::from_intent(&intent, &shell(), 0.0001).is_ok());
    }

    #[test]
    fn min_r_script_evaluates_against_geometry() {
        // Script demands 1.5R when long, otherwise 1.0R.
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::from_script(
            "if direction == \"long\" { 1.5 } else { 1.0 }",
        ));
        assert!(Resolved::from_intent(&intent, &shell(), 0.0001).is_ok());
    }

    #[test]
    fn min_r_script_below_floor_rejected() {
        // Script returns 0.5, below floor → MinRBelowFloor.
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::from_script("0.5"));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::MinRBelowFloor { requested }) => {
                assert!((requested - 0.5).abs() < 1e-9);
            }
            other => panic!("expected MinRBelowFloor, got {other:?}"),
        }
    }

    #[test]
    fn min_r_script_returning_nan_rejected() {
        // NaN is non-finite — must be rejected as MinRBelowFloor.
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::from_script("0.0 / 0.0"));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001),
            Err(ResolveError::MinRBelowFloor { .. })
        ));
    }

    #[test]
    fn min_r_script_parse_error_surfaces() {
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::from_script("if if if"));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::ScriptFailed { field, kind, .. }) => {
                assert_eq!(field, "min_r");
                assert_eq!(kind, "parse");
            }
            other => panic!("expected ScriptFailed(parse), got {other:?}"),
        }
    }

    #[test]
    fn min_r_script_wrong_return_type_surfaces() {
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::from_script("true"));
        match Resolved::from_intent(&intent, &shell(), 0.0001) {
            Err(ResolveError::ScriptFailed { field, kind, .. }) => {
                assert_eq!(field, "min_r");
                assert_eq!(kind, "wrong-type");
            }
            other => panic!("expected ScriptFailed(wrong-type), got {other:?}"),
        }
    }

    #[test]
    fn min_r_yaml_static_parses() {
        let yaml = r#"
v: 1
id: msg-1
not_after: "2026-06-01T00:00:00Z"
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss: { from: low, offset_pips: -2.0 }
take_profit: { from: close, offset_r: 2.0 }
risk_pct: 0.5
min_r: 1.5
"#;
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(intent.min_r, Some(Tunable::Static(v)) if (v - 1.5).abs() < 1e-9));
    }

    #[test]
    fn min_r_yaml_script_parses() {
        let yaml = r#"
v: 1
id: msg-1
not_after: "2026-06-01T00:00:00Z"
action: enter
instrument: EUR_USD
direction: long
entry: { type: market }
stop_loss: { from: low, offset_pips: -2.0 }
take_profit: { from: close, offset_r: 2.0 }
risk_pct: 0.5
min_r: !script "if direction == \"long\" { 1.5 } else { 1.0 }"
"#;
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(intent.min_r, Some(Tunable::Script(_))));
    }
}
