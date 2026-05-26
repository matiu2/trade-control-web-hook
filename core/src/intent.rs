//! The trade intent (decrypted JSON) and the plaintext shell (TradingView-substituted
//! prices), plus the logic that merges the two into a `Resolved` intent ready for
//! risk-gating and order placement.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod resolution;

#[cfg(feature = "cli")]
pub use resolution::MIN_R_FLOOR;
pub use resolution::{Resolved, ResolvedEntry, RiskBudget};

/// Plaintext outer YAML — the part TradingView substitutes `{{...}}` into.
/// The intent fields sit alongside these at the top level of the signed
/// body; the HMAC over the whole thing lives in a separate `sig` field
/// (not modelled here — handled raw in `core::sig`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Shell {
    pub close: f64,
    pub high: f64,
    pub low: f64,
    /// ISO-8601 timestamp from TradingView. Used as an upper bound on the
    /// alert's freshness — alerts from yesterday should be obvious.
    pub time: DateTime<Utc>,
    /// Signal extremes latched by the Pine indicator and substituted into
    /// the alert message via {{plot("signal_high")}} / {{plot("signal_low")}}.
    /// For a 1-bar pinbar these are the signal bar's high/low; for a
    /// 2-bar tweezer or engulfer they span both bars; for a 3-bar
    /// double-tweezer they span all three. Optional because pre-2026-05
    /// signed templates didn't carry them and control-action shells
    /// (status / unlock / etc.) don't either. Populated by Pine
    /// `candle-signals-v2.pine` (v2.2+) and read by the Rhai engine to
    /// drive entry-gate scripts, dynamic SL/TP, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_high: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_low: Option<f64>,
    /// `signal_high - signal_low`, pre-computed Pine-side so scripts can
    /// reference the signal's geometry without recomputing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal_range: Option<f64>,
    /// Bar-open time of the *earliest* bar that's part of the signal —
    /// `time[0]` for a pinbar, `time[1]` for tweezer/engulfers,
    /// `time[2]` for a double-tweezer. Disambiguates from the
    /// current-bar-of-fire `time` field. Pine ships this as milliseconds
    /// since epoch (integer), not RFC3339 — see `signal_time_serde`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "signal_time_serde"
    )]
    pub signal_start_time: Option<DateTime<Utc>>,
    /// Which signal detector fired. Pine emits a float code (1..=5);
    /// the serde adapter maps it to a [`SignalKind`] variant on the way
    /// in and back to the float on the way out. Pine `alertcondition`
    /// messages can't ride string or enum values — only float plots and
    /// built-ins — so the float-on-wire detour is unavoidable.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "signal_kind_serde"
    )]
    pub signal_kind: Option<SignalKind>,
    /// True iff the latched signal is "golden" by the indicator's
    /// definition (close near the extreme, etc.). Pine emits 1/0 as a
    /// number — see `bool_one_zero_serde`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "bool_one_zero_serde"
    )]
    pub golden: Option<bool>,
    /// ATR value latched at signal time. Lets Rhai scripts compare
    /// candle range / SL distance against volatility without needing
    /// their own indicator pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atr: Option<f64>,
    /// True iff the latched signal has been validated by a confirming
    /// push within the indicator's `confirm_bars` window. Pine emits
    /// 1/0 as a number — see `bool_one_zero_serde`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "bool_one_zero_serde"
    )]
    pub signal_confirmed: Option<bool>,
    /// Highest high over the indicator's `sl_lookback` window of bars
    /// *strictly preceding* the signal bar. Intended as a robust SL
    /// anchor for short trades that doesn't depend on the signal
    /// candle's own wick. Pine emits via `{{plot("recent_high")}}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_high: Option<f64>,
    /// Lowest low over the same window. SL anchor for long trades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_low: Option<f64>,
}

/// Which candle signal detector fired. Mirrors the `KIND_*` constants
/// in `candle-signals-v2.pine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Pinbar,
    Tweezer,
    RegularEngulfer,
    FloatingEngulfer,
    DoubleTweezer,
}

impl SignalKind {
    /// Pine-side float code — keep in lockstep with the `KIND_*` consts
    /// in `candle-signals-v2.pine`.
    pub fn from_code(code: f64) -> Option<Self> {
        match code.round() as i64 {
            1 => Some(Self::Pinbar),
            2 => Some(Self::Tweezer),
            3 => Some(Self::RegularEngulfer),
            4 => Some(Self::FloatingEngulfer),
            5 => Some(Self::DoubleTweezer),
            _ => None,
        }
    }

    pub fn to_code(self) -> u8 {
        match self {
            Self::Pinbar => 1,
            Self::Tweezer => 2,
            Self::RegularEngulfer => 3,
            Self::FloatingEngulfer => 4,
            Self::DoubleTweezer => 5,
        }
    }
}

/// Pine emits `signal_start_time` as a millisecond-precision Unix epoch
/// integer (e.g. `1779742740000`). Convert to/from `DateTime<Utc>`.
mod signal_time_serde {
    use chrono::{DateTime, TimeZone, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(dt) => s.serialize_i64(dt.timestamp_millis()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        let v: Option<serde_yaml::Value> = Option::deserialize(d)?;
        let Some(v) = v else { return Ok(None) };
        let ms = match v {
            serde_yaml::Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
            serde_yaml::Value::String(s) => s.parse::<i64>().ok(),
            _ => None,
        }
        .ok_or_else(|| serde::de::Error::custom("signal_start_time: expected integer ms epoch"))?;
        Utc.timestamp_millis_opt(ms)
            .single()
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("signal_start_time: ms out of range"))
    }
}

/// Pine emits boolean shell fields as `0` or `1` (number). Accept
/// numbers and the strings "0"/"1"/"true"/"false" defensively.
mod bool_one_zero_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<bool>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(b) => s.serialize_u8(u8::from(*b)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
        let v: Option<serde_yaml::Value> = Option::deserialize(d)?;
        let Some(v) = v else { return Ok(None) };
        match v {
            serde_yaml::Value::Bool(b) => Ok(Some(b)),
            serde_yaml::Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                Ok(Some(f > 0.5))
            }
            serde_yaml::Value::String(s) => match s.as_str() {
                "1" | "true" | "True" | "TRUE" => Ok(Some(true)),
                "0" | "false" | "False" | "FALSE" => Ok(Some(false)),
                other => Err(serde::de::Error::custom(format!(
                    "expected 0/1/true/false, got {other:?}"
                ))),
            },
            _ => Err(serde::de::Error::custom("expected bool/number/string")),
        }
    }
}

/// Pine emits `signal_kind` as a float code (1.0..=5.0). Map to/from
/// [`SignalKind`].
mod signal_kind_serde {
    use super::SignalKind;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<SignalKind>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(k) => s.serialize_u8(k.to_code()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<SignalKind>, D::Error> {
        let v: Option<serde_yaml::Value> = Option::deserialize(d)?;
        let Some(v) = v else { return Ok(None) };
        let code = match v {
            serde_yaml::Value::Number(n) => n.as_f64(),
            serde_yaml::Value::String(s) => s.parse::<f64>().ok(),
            _ => None,
        }
        .ok_or_else(|| serde::de::Error::custom("signal_kind: expected numeric code"))?;
        SignalKind::from_code(code)
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom(format!("signal_kind: unknown code {code}")))
    }
}

/// The fully-decrypted intent. `v` lets us reject future protocol versions cleanly.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Intent {
    /// Protocol version, must be `1`.
    pub v: u32,
    /// Unique id per intended trade, used for replay protection.
    pub id: String,
    /// Optional earliest time the alert is allowed to fire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<Utc>>,
    /// Hard expiry — alerts that arrive after this are rejected.
    pub not_after: DateTime<Utc>,
    /// What to do.
    pub action: Action,
    /// OANDA instrument name, e.g. `EUR_USD`.
    pub instrument: String,
    /// Required for `enter`; ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    /// Required for `enter`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntrySpec>,
    /// Required for `enter`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_loss: Option<PriceRef>,
    /// Required for `enter`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take_profit: Option<TakeProfit>,
    /// Risk per trade as % of account equity; the server-side cap
    /// clamps it. Defaults to `Tunable::Static(1.0)` — 1% — which is
    /// the operator's standard setting. `risk_amount` and `size_units`,
    /// when set, override this (they're mutually exclusive with each
    /// other, but either supersedes the risk_pct default).
    ///
    /// A [`Tunable<f64>`] — operators can supply a static literal
    /// (`risk_pct: 0.5`) or a Rhai script (`risk_pct: !script "..."`)
    /// that resolves against the standard three-phase scope (shell
    /// anchors + derived geometry). Scripts that return a non-finite
    /// or non-positive value are rejected at resolve time.
    #[serde(
        default = "default_risk_pct",
        skip_serializing_if = "is_default_risk_pct"
    )]
    pub risk_pct: crate::tunable::Tunable<f64>,
    /// Alternative to `risk_pct`: a fixed money amount to risk per
    /// trade, in the account's own currency (e.g. `1.0` for "bet $1").
    /// Useful on a live account to keep position sizes constant
    /// regardless of equity growth. Exactly one of `risk_pct` /
    /// `risk_amount` / `size_units` must be set; mixing is rejected
    /// at resolve time. The `MAX_RISK_PCT_PER_TRADE` cap still
    /// applies — at fire time the worker translates the amount to an
    /// effective percent (`amount / equity * 100`) and rejects if
    /// that exceeds the cap.
    ///
    /// A [`Tunable<f64>`] — operators can supply a static literal
    /// (`risk_amount: 1.0`) or a Rhai script (`risk_amount: !script
    /// "..."`) that resolves against the standard three-phase scope
    /// (shell anchors + derived geometry). Scripts returning a
    /// non-finite or non-positive value are rejected at resolve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_amount: Option<crate::tunable::Tunable<f64>>,
    /// Alternative to `risk_pct` / `risk_amount`: a fixed position
    /// size in instrument units (e.g. `0.01` for one micro-lot of FX,
    /// or a literal contract count for CFDs). Bypasses sizing math
    /// entirely — the worker just sends this many units. The risk-cap
    /// check is still applied by reconstructing the implied money
    /// risk (`size_units * stop_distance`) and dividing by equity.
    /// Exactly one of `risk_pct` / `risk_amount` / `size_units` must
    /// be set.
    ///
    /// A [`Tunable<f64>`] — operators can supply a static literal
    /// (`size_units: 0.01`) or a Rhai script (`size_units: !script
    /// "..."`) that resolves against the standard three-phase scope
    /// (shell anchors + derived geometry). Scripts returning a
    /// non-finite or non-positive value are rejected at resolve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_units: Option<crate::tunable::Tunable<f64>>,
    /// When true, the worker resolves the intent, logs the sizing
    /// inputs / calculations / output, then returns success **without
    /// placing the order**. Useful for verifying new sizing modes
    /// (e.g. `risk_amount`) safely on a live account, and for sanity-
    /// checking a fresh template before live-firing it. Defaults to
    /// false. Applies to `enter` only; non-entry actions ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// Required for `invalidate`.
    ///
    /// A [`Tunable<u32>`] — operators can supply a static literal
    /// (`cooldown_hours: 12`) or a Rhai script (`cooldown_hours:
    /// !script "..."`) that resolves against Phase 1 scope only
    /// (shell anchors — invalidate runs without geometry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_hours: Option<crate::tunable::Tunable<u32>>,
    /// Minimum acceptable R-multiple — server rejects entries whose
    /// implicit `(TP - entry) / (entry - SL)` falls below this. Defaults
    /// to 1.0 when omitted. Overrides must be `>= 1.0`; below-floor values
    /// are rejected both at the encoder and on the server.
    ///
    /// A [`Tunable<f64>`] — operators can supply a static literal
    /// (`min_r: 1.5`) or a Rhai script (`min_r: !script "..."`) that
    /// resolves against the standard three-phase scope (shell anchors
    /// plus derived geometry). Scripts returning a non-finite value or
    /// one below the floor are rejected at resolve time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_r: Option<crate::tunable::Tunable<f64>>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// Required for `veto` / `clear-veto`. The named condition
    /// blocking entries (e.g. `news-window`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Required for `prep` / `veto`. TTL in hours for the flag.
    /// Default is `Static(0)` — the wire-elided sentinel that means
    /// "not set"; validation rejects it on prep/veto. Other actions
    /// ignore the field. Scripts always count as opted-in, so a script
    /// that happens to resolve to 0 produces a TTL of 0 ("expire
    /// immediately") rather than being treated as the default.
    ///
    /// A [`Tunable<u32>`] — operators can supply a static literal
    /// (`ttl_hours: 6`) or a Rhai script (`ttl_hours: !script "..."`)
    /// that resolves against Phase 1 scope only (shell anchors —
    /// prep/veto run without geometry).
    #[serde(default, skip_serializing_if = "is_default_ttl_hours")]
    pub ttl_hours: crate::tunable::Tunable<u32>,
    /// Escalation level for a `veto` action. Default is
    /// [`VetoLevel::StopNextEntry`] (flag-only, no broker side effects).
    /// Higher levels also cancel pending orders and/or close positions
    /// at fire time. The flag itself only blocks future entries — the
    /// side effects are one-shot at fire time, re-fire to repeat them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<VetoLevel>,
    /// Optional gate on `enter`. Ordered list of named preps that must
    /// be active for this instrument; each prep's `set_at` timestamp
    /// must be strictly greater than the previous prep's. Absent /
    /// empty means no prep gate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_preps: Vec<String>,
    /// Optional gate on `enter`. Entry is rejected if any of these named
    /// vetos are active for this instrument. Absent / empty means no
    /// veto gate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    /// Optional grouping id. Every alert that the CLI emits as part of
    /// one "trade" (e.g. an H&S template's 5 alerts: too-high veto,
    /// trade-expiry veto, break-and-close prep, retest prep, enter)
    /// shares the same `trade_id`. The worker uses it as a free-form
    /// log/group correlator and will later expose it for bulk-cancel /
    /// status-filter; no trade-level invariants are enforced at the
    /// wire layer — every alert stands alone.
    ///
    /// Format is a slug: lowercase ASCII letters / digits / hyphens,
    /// 1–64 chars, no leading or trailing hyphen, no consecutive
    /// hyphens. Validation runs at parse time so junk doesn't end up
    /// in the seen-index. Absent on the wire means "no group" and is
    /// back-compatible with intents minted before this field landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_id: Option<String>,
    /// Cap on placed enter attempts within the trade window. The default
    /// is `Tunable::Static(0)` — single-shot behaviour, byte-identical
    /// wire form to pre-feature intents. Any non-default value opts the
    /// enter into multi-shot: the alert may fire on multiple firing bars
    /// within the `not_after` window, dedup'd by `(trade_id,
    /// shell.time)`. Total **placed** entries are capped at the resolved
    /// value. Multi-shot requires `trade_id` to be set and `action` to
    /// be [`Action::Enter`]; rejected at validate time otherwise.
    ///
    /// A [`Tunable<u32>`] — operators can supply a static literal
    /// (`max_retries: 3`) or a Rhai script (`max_retries: !script
    /// "..."`) that resolves at gate time against Phase 1 scope only
    /// (shell anchors — the gate runs before geometry is built, so
    /// derived bindings are unavailable). Scripts that resolve to zero
    /// are rejected at resolve time (a script that wants to opt out of
    /// retries should not be in the field in the first place).
    #[serde(default, skip_serializing_if = "is_default_max_retries")]
    pub max_retries: crate::tunable::Tunable<u32>,
    /// Optional Rhai gate on `enter`. When set, the worker resolves
    /// the [`Tunable<bool>`] (Static or `!script`) after passing the
    /// retry / prep / veto gates and rejects the entry with a 412 if
    /// it evaluates to `false`. Composes with `max_retries`: each
    /// retry re-evaluates against its own incoming shell, so a
    /// `!script "signal_confirmed == true"` gate naturally implements
    /// wait-for-confirmation by letting the worker burn attempts
    /// until a confirming signal arrives.
    ///
    /// Resolved against the standard three-phase scope (shell anchors
    /// plus derived geometry). Scripts can reference any field bound
    /// by `crate::rules::bind_shell_anchors` and
    /// `crate::rules::bind_intent_derived`, plus the `pct` / `pips`
    /// helpers. Returning non-bool is a 412.
    ///
    /// Default-absent = unconditional allow; byte-identical wire form
    /// to pre-`allow_entry` intents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_entry: Option<crate::tunable::Tunable<bool>>,
    /// When true, the worker rejects the entry unless the incoming shell
    /// carries `golden: Some(true)`. AND-composed with [`Self::allow_entry`]
    /// — both gates must pass. Promoted to a typed field (rather than a
    /// script idiom like `golden == true`) because operators reach for it
    /// often; the typed form avoids the Rhai `()` landmine when the shell
    /// omits the field. Default `false` = no gate, byte-identical wire
    /// form to pre-feature intents. Only meaningful on `Action::Enter`;
    /// rejected at validate time on other actions.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_golden: bool,
}

/// Skip-serializing predicate for [`Intent::max_retries`]. Returns true
/// when the field carries its default single-shot value (`Static(0)`)
/// so the wire form stays byte-identical to pre-feature intents. A
/// `Script` is never elided — we can't know its resolved value at
/// serialise time, and an operator who wrote a script clearly meant it.
fn is_default_max_retries(t: &crate::tunable::Tunable<u32>) -> bool {
    matches!(t, crate::tunable::Tunable::Static(0))
}

/// Default `risk_pct` — 1% per trade is the operator's standard
/// setting, so we ship it as the default and only require operators to
/// write the field when they want something different.
fn default_risk_pct() -> crate::tunable::Tunable<f64> {
    crate::tunable::Tunable::Static(1.0)
}

/// Skip-serializing predicate for [`Intent::risk_pct`]. Returns true on
/// the default `Static(1.0)` so the wire form stays byte-identical to
/// intents that omit the field. Scripts are never elided.
fn is_default_risk_pct(t: &crate::tunable::Tunable<f64>) -> bool {
    matches!(t, crate::tunable::Tunable::Static(v) if *v == 1.0)
}

/// Skip-serializing predicate for [`Intent::ttl_hours`]. `Static(0)` is
/// the "not set" sentinel — required on prep/veto (validated), ignored
/// elsewhere. Scripts are never elided.
fn is_default_ttl_hours(t: &crate::tunable::Tunable<u32>) -> bool {
    matches!(t, crate::tunable::Tunable::Static(0))
}

/// Maximum length of a `trade_id` slug. 64 chars is plenty for
/// `<instrument>-<direction>-<short-random>` style ids and keeps the
/// seen-index entry small.
pub const TRADE_ID_MAX_LEN: usize = 64;

/// Returns true if `s` is a valid `trade_id` slug: lowercase ASCII
/// alphanumerics + hyphens, 1..=64 chars, no leading/trailing hyphen,
/// no consecutive hyphens. Used by [`Intent::validate`] and by the CLI
/// emitter (which mints the id) so both ends agree on the shape.
pub fn is_valid_trade_id(s: &str) -> bool {
    if s.is_empty() || s.len() > TRADE_ID_MAX_LEN {
        return false;
    }
    if s.starts_with('-') || s.ends_with('-') {
        return false;
    }
    if s.contains("--") {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Error returned by [`Intent::validate`].
#[derive(Debug, PartialEq, Eq)]
pub enum IntentValidationError {
    /// `trade_id` failed [`is_valid_trade_id`].
    InvalidTradeId,
    /// `max_retries` non-default without a `trade_id` — the retry gate
    /// is keyed on `(account, trade_id)` so the field is mandatory once
    /// the operator opts into multi-shot.
    MaxRetriesWithoutTradeId,
    /// `max_retries` non-default on a non-Enter action — retries only
    /// make sense for `enter`.
    MaxRetriesOnNonEnter,
    /// `allow_entry: Some(_)` on a non-Enter action — the gate is
    /// only checked on `enter`.
    AllowEntryOnNonEnter,
    /// `needs_golden: true` on a non-Enter action — the gate is only
    /// checked on `enter`.
    NeedsGoldenOnNonEnter,
    /// `ttl_hours` missing (i.e. defaulted to `Static(0)`) on a prep or
    /// veto action where it's required to set the KV flag's lifetime.
    MissingTtlHours,
}

impl core::fmt::Display for IntentValidationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidTradeId => f.write_str(
                "invalid trade_id (must be 1-64 chars of lowercase alphanumerics + hyphens, \
                 no leading/trailing or consecutive hyphens)",
            ),
            Self::MaxRetriesWithoutTradeId => {
                f.write_str("max_retries requires trade_id to be set")
            }
            Self::MaxRetriesOnNonEnter => f.write_str("max_retries is only valid on action: enter"),
            Self::AllowEntryOnNonEnter => f.write_str("allow_entry is only valid on action: enter"),
            Self::NeedsGoldenOnNonEnter => {
                f.write_str("needs_golden is only valid on action: enter")
            }
            Self::MissingTtlHours => f.write_str("ttl_hours is required on prep / veto actions"),
        }
    }
}

impl std::error::Error for IntentValidationError {}

impl Intent {
    /// Post-deserialise validation for fields that have a shape contract
    /// beyond what serde can express. Called by the incoming-payload
    /// pipeline; the field-by-field deser still works on its own so
    /// round-trip tests don't need to go through this gate.
    pub fn validate(&self) -> Result<(), IntentValidationError> {
        if let Some(id) = &self.trade_id
            && !is_valid_trade_id(id)
        {
            return Err(IntentValidationError::InvalidTradeId);
        }
        // Multi-shot gate: anything other than the default `Static(0)`
        // counts as opting in. Scripts always count — we can't know at
        // validate time whether they'll resolve to zero, and writing a
        // script means the operator meant it.
        if !is_default_max_retries(&self.max_retries) {
            if self.trade_id.is_none() {
                return Err(IntentValidationError::MaxRetriesWithoutTradeId);
            }
            if self.action != Action::Enter {
                return Err(IntentValidationError::MaxRetriesOnNonEnter);
            }
        }
        if self.allow_entry.is_some() && self.action != Action::Enter {
            return Err(IntentValidationError::AllowEntryOnNonEnter);
        }
        if self.needs_golden && self.action != Action::Enter {
            return Err(IntentValidationError::NeedsGoldenOnNonEnter);
        }
        // ttl_hours is required for prep / veto: the KV flag we write
        // expires on this clock. `Static(0)` is the wire-elided default
        // sentinel meaning "operator didn't set it" — reject. Scripts
        // are accepted unconditionally; the operator who wrote a script
        // meant it, even if it might resolve to 0 at gate time (treated
        // there as "expire immediately").
        if matches!(self.action, Action::Prep | Action::Veto)
            && is_default_ttl_hours(&self.ttl_hours)
        {
            return Err(IntentValidationError::MissingTtlHours);
        }
        Ok(())
    }
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
///
/// `RecentHigh` / `RecentLow` reference Pine's `recent_high` / `recent_low`
/// fields, which span the indicator's `sl_lookback` window of bars
/// *strictly preceding* the signal bar. Useful as an SL anchor that
/// doesn't depend on the signal candle's own wick. If the shell doesn't
/// carry them (older Pine indicator), they fall back to the signal
/// bar's `high` / `low` so behaviour degrades gracefully rather than
/// producing a panic. Pine v2 from 2026-05-26 onwards ships them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceAnchor {
    Close,
    High,
    Low,
    RecentHigh,
    RecentLow,
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
            // Fall back to signal-bar extremes if Pine didn't ship the
            // recent_* field — keeps older indicators usable (with a
            // tighter SL, which is the conservative direction to err in).
            PriceAnchor::RecentHigh => self.recent_high.unwrap_or(self.high),
            PriceAnchor::RecentLow => self.recent_low.unwrap_or(self.low),
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

    #[test]
    fn anchor_price_returns_correct_field() {
        let s = shell();
        assert_eq!(s.anchor_price(PriceAnchor::Close), 1.1000);
        assert_eq!(s.anchor_price(PriceAnchor::High), 1.1020);
        assert_eq!(s.anchor_price(PriceAnchor::Low), 1.0980);
    }

    #[test]
    fn anchor_price_recent_uses_shell_field_when_present() {
        let mut s = shell();
        s.recent_high = Some(1.1050);
        s.recent_low = Some(1.0950);
        assert_eq!(s.anchor_price(PriceAnchor::RecentHigh), 1.1050);
        assert_eq!(s.anchor_price(PriceAnchor::RecentLow), 1.0950);
    }

    #[test]
    fn anchor_price_recent_falls_back_to_bar_extreme_when_missing() {
        // Older Pine indicators don't ship recent_*. We fall back to the
        // signal bar's own high/low so the worker doesn't panic — degrades
        // to a tighter SL, never a looser one.
        let s = shell();
        assert!(s.recent_high.is_none());
        assert!(s.recent_low.is_none());
        assert_eq!(s.anchor_price(PriceAnchor::RecentHigh), s.high);
        assert_eq!(s.anchor_price(PriceAnchor::RecentLow), s.low);
    }

    #[test]
    fn price_anchor_recent_round_trips_through_yaml() {
        let from: PriceAnchor = serde_yaml::from_str("recent_high").unwrap();
        assert_eq!(from, PriceAnchor::RecentHigh);
        let out = serde_yaml::to_string(&PriceAnchor::RecentLow).unwrap();
        assert!(out.contains("recent_low"), "got: {out}");
    }

    #[test]
    fn shell_round_trips_recent_fields() {
        let y = "close: 1.1\nhigh: 1.11\nlow: 1.09\ntime: \"2026-05-26T10:00:00Z\"\nrecent_high: 1.115\nrecent_low: 1.085\n";
        let s: Shell = serde_yaml::from_str(y).unwrap();
        assert_eq!(s.recent_high, Some(1.115));
        assert_eq!(s.recent_low, Some(1.085));
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
    fn emitted_intent_has_no_tildes() {
        // Tildes (YAML null markers) appeared in emitted templates when
        // `Option<T>` fields without `skip_serializing_if` were re-
        // serialised after parsing. They're noisy and make the template
        // body hard for the operator to read. Every `Option<T>` field
        // on `Intent` must skip when None — this test guards against
        // accidentally adding a new optional field without the attr.
        let yaml = "
            v: 1
            id: a-tilde
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
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(
            !back.contains('~'),
            "emitted yaml contains a tilde — a new Option<T> field is \
             missing `skip_serializing_if = \"Option::is_none\"`. \
             Output was:\n{back}"
        );
        assert!(
            !back.contains(": null"),
            "emitted yaml contains an explicit null marker. \
             Output was:\n{back}"
        );
    }

    #[test]
    fn emitted_intent_omits_empty_prep_and_veto_lists() {
        // `requires_preps` / `vetos` skip when empty so a vanilla
        // enter doesn't carry `requires_preps: []` / `vetos: []` lines.
        let yaml = "
            v: 1
            id: a-empty
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
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(!back.contains("requires_preps"));
        assert!(!back.contains("vetos"));
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
        match &intent.cooldown_hours {
            Some(crate::tunable::Tunable::Static(n)) => assert_eq!(*n, 12),
            other => panic!("expected Static(12) cooldown_hours, got {other:?}"),
        }
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
        match &intent.ttl_hours {
            crate::tunable::Tunable::Static(n) => assert_eq!(*n, 4),
            other => panic!("expected Static(4) ttl_hours, got {other:?}"),
        }
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
        match &intent.ttl_hours {
            crate::tunable::Tunable::Static(n) => assert_eq!(*n, 6),
            other => panic!("expected Static(6) ttl_hours, got {other:?}"),
        }
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
    fn intent_defaults_trade_id_to_none() {
        // Pre-existing intents on the wire don't carry `trade_id`; the
        // field must be back-compatible.
        let yaml = "
            v: 1
            id: t-1
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
        assert_eq!(intent.trade_id, None);
        intent.validate().unwrap();
    }

    #[test]
    fn intent_parses_explicit_trade_id() {
        let yaml = "
            v: 1
            id: t-2
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
            trade_id: eurusd-short-01jb2x
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.trade_id.as_deref(), Some("eurusd-short-01jb2x"));
        intent.validate().unwrap();
    }

    #[test]
    fn intent_trade_id_round_trips_through_yaml() {
        let yaml = "
            v: 1
            id: t-3
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            trade_id: usdjpy-long-abc123
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("trade_id: usdjpy-long-abc123"));
    }

    #[test]
    fn intent_omits_trade_id_when_none() {
        // Mirror of the `account` skip — keeps the wire form unchanged
        // for pre-trade-grouping intents, so signed-mode replay paths
        // see exactly the same bytes either side.
        let yaml = "
            v: 1
            id: t-4
            not_after: \"2026-05-13T20:00:00Z\"
            action: status
            instrument: ALL
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(!back.contains("trade_id"));
    }

    #[test]
    fn is_valid_trade_id_accepts_typical_slugs() {
        assert!(is_valid_trade_id("eurusd-short-01jb2x"));
        assert!(is_valid_trade_id("a"));
        assert!(is_valid_trade_id("usdjpy-long-abc123"));
        assert!(is_valid_trade_id("h-and-s-1"));
    }

    #[test]
    fn is_valid_trade_id_rejects_bad_shapes() {
        assert!(!is_valid_trade_id("")); // empty
        assert!(!is_valid_trade_id("-leading")); // leading hyphen
        assert!(!is_valid_trade_id("trailing-")); // trailing hyphen
        assert!(!is_valid_trade_id("double--hyphen")); // consecutive
        assert!(!is_valid_trade_id("UPPER")); // uppercase
        assert!(!is_valid_trade_id("has space")); // whitespace
        assert!(!is_valid_trade_id("with_underscore")); // underscore
        assert!(!is_valid_trade_id("dot.separator"));
        // 65 chars — one past the limit
        assert!(!is_valid_trade_id(&"a".repeat(65)));
        // 64 chars — at the boundary, allowed
        assert!(is_valid_trade_id(&"a".repeat(64)));
    }

    #[test]
    fn intent_validate_rejects_bad_trade_id() {
        // serde will happily parse any string into trade_id; the
        // separate validate() step catches shape violations so junk
        // ids don't end up in the seen-index.
        let yaml = "
            v: 1
            id: t-bad
            not_after: \"2026-05-13T20:00:00Z\"
            action: status
            instrument: ALL
            trade_id: \"Bad Trade Id!\"
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::InvalidTradeId)
        );
    }

    #[test]
    fn intent_defaults_max_retries_to_static_zero() {
        let yaml = "
            v: 1
            id: mr-1
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
        assert_eq!(intent.max_retries, crate::tunable::Tunable::Static(0));
    }

    #[test]
    fn intent_max_retries_round_trips_through_yaml() {
        let yaml = "
            v: 1
            id: mr-2
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            trade_id: eurusd-long-mr2
            max_retries: 3
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        match &intent.max_retries {
            crate::tunable::Tunable::Static(n) => assert_eq!(*n, 3),
            other => panic!("expected Static(3) max_retries, got {other:?}"),
        }
        intent.validate().unwrap();
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("max_retries: 3"));
    }

    #[test]
    fn intent_omits_max_retries_when_default() {
        // skip_serializing_if guard — keeps the wire form unchanged for
        // single-shot intents minted before this field landed.
        let yaml = "
            v: 1
            id: mr-3
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
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(!back.contains("max_retries"));
    }

    #[test]
    fn validate_accepts_zero_max_retries_as_default() {
        // Static(0) is the default single-shot value — must validate
        // cleanly regardless of trade_id / action since it's the
        // wire-equivalent of "no retries opted in".
        let yaml = "
            v: 1
            id: mr-zero
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            max_retries: 0
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().unwrap();
    }

    #[test]
    fn validate_rejects_max_retries_without_trade_id() {
        let yaml = "
            v: 1
            id: mr-no-tid
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            max_retries: 2
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MaxRetriesWithoutTradeId)
        );
    }

    #[test]
    fn validate_rejects_max_retries_on_non_enter_action() {
        let yaml = "
            v: 1
            id: mr-prep
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
            trade_id: eurusd-long-mrp
            max_retries: 2
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MaxRetriesOnNonEnter)
        );
    }

    #[test]
    fn validate_rejects_missing_ttl_hours_on_prep() {
        // Prep without ttl_hours: the field defaults to Static(0) which
        // is the wire-elided sentinel meaning "not set" — required on
        // prep/veto where the KV flag needs a lifetime.
        let yaml = "
            v: 1
            id: prep-no-ttl
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingTtlHours)
        );
    }

    #[test]
    fn validate_rejects_missing_ttl_hours_on_veto() {
        let yaml = "
            v: 1
            id: veto-no-ttl
            not_after: \"2026-05-13T20:00:00Z\"
            action: veto
            instrument: EUR_USD
            name: news-window
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingTtlHours)
        );
    }

    #[test]
    fn validate_accepts_script_ttl_hours_on_prep() {
        // A script is "opted in" by definition — we can't know what it
        // resolves to, but the operator wrote it intentionally. Treat
        // it as not-default.
        let yaml = "
            v: 1
            id: prep-script-ttl
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: !script \"if golden == true { 6 } else { 4 }\"
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().unwrap();
    }

    #[test]
    fn validate_accepts_missing_ttl_hours_on_enter() {
        // Enter / status / unlock / invalidate / clear-* don't need
        // ttl_hours — the field is meaningful only for prep/veto.
        let yaml = "
            v: 1
            id: enter-no-ttl
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
        intent.validate().unwrap();
    }

    #[test]
    fn intent_defaults_allow_entry_to_none() {
        let yaml = "
            v: 1
            id: ae-1
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
        assert!(intent.allow_entry.is_none());
        intent.validate().unwrap();
    }

    #[test]
    fn intent_parses_allow_entry_static_true() {
        let yaml = "
            v: 1
            id: ae-2
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            allow_entry: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        match intent.allow_entry {
            Some(crate::tunable::Tunable::Static(v)) => assert!(v),
            other => panic!("expected Static(true), got {other:?}"),
        }
        intent.validate().unwrap();
    }

    #[test]
    fn intent_parses_allow_entry_script() {
        let yaml = "
            v: 1
            id: ae-3
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            allow_entry: !script \"signal_confirmed == true\"
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        match &intent.allow_entry {
            Some(crate::tunable::Tunable::Script(s)) => {
                assert_eq!(s.source, "signal_confirmed == true");
            }
            other => panic!("expected Script, got {other:?}"),
        }
        intent.validate().unwrap();
    }

    #[test]
    fn intent_allow_entry_round_trips_through_yaml() {
        let yaml = "
            v: 1
            id: ae-4
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            allow_entry: !script \"signal_confirmed == true\"
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("!script"), "got:\n{back}");
        assert!(back.contains("signal_confirmed == true"), "got:\n{back}");
        // Re-parse the emitted form to confirm full round-trip.
        let again: Intent = serde_yaml::from_str(&back).unwrap();
        assert_eq!(intent.allow_entry, again.allow_entry);
    }

    #[test]
    fn intent_omits_allow_entry_when_none() {
        // skip_serializing_if guard — pre-allow_entry intents must
        // serialise byte-identical to before this field landed.
        let yaml = "
            v: 1
            id: ae-5
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
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(!back.contains("allow_entry"), "got:\n{back}");
    }

    #[test]
    fn intent_needs_golden_round_trips_through_yaml() {
        let yaml = "
            v: 1
            id: ng-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            needs_golden: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(intent.needs_golden);
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("needs_golden: true"), "got: {back}");
    }

    #[test]
    fn intent_defaults_needs_golden_to_false_and_omits_on_serialise() {
        // Wire-compat: pre-feature intents have no needs_golden field.
        // Absent → false (single-source-of-truth default), and serialising
        // a false value back doesn't introduce the field — that keeps
        // already-signed intents byte-identical.
        let yaml = "
            v: 1
            id: ng-default
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
        assert!(!intent.needs_golden);
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(!back.contains("needs_golden"), "got: {back}");
    }

    #[test]
    fn validate_rejects_needs_golden_on_non_enter_action() {
        // Same defense-in-depth as allow_entry / max_retries — the gate
        // only runs on Enter, so silently ignoring it on a prep would
        // mislead the operator.
        let yaml = "
            v: 1
            id: ng-prep
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
            needs_golden: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::NeedsGoldenOnNonEnter)
        );
    }

    #[test]
    fn validate_rejects_allow_entry_on_non_enter_action() {
        // Same defense-in-depth as max_retries — the gate only runs on
        // the Enter path so allowing it on prep / veto would be silently
        // ignored at the worker. Better to reject at validate time.
        let yaml = "
            v: 1
            id: ae-prep
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
            allow_entry: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::AllowEntryOnNonEnter)
        );
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
