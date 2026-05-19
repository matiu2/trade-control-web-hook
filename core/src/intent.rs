//! The trade intent (decrypted JSON) and the plaintext shell (TradingView-substituted
//! prices), plus the logic that merges the two into a `Resolved` intent ready for
//! risk-gating and order placement.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod resolution;

#[cfg(feature = "cli")]
pub use resolution::MIN_R_FLOOR;
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
    /// Minimum acceptable R-multiple — server rejects entries whose
    /// implicit `(TP - entry) / (entry - SL)` falls below this. Defaults
    /// to 1.0 when omitted. Overrides must be `>= 1.0`; below-floor values
    /// are rejected both at the encoder and on the server.
    #[serde(default)]
    pub min_r: Option<f64>,
    /// Which broker the worker should route this intent to. Defaults to
    /// `oanda` when absent so intents encrypted before the multi-broker
    /// dispatch landed still work.
    #[serde(default)]
    pub broker: BrokerKind,
    /// Named account in the worker's account index to fulfil this
    /// intent against. When absent, the worker falls back to the
    /// pre-accounts lookup path (one shared session for `tradenation`,
    /// the global API key for `oanda`). The account's recorded
    /// `broker` must match this intent's `broker`; mismatch is
    /// rejected at dispatch time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Required for `prep` / `clear-prep`. The named step that landed
    /// (e.g. `break-and-close`, `retest`).
    #[serde(default)]
    pub step: Option<String>,
    /// Required for `veto` / `clear-veto`. The named condition
    /// blocking entries (e.g. `news-window`).
    #[serde(default)]
    pub name: Option<String>,
    /// Required for `prep` / `veto`. TTL in hours for the flag.
    #[serde(default)]
    pub ttl_hours: Option<u32>,
    /// Escalation level for a `veto` action. Default is
    /// [`VetoLevel::StopNextEntry`] (flag-only, no broker side effects).
    /// Higher levels also cancel pending orders and/or close positions
    /// at fire time. The flag itself only blocks future entries — the
    /// side effects are one-shot at fire time, re-fire to repeat them.
    #[serde(default)]
    pub level: Option<VetoLevel>,
    /// Optional gate on `enter`. Ordered list of named preps that must
    /// be active for this instrument; each prep's `set_at` timestamp
    /// must be strictly greater than the previous prep's. Absent /
    /// empty means no prep gate.
    #[serde(default)]
    pub requires_preps: Vec<String>,
    /// Optional gate on `enter`. Entry is rejected if any of these named
    /// vetos are active for this instrument. Absent / empty means no
    /// veto gate.
    #[serde(default)]
    pub vetos: Vec<String>,
    /// Names to clear *before* setting the new prep/veto. Used to model
    /// ordered prep sequences where landing an earlier step must
    /// invalidate any stale later step.
    ///
    /// Example: a `prep` action setting `break-and-close` with
    /// `clears: [retest]` will drop any pre-existing `retest` prep on
    /// the same instrument before recording the new break-and-close.
    /// Otherwise a stale retest from before the break-and-close would
    /// stick around and satisfy a future `requires_preps:
    /// [break-and-close, retest]` gate without the operator ever
    /// observing a fresh retest.
    ///
    /// On `Prep` actions the names are interpreted as prep steps; on
    /// `Veto` actions they are veto names. Missing/empty list is a
    /// no-op and back-compatible with intents encrypted before the
    /// field landed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clears: Vec<String>,
}

/// Which broker fulfils an intent. The serialised form is the
/// lowercase variant name (`oanda`, `tradenation`); absent on the wire
/// means [`BrokerKind::Oanda`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BrokerKind {
    #[default]
    Oanda,
    TradeNation,
}

/// Escalation level for a `veto` action. Vetos always set the named
/// KV flag (the gate on future entries); higher levels also act on the
/// broker right now.
///
/// Levels are ordered:
///   1. [`StopNextEntry`] — KV flag only. The next `enter` that opts
///      into checking this name gets rejected. No broker call.
///   2. [`CancelPending`] — also cancels resting stop/limit orders for
///      the instrument. Open positions are left alone.
///   3. [`ClosePositions`] — also closes open positions for the
///      instrument.
///
/// The KV flag survives across all levels — re-firing a level-2 veto
/// at the same name re-cancels pending orders (alerts can drop;
/// re-applying side effects is cheap and defensive).
///
/// Distinct from `invalidate`, which is an instrument-wide cooldown:
/// every enter on that instrument is blocked. A veto only blocks
/// entries that *opt in* by listing the name in their `vetos:` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VetoLevel {
    /// KV flag only. No broker side effects.
    #[default]
    StopNextEntry,
    /// Flag + cancel resting pending orders for the instrument.
    CancelPending,
    /// Flag + cancel pending orders + close open positions.
    ClosePositions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Enter,
    Close,
    Invalidate,
    /// Read-only snapshot of cooldowns + recent seen ids. `instrument` is
    /// required by the schema but ignored — use any placeholder.
    Status,
    /// Clear a single instrument's cooldown.
    Unlock,
    /// Record a named "prep" step for an instrument with a TTL. Used to
    /// build up multi-event setups (e.g. break-and-close → retest →
    /// entry) where the `enter` checks for prior preps.
    Prep,
    /// Record a named "veto" for an instrument with a TTL. Active vetos
    /// block any `enter` that opts into checking them.
    Veto,
    /// Clear a single instrument's prep flag.
    ClearPrep,
    /// Clear a single instrument's veto flag.
    ClearVeto,
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

/// Reference to a price. Either anchored to the plaintext shell with a pip
/// offset (TradingView fills in the anchor at fire time) or a fixed absolute
/// price set at encode time (the worker uses it verbatim, ignoring the shell).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum PriceRef {
    /// `{ absolute: 1.86236 }` — fixed price; shell ignored.
    Absolute { absolute: f64 },
    /// `{ from: low, offset_pips: -2 }` — anchor + signed pip offset.
    Anchored {
        from: PriceAnchor,
        /// Offset in pips. Sign matters: -2 means "low - 2 pips" regardless
        /// of direction. The "pip" here is the instrument's pip size; the
        /// caller supplies that.
        #[serde(default)]
        offset_pips: f64,
    },
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
        match self {
            PriceRef::Absolute { absolute } => *absolute,
            PriceRef::Anchored { from, offset_pips } => {
                shell.anchor_price(*from) + offset_pips * pip_size
            }
        }
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
        let sl = PriceRef::Anchored {
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
        // Pre-existing intents on the wire don't carry a `broker:` field; they
        // must keep routing to OANDA.
        assert_eq!(intent.broker, BrokerKind::Oanda);
    }

    #[test]
    fn intent_defaults_account_to_none() {
        // Absent `account:` on the wire — the worker uses the
        // pre-accounts fallback path. Required for back-compat with
        // intents minted before the account field landed.
        let yaml = "
            v: 1
            id: a-1
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
        assert_eq!(intent.account, None);
    }

    #[test]
    fn intent_parses_explicit_account() {
        let yaml = "
            v: 1
            id: a-2
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            broker: tradenation
            account: reversals
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.account.as_deref(), Some("reversals"));
        assert_eq!(intent.broker, BrokerKind::TradeNation);
    }

    #[test]
    fn intent_account_round_trip_yaml() {
        // Round-trip serialisation must preserve the account field; if
        // it's dropped on re-serialise the encryption / signing paths
        // would lose the field on the wire.
        let yaml = "
            v: 1
            id: a-3
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            broker: tradenation
            account: live-prod
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("account: live-prod"));
    }

    #[test]
    fn intent_omits_account_when_none() {
        // `skip_serializing_if = Option::is_none` keeps the wire form
        // unchanged for pre-accounts intents — no spurious `account:
        // null` lines that would confuse signed-mode replay.
        let yaml = "
            v: 1
            id: a-4
            not_after: \"2026-05-13T20:00:00Z\"
            action: status
            instrument: ALL
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(!back.contains("account"));
    }

    #[test]
    fn intent_parses_explicit_broker() {
        let yaml = "
            v: 1
            id: tn-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            broker: tradenation
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.broker, BrokerKind::TradeNation);
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

    #[test]
    fn prep_intent_parses_with_step_and_ttl() {
        let yaml = "
            v: 1
            id: prep-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.action, Action::Prep);
        assert_eq!(intent.step.as_deref(), Some("break-and-close"));
        assert_eq!(intent.ttl_hours, Some(4));
    }

    #[test]
    fn prep_intent_defaults_clears_to_empty() {
        let yaml = "
            v: 1
            id: prep-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(intent.clears.is_empty());
    }

    #[test]
    fn prep_intent_parses_clears_list() {
        // A break-and-close prep declaring it invalidates any stale retest
        // — the central piece of the prep-ordering fix.
        let yaml = "
            v: 1
            id: prep-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
            clears: [retest]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.clears, vec!["retest".to_string()]);
    }

    #[test]
    fn empty_clears_is_omitted_when_serialised() {
        // Wire-compat: an intent with no clears must serialise without a
        // `clears:` line, so pre-existing replay paths that round-trip
        // through YAML don't change shape.
        let yaml = "
            v: 1
            id: prep-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        let out = serde_yaml::to_string(&intent).unwrap();
        assert!(!out.contains("clears:"), "got:\n{out}");
    }

    #[test]
    fn veto_intent_parses_with_name_and_ttl() {
        let yaml = "
            v: 1
            id: veto-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: veto
            instrument: EUR_USD
            name: news-window
            ttl_hours: 6
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.action, Action::Veto);
        assert_eq!(intent.name.as_deref(), Some("news-window"));
        assert_eq!(intent.ttl_hours, Some(6));
    }

    #[test]
    fn veto_intent_defaults_level_to_stop_next_entry() {
        let yaml = "
            v: 1
            id: veto-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: veto
            instrument: EUR_USD
            name: news-window
            ttl_hours: 6
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        // Absent on the wire defaults to None; the worker reads it as
        // `unwrap_or_default()` == StopNextEntry.
        assert_eq!(intent.level, None);
        assert_eq!(intent.level.unwrap_or_default(), VetoLevel::StopNextEntry);
    }

    #[test]
    fn veto_intent_parses_cancel_pending_level() {
        let yaml = "
            v: 1
            id: veto-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: veto
            instrument: EUR_USD
            name: structure-broken
            ttl_hours: 4
            level: cancel-pending
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.level, Some(VetoLevel::CancelPending));
    }

    #[test]
    fn veto_intent_parses_close_positions_level() {
        let yaml = "
            v: 1
            id: veto-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: veto
            instrument: EUR_USD
            name: structure-broken
            ttl_hours: 4
            level: close-positions
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.level, Some(VetoLevel::ClosePositions));
    }

    #[test]
    fn veto_level_round_trips_through_yaml() {
        for level in [
            VetoLevel::StopNextEntry,
            VetoLevel::CancelPending,
            VetoLevel::ClosePositions,
        ] {
            let yaml = serde_yaml::to_string(&level).unwrap();
            let parsed: VetoLevel = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(parsed, level);
        }
    }

    #[test]
    fn clear_prep_action_parses_kebab_case() {
        let yaml = "
            v: 1
            id: clear-prep-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: clear-prep
            instrument: EUR_USD
            step: retest
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.action, Action::ClearPrep);
        assert_eq!(intent.step.as_deref(), Some("retest"));
    }

    #[test]
    fn clear_veto_action_parses_kebab_case() {
        let yaml = "
            v: 1
            id: clear-veto-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: clear-veto
            instrument: EUR_USD
            name: news-window
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.action, Action::ClearVeto);
        assert_eq!(intent.name.as_deref(), Some("news-window"));
    }

    #[test]
    fn enter_intent_defaults_empty_gate_lists() {
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
        assert!(intent.requires_preps.is_empty());
        assert!(intent.vetos.is_empty());
    }

    #[test]
    fn enter_intent_parses_requires_preps_and_vetos() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: short
            entry: { type: market }
            stop_loss: { from: high, offset_pips: 2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            requires_preps: [break-and-close, retest]
            vetos: [news-window]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.requires_preps,
            vec!["break-and-close".to_string(), "retest".to_string()]
        );
        assert_eq!(intent.vetos, vec!["news-window".to_string()]);
    }

    #[test]
    fn existing_actions_still_serialise_lowercase_after_kebab_switch() {
        // `enter`, `close`, `invalidate`, `status`, `unlock` are single
        // words — kebab-case and lowercase render identically. This test
        // pins that contract so wire-compat with pre-existing encrypted
        // intents survives the rename_all change.
        assert_eq!(
            serde_yaml::to_string(&Action::Enter).unwrap().trim(),
            "enter"
        );
        assert_eq!(
            serde_yaml::to_string(&Action::Close).unwrap().trim(),
            "close"
        );
        assert_eq!(
            serde_yaml::to_string(&Action::Invalidate).unwrap().trim(),
            "invalidate"
        );
        assert_eq!(
            serde_yaml::to_string(&Action::Status).unwrap().trim(),
            "status"
        );
        assert_eq!(
            serde_yaml::to_string(&Action::Unlock).unwrap().trim(),
            "unlock"
        );
    }
}
