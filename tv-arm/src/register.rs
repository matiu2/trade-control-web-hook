//! Registering a built trade as a server-side engine plan, and replacing a
//! prior one.
//!
//! This is the tail of an arm: by the time anything here runs, the signed alert
//! bundle is already on disk, so a failure to register loses the *registration*
//! but never the trade.
//!
//! Three things live here because they're one concern — getting exactly one
//! current plan per setup into the engine:
//!
//! - [`register_trade_plan`] folds the built trade into a signed `register`
//!   plan and POSTs it (or, for `--plan-out` without `--register-plan`, just
//!   writes the JSON for offline replay).
//! - [`replace_existing_plan`] clears the prior plan first, so a re-arm doesn't
//!   leave a stale plan ticking beside the new one.
//! - [`resolve_replace_target`] is the pure decision of *which* plan that is —
//!   split out from the I/O so its rules are unit-testable without a worker.

use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use tracing::info;
use trade_control_cli as cli;
use trade_control_conventions::Direction;
use trade_control_core::sig::KEY_LEN;

use crate::broker_kind::kind_to_broker;
use crate::broker_read::read_mid_blocking;
use crate::control_bundle::{Bundle, NewsKind, PauseKind};
use crate::pipeline::effective_arm_time;
use crate::plan_geometry::PlanGeometry;
use crate::register_post::{post_intent_blocking, post_register_blocking};
use crate::trade_plan_build::{append_control_rules, build_trade_plan, resolution_to_granularity};

/// One registered plan as seen in the `plan-list` response — only the two
/// fields `--replace` needs to resolve a target. Other fields are ignored.
#[derive(serde::Deserialize)]
struct PlanListEntry {
    trade_id: String,
    instrument: String,
}

/// Decide which trade_id `--replace` should delete.
///
/// - An explicit, non-empty `target` is used verbatim (delete exactly that).
/// - An empty `target` (bare `--replace`) auto-resolves by instrument: exactly
///   one registered plan on `instrument` → delete it; none → `Ok(None)`
///   (nothing to clear, proceed); more than one → a hard error naming the
///   candidates so the operator re-runs with an explicit id.
///
/// Pure (takes the parsed plan list), so the resolution rules are unit-tested
/// without the worker.
fn resolve_replace_target(
    target: &str,
    instrument: &str,
    plans: &[PlanListEntry],
) -> Result<Option<String>> {
    let target = target.trim();
    if !target.is_empty() {
        return Ok(Some(target.to_string()));
    }
    let matches: Vec<&str> = plans
        .iter()
        .filter(|p| p.instrument == instrument)
        .map(|p| p.trade_id.as_str())
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some((*only).to_string())),
        many => Err(eyre!(
            "--replace: {} plans registered for {instrument} ({}); \
             pass the trade_id explicitly: --replace <trade-id>",
            many.len(),
            many.join(", "),
        )),
    }
}

/// Re-arm support for `--register-plan`: resolve the prior plan for this
/// instrument (or the explicit `--replace <id>`) and delete it from the engine
/// before the fresh register. Queries `plan-list`, applies
/// [`resolve_replace_target`], then POSTs a signed `plan-delete` (which clears
/// both the `plan:` and `plan-state:` KV rows). A no-target resolution is a
/// logged no-op. Hard-errors on an ambiguous auto-resolve or a worker rejection
/// — better to stop than to leave a stale plan ticking beside the new one.
pub(crate) fn replace_existing_plan(
    target: &str,
    instrument: &str,
    key: &[u8; KEY_LEN],
    now: DateTime<Utc>,
) -> Result<()> {
    // Query the registered plans so an auto-resolve can count them per
    // instrument. Live plans only (`include_archived: false`) — a terminated
    // plan in the archive must not count against the per-instrument tally.
    let list_intent = cli::build_plan_list_intent(now, &register_suffix(now), false);
    let list_body = cli::wrap_signed(&list_intent, key, now).wrap_err("sign plan-list intent")?;
    let yaml = post_intent_blocking(list_body).wrap_err("query plan-list for --replace")?;
    let plans: Vec<PlanListEntry> =
        serde_yaml::from_str(&yaml).wrap_err("parse plan-list response")?;

    let Some(trade_id) = resolve_replace_target(target, instrument, &plans)? else {
        info!(instrument = %instrument, "--replace: no existing plan for this instrument; nothing to delete");
        return Ok(());
    };

    let del_intent = cli::build_plan_delete_intent(&trade_id, now, &register_suffix(now));
    let del_body = cli::wrap_signed(&del_intent, key, now).wrap_err("sign plan-delete intent")?;
    info!(trade_id = %trade_id, instrument = %instrument, "--replace: deleting prior registered plan");
    post_intent_blocking(del_body).wrap_err("delete prior plan for --replace")?;
    info!(trade_id = %trade_id, "--replace: prior plan deleted");
    Ok(())
}

/// Fold the built trade into one signed `register` `TradePlan` and (when
/// `register` is true) POST it to the worker's server-side engine.
///
/// When `register` is false (`--plan-out` without `--register-plan`) the plan is
/// still built and, if `plan_out` is set, written to disk — but no worker POST
/// happens. This is the offline "just give me the JSON for replay" path.
///
/// The plan re-expresses every alert's condition as an engine [`Trigger`] (via
/// [`build_trade_plan`], the inverse of `alert_spec`) and carries each alert's
/// embedded intent verbatim. The pause/news/calendar **control bars** built
/// upstream are folded in too — one `TimeReached` rule per bundle alert (see
/// [`append_control_rules`]) — so the registered plan opens/closes the same
/// blackout + news windows the legacy TV-alert path used to POST. It's
/// signed with the same key + whole-body HMAC as the control intents (the plan
/// rides `trade_plan` as single-line flow JSON, so it's fully signed) and
/// POSTed directly to the baked webhook.
///
/// Hard-errors on an unsupported chart resolution or a worker rejection — but
/// the signed alert bundle is already on disk by the time this runs, so the
/// trade isn't lost on a register failure.
/// Takes `geom`, **not `Roles`** — the geometry is extracted exactly once, in
/// [`run`], and passed down. This function used to take `&Roles` and re-derive
/// `PlanGeometry::from_roles` itself, which meant the extraction ran *twice* per
/// arm off two different borrows of the same drawings. Harmless in practice
/// (`from_roles` is pure), but it re-opened the seam `PlanGeometry` exists to
/// close: as long as a `&Roles` reaches this far, a future edit can read a
/// drawing here that no frozen spec could supply. Same reasoning as
/// `resolve_hs_trade` losing its `roles` parameter — close it by type.
#[allow(clippy::too_many_arguments)]
pub(crate) fn register_trade_plan(
    built_trade: &cli::BuiltTrade,
    direction: Direction,
    geom: &PlanGeometry,
    resolution: &str,
    pause_bundles: &[Bundle<PauseKind>],
    news_bundles: &[Bundle<NewsKind>],
    key: &[u8; KEY_LEN],
    account: &str,
    now: DateTime<Utc>,
    shadow: bool,
    plan_out: Option<&Path>,
    register: bool,
    replay_start: Option<i64>,
    retest_atr_step: f64,
    cross_buffer_pct: f64,
    cross_buffer_atr: f64,
    bcr_require_golden: bool,
    armed_sentiment: Option<trade_control_core::plan_sentiment::PlanSentiment>,
    trend: crate::trade_plan_build::TrendFollow,
) -> Result<()> {
    use cli::TradePattern;
    let is_mw = matches!(built_trade.spec.pattern, TradePattern::M | TradePattern::W);
    let granularity = resolution_to_granularity(resolution).ok_or_else(|| {
        eyre!(
            "chart resolution {resolution:?} has no engine granularity; \
             cannot register a server-side plan (supported: 1/5/15/60/240/D)"
        )
    })?;
    // Effective arm time: when `--start` (journaling replay) is given, record
    // the plan *as if* it were armed at that cursor, not at the wall-clock run
    // time — so a replayed arming reads back the historical moment. Otherwise
    // use the real `now`.
    let armed_at = effective_arm_time(replay_start, now);
    // Pullback prep (--pull-back): capture the arm-time anchor (live mid) and the
    // ATR multiple so `build_trade_plan` can bake them onto the trigger. Read only
    // when a pullback is armed. A live-mid read failure is fatal — a bad/guessed
    // anchor would silently mis-fire every pullback (same discipline as the M/W
    // arm-time spread read).
    let pullback_arm = match built_trade.spec.pull_back {
        Some(atr_mult) => {
            let broker = built_trade.spec.broker;
            let anchor_open = read_mid_blocking(kind_to_broker(broker), &built_trade.instrument)
                .wrap_err("read live mid for --pull-back anchor")?;
            Some(crate::trade_plan_build::PullbackArm {
                anchor_open,
                atr_mult,
            })
        }
        None => None,
    };
    // Arm-time screenshot: if the operator hit TradingView's camera button
    // before arming, the clipboard holds a snapshot URL — bake it on so the
    // journal can show the chart as it looked at this moment. Fail-soft: a
    // clipboard holding anything else (or no clipboard tool at all) yields
    // `None` and arming proceeds, same as `armed_sentiment`.
    let screenshot_url = crate::clipboard::screenshot_url_from_clipboard();
    let mut plan = build_trade_plan(
        &built_trade.trade_id,
        &built_trade.instrument,
        &built_trade.alerts,
        direction,
        geom,
        granularity,
        is_mw,
        shadow,
        replay_start,
        retest_atr_step,
        cross_buffer_pct,
        cross_buffer_atr,
        bcr_require_golden,
        armed_at,
        armed_sentiment,
        pullback_arm,
        screenshot_url,
        trend,
    );
    // Unwrap the tv-arm bundle wrappers to the cli `BuiltPause`/`BuiltNews` the
    // appender reads (each carries the signed intents + window times).
    let pauses: Vec<&cli::BuiltPause> = pause_bundles.iter().map(|b| &b.built).collect();
    let newses: Vec<&cli::BuiltNews> = news_bundles.iter().map(|b| &b.built).collect();
    append_control_rules(&mut plan, &pauses, &newses);
    let rule_count = plan.rules.len();
    // Dump the fully-built plan (control rules folded in) for offline replay,
    // before `build_register_intent` moves it into the register intent.
    if let Some(path) = plan_out {
        let json = serde_json::to_string_pretty(&plan).wrap_err("serialise trade plan")?;
        fs::write(path, json).wrap_err_with(|| format!("write plan to {}", path.display()))?;
        info!(path = %path.display(), "wrote trade plan JSON");
    }
    // Offline path: `--plan-out` without `--register-plan` stops here — the JSON
    // is on disk, but we never POST the plan to the worker.
    if !register {
        info!(
            trade_id = %built_trade.trade_id,
            "plan built (--plan-out only); not registering with worker"
        );
        return Ok(());
    }
    // Mint a fresh register intent carrying the plan, sign it, POST it.
    let suffix = register_suffix(now);
    let intent = cli::build_register_intent(plan, Some(account), now, &suffix);
    let body = cli::wrap_signed(&intent, key, now).wrap_err("sign register intent")?;
    info!(
        trade_id = %built_trade.trade_id,
        instrument = %built_trade.instrument,
        granularity = ?granularity,
        rules = rule_count,
        shadow = shadow,
        "registering server-side trade plan",
    );
    post_register_blocking(body).wrap_err("register trade plan with worker")?;
    info!(trade_id = %built_trade.trade_id, "trade plan registered");
    Ok(())
}

/// A short per-call tag for the register intent id so two arms of the same
/// trade_id in the same second don't collide on the worker's seen-id check.
/// Derived from the sub-second clock — no rand dependency.
fn register_suffix(now: DateTime<Utc>) -> String {
    format!("{:06}", now.timestamp_subsec_micros() % 1_000_000)
}

#[cfg(test)]
mod tests {
    //! `--replace` target resolution. The I/O halves (`replace_existing_plan`,
    //! `register_trade_plan`) are covered by the demo protocol rather than
    //! here — which is exactly why the *decision* was split out pure.

    use super::*;

    fn plan_entry(trade_id: &str, instrument: &str) -> PlanListEntry {
        PlanListEntry {
            trade_id: trade_id.into(),
            instrument: instrument.into(),
        }
    }

    /// The wire the unit tests below `build_trade_plan` cannot see:
    /// `register_trade_plan` is the ONE production call site, and if it passed a
    /// hardcoded `TrendFollow(false)` (or the wrong positional `bool`) every
    /// `--trend` arm would silently emit a wick-triggered pcl veto — the exact
    /// bug the flag exists to fix, back again, with green tests and a plausible
    /// plan. So this drives the real function end-to-end and reads the plan JSON
    /// it writes.
    ///
    /// Runs offline: `register: false` + `plan_out: Some` returns before any
    /// POST, and with no `--pull-back` there is no live-mid read. The clipboard
    /// probe is fail-soft.
    ///
    /// `tag` names the caller, so two tests running in parallel don't share a
    /// scratch directory (one would delete it while the other was writing).
    fn plan_json_for(trend: bool, tag: &str) -> serde_json::Value {
        use crate::args::Args;
        use clap::Parser;
        use trade_control_conventions::Direction as ConvDirection;

        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("valid");
        let mut argv = vec!["tv-arm"];
        if trend {
            argv.push("--trend");
        }
        let args = Args::try_parse_from(argv).expect("parse").apply_aliases();

        // Fib head 1.2000 / neckline 1.1000 => TP 1.0000, pcl-exhausted 1.0200.
        let geom = crate::plan_geometry::PlanGeometry {
            invalidation: Some(1.2000),
            fib_head_neckline: Some((1.2000, 1.1000)),
            trade_expiry_epoch: Some(now.timestamp() + 86_400),
            ..Default::default()
        };
        let spec = crate::hs_resolve::build_trade_spec(
            &args,
            "EUR_USD",
            "ms-oanda-1",
            trade_control_conventions::Broker::Oanda,
            ConvDirection::Short,
            now + chrono::Duration::days(1),
            1.0000,
            &geom,
            false,
            0.0001,
            0.0001,
            Vec::new(),
            None,
        );
        let built = cli::build_trade_from_spec(spec, now, cli::BuildStrictness::Lenient)
            .expect("build trade bundle");

        let dir = std::env::temp_dir().join(format!("tv-arm-trend-wire-{tag}-{trend}"));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("plan.json");
        register_trade_plan(
            &built,
            ConvDirection::Short,
            &geom,
            "60",
            &[],
            &[],
            &[0u8; KEY_LEN],
            "ms-oanda-1",
            now,
            false,       // shadow
            Some(&path), // plan_out
            false,       // register — stops before any POST
            None,        // replay_start
            trade_control_core::trade_plan::DEFAULT_RETEST_ATR_STEP,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_PCT,
            trade_control_core::trade_plan::DEFAULT_CROSS_BUFFER_ATR,
            false, // bcr_require_golden
            None,  // armed_sentiment
            crate::trade_plan_build::TrendFollow(args.trend),
        )
        .expect("build + write plan offline");

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read plan"))
                .expect("parse plan json");
        std::fs::remove_dir_all(&dir).ok();
        json
    }

    /// The pcl-exhausted rule as the emitted plan JSON spells it.
    fn pcl_trigger(plan: &serde_json::Value) -> serde_json::Value {
        plan["rules"]
            .as_array()
            .expect("rules array")
            .iter()
            .find(|r| r["rule_id"] == "01-veto-too-low")
            .expect("short H&S emits a `01-veto-too-low` pcl-exhausted rule")["trigger"]
            .clone()
    }

    #[test]
    fn trend_flag_reaches_the_emitted_plan_json() {
        // Default arm: wick-through straddle (today's behaviour, unchanged).
        let off = pcl_trigger(&plan_json_for(false, "reaches"));
        assert_eq!(
            off["bar"], "intrabar",
            "without --trend the pcl-exhausted veto stays a wick-through: {off}"
        );

        // `--trend`: same level, close-confirmed.
        let on = pcl_trigger(&plan_json_for(true, "reaches"));
        assert_eq!(
            on["bar"], "on_close",
            "--trend must reach the registered plan as a close-confirmed pcl veto: {on}"
        );
        assert_eq!(
            off["level"], on["level"],
            "--trend moves the confirm mode, never the level"
        );
    }

    /// `--trend` is also a `--skip-bcr`, and the plan is where that shows: the
    /// prep rules must be gone. Asserted on the SAME emitted JSON, so a
    /// half-wired alias (flag reaches the plan builder but not the prep
    /// decision) can't pass.
    #[test]
    fn trend_arm_emits_no_prep_rules() {
        let plan = plan_json_for(true, "no-preps");
        let ids: Vec<&str> = plan["rules"]
            .as_array()
            .expect("rules")
            .iter()
            .filter_map(|r| r["rule_id"].as_str())
            .collect();
        assert!(
            !ids.iter()
                .any(|id| id.starts_with("03-prep") || id.starts_with("04-prep")),
            "--trend implies --skip-bcr: no break-and-close/retest prep rules, got {ids:?}"
        );
    }

    #[test]
    fn replace_explicit_target_used_verbatim() {
        // An explicit id is deleted regardless of how many plans exist.
        let plans = [
            plan_entry("hs-eurusd-aaaa", "EUR_USD"),
            plan_entry("hs-eurusd-bbbb", "EUR_USD"),
        ];
        let got = resolve_replace_target("hs-eurusd-bbbb", "EUR_USD", &plans).unwrap();
        assert_eq!(got.as_deref(), Some("hs-eurusd-bbbb"));
    }

    #[test]
    fn replace_auto_resolves_single_plan_for_instrument() {
        let plans = [
            plan_entry("hs-eurusd-aaaa", "EUR_USD"),
            plan_entry("hs-gbpusd-cccc", "GBP_USD"),
        ];
        let got = resolve_replace_target("", "EUR_USD", &plans).unwrap();
        assert_eq!(got.as_deref(), Some("hs-eurusd-aaaa"));
    }

    #[test]
    fn replace_auto_no_plan_for_instrument_is_noop() {
        let plans = [plan_entry("hs-gbpusd-cccc", "GBP_USD")];
        let got = resolve_replace_target("", "EUR_USD", &plans).unwrap();
        assert!(got.is_none(), "no plan on instrument → nothing to delete");
    }

    #[test]
    fn replace_auto_multiple_plans_is_hard_error() {
        let plans = [
            plan_entry("hs-eurusd-aaaa", "EUR_USD"),
            plan_entry("mw-eurusd-bbbb", "EUR_USD"),
        ];
        let err = resolve_replace_target("", "EUR_USD", &plans).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("2 plans"), "msg = {msg}");
        assert!(msg.contains("hs-eurusd-aaaa"), "names candidates: {msg}");
        assert!(msg.contains("mw-eurusd-bbbb"), "names candidates: {msg}");
        // The error text points the operator at the *new* flag name.
        assert!(msg.contains("--replace"), "error names --replace: {msg}");
    }

    #[test]
    fn replace_whitespace_target_is_treated_as_auto() {
        // clap's default_missing_value for a bare `--replace` is "" → auto.
        let plans = [plan_entry("hs-eurusd-aaaa", "EUR_USD")];
        let got = resolve_replace_target("  ", "EUR_USD", &plans).unwrap();
        assert_eq!(got.as_deref(), Some("hs-eurusd-aaaa"));
    }
}
