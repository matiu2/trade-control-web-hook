//! Helpers for building `status` / `unlock` control intents and signed
//! bodies from CLI args.
//!
//! TradingView is not in the loop for these — the CLI POSTs them
//! directly. The shell fields are still required by the worker's
//! parser, so we fill them with concrete zero values plus a real
//! timestamp.

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Result, eyre};

use trade_control_core::intent::{Action, BrokerKind, Intent, VetoLevel};
use trade_control_core::sig::KEY_LEN;

/// How long a control envelope stays valid. Short — these are one-shot
/// commands run by hand, so we don't need a long replay window.
const CONTROL_TTL: Duration = Duration::minutes(5);

/// Placeholder instrument string used in `status` envelopes. The action
/// ignores the field, but `ALWAYS_REQUIRED` insists it be present.
const STATUS_INSTRUMENT: &str = "ALL";

/// Skeleton control `Intent` — fills the always-required fields plus
/// `Action` / `instrument` / `id`, leaves every optional field empty.
/// Specific builders override only what they care about.
fn control_skeleton(action: Action, instrument: &str, id: String, now: DateTime<Utc>) -> Intent {
    Intent {
        entry_level_vetos: Vec::new(),
        v: 1,
        id,
        not_before: None,
        not_after: now + CONTROL_TTL,
        action,
        instrument: instrument.to_string(),
        direction: None,
        entry: None,
        stop_loss: None,
        take_profit: None,
        risk_pct: trade_control_core::tunable::Tunable::Static(1.0),
        risk_amount: None,
        size_units: None,
        dry_run: None,
        cooldown_hours: None,
        min_r: None,
        broker: BrokerKind::Oanda,
        step: None,
        name: None,
        ttl_hours: trade_control_core::tunable::Tunable::Static(0),
        level: None,
        requires_preps: Vec::new(),
        vetos: Vec::new(),
        clears: Vec::new(),
        account: None,
        trade_id: None,
        max_retries: trade_control_core::tunable::Tunable::Static(0),
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
        blackout_close: trade_control_core::intent::BlackoutCloseAction::default(),
        breakeven: None,
        include_archived: false,
    }
}

/// Build a status `Intent`. `suffix` is a short random tag appended to the
/// id so two concurrent status calls don't collide on replay protection.
pub fn build_status_intent(now: DateTime<Utc>, suffix: &str) -> Intent {
    let id = format!("status-{}-{suffix}", now.format("%Y-%m-%dT%H%M%S"));
    control_skeleton(Action::Status, STATUS_INSTRUMENT, id, now)
}

/// Build an unlock `Intent` for a single instrument.
pub fn build_unlock_intent(instrument: &str, now: DateTime<Utc>, suffix: &str) -> Intent {
    let id = format!(
        "unlock-{instrument}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    control_skeleton(Action::Unlock, instrument, id, now)
}

/// Build a `register` Intent carrying a server-side [`TradePlan`].
///
/// The worker's `handle_register` requires the plan to be present and, if
/// the carrier intent has a `trade_id`, that it match the plan's — so we set
/// both `trade_id` and `instrument` from the plan. The id is unique per call
/// (timestamp + suffix) so a re-arm of the same trade isn't rejected by the
/// seen-id replay check. The plan rides the whole-body HMAC via `trade_plan`
/// (rendered as single-line flow JSON by `build_signed_body`).
///
/// `account` is the operator-facing account name that scopes the plan in KV
/// (`plan:{account}:{trade_id}`). It must match the account used for the
/// trade's vetos/preps so the engine's scoped lookups line up — otherwise the
/// plan lands in the global `_` scope and `plan list` shows `-` under ACCOUNT.
/// `None` keeps the legacy global scope for callers with no account context.
pub fn build_register_intent(
    plan: trade_control_core::trade_plan::TradePlan,
    account: Option<&str>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "register-{}-{}-{suffix}",
        plan.trade_id,
        now.format("%Y-%m-%dT%H%M%S")
    );
    let instrument = plan.instrument.clone();
    let trade_id = plan.trade_id.clone();
    let mut intent = control_skeleton(Action::Register, &instrument, id, now);
    intent.trade_id = Some(trade_id);
    intent.account = account.map(str::to_owned);
    intent.trade_plan = Some(plan);
    intent
}

/// Build a `market-info` query `Intent` for a single instrument. The
/// worker resolves `instrument` (a TradeNation MarketName, e.g.
/// `"Wall Street 30"`) against the broker and returns its session hours /
/// spread / margin. TradeNation-only — the skeleton defaults `broker` to
/// OANDA, so we override it here; the worker rejects a non-TN market-info.
pub fn build_market_info_intent(instrument: &str, now: DateTime<Utc>, suffix: &str) -> Intent {
    let id = format!(
        "market-info-{instrument}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    Intent {
        broker: BrokerKind::TradeNation,
        ..control_skeleton(Action::MarketInfo, instrument, id, now)
    }
}

/// Build a `plan-list` query `Intent`. Read-only; lists every registered
/// server-side plan. Like `status`, `instrument` is an ignored placeholder.
/// `include_archived` (the `--include-all` flag) also enumerates terminated
/// (vetoed/completed) plans retained in the archive keyspace.
pub fn build_plan_list_intent(now: DateTime<Utc>, suffix: &str, include_archived: bool) -> Intent {
    let id = format!("plan-list-{}-{suffix}", now.format("%Y-%m-%dT%H%M%S"));
    Intent {
        include_archived,
        ..control_skeleton(Action::PlanList, STATUS_INSTRUMENT, id, now)
    }
}

/// Build a `plan-show` query `Intent` for one `trade_id`. The worker scans
/// every account scope for a plan with that id. `instrument` is an ignored
/// placeholder; the target rides on `trade_id`.
pub fn build_plan_show_intent(trade_id: &str, now: DateTime<Utc>, suffix: &str) -> Intent {
    let id = format!(
        "plan-show-{trade_id}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::PlanShow, STATUS_INSTRUMENT, id, now);
    intent.trade_id = Some(trade_id.to_string());
    intent
}

/// Build a `plan-timeline` query `Intent` for one `trade_id`. The worker
/// returns every recorded `RequestRecord` for that trade (oldest first).
/// `instrument` is an ignored placeholder; the target rides on `trade_id`.
pub fn build_plan_timeline_intent(trade_id: &str, now: DateTime<Utc>, suffix: &str) -> Intent {
    let id = format!(
        "plan-timeline-{trade_id}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::PlanTimeline, STATUS_INSTRUMENT, id, now);
    intent.trade_id = Some(trade_id.to_string());
    intent
}

/// Build a `plan-delete` `Intent` for one `trade_id` — the inverse of
/// `register`. The worker scans every account scope and drops the matching
/// plan + plan-state rows. `instrument` is an ignored placeholder; the target
/// rides on `trade_id`.
pub fn build_plan_delete_intent(trade_id: &str, now: DateTime<Utc>, suffix: &str) -> Intent {
    let id = format!(
        "plan-delete-{trade_id}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::PlanDelete, STATUS_INSTRUMENT, id, now);
    intent.trade_id = Some(trade_id.to_string());
    intent
}

/// Build a `plan-purge` `Intent` for one `trade_id` — a superset of
/// `plan-delete` that also drops the trade's per-trade lifecycle rows
/// (entry-attempt / order-body / control-event), enumerable trade-scoped
/// controls (pause / news), and its R2 `ticks/` bundles. Use after journaling.
pub fn build_plan_purge_intent(trade_id: &str, now: DateTime<Utc>, suffix: &str) -> Intent {
    let id = format!(
        "plan-purge-{trade_id}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::PlanPurge, STATUS_INSTRUMENT, id, now);
    intent.trade_id = Some(trade_id.to_string());
    intent
}

/// Build a `purge-older-than` `Intent`. The `cutoff` is carried in
/// `not_before` (reused as "delete R2 bundles dated before this"); the worker
/// sweeps both `req/` and `ticks/`. No `trade_id` — it's a bulk date sweep.
pub fn build_purge_older_than_intent(
    cutoff: DateTime<Utc>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "purge-older-than-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::PurgeOlderThan, STATUS_INSTRUMENT, id, now);
    intent.not_before = Some(cutoff);
    intent
}

/// Build a `prep` Intent for a single (instrument, step) pair with a TTL.
///
/// `clears` is the list of other prep steps to drop before recording
/// this one — used to encode ordered prep sequences where landing an
/// upstream step must invalidate stale downstream preps.
pub fn build_prep_intent(
    instrument: &str,
    step: &str,
    ttl_hours: u32,
    clears: Vec<String>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "prep-{instrument}-{step}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::Prep, instrument, id, now);
    intent.step = Some(step.to_string());
    intent.ttl_hours = trade_control_core::tunable::Tunable::Static(ttl_hours);
    intent.clears = clears;
    intent
}

/// Build a `veto` Intent for a single (trade_id, instrument, name) with a
/// TTL. `level` controls broker side effects at fire time; pass `None` for
/// the default flag-only behaviour ([`VetoLevel::StopNextEntry`]). `clears`
/// lists other vetos to drop before recording this one. `trade_id` scopes
/// the veto to one setup so it can't bleed into another on the same
/// instrument — the worker rejects a veto without one.
#[allow(clippy::too_many_arguments)]
pub fn build_veto_intent(
    instrument: &str,
    trade_id: &str,
    name: &str,
    ttl_hours: u32,
    level: Option<VetoLevel>,
    clears: Vec<String>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "veto-{instrument}-{name}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::Veto, instrument, id, now);
    intent.name = Some(name.to_string());
    intent.trade_id = Some(trade_id.to_string());
    intent.ttl_hours = trade_control_core::tunable::Tunable::Static(ttl_hours);
    intent.level = level;
    intent.clears = clears;
    intent
}

/// Build a `clear-prep` Intent for a single (instrument, step) pair.
///
/// `account` scopes the clear: `None` targets the global (`_`) prep,
/// `Some("reversals")` an account-scoped one. It must match the scope the
/// prep was set under (the worker keys preps by `(account, instrument,
/// step)`), else the clear is a silent no-op.
pub fn build_clear_prep_intent(
    instrument: &str,
    step: &str,
    account: Option<&str>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "clear-prep-{instrument}-{step}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::ClearPrep, instrument, id, now);
    intent.step = Some(step.to_string());
    intent.account = account.map(str::to_string);
    intent
}

/// Build a `clear-veto` Intent for a single (instrument, name) pair.
///
/// `account` scopes the clear the same way as [`build_clear_prep_intent`]
/// (the worker keys vetos by `(account, trade_id, instrument, name)`).
pub fn build_clear_veto_intent(
    instrument: &str,
    trade_id: &str,
    name: &str,
    account: Option<&str>,
    now: DateTime<Utc>,
    suffix: &str,
) -> Intent {
    let id = format!(
        "clear-veto-{instrument}-{name}-{}-{suffix}",
        now.format("%Y-%m-%dT%H%M%S")
    );
    let mut intent = control_skeleton(Action::ClearVeto, instrument, id, now);
    intent.name = Some(name.to_string());
    intent.trade_id = Some(trade_id.to_string());
    intent.account = account.map(str::to_string);
    intent
}

/// Build the signed wire body for control-path intents (status,
/// unlock, prep, veto, clear-prep, clear-veto). The intent fields go at
/// the top level next to the shell fields, and a `sig:` line is appended
/// at the bottom. The shell carries concrete zeros + `now` — TradingView
/// isn't in this loop. See `core::sig` for the canonical form.
pub fn wrap_signed(intent: &Intent, key: &[u8; KEY_LEN], now: DateTime<Utc>) -> Result<String> {
    build_signed_body(intent, key, &shell_for_control(now))
}

/// Build a signed body for a TradingView alert that fires from a Pine
/// study (e.g. `Candle Signals`'s `Long/Short Pattern` alertcondition).
/// Shell includes the `{{plot("…")}}` placeholders that only resolve
/// when the alert is bound to a study — see [`shell_for_tv_template_pine`].
///
/// When the intent opts into bar-based order expiry (`expiry_bars`
/// set), the shell additionally carries the
/// `next_candle_timestamp_1..5` menu placeholders. They're appended
/// only in that case so trades that don't use the feature stay
/// byte-identical on the wire and don't depend on an indicator that
/// ships the menu plots — an operator who sets `expiry_bars` is
/// asserting they're on the v2+ indicator that does.
pub fn wrap_signed_template(intent: &Intent, key: &[u8; KEY_LEN]) -> Result<String> {
    let mut shell = shell_for_tv_template_pine();
    if intent.expiry_bars.is_some() {
        shell.extend(shell_for_next_candle_menu());
    }
    build_signed_body(intent, key, &shell)
}

/// Build a signed body for a TradingView alert that fires from a
/// drawing (horizontal line, vertical line, trendline). Drawings have
/// no Pine context, so `{{plot("…")}}` placeholders would be delivered
/// literally and crash the worker's YAML parser. Only the four
/// universally-substituted placeholders (`close`/`high`/`low`/`time`)
/// are included; signal-bar fields are simply absent and the worker's
/// optional `Shell` fields stay `None`.
pub fn wrap_signed_template_drawing(intent: &Intent, key: &[u8; KEY_LEN]) -> Result<String> {
    build_signed_body(intent, key, &shell_for_tv_template_drawing())
}

/// Build a signed body for an enter intent **POSTed directly to the
/// worker** (no TradingView in the loop) — the position-tool direct
/// entry. There are no `{{plot(…)}}` placeholders to substitute, so the
/// shell carries a concrete `reference_price` as `close`/`high`/`low`
/// and `now` as `time`.
///
/// `reference_price` must be the entry price the operator drew: the
/// worker's resolver range-checks `stop_loss < close < take_profit`
/// (long) / `take_profit < close < stop_loss` (short) and derives the
/// R-multiple from it, so a placeholder zero would be rejected as
/// `EntryOutsideRange`. The actual broker fill for a `Market` entry is
/// at live price; this reference is the operator's drawn entry, which is
/// the right basis for the geometry/R checks.
pub fn wrap_signed_direct_enter(
    intent: &Intent,
    key: &[u8; KEY_LEN],
    reference_price: f64,
    now: DateTime<Utc>,
) -> Result<String> {
    build_signed_body(intent, key, &shell_for_direct_enter(reference_price, now))
}

/// Self-contained shell for a directly-POSTed enter: the drawn entry
/// price stamped on `close`/`high`/`low`, and `now` on `time`.
fn shell_for_direct_enter(reference_price: f64, now: DateTime<Utc>) -> Vec<(&'static str, String)> {
    let price = format!("{reference_price}");
    vec![
        ("close", price.clone()),
        ("high", price.clone()),
        ("low", price),
        ("time", format!("\"{}\"", now.to_rfc3339())),
    ]
}

fn build_signed_body(
    intent: &Intent,
    key: &[u8; KEY_LEN],
    shell_lines: &[(&str, String)],
) -> Result<String> {
    // Serialise the intent and re-parse so each field becomes a YAML
    // value. Then emit each top-level field on a single line, where
    // nested values are flow-style (`{type: market}`). Both the CLI
    // emit step and the worker verify step line-scan the same text.
    let intent_yaml = serde_yaml::to_string(intent).map_err(|e| eyre!("serialise intent: {e}"))?;
    let intent_value: serde_yaml::Value =
        serde_yaml::from_str(&intent_yaml).map_err(|e| eyre!("re-parse intent: {e}"))?;
    let intent_map = intent_value
        .as_mapping()
        .ok_or_else(|| eyre!("intent did not serialise to a mapping"))?;

    let mut lines: Vec<String> = Vec::new();
    for (k, v) in shell_lines {
        lines.push(format!("{k}: {v}"));
    }
    for (k, v) in intent_map {
        let key_str = k.as_str().ok_or_else(|| eyre!("non-string intent key"))?;
        let val_str = render_value(v)?;
        lines.push(format!("{key_str}: {val_str}"));
    }
    // Build the body without the sig line, then line-scan it the same
    // way the worker will. This guarantees the canonical pair list is
    // exactly what the verify side will reconstruct.
    let body_without_sig = format!("{}\n", lines.join("\n"));
    let pairs = trade_control_core::incoming::signed_pairs_from_text(&body_without_sig)
        .map_err(|e| eyre!("build signed pairs: {e}"))?;
    let sig = trade_control_core::sig::sign(key, &pairs).map_err(|e| eyre!("sign: {e}"))?;
    Ok(format!("{body_without_sig}sig: \"{sig}\"\n"))
}

fn shell_for_control(now: DateTime<Utc>) -> Vec<(&'static str, String)> {
    vec![
        ("close", "0".to_string()),
        ("high", "0".to_string()),
        ("low", "0".to_string()),
        ("time", format!("\"{}\"", now.to_rfc3339())),
    ]
}

fn shell_for_tv_template_pine() -> Vec<(&'static str, String)> {
    let mut lines = shell_for_tv_template_drawing();
    // signal_* / golden / atr come from candle-signals-v2.pine's
    // hidden plots, populated when the Long/Short Pattern
    // alertcondition fires. The worker treats them as optional —
    // pre-2026-05 v2 signed templates parse unchanged. Note:
    // {{plot("…")}} names the Pine plot title; the YAML key on
    // the left is the wire-key the worker deserialises into the
    // Shell. They don't have to match — `golden` / `atr` live
    // under `signal_golden` / `signal_atr` Pine titles.
    lines.extend([
        ("signal_high", "{{plot(\"signal_high\")}}".to_string()),
        ("signal_low", "{{plot(\"signal_low\")}}".to_string()),
        ("signal_range", "{{plot(\"signal_range\")}}".to_string()),
        (
            "signal_start_time",
            "{{plot(\"signal_start_time\")}}".to_string(),
        ),
        ("signal_kind", "{{plot(\"signal_kind\")}}".to_string()),
        ("golden", "{{plot(\"signal_golden\")}}".to_string()),
        ("atr", "{{plot(\"signal_atr\")}}".to_string()),
        (
            "signal_confirmed",
            "{{plot(\"signal_confirmed\")}}".to_string(),
        ),
        ("recent_high", "{{plot(\"recent_high\")}}".to_string()),
        ("recent_low", "{{plot(\"recent_low\")}}".to_string()),
    ]);
    lines
}

/// The five forward bar-close menu placeholders, filled by Pine at
/// fire-time via `time_close(timeframe.period, bars_back=-k)`. Appended
/// to the Pine-bound enter shell only when `expiry_bars` is set. The
/// worker indexes this menu with the signed `expiry_bars` to derive the
/// order's `cancel_at`. Like the `signal_*` plots, the YAML key on the
/// left is what the worker deserialises; the `{{plot("…")}}` names the
/// Pine plot title.
fn shell_for_next_candle_menu() -> Vec<(&'static str, String)> {
    (1..=5)
        .map(|n| {
            let key: &'static str = match n {
                1 => "next_candle_timestamp_1",
                2 => "next_candle_timestamp_2",
                3 => "next_candle_timestamp_3",
                4 => "next_candle_timestamp_4",
                _ => "next_candle_timestamp_5",
            };
            (key, format!("{{{{plot(\"next_candle_timestamp_{n}\")}}}}"))
        })
        .collect()
}

fn shell_for_tv_template_drawing() -> Vec<(&'static str, String)> {
    vec![
        ("close", "{{close}}".to_string()),
        ("high", "{{high}}".to_string()),
        ("low", "{{low}}".to_string()),
        // `open` is a TradingView built-in (no plot-index risk). It rides
        // every TV-template shell so the M/W enter can compute candle-body
        // extremes (rogue-wick handling, dynamic neckline revision). Its
        // value is unsigned (TV fills it post-sign); see
        // `trade_control_core::sig::UNSIGNED_VALUE_KEYS`. Optional on the
        // worker side, so older charts without it still verify.
        ("open", "{{open}}".to_string()),
        ("time", "\"{{time}}\"".to_string()),
    ]
}

/// Render a YAML value as a single-line string, suitable for going on
/// the right side of a top-level `key: value` line. Scalars use their
/// raw form; nested values use serde_yaml's flow-style serialisation.
fn render_value(v: &serde_yaml::Value) -> Result<String> {
    match v {
        serde_yaml::Value::Null => Ok("~".to_string()),
        serde_yaml::Value::Bool(b) => Ok(b.to_string()),
        serde_yaml::Value::Number(n) => Ok(n.to_string()),
        serde_yaml::Value::String(s) => {
            // Quote strings that might confuse the line-scan parser
            // (timestamps with colons, etc.). Quoting is safe for any
            // string — the worker strips matching quotes.
            if needs_quoting(s) {
                Ok(format!("\"{}\"", s.replace('"', "\\\"")))
            } else {
                Ok(s.clone())
            }
        }
        _ => {
            // serde_yaml emits block-style by default. To force a single
            // line for nested structures we serialise via JSON (which is
            // a valid subset of flow YAML).
            let json = serde_json::to_string(v).map_err(|e| eyre!("flow-style serialise: {e}"))?;
            Ok(json)
        }
    }
}

fn needs_quoting(s: &str) -> bool {
    // Quote anything containing characters that aren't safe in a plain
    // YAML scalar on a single line: colons (timestamps), leading/trailing
    // whitespace, or starts that YAML would parse as a special type.
    if s.is_empty() {
        return true;
    }
    if s.chars()
        .any(|c| matches!(c, ':' | '#' | '\'' | '"' | '\n'))
    {
        return true;
    }
    if s.starts_with(|c: char| c.is_whitespace()) || s.ends_with(|c: char| c.is_whitespace()) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> DateTime<Utc> {
        "2026-05-14T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn status_intent_has_status_action() {
        let intent = build_status_intent(t(), "ab12");
        assert_eq!(intent.action, Action::Status);
        assert_eq!(intent.instrument, "ALL");
        assert_eq!(intent.not_after, t() + CONTROL_TTL);
        assert!(intent.id.starts_with("status-"));
        assert!(intent.id.ends_with("-ab12"));
    }

    #[test]
    fn unlock_intent_carries_instrument() {
        let intent = build_unlock_intent("EUR_USD", t(), "cd34");
        assert_eq!(intent.action, Action::Unlock);
        assert_eq!(intent.instrument, "EUR_USD");
        assert_eq!(intent.not_after, t() + CONTROL_TTL);
        assert!(intent.id.starts_with("unlock-EUR_USD-"));
        assert!(intent.id.ends_with("-cd34"));
    }

    #[test]
    fn intent_round_trips_through_yaml() {
        // The Intent we build must deserialise back through serde_yaml so
        // the worker's parser is happy with it end-to-end.
        let intent = build_unlock_intent("USD_JPY", t(), "x1");
        let yaml = serde_yaml::to_string(&intent).unwrap();
        let parsed: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.action, Action::Unlock);
        assert_eq!(parsed.instrument, "USD_JPY");
    }

    #[test]
    fn prep_intent_round_trips() {
        let intent = build_prep_intent("EUR_USD", "break-and-close", 4, Vec::new(), t(), "ab12");
        assert_eq!(intent.action, Action::Prep);
        assert_eq!(intent.instrument, "EUR_USD");
        assert_eq!(intent.step.as_deref(), Some("break-and-close"));
        match &intent.ttl_hours {
            trade_control_core::tunable::Tunable::Static(n) => assert_eq!(*n, 4),
            other => panic!("expected Static(4) ttl_hours, got {other:?}"),
        }
        assert!(intent.clears.is_empty());
        let yaml = serde_yaml::to_string(&intent).unwrap();
        let parsed: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.action, Action::Prep);
        assert_eq!(parsed.step.as_deref(), Some("break-and-close"));
        assert!(parsed.clears.is_empty());
    }

    #[test]
    fn prep_intent_carries_clears_through_yaml() {
        let intent = build_prep_intent(
            "EUR_USD",
            "break-and-close",
            4,
            vec!["retest".into()],
            t(),
            "ab12",
        );
        assert_eq!(intent.clears, vec!["retest".to_string()]);
        let yaml = serde_yaml::to_string(&intent).unwrap();
        assert!(yaml.contains("clears:"), "yaml was:\n{yaml}");
        let parsed: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.clears, vec!["retest".to_string()]);
    }

    #[test]
    fn veto_intent_round_trips() {
        let intent = build_veto_intent(
            "EUR_USD",
            "eurusd-hs-1",
            "news-window",
            6,
            None,
            Vec::new(),
            t(),
            "cd34",
        );
        assert_eq!(intent.action, Action::Veto);
        assert_eq!(intent.name.as_deref(), Some("news-window"));
        assert_eq!(intent.trade_id.as_deref(), Some("eurusd-hs-1"));
        match &intent.ttl_hours {
            trade_control_core::tunable::Tunable::Static(n) => assert_eq!(*n, 6),
            other => panic!("expected Static(6) ttl_hours, got {other:?}"),
        }
        assert!(intent.clears.is_empty());
        let yaml = serde_yaml::to_string(&intent).unwrap();
        let parsed: Intent = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.action, Action::Veto);
        // The built intent must pass worker-side validation (trade_id
        // is now mandatory on veto).
        intent.validate().expect("veto intent should validate");
    }

    #[test]
    fn clear_prep_intent_carries_step() {
        // Default (global) scope: no account.
        let intent = build_clear_prep_intent("EUR_USD", "retest", None, t(), "ef56");
        assert_eq!(intent.action, Action::ClearPrep);
        assert_eq!(intent.step.as_deref(), Some("retest"));
        assert_eq!(intent.account, None);
        assert!(matches!(
            intent.ttl_hours,
            trade_control_core::tunable::Tunable::Static(0)
        ));
    }

    #[test]
    fn clear_prep_intent_carries_account_scope() {
        let intent = build_clear_prep_intent("EUR_USD", "retest", Some("reversals"), t(), "ef56");
        assert_eq!(intent.account.as_deref(), Some("reversals"));
    }

    #[test]
    fn clear_veto_intent_carries_name() {
        let intent =
            build_clear_veto_intent("EUR_USD", "eurusd-hs-1", "news-window", None, t(), "gh78");
        assert_eq!(intent.action, Action::ClearVeto);
        assert_eq!(intent.name.as_deref(), Some("news-window"));
        assert_eq!(intent.trade_id.as_deref(), Some("eurusd-hs-1"));
        intent
            .validate()
            .expect("clear-veto intent should validate");
    }

    #[test]
    fn signed_control_body_contains_concrete_shell_and_sig() {
        let key = [0u8; KEY_LEN];
        let intent = build_status_intent(t(), "ab12");
        let body = wrap_signed(&intent, &key, t()).unwrap();
        // Shell must be concrete (no TradingView placeholders).
        assert!(body.contains("close: 0"), "body was:\n{body}");
        assert!(body.contains("high: 0"), "body was:\n{body}");
        assert!(body.contains("low: 0"), "body was:\n{body}");
        assert!(body.contains("time:"), "body was:\n{body}");
        assert!(!body.contains("{{close}}"), "body was:\n{body}");
        assert!(body.contains("sig: \"v1-sig."), "body was:\n{body}");
    }

    /// A minimal one-rule plan for register signing tests.
    fn sample_plan() -> trade_control_core::trade_plan::TradePlan {
        use trade_control_core::broker::Granularity;
        use trade_control_core::trade_plan::{
            BarEvent, ConditionRule, CrossDir, FireMode, RuleKind, TradePlan, Trigger,
        };
        let rule_intent = control_skeleton(Action::Veto, "EUR_USD", "veto-1".into(), t());
        TradePlan {
            trade_id: "eurusd-hs-7".into(),
            instrument: "EUR_USD".into(),
            direction: trade_control_core::intent::Direction::Short,
            granularity: Granularity::H1,
            pip_size: 0.0001,
            rules: vec![ConditionRule {
                rule_id: "01-veto-too-high".into(),
                trigger: Trigger::HorizontalCross {
                    level: 1.2000,
                    dir: CrossDir::Up,
                    bar: BarEvent::Intrabar,
                },
                fire_mode: FireMode::Once,
                intent: rule_intent,
                kind: RuleKind::SetupInvalidation,
            }],
            shadow: false,
            cross_buffer_pct: 0.0,
            cross_buffer_atr: 0.0,
            retest_atr_step: trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            replay_start: None,
            armed_at: None,
            armed_sentiment: None,
        }
    }

    #[test]
    fn register_intent_binds_trade_id_and_carries_plan() {
        let intent = build_register_intent(sample_plan(), Some("reversals"), t(), "cd34");
        assert_eq!(intent.action, Action::Register);
        // The carrier's trade_id / instrument must match the plan so the
        // worker's handle_register cross-check passes.
        assert_eq!(intent.trade_id.as_deref(), Some("eurusd-hs-7"));
        assert_eq!(intent.instrument, "EUR_USD");
        // The account scopes the plan's KV row (`plan:{account}:{trade_id}`) so
        // `plan list` shows it under ACCOUNT instead of `-`.
        assert_eq!(intent.account.as_deref(), Some("reversals"));
        let plan = intent.trade_plan.as_ref().expect("plan present");
        assert_eq!(plan.rules.len(), 1);
    }

    /// The whole plan must ride the signature: a signed register body
    /// round-trips through the worker's `parse_and_verify`, reconstructing
    /// the nested `trade_plan` intact — proving the single-line flow-JSON
    /// rendering is both signed and re-parseable.
    #[test]
    fn signed_register_body_round_trips_through_verify() {
        let key = [7u8; KEY_LEN];
        let intent = build_register_intent(sample_plan(), None, t(), "cd34");
        let body = wrap_signed(&intent, &key, t()).unwrap();
        // The plan is a single top-level line (flow JSON), not block YAML.
        assert!(body.contains("trade_plan: {"), "body was:\n{body}");
        let verified = trade_control_core::incoming::parse_and_verify(&body, &key, t())
            .expect("register body should verify");
        assert_eq!(verified.intent.action, Action::Register);
        let plan = verified
            .intent
            .trade_plan
            .as_ref()
            .expect("plan survived verify");
        assert_eq!(plan.trade_id, "eurusd-hs-7");
        assert_eq!(plan.rules.len(), 1);
        assert_eq!(plan.rules[0].rule_id, "01-veto-too-high");
    }

    /// Tampering with the plan after signing must fail verification — the
    /// plan is inside the HMAC, not appended unsigned.
    #[test]
    fn tampered_register_plan_is_rejected() {
        let key = [7u8; KEY_LEN];
        let intent = build_register_intent(sample_plan(), None, t(), "cd34");
        let body = wrap_signed(&intent, &key, t()).unwrap();
        // Flip a digit in the plan's baked level (1.2 → 9.2) without re-signing.
        let tampered = body.replace("\"level\":1.2", "\"level\":9.2");
        assert_ne!(tampered, body, "tamper substitution must have matched");
        let err = trade_control_core::incoming::parse_and_verify(&tampered, &key, t())
            .expect_err("tampered plan must be rejected");
        assert!(
            matches!(err, trade_control_core::incoming::IncomingError::Sig(_)),
            "expected a signature error, got {err:?}"
        );
    }
}
