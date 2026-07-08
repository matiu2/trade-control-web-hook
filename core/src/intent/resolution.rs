//! Merge an `Intent` with its plaintext `Shell` into a `Resolved` trade ready
//! for risk-gating and OANDA order placement.

use chrono::{DateTime, Utc};

use super::{Action, Direction, EntrySpec, Intent, RecoverEntryAction, Shell, TakeProfit};
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

impl ResolvedEntry {
    /// The price the entry-level gates / risk math key off: the trigger for a
    /// Stop / Limit pending order, or the reference (close) for a Market fill.
    pub fn reference_price(&self) -> f64 {
        match self {
            ResolvedEntry::Market { reference_price } => *reference_price,
            ResolvedEntry::Stop { trigger_price } | ResolvedEntry::Limit { trigger_price } => {
                *trigger_price
            }
        }
    }
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

/// Resolved form of [`super::RecoverEntry`] — the stop-entry recovery
/// carried alongside the [`Resolved`] trade so the worker's `run_enter`
/// can recover from a `#19-10` rejection without re-reading the intent
/// or pip size. Threaded *next to* [`ResolvedEntry`] rather than inside
/// `ResolvedEntry::Stop` so the ~10 existing match sites (and the
/// upstream broker adapters, which don't carry it) stay untouched.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedRecoverEntry {
    /// The recovery action the operator opted into.
    pub action: RecoverEntryAction,
    /// Guard rail for `market` recovery in **price units** (already
    /// `max_slippage_pips × pip_size`). The worker re-places as a market
    /// order only if the current price is within this distance of the
    /// original stop trigger. `None` for `skip` / `limit`, and `None`
    /// for a malformed `market` (validation rejects that upstream, but
    /// the worker treats a `None` bound on `market` as "skip" defensively).
    pub max_slippage_price: Option<f64>,
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
    /// The effective R-floor this trade was held to at resolve time — the
    /// intent's `min_r` override, or [`MIN_R_FLOOR`] (1.0) when omitted. The
    /// resolve already enforced `implicit_R >= min_r`; carrying it forward lets
    /// the worker's SL-widen path re-check the floor against the *widened*
    /// geometry without re-resolving the (possibly scripted) `min_r` tunable.
    /// See [`super::widen_sl_to_spread_floor`].
    pub min_r: f64,
    /// Worker should compute the sizing as normal but skip placing the
    /// order — the inputs / calculations / output get logged instead.
    /// Defaults to false. See `Intent::dry_run`.
    pub dry_run: bool,
    /// Stop-entry recovery, resolved from `EntrySpec::Stop::recover_entry`.
    /// `None` for market / limit entries and for stop entries that didn't
    /// opt in (today's behaviour: a wrong-side stop is dropped at resolve
    /// time, or a `#19-10` rejection fails the placement and the next bar
    /// retries). See [`ResolvedRecoverEntry`].
    pub recover_entry: Option<ResolvedRecoverEntry>,
    /// Break-even stop management, copied verbatim from
    /// [`Intent::breakeven`](super::Intent::breakeven). `None` = no BE move
    /// (today's static-SL behaviour). The replay (`simulate_fill`) and the live
    /// worker's position cron read this to decide when to move the stop to the
    /// entry price — see [`super::Breakeven`]. Carried on the resolved trade so
    /// both consumers share one source of truth and can't drift.
    pub breakeven: Option<super::Breakeven>,
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
    /// An M/W `enter` bar that hasn't completed its real-time arming
    /// sequence yet — the right tower isn't confirmed, price hasn't
    /// crossed the middle-of-the-M, or the breakout stop would sit on the
    /// wrong side of the close. This is the **expected, recurring** outcome
    /// on most M/W fires ("decline this bar, stay armed for the next"), not
    /// a malformed request. The worker maps it to a benign 2xx decline
    /// rather than the 400 it gives genuine `InvalidGeometry`. See
    /// [`Resolved::from_mw_intent`].
    NotArmedYet,
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
    /// An anchored offset (entry / SL / TP) couldn't be turned into a price —
    /// e.g. `offset_atr_pct` set with the shell carrying no ATR (warmup), both
    /// offset forms set, a `Close` anchor, or a negative percent. See
    /// [`OffsetError`](super::OffsetError). The engine's `pine_entry_dispatchable`
    /// treats this (like every `from_intent` `Err`) as decline-this-bar-stay-armed,
    /// so a warmup `AtrUnavailable` simply retries on the next tick.
    Offset(super::OffsetError),
}

impl From<super::OffsetError> for ResolveError {
    fn from(e: super::OffsetError) -> Self {
        ResolveError::Offset(e)
    }
}

impl core::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingField(name) => write!(f, "missing field for entry: {name}"),
            Self::InvalidGeometry => f.write_str("stop/entry geometry inconsistent with direction"),
            Self::NotArmedYet => f.write_str("M/W setup has not completed its arming sequence yet"),
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
            Self::Offset(e) => write!(f, "offset resolution failed: {e}"),
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
        tick_size: f64,
    ) -> Result<Self, ResolveError> {
        if intent.action != Action::Enter {
            return Err(ResolveError::NotAnEntry);
        }
        // M/W setups derive entry/SL/TP from baked path params + the live
        // shell OHLC instead of `entry` / `stop_loss` / `take_profit`
        // (which are absent). Dedicated branch — see `mw_resolution`.
        if let Some(mw) = &intent.mw {
            return Self::from_mw_intent(intent, shell, mw, tick_size);
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

        let stop_loss = sl_ref.resolve(shell, pip_size)?;

        // Only stop entries carry a recovery; market / limit leave it
        // `None`. Resolved here (pips → price units) so the worker never
        // needs pip_size again.
        let mut recover_entry: Option<ResolvedRecoverEntry> = None;

        let (entry, reference_price) = match entry_spec {
            EntrySpec::Market => (
                ResolvedEntry::Market {
                    reference_price: shell.close,
                },
                shell.close,
            ),
            EntrySpec::Stop {
                from,
                offset_pips,
                offset_atr_pct,
                at,
                recover_entry: rec,
            } => {
                // `at` (absolute, set at encode time) wins over the
                // shell-anchored geometry, exactly like `PriceRef::Absolute`.
                let trigger = match at {
                    Some(absolute) => *absolute,
                    None => {
                        shell.anchor_price(*from)
                            + super::resolve_offset(
                                *from,
                                *offset_pips,
                                *offset_atr_pct,
                                shell,
                                pip_size,
                            )?
                    }
                };
                // A stop must sit on the *far* side of current price for its
                // direction: long stops above close, short stops below. When
                // the trigger has already been overtaken (long `trigger <=
                // close`, short `trigger >= close`) the stop is "wrong-side"
                // — this happens when the breakout ran during the
                // signal-confirmation wait. Recover instead of dropping when
                // the intent opted in (see `recover_entry`); otherwise return
                // `InvalidGeometry` exactly as before (zero blast radius for
                // un-opted stops). Skipped entirely for an absolute `at`: an
                // operator-drawn level carries no live price to compare
                // against, so the broker arbitrates wrong-side.
                let wrong_side = at.is_none()
                    && match direction {
                        Direction::Long => trigger <= shell.close,
                        Direction::Short => trigger >= shell.close,
                    };
                if wrong_side {
                    // Recover only when the operator opted into market/limit.
                    // `None` / `Skip` preserve today's drop. The recovered
                    // entry still flows through `finish_with_sizing`, which
                    // re-runs the ≥1R floor and the in-range check off the new
                    // `reference_price` — so a recovery that's too far toward
                    // TP (low R) or already past a level is still refused.
                    match rec.map(|o| o.action) {
                        Some(super::RecoverEntryAction::Market) => {
                            // Enter the confirmed breakout at market. The
                            // slippage bound is the explicit pips when set,
                            // else the derived SL→entry distance (handled by
                            // `resolve_recover_entry`).
                            recover_entry =
                                rec.map(|o| resolve_recover_entry(o, pip_size, stop_loss, trigger));
                            (
                                ResolvedEntry::Market {
                                    reference_price: shell.close,
                                },
                                shell.close,
                            )
                        }
                        Some(super::RecoverEntryAction::Limit) => {
                            // Rest a limit at the original trigger — waits for
                            // the pullback, preserving the planned R exactly.
                            // Re-check it would rest on the correct side vs the
                            // current close (long limit at/below, short
                            // at/above); a degenerate non-overrun case would be
                            // a wrong-side limit, so drop it.
                            let limit_ok = match direction {
                                Direction::Long => trigger <= shell.close,
                                Direction::Short => trigger >= shell.close,
                            };
                            if !limit_ok {
                                return Err(ResolveError::InvalidGeometry);
                            }
                            recover_entry = Some(ResolvedRecoverEntry {
                                action: super::RecoverEntryAction::Limit,
                                max_slippage_price: None,
                            });
                            (
                                ResolvedEntry::Limit {
                                    trigger_price: trigger,
                                },
                                trigger,
                            )
                        }
                        // No opt-in or explicit Skip → today's drop.
                        _ => return Err(ResolveError::InvalidGeometry),
                    }
                } else {
                    // Correct-side stop: resolve the recovery for the worker's
                    // broker `#19-10` path (it fires only if the broker still
                    // rejects the resting stop), with derived-slippage support.
                    recover_entry =
                        rec.map(|o| resolve_recover_entry(o, pip_size, stop_loss, trigger));
                    (
                        ResolvedEntry::Stop {
                            trigger_price: trigger,
                        },
                        trigger,
                    )
                }
            }
            EntrySpec::Limit {
                from,
                offset_pips,
                offset_atr_pct,
                at,
            } => {
                let trigger = match at {
                    Some(absolute) => *absolute,
                    None => {
                        shell.anchor_price(*from)
                            + super::resolve_offset(
                                *from,
                                *offset_pips,
                                *offset_atr_pct,
                                shell,
                                pip_size,
                            )?
                    }
                };
                // Limit sits on the *near* side of current price for the direction:
                // long limits below close, short limits above. If it's the wrong
                // side, OANDA would fill instantly (turning the limit into a
                // market order at a worse price) — reject as a typo. Skipped for
                // an absolute `at` (see the Stop arm: operator-drawn level, shell
                // close == trigger, broker arbitrates wrong-side).
                if at.is_none() {
                    match direction {
                        Direction::Long if trigger >= shell.close => {
                            return Err(ResolveError::InvalidGeometry);
                        }
                        Direction::Short if trigger <= shell.close => {
                            return Err(ResolveError::InvalidGeometry);
                        }
                        _ => {}
                    }
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
        )?;

        Self::finish_with_sizing(
            intent,
            shell,
            pip_size,
            tick_size,
            direction,
            entry,
            reference_price,
            stop_loss,
            take_profit,
            recover_entry,
        )
    }

    /// Shared tail for both the standard and the M/W resolution paths:
    /// range-check the entry, build the geometry snapshot scripts
    /// evaluate against, enforce `min_r`, and resolve the sizing mode.
    /// `reference_price` is the price the risk math keys off (the
    /// trigger for Stop/Limit, the close for Market). `entry` is the
    /// already-built [`ResolvedEntry`].
    #[allow(clippy::too_many_arguments)]
    pub(in crate::intent) fn finish_with_sizing(
        intent: &Intent,
        shell: &Shell,
        pip_size: f64,
        tick_size: f64,
        direction: Direction,
        entry: ResolvedEntry,
        reference_price: f64,
        stop_loss: f64,
        take_profit: f64,
        recover_entry: Option<ResolvedRecoverEntry>,
    ) -> Result<Self, ResolveError> {
        // Snap every order price onto the instrument's tick grid BEFORE the
        // in-range and R-floor checks below, so those invariants are verified
        // against the prices we'll actually send to the broker (an unrounded
        // price is rejected by OANDA as `PRICE_PRECISION_EXCEEDED`). Directional
        // rounding keeps the snap from silently changing risk: SL away from
        // entry (never tighter), TP toward entry (never inflates R), entry to
        // nearest. `tick_size <= 0` is identity — see `crate::rounding`.
        //
        // Both worker and replay resolve through here, so rounding here (not at
        // EntryRequest-build) keeps them in parity.
        let reference_price = crate::rounding::round_price(reference_price, tick_size);
        let entry = round_resolved_entry(entry, tick_size);
        let stop_loss = crate::rounding::round_stop_loss(stop_loss, reference_price, tick_size);
        let take_profit =
            crate::rounding::round_take_profit(take_profit, reference_price, tick_size);
        // Sizing-mode selection. `risk_pct` is always present (default
        // `Static(1.0)`), so it's the fallback. `risk_amount` and
        // `size_units` are mutually exclusive with each other and with
        // each other only — either overrides the risk_pct default.
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
            // Placeholder — the real effective floor is resolved just below
            // (min_r may be a script that reads this snapshot) and written
            // onto the returned `Resolved`.
            min_r: MIN_R_FLOOR,
            dry_run: intent.dry_run.unwrap_or(false),
            recover_entry,
            breakeven: intent.breakeven,
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
            min_r,
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

/// Resolve the wire [`super::RecoverEntry`] into the price-unit
/// [`ResolvedRecoverEntry`]. The slippage guard is converted from pips to
/// price units here so the worker compares against the trigger directly.
/// A `market` action with no explicit `max_slippage_pips` falls back to
/// the **derived** SL→entry distance (`|stop_loss − trigger|`, already in
/// price units) so the broker `#19-10` recovery path is bounded without
/// the operator having to supply a number. `limit` / `skip` carry no
/// bound.
/// Snap the price carried inside a [`ResolvedEntry`] onto the tick grid.
/// The trigger (or market reference) is a level, so it rounds to nearest —
/// the directional SL/TP rounding lives in `finish_with_sizing`. `tick_size
/// <= 0` is identity (see [`crate::rounding`]).
fn round_resolved_entry(entry: ResolvedEntry, tick_size: f64) -> ResolvedEntry {
    match entry {
        ResolvedEntry::Market { reference_price } => ResolvedEntry::Market {
            reference_price: crate::rounding::round_price(reference_price, tick_size),
        },
        ResolvedEntry::Stop { trigger_price } => ResolvedEntry::Stop {
            trigger_price: crate::rounding::round_price(trigger_price, tick_size),
        },
        ResolvedEntry::Limit { trigger_price } => ResolvedEntry::Limit {
            trigger_price: crate::rounding::round_price(trigger_price, tick_size),
        },
    }
}

fn resolve_recover_entry(
    rec: super::RecoverEntry,
    pip_size: f64,
    stop_loss: f64,
    trigger: f64,
) -> ResolvedRecoverEntry {
    let max_slippage_price = match (rec.action, rec.max_slippage_pips) {
        (_, Some(pips)) => Some(pips.abs() * pip_size),
        (super::RecoverEntryAction::Market, None) => Some((stop_loss - trigger).abs()),
        _ => None,
    };
    ResolvedRecoverEntry {
        action: rec.action,
        max_slippage_price,
    }
}

fn resolve_tp(
    spec: &TakeProfit,
    shell: &Shell,
    pip_size: f64,
    direction: Direction,
    entry: f64,
    stop_loss: f64,
) -> Result<f64, ResolveError> {
    match spec {
        TakeProfit::Anchored(price_ref) => Ok(price_ref.resolve(shell, pip_size)?),
        TakeProfit::RMultiple { from: _, offset_r } => {
            // R is the stop-loss distance in price units, always positive.
            let r = (entry - stop_loss).abs();
            Ok(match direction {
                Direction::Long => entry + offset_r * r,
                Direction::Short => entry - offset_r * r,
            })
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
            open: None,
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
            next_candle_timestamp_1: None,
            next_candle_timestamp_2: None,
            next_candle_timestamp_3: None,
            next_candle_timestamp_4: None,
            next_candle_timestamp_5: None,
        }
    }

    fn long_market_intent() -> Intent {
        Intent {
            entry_level_vetos: Vec::new(),
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
                offset_atr_pct: None,
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
            expiry_bars: None,
            allow_entry: None,
            allow_close: None,
            needs_golden: false,
            blackout_id: None,
            news_id: None,
            require_news_window: None,
            require_price_in_ranges: None,
            needs_confirmed: false,
            inside_window: Vec::new(),
            sr_bands: Vec::new(),
            veto_on_reversal: false,
            reason: None,
            mw: None,
            pip_size: None,
            tick_size: None,
            spread_window: None,
            trade_plan: None,
            blackout_close: crate::intent::BlackoutCloseAction::default(),
            breakeven: None,
            include_archived: false,
        }
    }

    #[test]
    fn long_market_resolves_r_multiple_tp() {
        let r = Resolved::from_intent(&long_market_intent(), &shell(), 0.0001, 0.0).unwrap();
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
            offset_atr_pct: None,
        });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Anchored {
            from: PriceAnchor::Low,
            offset_pips: -10.0,
            offset_atr_pct: None,
        }));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
        // trigger = 1.1020 + 2*0.0001 = 1.1022
        match r.entry {
            ResolvedEntry::Stop { trigger_price } => assert!((trigger_price - 1.1022).abs() < 1e-9),
            _ => panic!("expected stop entry"),
        }
        // R is computed from the trigger, not the close.
        // SL = 1.0978; trigger = 1.1022; R = 0.0044; TP = 1.1022 + 2*0.0044 = 1.1110
        assert!((r.take_profit - 1.1110).abs() < 1e-9);
    }

    /// A stop-entry with an absolute `at` ignores the shell anchor and uses
    /// the baked trigger verbatim — the position-tool direct-entry path. The
    /// signed shell carries the *same* level on `close` (the position tool
    /// only knows its drawn entry), so the wrong-side guard must be skipped:
    /// `trigger == close` would otherwise be `InvalidGeometry` for a long stop.
    #[test]
    fn stop_entry_absolute_at_overrides_anchor_and_skips_wrongside_guard() {
        let mut intent = long_market_intent();
        intent.entry = Some(EntrySpec::Stop {
            // `from`/`offset_pips` are inert when `at` is set.
            from: PriceAnchor::High,
            offset_pips: 2.0,
            offset_atr_pct: None,
            at: Some(1.1000),
            recover_entry: None,
        });
        // Absolute SL/TP so the geometry is self-contained. Direct-entry
        // shell stamps close = trigger = 1.1000.
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.0900 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.1200,
        }));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
        match r.entry {
            ResolvedEntry::Stop { trigger_price } => {
                assert!((trigger_price - 1.1000).abs() < 1e-9, "{trigger_price}")
            }
            _ => panic!("expected stop entry"),
        }
    }

    /// A limit-entry with an absolute `at` likewise uses the baked trigger
    /// and skips the wrong-side guard (trigger == close).
    #[test]
    fn limit_entry_absolute_at_overrides_anchor_and_skips_wrongside_guard() {
        let mut intent = long_market_intent();
        intent.entry = Some(EntrySpec::Limit {
            from: PriceAnchor::Low,
            offset_pips: 5.0,
            offset_atr_pct: None,
            at: Some(1.1000),
        });
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 1.0900 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.1100,
        }));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
        match r.entry {
            ResolvedEntry::Limit { trigger_price } => {
                assert!((trigger_price - 1.1000).abs() < 1e-9, "{trigger_price}")
            }
            _ => panic!("expected limit entry"),
        }
    }

    /// Regression for bug #10 finding A (`hs-adidas-b70c1d31`): an H&S short
    /// `enter` whose entry/SL anchor to `signal_low`/`signal_high` must resolve
    /// to *identical* geometry on the break-candle fire and the later confirmed
    /// re-fire — because the pattern levels (`signal_high`/`signal_low`) are the
    /// same on both, even though each candle's own `high`/`low` wick differs.
    ///
    /// The incident: with the old `Low`/`High` anchors the confirmed (narrower)
    /// candle handed a tighter, drifted stop (entry 173.30 / SL 174.30) instead
    /// of the designed pattern stop. Anchoring to the latched signal extremes
    /// removes that drift.
    #[test]
    fn hs_short_signal_anchored_enter_resolves_identically_across_refires() {
        use crate::intent::ResolvedEntry;

        // The latched H&S pattern levels — identical on both fires.
        let signal_high = 175.61;
        let signal_low = 173.99;
        let pip = 0.01; // ADS.DE quotes in 0.01 increments.

        // Short H&S enter: stop-entry below the break at signal_low + 1 pip,
        // SL above the pattern at signal_high + 1 pip, absolute designed TP.
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Short);
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::SignalLow,
            offset_pips: 1.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::SignalHigh,
            offset_pips: 1.0,
            offset_atr_pct: None,
        });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 171.07402,
        }));

        // Two shells that share the signal levels but carry different candle
        // wicks — the break candle (wide) vs the confirmed candle (narrow).
        let mut break_candle = shell();
        break_candle.close = 174.50;
        break_candle.high = 175.61;
        break_candle.low = 173.99;
        break_candle.signal_high = Some(signal_high);
        break_candle.signal_low = Some(signal_low);

        let mut confirmed_candle = shell();
        confirmed_candle.close = 174.50;
        confirmed_candle.high = 174.29; // the narrow re-fire wick from the incident
        confirmed_candle.low = 173.29;
        confirmed_candle.signal_high = Some(signal_high);
        confirmed_candle.signal_low = Some(signal_low);

        let r1 = Resolved::from_intent(&intent, &break_candle, pip, 0.0).unwrap();
        let r2 = Resolved::from_intent(&intent, &confirmed_candle, pip, 0.0).unwrap();

        // Stop-loss is the pattern high + 1 pip on BOTH fires — no drift.
        assert!((r1.stop_loss - (signal_high + pip)).abs() < 1e-9);
        assert!((r1.stop_loss - r2.stop_loss).abs() < 1e-9);

        // Entry trigger is the pattern low + 1 pip on BOTH fires — no drift.
        let trigger = |r: &Resolved| match r.entry {
            ResolvedEntry::Stop { trigger_price } => trigger_price,
            _ => panic!("expected stop entry"),
        };
        assert!((trigger(&r1) - (signal_low + pip)).abs() < 1e-9);
        assert!((trigger(&r1) - trigger(&r2)).abs() < 1e-9);
    }

    #[test]
    fn stop_entry_carries_resolved_recover_entry_market() {
        use crate::intent::{RecoverEntry, RecoverEntryAction};
        let mut intent = long_market_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::High,
            offset_pips: 2.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: Some(RecoverEntry {
                action: RecoverEntryAction::Market,
                max_slippage_pips: Some(8.0),
            }),
        });
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
        let rec = r.recover_entry.expect("recovery carried onto Resolved");
        assert_eq!(rec.action, RecoverEntryAction::Market);
        // 8 pips * 0.0001 pip_size = 0.0008 in price units.
        assert!((rec.max_slippage_price.unwrap() - 0.0008).abs() < 1e-12);
    }

    #[test]
    fn market_entry_has_no_recover_entry() {
        // Recovery is a stop-entry concept; a market entry never carries
        // it even if some future caller sets it.
        let r = Resolved::from_intent(&long_market_intent(), &shell(), 0.0001, 0.0).unwrap();
        assert!(r.recover_entry.is_none());
    }

    #[test]
    fn stop_entry_skip_resolves_with_no_slippage_bound() {
        use crate::intent::{RecoverEntry, RecoverEntryAction};
        let mut intent = long_market_intent();
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::High,
            offset_pips: 2.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: Some(RecoverEntry {
                action: RecoverEntryAction::Skip,
                max_slippage_pips: None,
            }),
        });
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
        let rec = r.recover_entry.unwrap();
        assert_eq!(rec.action, RecoverEntryAction::Skip);
        assert!(rec.max_slippage_price.is_none());
    }

    // ---- Wrong-side stop recovery (Commit 3) -----------------------------

    use crate::intent::{RecoverEntry, RecoverEntryAction};

    /// A short stop whose trigger has been overtaken by price (trigger >=
    /// close → wrong-side), with the given recovery policy. Mirrors the
    /// H&S short shape: SL above, TP below, stop-entry below.
    fn wrong_side_short_stop(rec: Option<RecoverEntry>) -> Intent {
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Short);
        // Stop trigger 1.1010 sits ABOVE the 1.1000 close → wrong-side for
        // a short (price already broke below the intended entry).
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 10.0,
            offset_atr_pct: None,
            at: None,
            recover_entry: rec,
        });
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: 40.0, // SL = 1.1040, above the trigger
            offset_atr_pct: None,
        });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: -90.0, // TP = 1.0910, well below
            offset_atr_pct: None,
        }));
        intent
    }

    #[test]
    fn wrong_side_short_stop_bare_still_drops() {
        // No recover_entry → today's behaviour: InvalidGeometry (drop).
        let r = Resolved::from_intent(&wrong_side_short_stop(None), &shell(), 0.0001, 0.0);
        assert!(matches!(r, Err(ResolveError::InvalidGeometry)));
    }

    #[test]
    fn wrong_side_short_stop_skip_drops() {
        let rec = Some(RecoverEntry {
            action: RecoverEntryAction::Skip,
            max_slippage_pips: None,
        });
        let r = Resolved::from_intent(&wrong_side_short_stop(rec), &shell(), 0.0001, 0.0);
        assert!(matches!(r, Err(ResolveError::InvalidGeometry)));
    }

    #[test]
    fn wrong_side_short_stop_market_recovers_at_close_with_derived_slippage() {
        let rec = Some(RecoverEntry {
            action: RecoverEntryAction::Market,
            max_slippage_pips: None, // → derived bound
        });
        let r = Resolved::from_intent(&wrong_side_short_stop(rec), &shell(), 0.0001, 0.0).unwrap();
        // Re-keyed to a market entry at the current close.
        match r.entry {
            ResolvedEntry::Market { reference_price } => {
                assert!((reference_price - 1.1000).abs() < 1e-9);
            }
            other => panic!("expected Market recovery, got {other:?}"),
        }
        let recd = r.recover_entry.expect("recovery carried");
        assert_eq!(recd.action, RecoverEntryAction::Market);
        // Derived slippage = |SL - trigger| = |1.1040 - 1.1010| = 0.0030.
        assert!((recd.max_slippage_price.unwrap() - 0.0030).abs() < 1e-9);
    }

    #[test]
    fn wrong_side_short_stop_market_explicit_slippage_wins() {
        let rec = Some(RecoverEntry {
            action: RecoverEntryAction::Market,
            max_slippage_pips: Some(12.0),
        });
        let r = Resolved::from_intent(&wrong_side_short_stop(rec), &shell(), 0.0001, 0.0).unwrap();
        let recd = r.recover_entry.unwrap();
        // Explicit 12 pips * 0.0001 = 0.0012 (overrides the derived 0.0030).
        assert!((recd.max_slippage_price.unwrap() - 0.0012).abs() < 1e-9);
    }

    #[test]
    fn wrong_side_short_stop_limit_rests_at_trigger() {
        let rec = Some(RecoverEntry {
            action: RecoverEntryAction::Limit,
            max_slippage_pips: None,
        });
        let r = Resolved::from_intent(&wrong_side_short_stop(rec), &shell(), 0.0001, 0.0).unwrap();
        match r.entry {
            ResolvedEntry::Limit { trigger_price } => {
                assert!((trigger_price - 1.1010).abs() < 1e-9);
            }
            other => panic!("expected Limit recovery, got {other:?}"),
        }
        let recd = r.recover_entry.unwrap();
        assert_eq!(recd.action, RecoverEntryAction::Limit);
        assert!(recd.max_slippage_price.is_none());
    }

    #[test]
    fn wrong_side_recovery_below_min_r_is_refused() {
        // Trigger 1.1010 wrong-side; recover at market@close 1.1000. Put TP
        // just below the close so R = (SL-entry)/(entry-TP) collapses below
        // 1.0 → finish_with_sizing's MIN_R_FLOOR rejects it.
        let mut intent = wrong_side_short_stop(Some(RecoverEntry {
            action: RecoverEntryAction::Market,
            max_slippage_pips: None,
        }));
        // SL = 1.1040 (30 pips above entry), TP = 1.0990 (10 pips below
        // entry) → R = 30/10... that's > 1. Flip: tiny SL, far TP would be
        // > 1R. For < 1R we need risk > reward: SL far, TP near.
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: 50.0, // SL 1.1050, risk = 50 pips from entry
            offset_atr_pct: None,
        });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: -20.0, // TP 1.0980, reward = 20 pips → R = 0.4
            offset_atr_pct: None,
        }));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0);
        assert!(
            matches!(r, Err(ResolveError::BelowMinR { .. })),
            "got {r:?}"
        );
    }

    #[test]
    fn wrong_side_recovery_past_tp_is_out_of_range() {
        // Recover at market@close, but put TP ABOVE the close so the entry
        // is no longer between SL and TP for a short → EntryOutsideRange.
        let mut intent = wrong_side_short_stop(Some(RecoverEntry {
            action: RecoverEntryAction::Market,
            max_slippage_pips: None,
        }));
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: 10.0, // TP 1.1010 — above the 1.1000 close
            offset_atr_pct: None,
        }));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0);
        assert!(
            matches!(r, Err(ResolveError::EntryOutsideRange { .. })),
            "got {r:?}"
        );
    }

    #[test]
    fn euro_wrong_side_short_limit_recovers() {
        // The real Euro Stocks 50 case (2026-06-23), pip_size 1.0:
        // signal_low 6307.2 → stop trigger 6306.2; by the confirm bar the
        // close had fallen to 6302.3 (trigger now wrong-side). SL 6329.0,
        // TP 6210.65. A `limit` recovery rests at 6306.2 for the pullback.
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Short);
        // Use the offset form (not absolute `at`) so the resolver's
        // wrong-side guard actually fires: trigger = close + 3.9 = 6306.2,
        // which sits above the 6302.3 close → wrong-side for a short.
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::Close,
            offset_pips: 3.9,
            offset_atr_pct: None,
            at: None,
            recover_entry: Some(RecoverEntry {
                action: RecoverEntryAction::Limit,
                max_slippage_pips: None,
            }),
        });
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: 26.7, // SL = 6302.3 + 26.7 = 6329.0
            offset_atr_pct: None,
        });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: -91.65, // TP = 6302.3 - 91.65 = 6210.65
            offset_atr_pct: None,
        }));
        let euro_shell = Shell {
            close: 6302.3,
            high: 6303.0,
            low: 6301.2,
            ..shell()
        };
        let r = Resolved::from_intent(&intent, &euro_shell, 1.0, 0.0).unwrap();
        match r.entry {
            ResolvedEntry::Limit { trigger_price } => {
                assert!(
                    (trigger_price - 6306.2).abs() < 1e-6,
                    "trigger {trigger_price}"
                );
            }
            other => panic!("expected Limit recovery, got {other:?}"),
        }
        assert!((r.stop_loss - 6329.0).abs() < 1e-6);
        assert!((r.take_profit - 6210.65).abs() < 1e-2);
    }

    #[test]
    fn long_limit_below_close_resolves() {
        let mut intent = long_market_intent();
        // Long limit at 1.0985 — below the 1.1000 close.
        intent.entry = Some(EntrySpec::Limit {
            from: PriceAnchor::Low,
            offset_pips: 5.0,
            offset_atr_pct: None,
            at: None,
        });
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
            offset_atr_pct: None,
            at: None,
        });
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
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
            offset_atr_pct: None,
        });
        // Short limit at 1.1015 — above the 1.1000 close.
        intent.entry = Some(EntrySpec::Limit {
            from: PriceAnchor::High,
            offset_pips: -5.0,
            offset_atr_pct: None,
            at: None,
        });
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
            offset_atr_pct: None,
            at: None,
            recover_entry: None,
        });
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::InvalidGeometry)
        ));
    }

    #[test]
    fn long_with_sl_above_entry_rejected() {
        let mut intent = long_market_intent();
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::High,
            offset_pips: 2.0,
            offset_atr_pct: None,
        });
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
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
        let r = Resolved::from_intent(&intent, &s, 0.0001, 0.0).unwrap();
        assert!((r.stop_loss - 1.86236).abs() < 1e-9);
        assert!((r.take_profit - 1.86899).abs() < 1e-9);
    }

    #[test]
    fn au200_short_rounds_prices_to_tick_grid() {
        // The 2026-07-08 incident: AU200_AUD ticks 0.1 but the resolver produced
        // a 5-dp short. With the instrument tick passed, entry/SL/TP snap to the
        // 0.1 grid so OANDA no longer rejects `PRICE_PRECISION_EXCEEDED`.
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Short);
        intent.stop_loss = Some(PriceRef::Absolute {
            absolute: 8841.39216,
        });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 8730.56867,
        }));
        let mut s = shell();
        s.close = 8806.70784; // market entry at the un-snapped close
        let r = Resolved::from_intent(&intent, &s, 1.0, 0.1).unwrap();
        // Entry snapped to nearest tick.
        assert_eq!(r.entry.reference_price(), 8806.7);
        // Short SL is above entry → rounds UP (away from entry, never tighter).
        assert_eq!(r.stop_loss, 8841.4);
        // Short TP is below entry → rounds UP (toward entry, never inflates R).
        assert_eq!(r.take_profit, 8730.6);
        // Every price is on the 0.1 grid (no sub-tick dust).
        for p in [r.entry.reference_price(), r.stop_loss, r.take_profit] {
            let steps = p / 0.1;
            assert!((steps - steps.round()).abs() < 1e-6, "{p} not on 0.1 grid");
        }
    }

    #[test]
    fn tick_rounding_below_min_r_is_rejected() {
        // A geometry that clears 1R raw but drops below it once snapped to a
        // coarse (1.0) grid must be rejected, not placed sub-1R. Entry 100.4,
        // SL 100.0 (0.4 risk), TP 100.85 (0.45 reward, R=1.125 raw). On a 1.0
        // grid: entry→100.0 collides with SL after away-rounding — the in-range
        // or R check refuses it rather than shipping a degenerate trade.
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Long);
        intent.stop_loss = Some(PriceRef::Absolute { absolute: 100.0 });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 100.85,
        }));
        let mut s = shell();
        s.close = 100.4;
        // Raw (no rounding) this resolves fine…
        assert!(Resolved::from_intent(&intent, &s, 0.01, 0.0).is_ok());
        // …but snapped to a 1.0 grid the geometry collapses and is refused.
        assert!(matches!(
            Resolved::from_intent(&intent, &s, 0.01, 1.0),
            Err(ResolveError::EntryOutsideRange { .. } | ResolveError::BelowMinR { .. })
        ));
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
        match Resolved::from_intent(&intent, &s, 0.0001, 0.0) {
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
            Resolved::from_intent(&intent, &s, 0.0001, 0.0),
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
            Resolved::from_intent(&intent, &s, 0.0001, 0.0),
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
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::InvalidRiskAmount { .. })
        ));
    }

    #[test]
    fn risk_amount_negative_rejected() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::Static(-1.0));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::InvalidRiskAmount { .. })
        ));
    }

    #[test]
    fn risk_amount_nan_rejected() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::Static(f64::NAN));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::InvalidRiskAmount { .. })
        ));
    }

    #[test]
    fn size_units_resolves_to_units_budget() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(0.01));
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::BothRiskModesSet)
        ));
    }

    #[test]
    fn size_units_zero_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(0.0));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::InvalidSizeUnits { .. })
        ));
    }

    #[test]
    fn size_units_negative_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(-0.01));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::InvalidSizeUnits { .. })
        ));
    }

    #[test]
    fn size_units_nan_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::Static(f64::NAN));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::InvalidSizeUnits { .. })
        ));
    }

    #[test]
    fn dry_run_defaults_false() {
        let r = Resolved::from_intent(&long_market_intent(), &shell(), 0.0001, 0.0).unwrap();
        assert!(!r.dry_run);
    }

    #[test]
    fn dry_run_propagates_to_resolved() {
        let mut intent = long_market_intent();
        intent.dry_run = Some(true);
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
        assert!(r.dry_run);
    }

    #[test]
    fn default_min_r_passes_when_r_above_one() {
        // R = 2.0 trade (R-multiple TP at 2.0), no min_r override → passes.
        let r = Resolved::from_intent(&long_market_intent(), &shell(), 0.0001, 0.0).unwrap();
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
            offset_atr_pct: None,
        }));
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::BelowMinR { .. })
        ));
    }

    #[test]
    fn min_r_override_above_floor_passes_when_met() {
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::Static(2.0)); // exactly meets the actual R
        assert!(Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).is_ok());
    }

    #[test]
    fn min_r_below_floor_rejected_at_server() {
        // Defense in depth: even if encoder somehow allowed it.
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::Static(0.5));
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::MinRBelowFloor { .. })
        ));
    }

    #[test]
    fn min_r_negative_rejected_at_server() {
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::Static(-1.0));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
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
            offset_atr_pct: None,
        });
        // Anchored TP 1 pip below entry — way under 1R for a 30-pip SL.
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Anchored {
            from: PriceAnchor::Close,
            offset_pips: -1.0,
            offset_atr_pct: None,
        }));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::BelowMinR { .. })
        ));
    }

    #[test]
    fn non_entry_action_rejected() {
        let mut intent = long_market_intent();
        intent.action = Action::Close;
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::NotAnEntry)
        ));
    }

    // ---- risk_pct as Tunable ----

    #[test]
    fn risk_pct_static_path_unchanged() {
        // Sanity: the existing tests already cover this via
        // long_market_intent(), but pin it explicitly.
        let intent = long_market_intent();
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
        let r = Resolved::from_intent(&intent, &s, 0.0001, 0.0).unwrap();
        assert!(matches!(r.risk, RiskBudget::Percent(p) if (p - 1.0).abs() < 1e-9));
    }

    #[test]
    fn risk_pct_script_returning_zero_rejected() {
        let mut intent = long_market_intent();
        intent.risk_pct = Tunable::from_script("0.0");
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
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
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
        let r = Resolved::from_intent(&intent, &s, 0.0001, 0.0).unwrap();
        assert!(matches!(r.risk, RiskBudget::Amount(a) if (a - 5.0).abs() < 1e-9));
    }

    #[test]
    fn risk_amount_script_returning_zero_rejected() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::from_script("0.0"));
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
            Err(ResolveError::InvalidRiskAmount { value }) => assert_eq!(value, 0.0),
            other => panic!("expected InvalidRiskAmount, got {other:?}"),
        }
    }

    #[test]
    fn risk_amount_script_returning_negative_rejected() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::from_script("-1.0"));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::InvalidRiskAmount { .. })
        ));
    }

    #[test]
    fn risk_amount_script_parse_error_surfaces() {
        let mut intent = long_market_intent();
        intent.risk_amount = Some(Tunable::from_script("if if if"));
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
        let r = Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).unwrap();
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
        let r = Resolved::from_intent(&intent, &s, 0.0001, 0.0).unwrap();
        assert!(matches!(r.risk, RiskBudget::Units(u) if (u - 0.05).abs() < 1e-9));
    }

    #[test]
    fn size_units_script_returning_zero_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::from_script("0.0"));
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
            Err(ResolveError::InvalidSizeUnits { value }) => assert_eq!(value, 0.0),
            other => panic!("expected InvalidSizeUnits, got {other:?}"),
        }
    }

    #[test]
    fn size_units_script_returning_negative_rejected() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::from_script("-0.01"));
        assert!(matches!(
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::InvalidSizeUnits { .. })
        ));
    }

    #[test]
    fn size_units_script_parse_error_surfaces() {
        let mut intent = long_market_intent();
        intent.size_units = Some(Tunable::from_script("if if if"));
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
        assert!(Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).is_ok());
    }

    #[test]
    fn min_r_script_evaluates_against_geometry() {
        // Script demands 1.5R when long, otherwise 1.0R.
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::from_script(
            "if direction == \"long\" { 1.5 } else { 1.0 }",
        ));
        assert!(Resolved::from_intent(&intent, &shell(), 0.0001, 0.0).is_ok());
    }

    #[test]
    fn min_r_script_below_floor_rejected() {
        // Script returns 0.5, below floor → MinRBelowFloor.
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::from_script("0.5"));
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
            Resolved::from_intent(&intent, &shell(), 0.0001, 0.0),
            Err(ResolveError::MinRBelowFloor { .. })
        ));
    }

    #[test]
    fn min_r_script_parse_error_surfaces() {
        let mut intent = long_market_intent();
        intent.min_r = Some(Tunable::from_script("if if if"));
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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
        match Resolved::from_intent(&intent, &shell(), 0.0001, 0.0) {
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

    // ---- ATR-based offset buffer (offset_atr_pct) ------------------------

    use crate::intent::OffsetError;

    /// A short H&S-style stop-entry: entry below the pattern at `signal_low`,
    /// SL above at `signal_high`, both buffered by `pct`% of ATR. The shell
    /// carries the latched pattern extremes + ATR.
    fn atr_short_intent(pct: f64) -> Intent {
        let mut intent = long_market_intent();
        intent.direction = Some(Direction::Short);
        intent.entry = Some(EntrySpec::Stop {
            from: PriceAnchor::SignalLow,
            offset_pips: 0.0,
            offset_atr_pct: Some(pct),
            at: None,
            recover_entry: None,
        });
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::SignalHigh,
            offset_pips: 0.0,
            offset_atr_pct: Some(pct),
        });
        intent.take_profit = Some(TakeProfit::Anchored(PriceRef::Absolute {
            absolute: 1.0700,
        }));
        intent
    }

    /// A shell with the H&S pattern levels + a latched ATR. close sits between
    /// the entry trigger and SL so the short geometry is in-range.
    fn atr_short_shell(atr: f64) -> Shell {
        let mut s = shell();
        s.close = 1.0950; // between trigger (below signal_low) and SL (above signal_high)
        s.signal_high = Some(1.1000);
        s.signal_low = Some(1.0900);
        s.atr = Some(atr);
        s
    }

    #[test]
    fn atr_pct_buffer_pushes_levels_away_from_candle() {
        // atr = 0.0040, pct = 0.5 → buffer = 0.5/100 * 0.0040 = 0.00002.
        // Entry anchor signal_low (1.0900) pushes DOWN: 1.0900 - 0.00002.
        // SL anchor signal_high (1.1000) pushes UP:   1.1000 + 0.00002.
        let r = Resolved::from_intent(
            &atr_short_intent(0.5),
            &atr_short_shell(0.0040),
            0.0001,
            0.0,
        )
        .unwrap();
        let buffer = 0.5 / 100.0 * 0.0040;
        match r.entry {
            ResolvedEntry::Stop { trigger_price } => {
                assert!(
                    (trigger_price - (1.0900 - buffer)).abs() < 1e-12,
                    "{trigger_price}"
                );
            }
            other => panic!("expected stop entry, got {other:?}"),
        }
        assert!(
            (r.stop_loss - (1.1000 + buffer)).abs() < 1e-12,
            "{}",
            r.stop_loss
        );
    }

    #[test]
    fn atr_pct_scales_with_volatility() {
        // Same pct, different ATR → proportionally wider buffer. A 4x ATR
        // gives a 4x buffer (the whole point of the feature).
        let tight = Resolved::from_intent(
            &atr_short_intent(0.5),
            &atr_short_shell(0.0010),
            0.0001,
            0.0,
        )
        .unwrap();
        let wide = Resolved::from_intent(
            &atr_short_intent(0.5),
            &atr_short_shell(0.0040),
            0.0001,
            0.0,
        )
        .unwrap();
        let tight_dist = (1.0900 - tight.entry.reference_price()).abs();
        let wide_dist = (1.0900 - wide.entry.reference_price()).abs();
        assert!(
            (wide_dist / tight_dist - 4.0).abs() < 1e-9,
            "{wide_dist} vs {tight_dist}"
        );
    }

    #[test]
    fn atr_pct_with_no_atr_rejects() {
        // Warmup / short-feed: shell carries no ATR. Fail-closed.
        let mut s = atr_short_shell(0.0040);
        s.atr = None;
        let r = Resolved::from_intent(&atr_short_intent(0.5), &s, 0.0001, 0.0);
        assert!(
            matches!(r, Err(ResolveError::Offset(OffsetError::AtrUnavailable))),
            "got {r:?}"
        );
    }

    #[test]
    fn atr_pct_and_offset_pips_both_set_rejects() {
        let mut intent = atr_short_intent(0.5);
        // Re-set the SL with BOTH offsets populated.
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::SignalHigh,
            offset_pips: 1.0,
            offset_atr_pct: Some(0.5),
        });
        let r = Resolved::from_intent(&intent, &atr_short_shell(0.0040), 0.0001, 0.0);
        assert!(
            matches!(r, Err(ResolveError::Offset(OffsetError::BothOffsetsSet))),
            "got {r:?}"
        );
    }

    #[test]
    fn atr_pct_on_close_anchor_rejects() {
        let mut intent = atr_short_intent(0.5);
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::Close, // directionless
            offset_pips: 0.0,
            offset_atr_pct: Some(0.5),
        });
        let r = Resolved::from_intent(&intent, &atr_short_shell(0.0040), 0.0001, 0.0);
        assert!(
            matches!(
                r,
                Err(ResolveError::Offset(OffsetError::AtrPctOnCloseAnchor))
            ),
            "got {r:?}"
        );
    }

    #[test]
    fn negative_atr_pct_rejects() {
        let r = Resolved::from_intent(
            &atr_short_intent(-0.5),
            &atr_short_shell(0.0040),
            0.0001,
            0.0,
        );
        assert!(
            matches!(r, Err(ResolveError::Offset(OffsetError::NegativeAtrPct(_)))),
            "got {r:?}"
        );
    }

    #[test]
    fn offset_pips_path_unchanged_when_no_atr_pct() {
        // The deprecated offset_pips path still resolves exactly as before
        // when offset_atr_pct is absent — even if the shell carries an ATR.
        let mut intent = long_market_intent();
        intent.stop_loss = Some(PriceRef::Anchored {
            from: PriceAnchor::Low,
            offset_pips: -2.0,
            offset_atr_pct: None,
        });
        let mut s = shell();
        s.atr = Some(0.0040); // present but must be ignored on the pips path
        let r = Resolved::from_intent(&intent, &s, 0.0001, 0.0).unwrap();
        // 1.0980 - 2*0.0001 = 1.0978, unchanged.
        assert!((r.stop_loss - 1.0978).abs() < 1e-9, "{}", r.stop_loss);
    }
}
