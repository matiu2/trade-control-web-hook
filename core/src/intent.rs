//! The trade intent (decrypted JSON) and the plaintext shell (TradingView-substituted
//! prices), plus the logic that merges the two into a `Resolved` intent ready for
//! risk-gating and order placement.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

mod blackout;
mod entry_level_veto;
mod expiry;
mod mw_resolution;
mod mw_state;
mod resolution;
mod sl_spread_floor;

pub use blackout::{
    BlackoutCloseAction, Buffers, MINUTES_PER_DAY, NoEntryWindow, is_inside_any, is_inside_window,
    windows_from_session,
};
pub use entry_level_veto::{EntryLevelVeto, VetoSide};
pub use expiry::{ExpiryError, MAX_EXPIRY_BARS, resolve_cancel_at};
pub use mw_resolution::mw_static_prices;
pub use mw_state::{MwAnchors, MwUpdate, effective_mw_params, plan_mw_update};
#[cfg(feature = "cli")]
pub use resolution::MIN_R_FLOOR;
pub use resolution::{ResolveError, Resolved, ResolvedEntry, ResolvedRecoverEntry, RiskBudget};
pub use sl_spread_floor::{SL_MIN_SPREAD_MULTIPLE, sl_spread_floor_violation};

/// Plaintext outer YAML — the part TradingView substitutes `{{...}}` into.
/// The intent fields sit alongside these at the top level of the signed
/// body; the HMAC over the whole thing lives in a separate `sig` field
/// (not modelled here — handled raw in `core::sig`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Shell {
    pub close: f64,
    pub high: f64,
    pub low: f64,
    /// Bar **open** — added 2026-06 for M/W body-extreme logic (rogue-wick
    /// handling and dynamic neckline revision read `max(open,close)` /
    /// `min(open,close)`, not the wick high/low). Optional: control-action
    /// shells don't carry it, and charts armed under a pre-`open` Pine send
    /// no `open` field — body-based logic must fall back gracefully (treat
    /// `None` as "can't compute bodies this bar"). TV built-in `{{open}}`,
    /// so no plot-index risk. Value is in [`crate::sig::UNSIGNED_VALUE_KEYS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<f64>,
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
    /// Forward-projected close timestamps of the next 1..=5 bars, filled
    /// by Pine at fire-time via `time_close(timeframe.period, bars_back=-k)`.
    /// These respect the symbol's session calendar — so "the bar 3 ahead"
    /// of a Friday-close correctly lands on the next session open, not
    /// inside the weekend. The worker indexes this menu with the signed
    /// `Intent::expiry_bars` to derive a pending-order `cancel_at`. Pine
    /// ships each as a millisecond Unix-epoch integer (see
    /// `signal_time_serde`), or `na`/absent on non-time-based charts (in
    /// which case expiry falls back to `not_after`). Optional throughout —
    /// only Pine-bound enter alerts carry them.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "signal_time_serde"
    )]
    pub next_candle_timestamp_1: Option<DateTime<Utc>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "signal_time_serde"
    )]
    pub next_candle_timestamp_2: Option<DateTime<Utc>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "signal_time_serde"
    )]
    pub next_candle_timestamp_3: Option<DateTime<Utc>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "signal_time_serde"
    )]
    pub next_candle_timestamp_4: Option<DateTime<Utc>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "signal_time_serde"
    )]
    pub next_candle_timestamp_5: Option<DateTime<Utc>>,
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

/// Static M / W (double-top / double-bottom) parameters baked into a
/// signed `enter` intent at arm time by `tv-arm`.
///
/// The load-bearing reason this lives on the intent (rather than being
/// computed chart-side): the Pine study is attached to the chart/view,
/// not to a specific path drawing, so it can't take per-trade anchors
/// as inputs. `tv-arm` reads the 3 path anchors and the broker spread
/// at arm time and bakes them here; the worker combines these static
/// params with the live shell OHLC (every-bar-close) to compute the
/// stop-entry / SL / TP and place the order. See
/// `core/src/intent/mw_resolution.rs` for the mid→bid/ask formulas.
///
/// All three anchor prices are **MID** prices, exactly as read off the
/// chart. The spread correction happens in resolution, not here.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct MwParams {
    /// `C` — the neckline. Entry trigger anchor and abort level (mid).
    pub neckline: f64,
    /// `B` — the first peak (M) / first trough (W). The SL anchor base
    /// (mid): SL sits one pip + spread beyond it.
    pub first_point: f64,
    /// `A` — the runup start. Audit / log only; the worker doesn't use
    /// it for entry geometry (it fed the arm-time neckline-% gate).
    pub runup_start: f64,
    /// `D` — the **right shoulder**, when the operator drew a 4-point M/W
    /// path (the optional 4th anchor). MID price, on the same side of the
    /// neckline as `first_point` (above for an M, below for a W).
    ///
    /// When set, the second tower is already drawn, so the setup is
    /// **armed immediately** — the worker skips the live right-tower-reach
    /// and 50%-mid-cross gates (a 3-point path has to discover the right
    /// tower bar by bar; a 4-point path declares it). The arming math then
    /// keys the SL anchor / mid references off the **higher** of the two
    /// shoulders. `None` for a classic 3-point path (unchanged behaviour).
    /// `#[serde(default)]` keeps every in-flight 3-point signed intent
    /// byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right_shoulder: Option<f64>,
    /// Broker spread in pips, read at arm time. The worker has no live
    /// spread at entry, so this baked value drives the mid→bid/ask
    /// correction on every level. `>= 0`.
    pub spread_pips: f64,
    /// Instrument pip size at arm time (e.g. `0.0001` for EUR/USD,
    /// `0.01` for JPY pairs). `> 0`.
    pub pip_size: f64,
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
    /// Continuous **at-entry level vetos** (Bug #12). Each names a veto, a
    /// price level, and which side counts as "past". `run_enter` rejects the
    /// entry when the resolved entry/trigger price is already past the level
    /// — independent of whether any cross-event guard fired or wrote a KV
    /// veto. Restores the legacy behaviour where a persistent `too-low` /
    /// `too-high` KV veto blocked a confirmed enter. `#[serde(default)]`
    /// keeps every in-flight signed intent / stored plan deserialising
    /// unchanged. The `vetos` name-list above is left untouched and still
    /// gates any externally/guard-set KV veto.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry_level_vetos: Vec<EntryLevelVeto>,
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
    /// **What "retries" means here.** This is *not* a retry of
    /// placements that failed to reach the broker — those produce a
    /// terminal 502 and don't refire. It models the case where the
    /// first entry placed and filled, the trade has since closed
    /// (typically at stop loss), and a fresh signal bar inside the
    /// same alert window represents a new entry opportunity on the
    /// same setup. The gate that enforces this lives in
    /// `src/retry_gate.rs` — see that module's docs for the full
    /// rules, especially around what kinds of prior attempts allow
    /// another placement vs. block one.
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
    /// Optional bar-based expiry for a pending `enter` order. When set
    /// (1..=5), the worker derives a `cancel_at` for the resting order
    /// by indexing the shell's `next_candle_timestamp_1..5` menu (which
    /// Pine fills with session-calendar-aware forward bar-close times),
    /// capped at [`Self::not_after`]. The cron sweep cancels the order
    /// once `cancel_at` passes — so a breakout-stop that never fills is
    /// pulled within N bars instead of resting until the alert window
    /// closes. Out-of-range values (0, or > the 5-slot menu) are
    /// rejected at dispatch. Absent = today's behaviour (rest until
    /// `not_after`).
    ///
    /// A [`Tunable<u32>`] — the author picks N at arm time (static
    /// literal `expiry_bars: 3`, or a Rhai `!script` resolved against
    /// Phase 1 shell-anchor scope). The *absolute* timestamps come from
    /// Pine at fire-time, not from this field — this field only selects
    /// which menu slot to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_bars: Option<crate::tunable::Tunable<u32>>,
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
    /// Optional Rhai gate on `close`, symmetric with [`Self::allow_entry`].
    /// When set, the worker resolves the [`Tunable<bool>`] (Static or
    /// `!script`) **after** the contextual-window gate ([`Self::inside_window`])
    /// and the candle-quality gates ([`Self::needs_golden`] /
    /// [`Self::needs_confirmed`]); a `false` evaluation rejects the close
    /// with 412. Semantics are AND with every other Close gate — `allow_close`
    /// can only *tighten* the dispatch, never widen it.
    ///
    /// Resolved against the shell-anchor scope only (no resolved geometry
    /// — closes don't compute SL/TP). Scripts can reference any field
    /// bound by `crate::rules::bind_shell_anchors` plus the `pct` / `pips`
    /// helpers. Returning non-bool is a 412.
    ///
    /// Default-absent = unconditional allow; byte-identical wire form to
    /// pre-feature intents. Validated to be Close-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_close: Option<crate::tunable::Tunable<bool>>,
    /// When true, the worker rejects the action unless the incoming shell
    /// carries `golden: Some(true)`. AND-composed with [`Self::allow_entry`]
    /// (Enter) / [`Self::allow_close`] (Close) — both gates must pass.
    /// Promoted to a typed field (rather than a script idiom like
    /// `golden == true`) because operators reach for it often; the typed
    /// form avoids the Rhai `()` landmine when the shell omits the field.
    /// Default `false` = no gate, byte-identical wire form to pre-feature
    /// intents.
    ///
    /// Meaningful on `Action::Enter` and `Action::Close`; rejected at
    /// validate time on other actions. On Close the typical use is the
    /// consolidated reversal-close path: the operator wants the trade
    /// flattened only when the reversal candle is golden.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_golden: bool,
    /// When true, the worker rejects the action unless the incoming shell
    /// carries `confirmed: Some(true)`. Same shape as [`Self::needs_golden`]
    /// — typed boolean, AND-composed with the action's script gate,
    /// rejected on actions other than Enter|Close. Promoted out of
    /// script-land (`confirmed == true` inside `allow_entry`) so the
    /// operator-facing YAML reads cleaner.
    ///
    /// On Close: pairs with the consolidated reversal-close to express
    /// "confirmed reversal — golden is too strict for this setup". A
    /// reversal close that sets *neither* `needs_golden` nor
    /// `needs_confirmed` lets through any opposite-pattern candle (rare —
    /// usually the operator wants at least one).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_confirmed: bool,
    /// Per-blackout id on `pause` / `resume` actions. Lets a single
    /// `trade_id` carry multiple independent blackout windows
    /// concurrently (e.g. NFP + central-bank decision on the same
    /// trade): the pair-start sets `pause:<trade_id>:<blackout_id>`
    /// and only its matching `resume` clears that one. Must be a
    /// valid `trade_id`-shaped slug. Required on `pause` / `resume`;
    /// ignored on other actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blackout_id: Option<String>,
    /// Per-window id on `news-start` / `news-end` actions. Same role
    /// as `blackout_id` but for the independent news-window
    /// namespace: the pair-start sets `news:<trade_id>:<news_id>`
    /// and only its matching `news-end` clears that one. Slug-shaped
    /// (same rules as `trade_id`). Required on `news-start` /
    /// `news-end`; ignored on other actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub news_id: Option<String>,
    /// **Deprecated** — superseded by [`Self::inside_window`]. New trade
    /// templates should emit `inside_window: [news, ...]` instead.
    ///
    /// On `close` intents, one of two **OR-composed** gates. When
    /// `Some(true)`, the gate passes only if
    /// `list_news_windows_for_trade(trade_id)` returns at least one
    /// active window. If this is the **only** gate set the close is
    /// rejected with 423 when no window is active. When combined with
    /// [`Self::require_price_in_ranges`] the close still reaches the
    /// broker as long as *either* gate passes — the worker uses both
    /// as parallel "is this candle a real reversal" tests, not as
    /// must-both-hold preconditions. Default-absent = no news-window
    /// requirement (gate skipped). Only meaningful on `Action::Close`;
    /// rejected at validate time on other actions.
    ///
    /// Wire-compat: kept working for in-flight alerts. Mutually exclusive
    /// with [`Self::inside_window`] — validate rejects an intent that
    /// sets both forms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_news_window: Option<bool>,
    /// **Deprecated** — superseded by [`Self::inside_window`] +
    /// [`Self::sr_bands`]. New trade templates should emit
    /// `inside_window: [..., price]` + `sr_bands: [[lo, hi], ...]`
    /// instead.
    ///
    /// On `close` intents, one of two **OR-composed** gates (see
    /// [`Self::require_news_window`] for the other). The worker
    /// fetches the broker's current price at dispatch; this gate
    /// passes when the price sits inside at least one `[lo, hi]`
    /// band. If this is the **only** gate set the close is rejected
    /// with 423 when the price is outside every band. When combined
    /// with `require_news_window`, the close succeeds as long as
    /// *either* gate passes. Default-absent = no range gate. Only
    /// meaningful on `Action::Close`; rejected at validate time on
    /// other actions.
    ///
    /// Wire-compat: kept working for in-flight alerts. Mutually exclusive
    /// with [`Self::inside_window`] — validate rejects an intent that
    /// sets both forms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_price_in_ranges: Option<Vec<[f64; 2]>>,
    /// On `close` intents, the consolidated contextual-window gate.
    /// **OR-composed** list — the close reaches the broker as long as
    /// *at least one* listed window passes:
    /// - [`EventWindow::News`] — active news window for `trade_id`
    ///   (`list_news_windows_for_trade(trade_id)` returns non-empty).
    /// - [`EventWindow::Price`] — broker's current price for
    ///   `instrument` sits inside at least one [`Self::sr_bands`]
    ///   entry.
    ///
    /// Replaces the deprecated [`Self::require_news_window`] +
    /// [`Self::require_price_in_ranges`] pair with a single explicit
    /// list. The two-axis metaphor is the design intent: news is a
    /// *time* window, price is a *price* window; either kind of
    /// "we're inside something meaningful" is enough to justify the
    /// close. See [`EventWindow`] for details.
    ///
    /// Empty list = no contextual gate (unconditional close, same as
    /// omitting both deprecated fields). Operators almost always want
    /// at least one window — see the README for the reversal-close
    /// template.
    ///
    /// Validation:
    /// - Only meaningful on `Action::Close`; rejected elsewhere.
    /// - Mutually exclusive with the deprecated `require_*` fields.
    /// - If `Price` is listed, [`Self::sr_bands`] must be non-empty.
    /// - If `Price` is *not* listed, `sr_bands` must be empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inside_window: Vec<EventWindow>,
    /// Support/resistance price bands `[[lo, hi], ...]` for the
    /// [`EventWindow::Price`] gate. Required when `inside_window`
    /// contains `Price`; rejected when it doesn't. Every band must
    /// satisfy `lo <= hi`. Replaces the deprecated
    /// [`Self::require_price_in_ranges`]; the data shape is identical
    /// so the worker's existing band-hit machinery is reused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sr_bands: Vec<[f64; 2]>,
    /// **Experimental, default OFF.** On a reversal-close (`Action::Close`
    /// carrying a `Price` window — i.e. `inside_window` contains `Price`,
    /// or the deprecated `require_price_in_ranges` is set), also write a
    /// `reversal` veto for this `trade_id` when the close gate passes.
    ///
    /// The motivating case: a reversal off support/resistance that lands
    /// *before* the entry fires. Today the reversal-close just flattens an
    /// open position; if no position is open yet it's a no-op and the
    /// later `enter` goes in anyway — even though the reversal was a strong
    /// "this trade won't work" signal (see the 2026-06 incident note in
    /// the README). With this flag, the same gate-pass also records a
    /// `reversal` veto, so the later `enter` for this `trade_id` is
    /// rejected by the existing [`StateStore::is_vetoed`] gate.
    ///
    /// Semantics are **StopNextEntry-style**: the veto only blocks future
    /// entries. It never force-closes a position beyond the close this
    /// intent already performs — consistent with the rule that an
    /// entry-gate veto must not close a trade. The veto is written on
    /// **every** gate-pass (idempotent key, TTL refreshed); post-entry it
    /// harmlessly prevents a re-entry for the rest of the window.
    ///
    /// Validation: only legal on `Action::Close`, and only when a price
    /// window is actually configured (otherwise there's no reversal to
    /// veto on). Default `false` = byte-identical to pre-feature wire.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub veto_on_reversal: bool,
    /// Free-form human-readable label for a `pause` (or any other
    /// action that wants to record context). Surfaces in the seen
    /// index outcome string so operators can answer "why is this
    /// trade paused?" without cross-referencing chart drawings.
    /// Optional; bounded length is enforced on the wire by the
    /// general YAML body cap, not here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// M / W (double-top / double-bottom) static parameters, baked by
    /// `tv-arm` at arm time. Present only on M/W `enter` intents; the
    /// worker's resolution path takes a dedicated branch when this is
    /// `Some(_)`, deriving stop-entry / SL / TP from these params + the
    /// live shell OHLC instead of reading `entry` / `stop_loss` /
    /// `take_profit` (which are absent for M/W). See [`MwParams`].
    ///
    /// Default-absent = a non-M/W intent; byte-identical wire form to
    /// pre-feature intents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mw: Option<MwParams>,
    /// Instrument pip size, baked by `tv-arm`/`trade-control` at arm time
    /// from `instrument-lookup` (`asset.pip_size`): `0.0001` for major FX,
    /// `0.01` for JPY pairs, `1.0` for indices, etc. When present, the
    /// worker uses this value to scale every `offset_pips` into a price
    /// (entry/SL/TP) and binds it into the gate-script scope, instead of
    /// reading the per-instrument `PIP_SIZE_<instrument>` secret. Absent =
    /// the worker falls back to that secret, then the forex default —
    /// keeping the wire form byte-identical to pre-feature intents.
    ///
    /// For M/W intents the same value is also carried in
    /// [`MwParams::pip_size`] (the M/W resolution reads that copy directly);
    /// `tv-arm` sets both to the same number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pip_size: Option<f64>,
    /// Server-side [`TradePlan`](crate::trade_plan::TradePlan) — present only
    /// on an [`Action::Register`] intent, absent on every other action. The
    /// engine reads this, persists the plan, and from then on evaluates the
    /// plan's conditions itself on each cron tick (replacing the per-condition
    /// TradingView alerts). Signed as part of the whole-body HMAC like every
    /// other field, so the plan can't be tampered in flight.
    ///
    /// Default-absent = a non-register intent; byte-identical wire form to
    /// pre-feature intents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trade_plan: Option<crate::trade_plan::TradePlan>,
    /// Market-hours entry blackout policy: what to do with this trade's
    /// **resting order** when the daily blackout window opens (see
    /// [`BlackoutCloseAction`]). The single field both the webhook enter path
    /// and the engine's fired enter carry, so one reject gate + sweep branch
    /// honours it regardless of which path placed the order.
    ///
    /// Defaults to [`BlackoutCloseAction::CancelResting`] — cancel the unfilled
    /// order, leave any filled position alone (the incident fix). Signed as
    /// part of the whole-body HMAC; `#[serde(default)]` + a skip predicate keep
    /// the wire form byte-identical to pre-feature intents when it's the
    /// default. Meaningful on `Action::Enter`; ignored on other actions.
    #[serde(default, skip_serializing_if = "is_default_blackout_close")]
    pub blackout_close: BlackoutCloseAction,
    /// `plan list --include-all` flag: when true, the worker also enumerates the
    /// archived (terminated) plans alongside the live ones. Meaningful only on
    /// [`Action::PlanList`]; ignored elsewhere. Signed as part of the whole-body
    /// HMAC; `#[serde(default)]` + skip-if-false keep the wire form
    /// byte-identical to pre-feature `plan-list` intents.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_archived: bool,
}

/// Skip-serializing predicate for [`Intent::blackout_close`]. Returns true on
/// the default `CancelResting` so the wire form stays byte-identical to intents
/// minted before the field existed.
fn is_default_blackout_close(a: &BlackoutCloseAction) -> bool {
    matches!(a, BlackoutCloseAction::CancelResting)
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

/// Fixed veto name written by the worker's [`Intent::veto_on_reversal`]
/// hook and checked by the matching `enter` (whose `vetos` list must
/// include it for the veto to gate the entry). Single source of truth so
/// the worker's write side and the CLI's enter-builder can't drift apart.
pub const REVERSAL_VETO_NAME: &str = "reversal";

/// Fixed veto name for an M/W pattern cancellation. Written by the worker
/// when the live geometry breaches the 60% validity floor (a body too deep
/// into the runup) and by the chart-side `01-veto-mw-cancel` alert at the
/// 1.3 extension. The M/W `05-enter` lists this in its `vetos`, so either
/// write blocks future entries (StopNextEntry / CancelPending — never
/// closes an open position). Single source of truth so the worker write
/// side and the CLI enter-builder can't drift apart.
pub const MW_CANCEL_VETO_NAME: &str = "mw-cancel";

/// Fixed veto name for an M/W pattern overshoot. Written by the chart-side
/// `01-veto-mw-overshoot` alert when price runs 180% of the top→neckline
/// leg (the projected move is essentially complete; a fresh entry's R:R no
/// longer justifies opening). The M/W `05-enter` lists this in its `vetos`,
/// so the alert blocks future entries (CancelPending — never closes an open
/// position). Single source of truth so the chart write side and the CLI
/// enter-builder can't drift apart.
pub const MW_OVERSHOOT_VETO_NAME: &str = "mw-overshoot";

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
    /// `allow_close: Some(_)` on a non-Close action — the gate is
    /// only checked on `close`. Symmetric with [`Self::AllowEntryOnNonEnter`].
    AllowCloseOnNonClose,
    /// `needs_golden: true` on an action other than Enter or Close — the
    /// gate only runs on those two paths. Promoted from "Enter-only" on
    /// 2026-06-03 so the consolidated reversal close can require a
    /// golden candle.
    NeedsGoldenOnDisallowedAction,
    /// `needs_confirmed: true` on an action other than Enter or Close.
    /// Symmetric with [`Self::NeedsGoldenOnDisallowedAction`].
    NeedsConfirmedOnDisallowedAction,
    /// `inside_window: [...]` on a non-Close action — the gate is only
    /// checked on `close`.
    InsideWindowOnNonClose,
    /// `inside_window` listed `price` but [`Intent::sr_bands`] was
    /// empty, OR `sr_bands` was non-empty but `inside_window` did
    /// not list `price`. The two fields are the type-tag and the data
    /// for the same gate; either both present or both absent.
    InsideWindowSrBandsMismatch,
    /// One of the bands in [`Intent::sr_bands`] has `lo > hi`.
    InvalidSrBands,
    /// `veto_on_reversal: true` on an action other than `Close` — the
    /// veto-on-reversal hook only fires from a reversal-close.
    VetoOnReversalOnNonClose,
    /// `veto_on_reversal: true` on a `Close` that has no price window
    /// configured (`inside_window` lacks `Price` and
    /// `require_price_in_ranges` is unset) — there's no reversal band to
    /// hang the veto on, so the flag would never do anything.
    VetoOnReversalWithoutPriceWindow,
    /// New consolidated close-gate fields ([`Intent::inside_window`] or
    /// [`Intent::sr_bands`]) were set alongside the deprecated
    /// [`Intent::require_news_window`] or [`Intent::require_price_in_ranges`].
    /// Pick one form per intent — the worker accepts either independently
    /// but mixing them invites silent semantic drift.
    MixedOldAndNewCloseGates,
    /// `ttl_hours` missing (i.e. defaulted to `Static(0)`) on a prep or
    /// veto action where it's required to set the KV flag's lifetime.
    MissingTtlHours,
    /// `pause` / `resume` requires both `trade_id` (the parent setup)
    /// and `blackout_id` (this specific window). Without `trade_id`
    /// the worker can't key the KV entry; without `blackout_id` a
    /// resume couldn't tell sibling blackouts apart.
    MissingPauseFields,
    /// `blackout_id` is shaped like a `trade_id` slug (lowercase
    /// alphanumerics + hyphens) — reuses [`is_valid_trade_id`].
    InvalidBlackoutId,
    /// `news-start` / `news-end` requires both `trade_id` (the parent
    /// setup) and `news_id` (this specific window). Without `trade_id`
    /// the worker can't key the KV entry; without `news_id` a
    /// `news-end` couldn't tell sibling windows apart.
    MissingNewsFields,
    /// `news_id` is shaped like a `trade_id` slug — reuses
    /// [`is_valid_trade_id`].
    InvalidNewsId,
    /// `require_news_window: Some(_)` on a non-Close action — the gate
    /// is only checked on `close`.
    RequireNewsWindowOnNonClose,
    /// `require_price_in_ranges: Some(_)` on a non-Close action — the
    /// gate is only checked on `close`.
    RequirePriceInRangesOnNonClose,
    /// `require_price_in_ranges: Some(ranges)` is empty or contains a
    /// band where `lo > hi`. An empty list means "never matches" and
    /// is almost certainly a builder bug; a flipped band is the same.
    InvalidPriceRanges,
    /// `prep-expire` is missing `step` — the worker can't tell which
    /// prep to block without it.
    MissingPrepExpireStep,
    /// `mw: Some(_)` on a non-Enter action — the M/W static params only
    /// drive the worker's stop-entry geometry, which is an `enter`
    /// concept. A veto/prep/close carrying `mw` is a builder bug.
    MwOnNonEnter,
    /// One of the [`MwParams`] fields failed its shape contract: an
    /// anchor price was non-finite, `spread_pips` was negative or
    /// non-finite, or `pip_size` was non-positive or non-finite.
    MwFieldInvalid,
    /// The top-level `pip_size` was present but non-positive or
    /// non-finite. A baked pip drives every `offset_pips`→price scale, so
    /// a zero/negative/NaN value would silently zero or invert the trade
    /// geometry.
    PipSizeInvalid,
    /// `enter` / `veto` / `clear-veto` reached validation without a
    /// `trade_id`. The veto KV key is
    /// `veto:<account>:<trade_id>:<instrument>:<name>` — without a
    /// `trade_id` a veto would either bleed across every setup on the
    /// instrument (the 2026-06-11 bug) or fail to match the entry it's
    /// meant to block. Every veto and every entry carries one.
    MissingTradeId,
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
            Self::AllowCloseOnNonClose => f.write_str("allow_close is only valid on action: close"),
            Self::NeedsGoldenOnDisallowedAction => {
                f.write_str("needs_golden is only valid on action: enter | close")
            }
            Self::NeedsConfirmedOnDisallowedAction => {
                f.write_str("needs_confirmed is only valid on action: enter | close")
            }
            Self::InsideWindowOnNonClose => {
                f.write_str("inside_window is only valid on action: close")
            }
            Self::InsideWindowSrBandsMismatch => {
                f.write_str("inside_window must contain `price` iff sr_bands is non-empty")
            }
            Self::InvalidSrBands => f.write_str("sr_bands must have lo <= hi on every band"),
            Self::VetoOnReversalOnNonClose => {
                f.write_str("veto_on_reversal is only valid on action: close")
            }
            Self::VetoOnReversalWithoutPriceWindow => f.write_str(
                "veto_on_reversal requires a price window (inside_window: [..., price] + sr_bands, \
                 or require_price_in_ranges)",
            ),
            Self::MixedOldAndNewCloseGates => f.write_str(
                "the new close-gate fields (inside_window / sr_bands) cannot be mixed with \
                 the deprecated require_news_window / require_price_in_ranges — pick one form",
            ),
            Self::MissingTtlHours => f.write_str("ttl_hours is required on prep / veto actions"),
            Self::MissingPauseFields => {
                f.write_str("pause / resume require both trade_id and blackout_id")
            }
            Self::InvalidBlackoutId => f.write_str(
                "invalid blackout_id (same shape as trade_id: 1-64 chars of lowercase \
                 alphanumerics + hyphens, no leading/trailing or consecutive hyphens)",
            ),
            Self::MissingNewsFields => {
                f.write_str("news-start / news-end require both trade_id and news_id")
            }
            Self::InvalidNewsId => f.write_str(
                "invalid news_id (same shape as trade_id: 1-64 chars of lowercase \
                 alphanumerics + hyphens, no leading/trailing or consecutive hyphens)",
            ),
            Self::RequireNewsWindowOnNonClose => {
                f.write_str("require_news_window is only valid on action: close")
            }
            Self::RequirePriceInRangesOnNonClose => {
                f.write_str("require_price_in_ranges is only valid on action: close")
            }
            Self::InvalidPriceRanges => f.write_str(
                "require_price_in_ranges must be non-empty and every band must have lo <= hi",
            ),
            Self::MissingPrepExpireStep => {
                f.write_str("prep-expire requires `step` (which prep to block)")
            }
            Self::MwOnNonEnter => f.write_str("mw params are only valid on action: enter"),
            Self::MwFieldInvalid => f.write_str(
                "mw params invalid: anchor prices must be finite, spread_pips >= 0, pip_size > 0",
            ),
            Self::PipSizeInvalid => f.write_str("pip_size must be finite and > 0"),
            Self::MissingTradeId => {
                f.write_str("trade_id is required on action: enter | veto | clear-veto")
            }
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
        // Every veto-touching action must carry a trade_id: the veto KV
        // key is scoped per-setup (`veto:<account>:<trade_id>:<instrument>:
        // <name>`) so a veto from one setup can't bleed into another on
        // the same instrument. Enter is included because the entry gate
        // looks vetos up by the entry's own trade_id — an untagged entry
        // could never match a (correctly tagged) veto. See the 2026-06-11
        // cross-trade veto-bleed fix.
        // `PlanDelete` joins the trade-id-required set for a different reason:
        // `trade_id` *names the plan to drop*. Without it there is nothing to
        // delete, so reject the malformed control message before dispatch.
        if matches!(
            self.action,
            Action::Enter | Action::Veto | Action::ClearVeto | Action::PlanDelete
        ) && self.trade_id.is_none()
        {
            return Err(IntentValidationError::MissingTradeId);
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
        if self.allow_close.is_some() && self.action != Action::Close {
            return Err(IntentValidationError::AllowCloseOnNonClose);
        }
        // needs_golden / needs_confirmed: both check the shell at gate
        // time, both meaningful on Enter (entry-quality filter) and Close
        // (reversal-quality filter on the consolidated close-on-reversal
        // path). Rejected elsewhere — silent ignore would mask config
        // bugs.
        if self.needs_golden && !matches!(self.action, Action::Enter | Action::Close) {
            return Err(IntentValidationError::NeedsGoldenOnDisallowedAction);
        }
        if self.needs_confirmed && !matches!(self.action, Action::Enter | Action::Close) {
            return Err(IntentValidationError::NeedsConfirmedOnDisallowedAction);
        }
        // ttl_hours is required for prep / veto: the KV flag we write
        // expires on this clock. `Static(0)` is the wire-elided default
        // sentinel meaning "operator didn't set it" — reject. Scripts
        // are accepted unconditionally; the operator who wrote a script
        // meant it, even if it might resolve to 0 at gate time (treated
        // there as "expire immediately").
        if matches!(
            self.action,
            Action::Prep | Action::Veto | Action::PrepExpire
        ) && is_default_ttl_hours(&self.ttl_hours)
        {
            return Err(IntentValidationError::MissingTtlHours);
        }
        // prep-expire keys its KV block on `prep-blocked:<acct>:<instr>:<step>`,
        // so `step` is load-bearing — without it the worker can't tell
        // which prep to block. (Parallels the runtime `step`-required
        // check `prep` has in the worker, lifted to validate so a
        // malformed alert is caught before dispatch.)
        if self.action == Action::PrepExpire && self.step.is_none() {
            return Err(IntentValidationError::MissingPrepExpireStep);
        }
        // pause / resume: KV key is `pause:<trade_id>:<blackout_id>`,
        // so both fields are load-bearing. Reuse the trade_id slug
        // rules for blackout_id so the on-disk key segments stay
        // predictable and url-safe.
        if matches!(self.action, Action::Pause | Action::Resume) {
            let Some(blackout) = self.blackout_id.as_deref() else {
                return Err(IntentValidationError::MissingPauseFields);
            };
            if self.trade_id.is_none() {
                return Err(IntentValidationError::MissingPauseFields);
            }
            if !is_valid_trade_id(blackout) {
                return Err(IntentValidationError::InvalidBlackoutId);
            }
        }
        // news-start / news-end: parallel to pause/resume; KV key is
        // `news:<trade_id>:<news_id>`.
        if matches!(self.action, Action::NewsStart | Action::NewsEnd) {
            let Some(news) = self.news_id.as_deref() else {
                return Err(IntentValidationError::MissingNewsFields);
            };
            if self.trade_id.is_none() {
                return Err(IntentValidationError::MissingNewsFields);
            }
            if !is_valid_trade_id(news) {
                return Err(IntentValidationError::InvalidNewsId);
            }
        }
        // require_news_window gates only the close action; rejecting
        // it on other actions catches operator/template mistakes early.
        if self.require_news_window.is_some() && self.action != Action::Close {
            return Err(IntentValidationError::RequireNewsWindowOnNonClose);
        }
        // require_price_in_ranges: same close-only rule. Also reject
        // an empty list (never matches → silent dead alert) and any
        // band where lo > hi (builder bug).
        if let Some(ranges) = &self.require_price_in_ranges {
            if self.action != Action::Close {
                return Err(IntentValidationError::RequirePriceInRangesOnNonClose);
            }
            if ranges.is_empty() || ranges.iter().any(|[lo, hi]| lo > hi) {
                return Err(IntentValidationError::InvalidPriceRanges);
            }
        }
        // inside_window / sr_bands: new consolidated close-gate.
        // - Either field non-empty pins the intent to Close.
        // - The two are paired: `price` in the type-list iff the data
        //   list is non-empty. Either inconsistency is a builder bug.
        // - Bands must have lo <= hi (same rule as the deprecated form).
        // - Mutually exclusive with the deprecated fields — mixing the
        //   two forms invites silent semantic drift in future refactors.
        let has_new = !self.inside_window.is_empty() || !self.sr_bands.is_empty();
        let has_old = self.require_news_window.is_some() || self.require_price_in_ranges.is_some();
        if has_new && has_old {
            return Err(IntentValidationError::MixedOldAndNewCloseGates);
        }
        if has_new && self.action != Action::Close {
            return Err(IntentValidationError::InsideWindowOnNonClose);
        }
        let price_in_window = self.inside_window.contains(&EventWindow::Price);
        let bands_present = !self.sr_bands.is_empty();
        if price_in_window != bands_present {
            return Err(IntentValidationError::InsideWindowSrBandsMismatch);
        }
        if self.sr_bands.iter().any(|[lo, hi]| lo > hi) {
            return Err(IntentValidationError::InvalidSrBands);
        }
        // veto_on_reversal: experimental hook that turns a reversal-close
        // into an entry veto. Only meaningful on a `close` that actually
        // has a price window to reverse off — otherwise the flag is dead.
        if self.veto_on_reversal {
            if self.action != Action::Close {
                return Err(IntentValidationError::VetoOnReversalOnNonClose);
            }
            let has_price_window = self.inside_window.contains(&EventWindow::Price)
                || self.require_price_in_ranges.is_some();
            if !has_price_window {
                return Err(IntentValidationError::VetoOnReversalWithoutPriceWindow);
            }
        }
        // M/W static params: only on `enter` (they drive stop-entry
        // geometry), and every field must be sane — non-finite anchors,
        // a negative spread, or a non-positive pip size would feed NaN /
        // garbage into the worker's mid-correct resolution. The worker
        // derives entry/SL/TP from these, so the "enter requires
        // entry/stop_loss/take_profit" rule is relaxed when `mw` is set
        // (those fields are absent for M/W) — that relaxation lives in
        // `resolution::from_intent`, not here.
        if let Some(mw) = &self.mw {
            if self.action != Action::Enter {
                return Err(IntentValidationError::MwOnNonEnter);
            }
            let anchors_finite =
                mw.neckline.is_finite() && mw.first_point.is_finite() && mw.runup_start.is_finite();
            let spread_ok = mw.spread_pips.is_finite() && mw.spread_pips >= 0.0;
            let pip_ok = mw.pip_size.is_finite() && mw.pip_size > 0.0;
            if !(anchors_finite && spread_ok && pip_ok) {
                return Err(IntentValidationError::MwFieldInvalid);
            }
            // 4-point path: the right shoulder must be finite and sit on
            // the same side of the neckline as the left shoulder (above for
            // an M where first_point > neckline, below for a W). The richer
            // "within 1.3 of the shortest shoulder" validity is a *drawing*
            // gate enforced at arm time in tv-arm; here we only guard the
            // wire contract so a NaN / wrong-side baked value can't reach
            // the resolver.
            if let Some(rs) = mw.right_shoulder {
                let left_above = mw.first_point > mw.neckline;
                let rs_above = rs > mw.neckline;
                if !rs.is_finite() || rs_above != left_above {
                    return Err(IntentValidationError::MwFieldInvalid);
                }
            }
        }
        // Top-level baked pip: if present it scales every offset_pips into
        // a price, so a zero/negative/NaN value would silently zero or
        // invert the geometry. Independent of `mw` (which validates its
        // own copy above) — a non-M/W enter carries pip here.
        if let Some(pip) = self.pip_size
            && !(pip.is_finite() && pip > 0.0)
        {
            return Err(IntentValidationError::PipSizeInvalid);
        }
        // recover_entry: a `market` recovery no longer requires an explicit
        // `max_slippage_pips` — the resolver derives the bound from the
        // SL→entry distance when it's omitted (see
        // `resolution::resolve_recover_entry` and the wrong-side Stop arm),
        // so there is no malformed form to reject here.
        Ok(())
    }
}

/// Contextual-window type for the consolidated close-on-reversal gate
/// (see [`Intent::inside_window`]). Each variant names *one* family of
/// "is this candle a real reversal?" check; the list on the intent is
/// **OR-composed** (any window passing is sufficient).
///
/// The two-axis metaphor is intentional:
/// - [`Self::News`] — a *time* window (we're inside an
///   operator-armed `news:<trade_id>:<news_id>` pair).
/// - [`Self::Price`] — a *price* window (broker's current price sits
///   inside one of [`Intent::sr_bands`]).
///
/// Wire form is kebab-case (`news`, `price`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventWindow {
    /// Active news-window gate (looks up
    /// `list_news_windows_for_trade(trade_id)` at dispatch time).
    News,
    /// Price-band gate (broker's current price must sit inside one of
    /// [`Intent::sr_bands`]).
    Price,
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
    /// Block all future `prep` fires for one named `step` on an
    /// instrument. Fired by a `<prep>-expiry` chart line when the
    /// pattern's window for landing that prep has lapsed (e.g. an
    /// H&S break-and-close that never came within the allowed bar
    /// count). Once blocked, the entry's `requires_preps` gate for
    /// that step can never be satisfied, so the setup can't enter.
    /// A prep that *already* fired before the block is untouched —
    /// the block only stops *future* preps. No broker side effects.
    PrepExpire,
    /// Clear a single instrument's prep flag.
    ClearPrep,
    /// Clear a single instrument's veto flag.
    ClearVeto,
    /// Arm a blackout window for a `trade_id` + `blackout_id` pair.
    /// The `enter` gate rejects while any pause for the trade_id is
    /// active. Used to suspend a setup across news events without
    /// invalidating it. Paired with [`Action::Resume`] to clear.
    Pause,
    /// Clear the matching `pause:<trade_id>:<blackout_id>` entry. Both
    /// halves of the pair carry the same `blackout_id` so multiple
    /// concurrent blackouts on one trade don't clobber each other.
    Resume,
    /// Open a news window for a `trade_id` + `news_id` pair. While
    /// at least one window is open, a `close` with
    /// `require_news_window: true` is allowed to flatten the trade;
    /// outside any window the same close is rejected. Independent
    /// of [`Action::Pause`] — news windows don't block entries.
    /// Paired with [`Action::NewsEnd`] to clear.
    NewsStart,
    /// Clear the matching `news:<trade_id>:<news_id>` entry.
    NewsEnd,
    /// Register a server-side [`TradePlan`](crate::trade_plan::TradePlan) with
    /// the engine. The plan rides in [`Intent::trade_plan`]; the engine then
    /// evaluates its conditions on each cron tick and dispatches the embedded
    /// intents itself, replacing the per-condition TradingView alerts. A
    /// control-style action — no broker call at register time; idempotent
    /// (re-registering the same plan refreshes its row). See the engine crate.
    Register,
    /// Read-only query: return TradeNation's per-instrument market info
    /// (trading session hours, spread, margin, guaranteed-stop terms,
    /// expiry) for [`Intent::instrument`]. Unlike [`Action::Status`] this
    /// needs a live TradeNation broker (it calls `broker.market_info`), so
    /// it dispatches through the broker path rather than the KV-only
    /// control block. No state mutation, no broker order. TradeNation only
    /// — there is no OANDA equivalent yet. The hours feed the upcoming
    /// market-hours entry blackout.
    MarketInfo,
    /// Read-only query: list every registered server-side
    /// [`TradePlan`](crate::trade_plan::TradePlan) the engine is evaluating,
    /// each with a compact summary of its current
    /// [`PlanState`](crate::plan_state::PlanState) (phase, watermark, fired
    /// rules, shadow flag). Like [`Action::Status`] this is KV-only (no broker)
    /// and `instrument` is an ignored placeholder. Drives `trade-control plan
    /// list`. Recorded as seen on every completion (idempotent control op).
    PlanList,
    /// Read-only query: dump one registered plan in full — the entire
    /// [`TradePlan`](crate::trade_plan::TradePlan) plus its
    /// [`PlanState`](crate::plan_state::PlanState). The target is named by
    /// [`Intent::trade_id`] (not `instrument`, which is an ignored placeholder);
    /// the worker scans every account scope and returns the match(es). KV-only,
    /// idempotent. Drives `trade-control plan show <trade_id>`.
    PlanShow,
    /// Delete a registered server-side
    /// [`TradePlan`](crate::trade_plan::TradePlan) and its
    /// [`PlanState`](crate::plan_state::PlanState) — the inverse of
    /// [`Action::Register`]. The target is named by [`Intent::trade_id`]
    /// (`instrument` is an ignored placeholder); the worker scans every
    /// account scope and drops the matching `plan:` + `plan-state:` rows.
    /// KV-only (no broker), idempotent — deleting a plan that doesn't exist
    /// is a no-op, not an error. Lets the operator re-arm a setup after
    /// editing the chart: `plan delete <id>` then re-run `tv-arm`. Drives
    /// `trade-control plan delete <trade_id>`.
    PlanDelete,
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
///
/// `SignalHigh` / `SignalLow` reference Pine's `signal_high` / `signal_low`
/// — the *latched pattern extreme* (the H&S head / right-shoulder region),
/// frozen when the pattern formed. Unlike `High`/`Low` (the triggering
/// candle's own wick), these are **stable across a confirmation re-fire**:
/// the break-candle fire and the later `signal_confirmed: 1` fire carry the
/// same `signal_high`/`signal_low`, so an H&S enter anchored to them resolves
/// to identical entry/SL geometry both times. (Contrast `RecentHigh`, which
/// is the pre-signal lookback window, not the pattern extreme.) This anchor
/// is what fixes bug #10 finding A — see the H&S/IHS geometry in
/// `cli/src/trade_patterns.rs`. Same graceful `unwrap_or(high/low)` fallback
/// as the recent_* anchors for shells that predate the fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceAnchor {
    Close,
    High,
    Low,
    RecentHigh,
    RecentLow,
    SignalHigh,
    SignalLow,
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
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
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
        /// Absolute trigger price set at encode time. When `Some`, it
        /// overrides `from`/`offset_pips` and the worker uses it verbatim
        /// (the operator drew the exact level — e.g. a position tool's
        /// entry anchor). When `None`, today's behaviour: trigger =
        /// `anchor_price(from) + offset_pips × pip_size`. Signed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<f64>,
        /// Optional recovery for when a stop entry can't be placed as a
        /// resting stop because the trigger has already been overtaken by
        /// price — either at resolve time (the breakout happened during
        /// the signal-confirmation wait, so the stop is "wrong-side" for
        /// its direction) or at the broker (TradeNation `#19-10` "entry
        /// too close to / wrong side of market"). Absent = today's
        /// behaviour: the entry is dropped (resolve-time) or the
        /// placement fails (502, broker-time) and the next signal bar may
        /// retry. See [`RecoverEntry`].
        #[serde(
            default,
            alias = "on_too_close",
            skip_serializing_if = "Option::is_none"
        )]
        recover_entry: Option<RecoverEntry>,
    },
    /// Limit pending order; price resolves against the plaintext shell.
    /// Fills when price comes *back* to the level — used for pullback entries.
    /// Long limit sits *below* current price; short limit sits *above*.
    Limit {
        from: PriceAnchor,
        #[serde(default)]
        offset_pips: f64,
        /// Absolute trigger price set at encode time — same semantics as
        /// [`EntrySpec::Stop::at`]. When `Some`, overrides `from`/`offset_pips`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<f64>,
    },
}

/// What to do when a stop entry can't be placed as a resting stop
/// because the trigger has already been overtaken by price — at resolve
/// time (wrong-side for its direction after the signal-confirmation
/// wait) or at the broker (TradeNation `#19-10`). Opt-in on
/// [`EntrySpec::Stop`]; absent means "drop" (today's behaviour). The
/// strategy author encodes the intent in the alert; it is not a
/// universal default.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct RecoverEntry {
    /// Which recovery to attempt.
    pub action: RecoverEntryAction,
    /// Optional guard rail for [`RecoverEntryAction::Market`]: only
    /// re-place as a market order if current price is within this many
    /// pips of the original stop trigger; otherwise fall back to skip.
    /// When absent the resolver derives the bound from the SL→entry
    /// distance (`|stop_loss − trigger|`). Ignored for `skip`; unused for
    /// `limit` (a resting limit can't fill worse than its price).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_slippage_pips: Option<f64>,
}

/// The recovery action for [`RecoverEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RecoverEntryAction {
    /// Re-place as a market order, bounded by
    /// [`RecoverEntry::max_slippage_pips`] (or the resolver-derived
    /// SL→entry distance when absent). The break already happened;
    /// enter the confirmed breakout at market rather than dropping it.
    Market,
    /// Re-place the level as a limit order (wait for a pullback to the
    /// intended entry). Geometry-validated so it doesn't become a
    /// wrong-side limit (`#19-9`), preserving the planned R.
    Limit,
    /// Do nothing — drop the entry (resolve-time) or let the placement
    /// fail (502, no seen-id poison, broker-time) so the next signal bar
    /// can retry. Identical to omitting `recover_entry` entirely. The
    /// default (a bare stop neither recovers nor needs a slippage bound).
    #[default]
    Skip,
}

impl Shell {
    /// Synthesize a control-grade shell from one broker [`Candle`](crate::broker::Candle).
    ///
    /// The server-side engine has no TradingView alert to source a `Shell`
    /// from — it polls candles and fires the registered intents itself. This
    /// builds the minimal shell those intents need: OHLC + time. `open` is
    /// populated (the candle carries it), so M/W body-extreme logic
    /// ([`body_high`](Self::body_high) / [`body_low`](Self::body_low)) works.
    /// Every Pine-latched field (`signal_*`, `recent_*`, `golden`, `atr`,
    /// `next_candle_timestamp_*`) is `None`: a plain candle carries no pattern
    /// geometry. This is the right shell for M/W (reads only OHLC), vetos, and
    /// preps. An **H&S enter** instead uses [`Self::from_candle_and_signal`],
    /// which folds the latched signal geometry the Pine-detector port computed
    /// onto these fields.
    pub fn from_candle(candle: &crate::broker::Candle) -> Self {
        Self {
            close: candle.c,
            high: candle.h,
            low: candle.l,
            open: Some(candle.o),
            time: candle.time,
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

    /// Synthesize an **H&S enter** shell from the triggering candle plus the
    /// latched candle-pattern signal the engine's Pine-detector port computed.
    ///
    /// Starts from [`Self::from_candle`] (OHLC + time + `open`) and folds the
    /// signal geometry onto the `signal_*` / `recent_*` / `golden` / `atr` /
    /// `signal_confirmed` fields — the same values the TV alert's
    /// `{{plot("signal_high")}}` substitutions carried. The H&S enter resolves
    /// its entry/SL/TP against these (`PriceAnchor::SignalHigh` etc.), so a
    /// server-side fire resolves to identical geometry as the TV-driven one.
    ///
    /// `next_candle_timestamp_*` stays `None`: the engine derives a pending
    /// order's `cancel_at` from `expiry_bars` against the live tick clock, not
    /// from a Pine-projected bar-close menu.
    pub fn from_candle_and_signal(
        candle: &crate::broker::Candle,
        sig: &crate::signals::LatchedSignal,
    ) -> Self {
        let mut shell = Self::from_candle(candle);
        shell.signal_high = Some(sig.signal_high);
        shell.signal_low = Some(sig.signal_low);
        shell.signal_range = Some(sig.signal_range);
        shell.signal_start_time = Some(sig.signal_start_time);
        shell.signal_kind = Some(sig.kind);
        shell.golden = Some(sig.golden);
        shell.signal_confirmed = Some(sig.signal_confirmed);
        shell.atr = sig.atr;
        shell.recent_high = sig.recent_high;
        shell.recent_low = sig.recent_low;
        shell
    }

    /// Body top — `max(open, close)` — or `None` if this shell didn't
    /// carry `open` (control shells, or a chart still on the pre-`open`
    /// Pine). M/W dynamic geometry uses bodies, not wicks, so a lone rogue
    /// wick can't move the right shoulder or trip the cancel.
    pub fn body_high(&self) -> Option<f64> {
        self.open.map(|o| o.max(self.close))
    }

    /// Body bottom — `min(open, close)` — or `None` if `open` is absent.
    /// See [`Self::body_high`].
    pub fn body_low(&self) -> Option<f64> {
        self.open.map(|o| o.min(self.close))
    }

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
            // Latched pattern extreme — stable across a confirm re-fire.
            // Falls back to the candle wick if Pine didn't ship signal_*.
            PriceAnchor::SignalHigh => self.signal_high.unwrap_or(self.high),
            PriceAnchor::SignalLow => self.signal_low.unwrap_or(self.low),
        }
    }

    /// Return the Pine-filled forward bar-close timestamp for `n` (1..=5),
    /// or `None` if `n` is out of range or the slot wasn't populated (e.g.
    /// `na` on a non-time-based chart, or a control/drawing shell that
    /// never carries the menu).
    pub fn next_candle_timestamp(&self, n: u32) -> Option<DateTime<Utc>> {
        match n {
            1 => self.next_candle_timestamp_1,
            2 => self.next_candle_timestamp_2,
            3 => self.next_candle_timestamp_3,
            4 => self.next_candle_timestamp_4,
            5 => self.next_candle_timestamp_5,
            _ => None,
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
    fn anchor_price_signal_uses_shell_field_when_present() {
        // The latched pattern extreme, distinct from the candle wick.
        let mut s = shell();
        s.signal_high = Some(1.1075);
        s.signal_low = Some(1.0925);
        assert_eq!(s.anchor_price(PriceAnchor::SignalHigh), 1.1075);
        assert_eq!(s.anchor_price(PriceAnchor::SignalLow), 1.0925);
    }

    #[test]
    fn anchor_price_signal_falls_back_to_bar_extreme_when_missing() {
        // Pre-2026-05 shells didn't carry signal_*. Fall back to the
        // candle's own high/low rather than panic — same graceful
        // degradation as the recent_* anchors.
        let s = shell();
        assert!(s.signal_high.is_none());
        assert!(s.signal_low.is_none());
        assert_eq!(s.anchor_price(PriceAnchor::SignalHigh), s.high);
        assert_eq!(s.anchor_price(PriceAnchor::SignalLow), s.low);
    }

    #[test]
    fn price_anchor_signal_round_trips_through_yaml() {
        let from: PriceAnchor = serde_yaml::from_str("signal_high").unwrap();
        assert_eq!(from, PriceAnchor::SignalHigh);
        let out = serde_yaml::to_string(&PriceAnchor::SignalLow).unwrap();
        assert!(out.contains("signal_low"), "got: {out}");
    }

    #[test]
    fn body_extremes_none_without_open() {
        // The default shell() has open: None.
        let s = shell();
        assert_eq!(s.body_high(), None);
        assert_eq!(s.body_low(), None);
    }

    #[test]
    fn from_candle_populates_ohlc_and_open_but_no_pine_fields() {
        let candle = crate::broker::Candle {
            time: "2026-06-17T12:00:00Z".parse().unwrap(),
            o: 1.0990,
            h: 1.1020,
            l: 1.0980,
            c: 1.1000,
        };
        let s = Shell::from_candle(&candle);
        assert_eq!(s.close, 1.1000);
        assert_eq!(s.high, 1.1020);
        assert_eq!(s.low, 1.0980);
        assert_eq!(s.open, Some(1.0990));
        assert_eq!(s.time, candle.time);
        // open is present, so body extremes are computable (M/W relies on this).
        assert_eq!(s.body_high(), Some(1.1000));
        assert_eq!(s.body_low(), Some(1.0990));
        // Every Pine-latched field is absent — the engine doesn't run the indicator.
        assert_eq!(s.signal_kind, None);
        assert_eq!(s.golden, None);
        assert_eq!(s.atr, None);
        assert_eq!(s.recent_high, None);
        assert_eq!(s.next_candle_timestamp_1, None);
    }

    #[test]
    fn from_candle_and_signal_folds_pattern_geometry() {
        let candle = crate::broker::Candle {
            time: "2026-06-17T12:00:00Z".parse().unwrap(),
            o: 1.1200,
            h: 1.3000,
            l: 1.1000,
            c: 1.1150,
        };
        let sig = crate::signals::LatchedSignal {
            direction: Direction::Short,
            kind: SignalKind::Pinbar,
            signal_high: 1.3000,
            signal_low: 1.1000,
            signal_range: 0.2000,
            signal_start_time: candle.time,
            golden: true,
            signal_confirmed: true,
            atr: Some(0.05),
            recent_high: Some(1.2500),
            recent_low: Some(1.0500),
            fires: true,
        };
        let s = Shell::from_candle_and_signal(&candle, &sig);
        // OHLC still from the candle.
        assert_eq!(s.close, 1.1150);
        assert_eq!(s.open, Some(1.1200));
        // Pattern geometry folded on — the H&S enter anchors entry/SL to these.
        assert_eq!(s.signal_high, Some(1.3000));
        assert_eq!(s.signal_low, Some(1.1000));
        assert_eq!(s.signal_kind, Some(SignalKind::Pinbar));
        assert_eq!(s.golden, Some(true));
        assert_eq!(s.signal_confirmed, Some(true));
        assert_eq!(s.recent_high, Some(1.2500));
        assert_eq!(s.recent_low, Some(1.0500));
        assert_eq!(s.atr, Some(0.05));
        // The bug-010 SignalHigh/SignalLow anchors now resolve to the *pattern*
        // extremes (not the triggering candle's own wick), so a server-side H&S
        // fire anchors entry/SL identically to the TV-driven one.
        assert_eq!(s.anchor_price(PriceAnchor::SignalHigh), 1.3000);
        assert_eq!(s.anchor_price(PriceAnchor::SignalLow), 1.1000);
    }

    #[test]
    fn body_extremes_use_open_and_close_not_wicks() {
        let mut s = shell();
        // Bullish body: open 1.0990 < close 1.1000; wicks (high/low) are wider.
        s.open = Some(1.0990);
        s.close = 1.1000;
        s.high = 1.1020;
        s.low = 1.0980;
        assert_eq!(s.body_high(), Some(1.1000)); // max(open, close), not high
        assert_eq!(s.body_low(), Some(1.0990)); // min(open, close), not low
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
            Some(EntrySpec::Stop {
                from,
                offset_pips,
                at,
                recover_entry,
            }) => {
                assert_eq!(from, PriceAnchor::High);
                assert!((offset_pips - 2.0).abs() < 1e-9);
                assert_eq!(at, None);
                assert!(recover_entry.is_none());
            }
            _ => panic!("expected stop entry"),
        }
    }

    #[test]
    fn stop_entry_without_recover_entry_omits_field() {
        // Back-compat: a bare stop entry serialises without the
        // `recover_entry` key, so the wire form is byte-identical to
        // pre-feature intents.
        let spec = EntrySpec::Stop {
            from: PriceAnchor::High,
            offset_pips: 1.0,
            at: None,
            recover_entry: None,
        };
        let yaml = serde_yaml::to_string(&spec).unwrap();
        assert!(!yaml.contains("recover_entry"), "yaml was: {yaml}");
        assert!(!yaml.contains("at:"), "yaml was: {yaml}");
    }

    #[test]
    fn stop_entry_with_recover_entry_market_round_trips() {
        let yaml = "
            type: stop
            from: high
            offset_pips: 1.0
            recover_entry: { action: market, max_slippage_pips: 8.0 }
        ";
        let spec: EntrySpec = serde_yaml::from_str(yaml).unwrap();
        match spec {
            EntrySpec::Stop {
                recover_entry: Some(rec),
                ..
            } => {
                assert_eq!(rec.action, RecoverEntryAction::Market);
                assert!((rec.max_slippage_pips.unwrap() - 8.0).abs() < 1e-9);
            }
            other => panic!("expected stop with recover_entry, got {other:?}"),
        }
        // And it round-trips back through serialise.
        let spec = EntrySpec::Stop {
            from: PriceAnchor::High,
            offset_pips: 1.0,
            at: None,
            recover_entry: Some(RecoverEntry {
                action: RecoverEntryAction::Market,
                max_slippage_pips: Some(8.0),
            }),
        };
        let out = serde_yaml::to_string(&spec).unwrap();
        let back: EntrySpec = serde_yaml::from_str(&out).unwrap();
        assert!(matches!(
            back,
            EntrySpec::Stop {
                recover_entry: Some(RecoverEntry {
                    action: RecoverEntryAction::Market,
                    ..
                }),
                ..
            }
        ));
    }

    #[test]
    fn stop_entry_on_too_close_alias_still_parses() {
        // Back-compat: in-flight signed KV plans use the old key name
        // `on_too_close`; the serde alias keeps them parsing.
        let yaml = "
            type: stop
            from: high
            offset_pips: 1.0
            on_too_close: { action: market, max_slippage_pips: 8.0 }
        ";
        let spec: EntrySpec = serde_yaml::from_str(yaml).unwrap();
        match spec {
            EntrySpec::Stop {
                recover_entry: Some(rec),
                ..
            } => {
                assert_eq!(rec.action, RecoverEntryAction::Market);
                assert!((rec.max_slippage_pips.unwrap() - 8.0).abs() < 1e-9);
            }
            other => panic!("expected stop with recovery via alias, got {other:?}"),
        }
    }

    #[test]
    fn recover_entry_skip_and_limit_parse() {
        let skip: RecoverEntry = serde_yaml::from_str("{ action: skip }").unwrap();
        assert_eq!(skip.action, RecoverEntryAction::Skip);

        let limit: RecoverEntry = serde_yaml::from_str("{ action: limit }").unwrap();
        assert_eq!(limit.action, RecoverEntryAction::Limit);
    }

    #[test]
    fn recover_entry_market_without_slippage_parses() {
        // `market` without `max_slippage_pips` is valid: the resolver
        // derives the SL→entry slippage bound, so no explicit value is
        // required (was previously flagged as a validation error).
        let rec: RecoverEntry = serde_yaml::from_str("{ action: market }").unwrap();
        assert_eq!(rec.action, RecoverEntryAction::Market);
        assert!(rec.max_slippage_pips.is_none());
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
            Some(EntrySpec::Limit {
                from,
                offset_pips,
                at,
            }) => {
                assert_eq!(from, PriceAnchor::Low);
                assert!((offset_pips - -5.0).abs() < 1e-9);
                assert_eq!(at, None);
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
        // The `trade_id` field is serde-optional — an intent that omits
        // it deserialises with `trade_id: None`. (Enter / veto / clear-veto
        // then require one at validate time — see
        // `validate_rejects_enter_without_trade_id` — but a control action
        // like `status` doesn't, so it round-trips and validates here.)
        let yaml = "
            v: 1
            id: t-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: status
            instrument: EUR_USD
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
    fn intent_validate_pause_requires_trade_id_and_blackout_id() {
        let base = "
            v: 1
            id: pause-1
            not_after: \"2026-06-01T00:00:00Z\"
            action: pause
            instrument: EUR_USD
        ";
        // Missing both trade_id and blackout_id.
        let intent: Intent = serde_yaml::from_str(base).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingPauseFields)
        );

        // Missing blackout_id only.
        let yaml = format!("{base}\n            trade_id: eurusd-h-and-s-1");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingPauseFields)
        );

        // Missing trade_id only.
        let yaml = format!("{base}\n            blackout_id: nfp-2026-06-06");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingPauseFields)
        );
    }

    #[test]
    fn intent_validate_pause_rejects_bad_blackout_id() {
        let yaml = "
            v: 1
            id: pause-bad
            not_after: \"2026-06-01T00:00:00Z\"
            action: pause
            instrument: EUR_USD
            trade_id: eurusd-h-and-s-1
            blackout_id: \"NFP 2026!\"
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::InvalidBlackoutId)
        );
    }

    #[test]
    fn intent_validate_pause_accepts_well_shaped_pair() {
        for action in ["pause", "resume"] {
            let yaml = format!(
                "
                v: 1
                id: pause-ok-{action}
                not_after: \"2026-06-01T00:00:00Z\"
                action: {action}
                instrument: EUR_USD
                trade_id: eurusd-h-and-s-1
                blackout_id: nfp-2026-06-06
                reason: news:USD-NFP
            "
            );
            let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
            intent
                .validate()
                .unwrap_or_else(|e| panic!("{action} should validate, got {e:?}"));
        }
    }

    #[test]
    fn intent_pause_round_trips_through_yaml() {
        let yaml = "
            v: 1
            id: pause-rt
            not_after: \"2026-06-01T00:00:00Z\"
            action: pause
            instrument: EUR_USD
            trade_id: eurusd-h-and-s-1
            blackout_id: nfp-2026-06-06
            reason: \"news:USD-NFP\"
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("action: pause"));
        assert!(back.contains("blackout_id: nfp-2026-06-06"));
        assert!(back.contains("reason: news:USD-NFP"));
    }

    #[test]
    fn intent_validate_news_requires_trade_id_and_news_id() {
        let base = "
            v: 1
            id: news-1
            not_after: \"2026-06-01T00:00:00Z\"
            action: news-start
            instrument: EUR_USD
        ";
        // Missing both trade_id and news_id.
        let intent: Intent = serde_yaml::from_str(base).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingNewsFields)
        );

        // Missing news_id only.
        let yaml = format!("{base}\n            trade_id: eurusd-h-and-s-1");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingNewsFields)
        );

        // Missing trade_id only.
        let yaml = format!("{base}\n            news_id: usd-nfp-2026-06-06");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingNewsFields)
        );
    }

    #[test]
    fn intent_validate_news_rejects_bad_news_id() {
        let yaml = "
            v: 1
            id: news-bad
            not_after: \"2026-06-01T00:00:00Z\"
            action: news-start
            instrument: EUR_USD
            trade_id: eurusd-h-and-s-1
            news_id: \"NFP 2026!\"
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.validate(), Err(IntentValidationError::InvalidNewsId));
    }

    #[test]
    fn intent_validate_news_accepts_well_shaped_pair() {
        for action in ["news-start", "news-end"] {
            let yaml = format!(
                "
                v: 1
                id: news-ok-{action}
                not_after: \"2026-06-01T00:00:00Z\"
                action: {action}
                instrument: EUR_USD
                trade_id: eurusd-h-and-s-1
                news_id: usd-nfp-2026-06-06
                reason: USD-NFP
            "
            );
            let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
            intent
                .validate()
                .unwrap_or_else(|e| panic!("{action} should validate, got {e:?}"));
        }
    }

    #[test]
    fn intent_news_round_trips_through_yaml() {
        let yaml = "
            v: 1
            id: news-rt
            not_after: \"2026-06-01T00:00:00Z\"
            action: news-start
            instrument: EUR_USD
            trade_id: eurusd-h-and-s-1
            news_id: usd-nfp-2026-06-06
            reason: \"USD-NFP\"
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("action: news-start"));
        assert!(back.contains("news_id: usd-nfp-2026-06-06"));
    }

    #[test]
    fn intent_validate_require_news_window_only_on_close() {
        // OK on close.
        let yaml = "
            v: 1
            id: c-1
            not_after: \"2026-06-01T00:00:00Z\"
            action: close
            instrument: EUR_USD
            trade_id: eurusd-h-and-s-1
            require_news_window: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().expect("close+require_news_window valid");

        // Rejected on non-close (status here).
        let yaml = "
            v: 1
            id: s-1
            not_after: \"2026-06-01T00:00:00Z\"
            action: status
            instrument: EUR_USD
            require_news_window: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::RequireNewsWindowOnNonClose)
        );
    }

    #[test]
    fn intent_validate_require_price_in_ranges_only_on_close() {
        // OK on close.
        let yaml = "
            v: 1
            id: c-pr-1
            not_after: \"2026-06-01T00:00:00Z\"
            action: close
            instrument: EUR_USD
            trade_id: eurusd-h-and-s-1
            require_price_in_ranges:
              - [1.0950, 1.0970]
              - [1.1000, 1.1020]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent
            .validate()
            .expect("close + require_price_in_ranges valid");

        // Rejected on non-close.
        let yaml = "
            v: 1
            id: s-pr-1
            not_after: \"2026-06-01T00:00:00Z\"
            action: status
            instrument: EUR_USD
            require_price_in_ranges:
              - [1.0, 2.0]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::RequirePriceInRangesOnNonClose)
        );
    }

    #[test]
    fn intent_validate_rejects_empty_or_flipped_price_ranges() {
        // Empty list.
        let yaml = "
            v: 1
            id: c-pr-2
            not_after: \"2026-06-01T00:00:00Z\"
            action: close
            instrument: EUR_USD
            trade_id: t-1
            require_price_in_ranges: []
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::InvalidPriceRanges)
        );

        // Flipped band (lo > hi).
        let yaml = "
            v: 1
            id: c-pr-3
            not_after: \"2026-06-01T00:00:00Z\"
            action: close
            instrument: EUR_USD
            trade_id: t-1
            require_price_in_ranges:
              - [1.2, 1.1]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::InvalidPriceRanges)
        );
    }

    #[test]
    fn require_price_in_ranges_round_trips_through_yaml() {
        let yaml = "
            v: 1
            id: c-pr-4
            not_after: \"2026-06-01T00:00:00Z\"
            action: close
            instrument: EUR_USD
            trade_id: t-1
            require_price_in_ranges:
              - [1.0950, 1.0970]
              - [1.1000, 1.1020]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("require_price_in_ranges:"));
        assert!(back.contains("1.095"));
        assert!(back.contains("1.102"));
    }

    #[test]
    fn close_without_require_news_window_validates_unchanged() {
        // Backwards compat: a plain close intent (no `require_news_window`)
        // still validates — operator emergency-close path stays
        // unconditional.
        let yaml = "
            v: 1
            id: c-2
            not_after: \"2026-06-01T00:00:00Z\"
            action: close
            instrument: EUR_USD
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().expect("plain close validates");
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
            trade_id: eurusd-hs-1
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
    fn validate_rejects_enter_without_trade_id() {
        // An enter with no trade_id is rejected outright now (2026-06-11):
        // the veto gate looks vetos up by the entry's trade_id, so an
        // untagged entry could never match its setup's vetos. This fires
        // before the older max_retries-without-trade_id check (both would
        // be true here — MissingTradeId is the more fundamental contract).
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
            Err(IntentValidationError::MissingTradeId)
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
            trade_id: eurusd-hs-1
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
    fn validate_rejects_veto_without_trade_id() {
        // The veto KV key is scoped per-setup; a veto with no trade_id
        // would bleed across every setup on the instrument (the
        // 2026-06-11 bug). MissingTradeId fires before the ttl check.
        let yaml = "
            v: 1
            id: veto-no-tid
            not_after: \"2026-05-13T20:00:00Z\"
            action: veto
            instrument: EUR_USD
            name: too-high
            ttl_hours: 12
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingTradeId)
        );
    }

    #[test]
    fn validate_rejects_clear_veto_without_trade_id() {
        let yaml = "
            v: 1
            id: clear-veto-no-tid
            not_after: \"2026-05-13T20:00:00Z\"
            action: clear-veto
            instrument: EUR_USD
            name: too-high
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingTradeId)
        );
    }

    #[test]
    fn validate_rejects_plan_delete_without_trade_id() {
        let yaml = "
            v: 1
            id: plan-delete-no-tid
            not_after: \"2026-05-13T20:00:00Z\"
            action: plan-delete
            instrument: ALL
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingTradeId)
        );
    }

    #[test]
    fn validate_accepts_plan_delete_with_trade_id() {
        let yaml = "
            v: 1
            id: plan-delete-ok
            trade_id: eurusd-hs-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: plan-delete
            instrument: ALL
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().unwrap();
    }

    #[test]
    fn validate_accepts_veto_with_trade_id() {
        let yaml = "
            v: 1
            id: veto-ok
            trade_id: eurusd-hs-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: veto
            instrument: EUR_USD
            name: too-high
            ttl_hours: 12
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().unwrap();
    }

    #[test]
    fn validate_accepts_recover_entry_market_without_slippage() {
        // The resolver derives the slippage bound from the SL→entry
        // distance, so an explicit `max_slippage_pips` is no longer
        // required for a `market` recovery (was previously rejected).
        let yaml = "
            v: 1
            id: enter-1
            trade_id: eurusd-hs-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: stop, from: high, offset_pips: 1, recover_entry: { action: market } }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().unwrap();
    }

    #[test]
    fn validate_accepts_recover_entry_market_with_slippage() {
        let yaml = "
            v: 1
            id: enter-1
            trade_id: eurusd-hs-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: stop, from: high, offset_pips: 1, recover_entry: { action: market, max_slippage_pips: 8.0 } }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().unwrap();
    }

    #[test]
    fn validate_accepts_recover_entry_skip() {
        let yaml = "
            v: 1
            id: enter-1
            trade_id: eurusd-hs-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: stop, from: high, offset_pips: 1, recover_entry: { action: skip } }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().unwrap();
    }

    #[test]
    fn validate_accepts_well_formed_prep_expire() {
        let yaml = "
            v: 1
            id: bnc-expired
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep-expire
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 48
            trade_id: hs-eurusd-abc
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.action, Action::PrepExpire);
        intent.validate().unwrap();
    }

    #[test]
    fn validate_rejects_prep_expire_without_step() {
        let yaml = "
            v: 1
            id: bnc-expired-no-step
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep-expire
            instrument: EUR_USD
            ttl_hours: 48
            trade_id: hs-eurusd-abc
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MissingPrepExpireStep)
        );
    }

    #[test]
    fn validate_rejects_missing_ttl_hours_on_prep_expire() {
        // prep-expire shares the prep/veto ttl requirement — the KV
        // block needs a lifetime.
        let yaml = "
            v: 1
            id: bnc-expired-no-ttl
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep-expire
            instrument: EUR_USD
            step: break-and-close
            trade_id: hs-eurusd-abc
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
            trade_id: eurusd-hs-1
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
            trade_id: eurusd-hs-1
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
            trade_id: eurusd-hs-1
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
            trade_id: eurusd-hs-1
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
    fn validate_rejects_needs_golden_on_disallowed_action() {
        // Promoted from "Enter-only" to "Enter|Close" on 2026-06-03 so
        // the consolidated reversal close can require a golden candle.
        // Other actions still reject — silent ignore would mislead.
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
            Err(IntentValidationError::NeedsGoldenOnDisallowedAction)
        );
    }

    #[test]
    fn validate_accepts_needs_golden_on_close_action() {
        // The 2026-06-03 promotion: golden gate now valid on Close so
        // the reversal-close path can demand a golden candle.
        let yaml = "
            v: 1
            id: ng-close
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            needs_golden: true
            inside_window: [news]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent
            .validate()
            .expect("close + needs_golden + window valid");
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
    fn validate_rejects_allow_close_on_non_close_action() {
        // Symmetric guard with the Enter-side variant: `allow_close`
        // only runs on the Close path, so allowing it on Enter / Prep
        // would be silently ignored at the worker.
        let yaml = "
            v: 1
            id: ac-enter
            trade_id: eurusd-hs-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            allow_close: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::AllowCloseOnNonClose)
        );
    }

    #[test]
    fn allow_close_round_trips_through_yaml() {
        // Static-bool form lands as Tunable::Static; the script form
        // exercises the same path that allow_entry already does so we
        // don't repeat that test here.
        let yaml = "
            v: 1
            id: ac-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            trade_id: t-1
            allow_close: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().expect("close + allow_close valid");
        match intent.allow_close {
            Some(crate::tunable::Tunable::Static(true)) => {}
            other => panic!("expected Static(true) allow_close, got {other:?}"),
        }
    }

    #[test]
    fn needs_confirmed_round_trips_through_yaml() {
        let yaml = "
            v: 1
            id: nc-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            needs_confirmed: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(intent.needs_confirmed);
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("needs_confirmed: true"), "got: {back}");
    }

    #[test]
    fn needs_confirmed_default_false_and_elided_on_serialise() {
        // Same wire-compat rule as needs_golden: pre-feature intents
        // omit the field; absent → false; serialising false doesn't
        // introduce it.
        let yaml = "
            v: 1
            id: nc-default
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
        assert!(!intent.needs_confirmed);
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(!back.contains("needs_confirmed"), "got: {back}");
    }

    #[test]
    fn validate_rejects_needs_confirmed_on_disallowed_action() {
        let yaml = "
            v: 1
            id: nc-prep
            not_after: \"2026-05-13T20:00:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
            needs_confirmed: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::NeedsConfirmedOnDisallowedAction)
        );
    }

    #[test]
    fn inside_window_news_only_round_trips() {
        let yaml = "
            v: 1
            id: iw-news
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [news]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(intent.inside_window, vec![EventWindow::News]);
        assert!(intent.sr_bands.is_empty());
        intent
            .validate()
            .expect("close + inside_window:[news] valid");
        let back = serde_yaml::to_string(&intent).unwrap();
        assert!(back.contains("inside_window:"), "got: {back}");
        assert!(
            !back.contains("sr_bands"),
            "empty sr_bands should be elided: {back}"
        );
    }

    #[test]
    fn inside_window_price_requires_bands() {
        let yaml = "
            v: 1
            id: iw-price-missing
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [price]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::InsideWindowSrBandsMismatch)
        );
    }

    #[test]
    fn sr_bands_require_inside_window_entry() {
        // The mirror of the previous test: data without the type-tag.
        let yaml = "
            v: 1
            id: pb-orphan
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            sr_bands: [[1.0, 1.1]]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::InsideWindowSrBandsMismatch)
        );
    }

    #[test]
    fn inside_window_news_and_price_round_trips() {
        let yaml = "
            v: 1
            id: iw-both
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [news, price]
            sr_bands: [[1.0950, 1.0970], [1.1000, 1.1020]]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.inside_window,
            vec![EventWindow::News, EventWindow::Price]
        );
        assert_eq!(intent.sr_bands.len(), 2);
        intent
            .validate()
            .expect("close + inside_window:[news,price] valid");
    }

    #[test]
    fn validate_rejects_inside_window_on_non_close() {
        let yaml = "
            v: 1
            id: iw-enter
            trade_id: eurusd-hs-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            inside_window: [news]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::InsideWindowOnNonClose)
        );
    }

    #[test]
    fn validate_rejects_flipped_price_band() {
        let yaml = "
            v: 1
            id: pb-flip
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [price]
            sr_bands: [[1.1, 1.0]]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::InvalidSrBands)
        );
    }

    #[test]
    fn veto_on_reversal_defaults_off_and_skips_serialization() {
        // Absent on the wire → false; and a false flag must not serialize
        // so existing reversal-close alerts stay byte-identical.
        let yaml = "
            v: 1
            id: rev-default
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [price]
            sr_bands: [[1.0950, 1.0970]]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(!intent.veto_on_reversal);
        let out = serde_yaml::to_string(&intent).unwrap();
        assert!(
            !out.contains("veto_on_reversal"),
            "false flag must skip-serialize, got:\n{out}"
        );
    }

    #[test]
    fn veto_on_reversal_round_trips_when_set() {
        let yaml = "
            v: 1
            id: rev-on
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [price]
            sr_bands: [[1.0950, 1.0970]]
            veto_on_reversal: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(intent.veto_on_reversal);
        intent
            .validate()
            .expect("close + price window + veto_on_reversal is valid");
        let out = serde_yaml::to_string(&intent).unwrap();
        assert!(out.contains("veto_on_reversal: true"));
    }

    #[test]
    fn veto_on_reversal_accepts_deprecated_price_window() {
        // The old wire form (require_price_in_ranges) is still a price
        // window, so veto_on_reversal is valid alongside it.
        let yaml = "
            v: 1
            id: rev-old
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            require_price_in_ranges: [[1.0950, 1.0970]]
            veto_on_reversal: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent
            .validate()
            .expect("close + require_price_in_ranges + veto_on_reversal is valid");
    }

    #[test]
    fn validate_rejects_veto_on_reversal_on_non_close() {
        let yaml = "
            v: 1
            id: rev-enter
            trade_id: eurusd-hs-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            veto_on_reversal: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::VetoOnReversalOnNonClose)
        );
    }

    #[test]
    fn validate_rejects_veto_on_reversal_without_price_window() {
        // A news-only reversal-close has no band to reverse off — the
        // flag would be inert, so reject it as a builder bug.
        let yaml = "
            v: 1
            id: rev-news-only
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [news]
            veto_on_reversal: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::VetoOnReversalWithoutPriceWindow)
        );
    }

    #[test]
    fn validate_rejects_mixing_old_and_new_close_gates() {
        // An operator porting from the deprecated form to the new form
        // who leaves a stray field behind: catch it early.
        let yaml = "
            v: 1
            id: iw-mixed
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [news]
            require_news_window: true
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MixedOldAndNewCloseGates)
        );
    }

    #[test]
    fn validate_rejects_mixing_old_price_with_new_window() {
        let yaml = "
            v: 1
            id: iw-mixed-price
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
            inside_window: [price]
            sr_bands: [[1.0, 1.1]]
            require_price_in_ranges: [[1.2, 1.3]]
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MixedOldAndNewCloseGates)
        );
    }

    #[test]
    fn close_without_any_gate_still_validates() {
        // Empty new-form gates leave the close unconditional, matching
        // the old form (require_*: absent). Operators can build "always
        // flatten" closes if they want; the consolidated reversal-close
        // adds gates only when needed.
        let yaml = "
            v: 1
            id: close-bare
            trade_id: t1
            not_after: \"2026-05-13T20:00:00Z\"
            action: close
            instrument: EUR_USD
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        intent.validate().expect("bare close still valid");
    }

    #[test]
    fn event_window_serialises_kebab_case() {
        // Wire form for the consolidated close-on-reversal gate. The
        // operator types `inside_window: [news, price]` and these are
        // the exact tokens that must round-trip.
        assert_eq!(
            serde_yaml::to_string(&EventWindow::News).unwrap().trim(),
            "news"
        );
        assert_eq!(
            serde_yaml::to_string(&EventWindow::Price).unwrap().trim(),
            "price"
        );
        let back: EventWindow = serde_yaml::from_str("news").unwrap();
        assert_eq!(back, EventWindow::News);
        let back: EventWindow = serde_yaml::from_str("price").unwrap();
        assert_eq!(back, EventWindow::Price);
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

    /// A well-formed M/W enter carries the baked `mw` params and *no*
    /// entry/stop_loss/take_profit — the worker derives those. The
    /// `enter requires entry/SL/TP` relaxation lives in resolution, so
    /// `validate` accepts this shape on its own.
    fn mw_enter_yaml() -> &'static str {
        "
            v: 1
            id: mw-eurusd-abc
            trade_id: mw-eurusd-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: short
            mw:
              neckline: 1.1120
              first_point: 1.1200
              runup_start: 1.1000
              spread_pips: 0.8
              pip_size: 0.0001
        "
    }

    #[test]
    fn validate_accepts_well_formed_mw_enter() {
        let intent: Intent = serde_yaml::from_str(mw_enter_yaml()).unwrap();
        let mw = intent.mw.expect("mw params present");
        assert!((mw.neckline - 1.1120).abs() < 1e-9);
        assert!((mw.first_point - 1.1200).abs() < 1e-9);
        assert!((mw.spread_pips - 0.8).abs() < 1e-9);
        intent.validate().unwrap();
    }

    #[test]
    fn validate_rejects_mw_on_non_enter() {
        // Same mw block, but action = close. mw only drives stop-entry
        // geometry, so it has no business on a non-enter.
        let yaml = mw_enter_yaml().replace("action: enter", "action: close");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(intent.validate(), Err(IntentValidationError::MwOnNonEnter));
    }

    #[test]
    fn validate_rejects_mw_non_finite_anchor() {
        let yaml = mw_enter_yaml().replace("neckline: 1.1120", "neckline: .nan");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MwFieldInvalid)
        );
    }

    #[test]
    fn validate_rejects_mw_negative_spread() {
        let yaml = mw_enter_yaml().replace("spread_pips: 0.8", "spread_pips: -0.1");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MwFieldInvalid)
        );
    }

    #[test]
    fn validate_rejects_mw_non_positive_pip_size() {
        let yaml = mw_enter_yaml().replace("pip_size: 0.0001", "pip_size: 0.0");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::MwFieldInvalid)
        );
    }

    #[test]
    fn mw_field_elided_when_none() {
        // A non-M/W intent must serialise byte-identically to pre-feature
        // intents — no `mw:` key at all.
        let yaml = "
            v: 1
            id: hs-eurusd-abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(intent.mw.is_none());
        let out = serde_yaml::to_string(&intent).unwrap();
        assert!(!out.contains("mw:"), "mw key leaked into wire form:\n{out}");
    }

    #[test]
    fn mw_enter_round_trips() {
        let intent: Intent = serde_yaml::from_str(mw_enter_yaml()).unwrap();
        let out = serde_yaml::to_string(&intent).unwrap();
        let back: Intent = serde_yaml::from_str(&out).unwrap();
        assert_eq!(intent.mw, back.mw);
    }

    /// A plain H&S enter with a baked top-level `pip_size`. No `mw` block.
    fn pip_size_enter_yaml() -> &'static str {
        "
            v: 1
            id: hs-usdjpy-abc
            trade_id: usdjpy-hs-1
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: USD_JPY
            direction: long
            entry: { type: stop, from: high, offset_pips: 1.0 }
            stop_loss: { from: low, offset_pips: -1.0 }
            take_profit: { absolute: 152.0 }
            pip_size: 0.01
        "
    }

    #[test]
    fn validate_accepts_top_level_pip_size() {
        let intent: Intent = serde_yaml::from_str(pip_size_enter_yaml()).unwrap();
        assert_eq!(intent.pip_size, Some(0.01));
        intent.validate().unwrap();
    }

    #[test]
    fn validate_rejects_zero_pip_size() {
        let yaml = pip_size_enter_yaml().replace("pip_size: 0.01", "pip_size: 0.0");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::PipSizeInvalid)
        );
    }

    #[test]
    fn validate_rejects_negative_pip_size() {
        let yaml = pip_size_enter_yaml().replace("pip_size: 0.01", "pip_size: -0.01");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::PipSizeInvalid)
        );
    }

    #[test]
    fn validate_rejects_nan_pip_size() {
        let yaml = pip_size_enter_yaml().replace("pip_size: 0.01", "pip_size: .nan");
        let intent: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            intent.validate(),
            Err(IntentValidationError::PipSizeInvalid)
        );
    }

    #[test]
    fn pip_size_elided_when_none() {
        // A pip-less intent must serialise byte-identically to pre-feature
        // intents — no `pip_size:` key at all.
        let yaml = "
            v: 1
            id: hs-eurusd-abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            direction: long
            entry: { type: market }
        ";
        let intent: Intent = serde_yaml::from_str(yaml).unwrap();
        assert!(intent.pip_size.is_none());
        let out = serde_yaml::to_string(&intent).unwrap();
        assert!(
            !out.contains("pip_size:"),
            "pip_size key leaked into wire form:\n{out}"
        );
    }

    #[test]
    fn pip_size_round_trips() {
        let intent: Intent = serde_yaml::from_str(pip_size_enter_yaml()).unwrap();
        let out = serde_yaml::to_string(&intent).unwrap();
        let back: Intent = serde_yaml::from_str(&out).unwrap();
        assert_eq!(intent.pip_size, back.pip_size);
    }
}
