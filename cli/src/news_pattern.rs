//! `build-news` — emit a signed `news-start` / `news-end` alert pair
//! for a scheduled-news window on an existing trade.
//!
//! A news window doesn't block entries (that's [`pause_pattern`]'s
//! job); instead it *enables* a separate close intent that fires on
//! an opposing-direction golden-reversal candle, so the worker
//! flattens the trade only when a known volatility event is in play.
//! On the chart the operator draws two vertical lines labelled
//! `news-start` and `news-end`; `tv_arm_hs.py` pairs them up and
//! shells out here.
//!
//! Each window emits two signed YAMLs:
//!   - `01-news-start-<news_id>.yaml` → `action: news-start`
//!   - `02-news-end-<news_id>.yaml`   → `action: news-end`
//!
//! Both fire from vertical-line drawings, so they're built with
//! [`wrap_signed_template_drawing`] — no Pine placeholders.
//!
//! [`pause_pattern`]: crate::pause_pattern

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use serde::{Deserialize, Serialize};

use trade_control_conventions::AlertBasename;
use trade_control_core::intent::{Action, BrokerKind, Intent, is_valid_trade_id};
use trade_control_core::sig::KEY_LEN;

use crate::control::wrap_signed_template_drawing;

/// Default grace tail on the news-end alert's `not_after` past the
/// window's `end_time`. Stops the news-end from lapsing the instant
/// the window closes — clock skew between TradingView and the worker
/// otherwise leaves the window unkillable.
const DEFAULT_NEWS_END_GRACE: Duration = Duration::minutes(30);

/// Declarative form of one news window. Drives the CLI's `--from-file`
/// path so `tv_arm_hs.py` can write one of these per vertical-line
/// pair and shell out without recreating the formula.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsSpec {
    /// Parent trade the news window applies to. Must match the
    /// `trade_id` stamped on the corresponding `05-enter.yaml` —
    /// that's the key the worker's reversal-close gate looks up.
    pub trade_id: String,
    /// Stable id for this news window. Multiple concurrent windows on
    /// one trade carry different ids (e.g. `usd-nfp`, `eur-cpi`).
    /// Auto-minted from `<start>-<end>` epoch seconds when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub news_id: Option<String>,
    /// Wall-clock start of the window. The `news-start` alert fires
    /// when chart time crosses this anchor.
    pub start_time: DateTime<Utc>,
    /// Wall-clock end of the window. The `news-end` alert fires when
    /// chart time crosses this anchor.
    pub end_time: DateTime<Utc>,
    /// Optional human label surfaced on the seen-index outcome string
    /// (e.g. `"USD-NFP-2026-06-06"`). Recommended for operator
    /// debugging — without it the seen index just shows the news_id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Instrument the parent trade is on. Carried for log/operator
    /// readability — news windows key off `trade_id` only, so this is
    /// informational. Stays in the alert YAML because every signed
    /// envelope needs an `instrument` field by schema.
    pub instrument: String,
    /// Account the parent trade is on. Same role as `instrument` —
    /// informational; the news window is per-trade, not per-account.
    pub account: String,
    /// Broker the parent trade targets. Defaults to OANDA when
    /// omitted, though the worker's news-start / news-end handlers
    /// never call the broker.
    #[serde(default = "default_broker")]
    pub broker: BrokerKind,
}

fn default_broker() -> BrokerKind {
    BrokerKind::Oanda
}

/// One signed alert about to be flushed to disk. Mirrors the
/// `BuiltAlert` shape in `trade_patterns.rs`.
#[derive(Debug)]
pub struct BuiltNewsAlert {
    pub basename: String,
    pub purpose: String,
    pub intent: Intent,
}

/// Outputs of a `build-news` run before disk flush.
#[derive(Debug)]
pub struct BuiltNews {
    pub trade_id: String,
    pub news_id: String,
    pub instrument: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub alerts: Vec<BuiltNewsAlert>,
    pub spec: NewsSpec,
}

/// Build a `BuiltNews` from a fully-specified `NewsSpec`. Validates
/// shape (slug fields, time ordering) then mints the two alerts.
pub fn build_news_from_spec(spec: NewsSpec, now: DateTime<Utc>) -> Result<BuiltNews> {
    if !is_valid_trade_id(&spec.trade_id) {
        return Err(eyre!(
            "trade_id {:?} is not a valid slug (lowercase alphanumerics + hyphens, \
             1-64 chars, no leading/trailing/consecutive hyphens)",
            spec.trade_id
        ));
    }
    if spec.end_time <= spec.start_time {
        return Err(eyre!(
            "end_time ({}) must be strictly after start_time ({})",
            spec.end_time.to_rfc3339(),
            spec.start_time.to_rfc3339(),
        ));
    }
    if spec.end_time <= now {
        return Err(eyre!(
            "end_time {} is already in the past — refusing to arm a stale news window",
            spec.end_time.to_rfc3339()
        ));
    }
    if spec.instrument.trim().is_empty() {
        return Err(eyre!("instrument is required"));
    }
    if spec.account.trim().is_empty() {
        return Err(eyre!("account is required"));
    }

    let news_id = match &spec.news_id {
        Some(id) => {
            if !is_valid_trade_id(id) {
                return Err(eyre!(
                    "news_id {id:?} is not a valid slug (same shape as trade_id)"
                ));
            }
            id.clone()
        }
        None => mint_news_id(spec.start_time, spec.end_time),
    };

    let start_not_after = spec.end_time;
    let end_not_after = spec.end_time + DEFAULT_NEWS_END_GRACE;

    let start_intent = build_news_intent(
        &spec,
        &news_id,
        Action::NewsStart,
        format!("{}-news-start-{}", spec.trade_id, news_id),
        start_not_after,
    );
    let end_intent = build_news_intent(
        &spec,
        &news_id,
        Action::NewsEnd,
        format!("{}-news-end-{}", spec.trade_id, news_id),
        end_not_after,
    );
    start_intent
        .validate()
        .map_err(|e| eyre!("internal: built news-start intent failed validate: {e}"))?;
    end_intent
        .validate()
        .map_err(|e| eyre!("internal: built news-end intent failed validate: {e}"))?;

    let purpose_suffix = match &spec.reason {
        Some(r) => format!(" — {r}"),
        None => String::new(),
    };
    let alerts = vec![
        BuiltNewsAlert {
            basename: AlertBasename::NewsStart(news_id.clone())
                .as_str()
                .into_owned(),
            purpose: format!("news-start: arm window {news_id}{purpose_suffix}"),
            intent: start_intent,
        },
        BuiltNewsAlert {
            basename: AlertBasename::NewsEnd(news_id.clone())
                .as_str()
                .into_owned(),
            purpose: format!("news-end: clear window {news_id}{purpose_suffix}"),
            intent: end_intent,
        },
    ];

    Ok(BuiltNews {
        trade_id: spec.trade_id.clone(),
        news_id,
        instrument: spec.instrument.clone(),
        start_time: spec.start_time,
        end_time: spec.end_time,
        alerts,
        spec,
    })
}

fn mint_news_id(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!("{}-{}", start.timestamp(), end.timestamp())
}

/// Construct an `Intent` for one half of a news-start/news-end pair.
fn build_news_intent(
    spec: &NewsSpec,
    news_id: &str,
    action: Action,
    id: String,
    not_after: DateTime<Utc>,
) -> Intent {
    Intent {
        entry_level_vetos: Vec::new(),
        v: 1,
        id,
        not_before: None,
        not_after,
        action,
        instrument: spec.instrument.clone(),
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
        broker: spec.broker,
        account: Some(spec.account.clone()),
        step: None,
        name: None,
        ttl_hours: trade_control_core::tunable::Tunable::Static(0),
        level: None,
        requires_preps: Vec::new(),
        vetos: Vec::new(),
        clears: Vec::new(),
        trade_id: Some(spec.trade_id.clone()),
        max_retries: trade_control_core::tunable::Tunable::Static(0),
        expiry_bars: None,
        allow_entry: None,
        allow_close: None,
        needs_golden: false,
        blackout_id: None,
        news_id: Some(news_id.to_string()),
        require_news_window: None,
        require_price_in_ranges: None,
        needs_confirmed: false,
        inside_window: Vec::new(),
        sr_bands: Vec::new(),
        veto_on_reversal: false,
        reason: spec.reason.clone(),
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

/// Persist a built news window: each alert as `<basename>.yaml`
/// (signed, drawing-shell only), plus a `manifest.yaml` and
/// round-trippable `news.yaml` for reproducible rebuilds. Returns the
/// output directory.
pub fn write_news(news: &BuiltNews, key: &[u8; KEY_LEN], out_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    for alert in &news.alerts {
        let body = wrap_signed_template_drawing(&alert.intent, key)
            .map_err(|e| eyre!("sign {}: {e}", alert.basename))?;
        let path = out_dir.join(format!("{}.yaml", alert.basename));
        fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    let manifest = render_manifest(news);
    let manifest_path = out_dir.join("manifest.yaml");
    fs::write(&manifest_path, manifest)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    let spec_yaml = serde_yaml::to_string(&news.spec).context("serialising news.yaml")?;
    let spec_path = out_dir.join("news.yaml");
    fs::write(&spec_path, spec_yaml).with_context(|| format!("writing {}", spec_path.display()))?;
    Ok(out_dir.to_path_buf())
}

fn render_manifest(news: &BuiltNews) -> String {
    let mut out = String::new();
    out.push_str(&format!("trade_id: {}\n", news.trade_id));
    out.push_str(&format!("news_id: {}\n", news.news_id));
    out.push_str(&format!("instrument: {}\n", news.instrument));
    out.push_str(&format!(
        "start_time: \"{}\"\n",
        news.start_time.to_rfc3339()
    ));
    out.push_str(&format!("end_time: \"{}\"\n", news.end_time.to_rfc3339()));
    if let Some(r) = &news.spec.reason {
        out.push_str(&format!("reason: {r}\n"));
    }
    out.push_str("alerts:\n");
    for alert in &news.alerts {
        out.push_str(&format!("  - file: {}.yaml\n", alert.basename));
        out.push_str(&format!("    purpose: {}\n", alert.purpose));
        out.push_str(&format!("    action: {:?}\n", alert.intent.action));
        out.push_str(&format!(
            "    not_after: \"{}\"\n",
            alert.intent.not_after.to_rfc3339()
        ));
    }
    out
}

/// Load a `NewsSpec` from a YAML file at `path`.
pub fn load_spec_from_file(path: &Path) -> Result<NewsSpec> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use trade_control_core::incoming::parse_and_verify;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn sample_spec() -> NewsSpec {
        NewsSpec {
            trade_id: "eurusd-hs-1".into(),
            news_id: None,
            start_time: ts("2026-06-06T12:30:00Z"),
            end_time: ts("2026-06-06T13:00:00Z"),
            reason: Some("USD-NFP".into()),
            instrument: "EUR_USD".into(),
            account: "oanda-reversals-demo".into(),
            broker: BrokerKind::Oanda,
        }
    }

    #[test]
    fn build_news_mints_chronological_news_id_when_absent() {
        let now = ts("2026-06-06T00:00:00Z");
        let built = build_news_from_spec(sample_spec(), now).unwrap();
        let start_epoch = ts("2026-06-06T12:30:00Z").timestamp();
        let end_epoch = ts("2026-06-06T13:00:00Z").timestamp();
        assert_eq!(built.news_id, format!("{start_epoch}-{end_epoch}"));
    }

    #[test]
    fn build_news_emits_start_then_end_alerts() {
        let now = ts("2026-06-06T00:00:00Z");
        let built = build_news_from_spec(sample_spec(), now).unwrap();
        assert_eq!(built.alerts.len(), 2);
        assert!(built.alerts[0].basename.starts_with("01-news-start-"));
        assert!(built.alerts[1].basename.starts_with("02-news-end-"));
        assert_eq!(built.alerts[0].intent.action, Action::NewsStart);
        assert_eq!(built.alerts[1].intent.action, Action::NewsEnd);
        // Each half carries the same trade_id + news_id so the worker
        // can correlate them.
        assert_eq!(
            built.alerts[0].intent.trade_id,
            built.alerts[1].intent.trade_id
        );
        assert_eq!(
            built.alerts[0].intent.news_id,
            built.alerts[1].intent.news_id
        );
        // news-end gets a grace tail past end_time so it survives clock
        // skew, mirroring pause/resume.
        assert!(built.alerts[1].intent.not_after > built.end_time);
        assert_eq!(built.alerts[0].intent.not_after, built.end_time);
    }

    #[test]
    fn build_news_rejects_reversed_window() {
        let mut spec = sample_spec();
        spec.start_time = ts("2026-06-06T13:00:00Z");
        spec.end_time = ts("2026-06-06T12:30:00Z");
        let now = ts("2026-06-06T00:00:00Z");
        let err = build_news_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("strictly after"), "{err}");
    }

    #[test]
    fn build_news_rejects_past_window() {
        let mut spec = sample_spec();
        let now = ts("2026-06-07T00:00:00Z");
        spec.start_time = ts("2026-06-06T12:30:00Z");
        spec.end_time = ts("2026-06-06T13:00:00Z");
        let err = build_news_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("already in the past"), "{err}");
    }

    #[test]
    fn build_news_rejects_bad_news_id() {
        let mut spec = sample_spec();
        spec.news_id = Some("NFP!".into());
        let now = ts("2026-06-06T00:00:00Z");
        let err = build_news_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("news_id"), "{err}");
    }

    #[test]
    fn signed_alerts_round_trip_through_parse_and_verify() {
        let now = ts("2026-06-06T00:00:00Z");
        let built = build_news_from_spec(sample_spec(), now).unwrap();
        let key = [7u8; KEY_LEN];
        for alert in &built.alerts {
            let signed = wrap_signed_template_drawing(&alert.intent, &key).unwrap();
            let on_wire = signed
                .replace("{{close}}", "1.16438")
                .replace("{{high}}", "1.16440")
                .replace("{{low}}", "1.16430")
                .replace("{{open}}", "1.16435")
                .replace("{{time}}", "2026-06-06T12:30:00Z");
            let verify_now = ts("2026-06-06T12:30:30Z");
            let verified = parse_and_verify(&on_wire, &key, verify_now)
                .unwrap_or_else(|e| panic!("verify {}: {e}", alert.basename));
            assert_eq!(verified.intent.action, alert.intent.action);
            assert_eq!(verified.intent.trade_id.as_deref(), Some("eurusd-hs-1"));
            assert!(verified.intent.news_id.is_some());
        }
    }

    #[test]
    fn write_news_emits_manifest_news_yaml_and_alerts() {
        let now = ts("2026-06-06T00:00:00Z");
        let built = build_news_from_spec(sample_spec(), now).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let key = [3u8; KEY_LEN];
        write_news(&built, &key, dir.path()).unwrap();
        for alert in &built.alerts {
            let p = dir.path().join(format!("{}.yaml", alert.basename));
            assert!(p.exists(), "missing alert file: {p:?}");
        }
        assert!(dir.path().join("manifest.yaml").exists());
        let news_yaml = fs::read_to_string(dir.path().join("news.yaml")).unwrap();
        let parsed: NewsSpec = serde_yaml::from_str(&news_yaml).unwrap();
        assert_eq!(parsed.trade_id, built.trade_id);
        assert_eq!(parsed.start_time, built.start_time);
    }
}
