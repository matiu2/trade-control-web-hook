//! `build-pause` — emit a signed `pause` / `resume` alert pair for a
//! news-event blackout window on an existing trade.
//!
//! A blackout protects one specific [`trade_id`][TradeId] from firing
//! across a known volatility window (NFP, CPI, central-bank decisions,
//! etc.). On the chart, the operator draws two vertical lines labelled
//! `blackout-start` and `blackout-end` (or the `pause` / `resume`
//! aliases); `tv_arm_hs.py` pairs them up and shells out here.
//!
//! Each window emits two signed YAMLs:
//!   - `01-pause-<blackout_id>.yaml`  → `action: pause`
//!   - `02-resume-<blackout_id>.yaml` → `action: resume`
//!
//! Both fire from vertical-line drawings on a time-cross, not from a
//! Pine study, so they're built via [`wrap_signed_template_drawing`] —
//! `{{plot("…")}}` placeholders would arrive literal and crash the
//! worker's YAML parser.
//!
//! The wire-side contract is in `core/src/intent.rs` —
//! [`Action::Pause`][trade_control_core::intent::Action::Pause] +
//! [`blackout_id`][trade_control_core::intent::Intent::blackout_id].
//! See also the per-trade gate in `src/lib.rs::run_enter`.
//!
//! [TradeId]: trade_control_core::intent::Intent::trade_id

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Context, Result, eyre};
use serde::{Deserialize, Serialize};

use trade_control_conventions::AlertBasename;
use trade_control_core::intent::{Action, BrokerKind, Intent, is_valid_trade_id};
use trade_control_core::sig::KEY_LEN;

use crate::control::wrap_signed_template_drawing;

/// Default grace tail on the resume alert's `not_after` past the
/// blackout's `end_time`. Stops the resume from lapsing the instant
/// the window closes and leaving the pause unkillable on clock skew.
const DEFAULT_RESUME_GRACE: Duration = Duration::minutes(30);

/// Declarative form of one blackout window. Drives the CLI's
/// `--from-file` path so `tv_arm_hs.py` can write one of these per
/// vertical-line pair and shell out without recreating the formula.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PauseSpec {
    /// Parent trade the pause guards. Must match the `trade_id`
    /// stamped on the corresponding `05-enter.yaml` — that's the key
    /// the worker's pause gate looks up.
    pub trade_id: String,
    /// Stable id for this blackout window. Multiple concurrent
    /// blackouts on one trade carry different ids (e.g. `nfp`,
    /// `cb-rate-decision`). Auto-minted from `<start>-<end>` epoch
    /// seconds when absent so chronological pairs get stable,
    /// human-recognisable ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blackout_id: Option<String>,
    /// Wall-clock start of the blackout. The `pause` alert is
    /// configured to fire when chart time crosses this anchor.
    pub start_time: DateTime<Utc>,
    /// Wall-clock end of the blackout. The `resume` alert fires when
    /// chart time crosses this anchor.
    pub end_time: DateTime<Utc>,
    /// Optional human label surfaced on the seen-index outcome string
    /// (e.g. `"news:USD-NFP-2026-06-06"`). Recommended for operator
    /// debugging — without it the seen index just shows the
    /// blackout_id, which won't ring a bell weeks later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Instrument the parent trade is on. Carried for log/operator
    /// readability — pauses key off `trade_id` only, so this is
    /// informational. Stays in the alert YAML because every signed
    /// envelope needs an `instrument` field by schema.
    pub instrument: String,
    /// Account the parent trade is on. Same role as `instrument` —
    /// informational; the pause itself is per-trade, not per-account.
    pub account: String,
    /// Broker the parent trade targets. Defaults to OANDA when omitted
    /// for symmetry with `TradeSpec` defaults, though the worker's
    /// pause / resume handlers never call the broker.
    #[serde(default = "default_broker")]
    pub broker: BrokerKind,
}

fn default_broker() -> BrokerKind {
    BrokerKind::Oanda
}

/// One signed alert about to be flushed to disk. Mirrors the
/// `BuiltAlert` shape in `trade_patterns.rs` to keep the manifest
/// renderer downstream symmetrical.
#[derive(Debug)]
pub struct BuiltPauseAlert {
    pub basename: String,
    pub purpose: String,
    pub intent: Intent,
}

/// Outputs of a `build-pause` run before disk flush.
#[derive(Debug)]
pub struct BuiltPause {
    pub trade_id: String,
    pub blackout_id: String,
    pub instrument: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub alerts: Vec<BuiltPauseAlert>,
    pub spec: PauseSpec,
}

/// Build a `BuiltPause` from a fully-specified `PauseSpec`. Validates
/// shape (slug fields, time ordering) then mints the two alerts.
pub fn build_pause_from_spec(spec: PauseSpec, now: DateTime<Utc>) -> Result<BuiltPause> {
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
            "end_time {} is already in the past — refusing to arm a stale blackout",
            spec.end_time.to_rfc3339()
        ));
    }
    if spec.instrument.trim().is_empty() {
        return Err(eyre!("instrument is required"));
    }
    if spec.account.trim().is_empty() {
        return Err(eyre!("account is required"));
    }

    let blackout_id = match &spec.blackout_id {
        Some(id) => {
            if !is_valid_trade_id(id) {
                return Err(eyre!(
                    "blackout_id {id:?} is not a valid slug (same shape as trade_id)"
                ));
            }
            id.clone()
        }
        None => mint_blackout_id(spec.start_time, spec.end_time),
    };

    // not_after for each alert:
    //   - pause:  end_time (no point firing the start after the window has closed)
    //   - resume: end_time + grace (give a clock-skew tail)
    let pause_not_after = spec.end_time;
    let resume_not_after = spec.end_time + DEFAULT_RESUME_GRACE;

    let pause_intent = build_pause_intent(
        &spec,
        &blackout_id,
        Action::Pause,
        format!("{}-pause-{}", spec.trade_id, blackout_id),
        pause_not_after,
    );
    let resume_intent = build_pause_intent(
        &spec,
        &blackout_id,
        Action::Resume,
        format!("{}-resume-{}", spec.trade_id, blackout_id),
        resume_not_after,
    );
    pause_intent
        .validate()
        .map_err(|e| eyre!("internal: built pause intent failed validate: {e}"))?;
    resume_intent
        .validate()
        .map_err(|e| eyre!("internal: built resume intent failed validate: {e}"))?;

    let purpose_suffix = match &spec.reason {
        Some(r) => format!(" — {r}"),
        None => String::new(),
    };
    let alerts = vec![
        BuiltPauseAlert {
            basename: AlertBasename::PauseStart(blackout_id.clone())
                .as_str()
                .into_owned(),
            purpose: format!("pause: arm blackout {blackout_id}{purpose_suffix}"),
            intent: pause_intent,
        },
        BuiltPauseAlert {
            basename: AlertBasename::PauseResume(blackout_id.clone())
                .as_str()
                .into_owned(),
            purpose: format!("resume: clear blackout {blackout_id}{purpose_suffix}"),
            intent: resume_intent,
        },
    ];

    Ok(BuiltPause {
        trade_id: spec.trade_id.clone(),
        blackout_id,
        instrument: spec.instrument.clone(),
        start_time: spec.start_time,
        end_time: spec.end_time,
        alerts,
        spec,
    })
}

/// Mint a `<start>-<end>` blackout id from epoch seconds. Stable for
/// a given window, human-readable in log lines, and slug-shape valid.
fn mint_blackout_id(start: DateTime<Utc>, end: DateTime<Utc>) -> String {
    format!("{}-{}", start.timestamp(), end.timestamp())
}

/// Construct an `Intent` for one half of a pause/resume pair. Both
/// halves share `trade_id` / `blackout_id` / `reason` so the worker
/// can correlate them via the KV key.
fn build_pause_intent(
    spec: &PauseSpec,
    blackout_id: &str,
    action: Action,
    id: String,
    not_after: DateTime<Utc>,
) -> Intent {
    Intent {
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
        blackout_id: Some(blackout_id.to_string()),
        news_id: None,
        require_news_window: None,
        require_price_in_ranges: None,
        needs_confirmed: false,
        inside_window: Vec::new(),
        sr_bands: Vec::new(),
        veto_on_reversal: false,
        // Reason rides on both halves so a `resume` log line is also
        // self-describing (operators see "what was this for?" without
        // grepping the matching pause).
        reason: spec.reason.clone(),
        mw: None,
        pip_size: None,
    }
}

/// Persist a built pause window: each alert as `<basename>.yaml`
/// (signed, drawing-shell only — no Pine placeholders), plus a
/// `manifest.yaml` and round-trippable `pause.yaml` for reproducible
/// rebuilds. Returns the output directory.
pub fn write_pause(pause: &BuiltPause, key: &[u8; KEY_LEN], out_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    for alert in &pause.alerts {
        // Pause / resume always fire from vertical-line drawings, never
        // from a Pine study — see module docs for why.
        let body = wrap_signed_template_drawing(&alert.intent, key)
            .map_err(|e| eyre!("sign {}: {e}", alert.basename))?;
        let path = out_dir.join(format!("{}.yaml", alert.basename));
        fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    let manifest = render_manifest(pause);
    let manifest_path = out_dir.join("manifest.yaml");
    fs::write(&manifest_path, manifest)
        .with_context(|| format!("writing {}", manifest_path.display()))?;
    let spec_yaml = serde_yaml::to_string(&pause.spec).context("serialising pause.yaml")?;
    let spec_path = out_dir.join("pause.yaml");
    fs::write(&spec_path, spec_yaml).with_context(|| format!("writing {}", spec_path.display()))?;
    Ok(out_dir.to_path_buf())
}

fn render_manifest(pause: &BuiltPause) -> String {
    let mut out = String::new();
    out.push_str(&format!("trade_id: {}\n", pause.trade_id));
    out.push_str(&format!("blackout_id: {}\n", pause.blackout_id));
    out.push_str(&format!("instrument: {}\n", pause.instrument));
    out.push_str(&format!(
        "start_time: \"{}\"\n",
        pause.start_time.to_rfc3339()
    ));
    out.push_str(&format!("end_time: \"{}\"\n", pause.end_time.to_rfc3339()));
    if let Some(r) = &pause.spec.reason {
        out.push_str(&format!("reason: {r}\n"));
    }
    out.push_str("alerts:\n");
    for alert in &pause.alerts {
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

/// Load a `PauseSpec` from a YAML file at `path`.
pub fn load_spec_from_file(path: &Path) -> Result<PauseSpec> {
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

    fn sample_spec() -> PauseSpec {
        PauseSpec {
            trade_id: "eurusd-hs-1".into(),
            blackout_id: None,
            start_time: ts("2026-06-06T12:30:00Z"),
            end_time: ts("2026-06-06T13:00:00Z"),
            reason: Some("news:USD-NFP".into()),
            instrument: "EUR_USD".into(),
            account: "oanda-reversals-demo".into(),
            broker: BrokerKind::Oanda,
        }
    }

    #[test]
    fn build_pause_mints_chronological_blackout_id_when_absent() {
        let now = ts("2026-06-06T00:00:00Z");
        let built = build_pause_from_spec(sample_spec(), now).unwrap();
        let start_epoch = ts("2026-06-06T12:30:00Z").timestamp();
        let end_epoch = ts("2026-06-06T13:00:00Z").timestamp();
        assert_eq!(built.blackout_id, format!("{start_epoch}-{end_epoch}"));
    }

    #[test]
    fn build_pause_emits_pause_then_resume_alerts() {
        let now = ts("2026-06-06T00:00:00Z");
        let built = build_pause_from_spec(sample_spec(), now).unwrap();
        assert_eq!(built.alerts.len(), 2);
        assert!(built.alerts[0].basename.starts_with("01-pause-"));
        assert!(built.alerts[1].basename.starts_with("02-resume-"));
        assert_eq!(built.alerts[0].intent.action, Action::Pause);
        assert_eq!(built.alerts[1].intent.action, Action::Resume);
        // Each half carries the same trade_id + blackout_id so the
        // worker can correlate them.
        assert_eq!(
            built.alerts[0].intent.trade_id,
            built.alerts[1].intent.trade_id
        );
        assert_eq!(
            built.alerts[0].intent.blackout_id,
            built.alerts[1].intent.blackout_id
        );
        // Resume gets a grace tail past end_time so it survives clock skew.
        assert!(built.alerts[1].intent.not_after > built.end_time);
        assert_eq!(built.alerts[0].intent.not_after, built.end_time);
    }

    #[test]
    fn build_pause_rejects_reversed_window() {
        let mut spec = sample_spec();
        spec.start_time = ts("2026-06-06T13:00:00Z");
        spec.end_time = ts("2026-06-06T12:30:00Z");
        let now = ts("2026-06-06T00:00:00Z");
        let err = build_pause_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("strictly after"), "{err}");
    }

    #[test]
    fn build_pause_rejects_past_window() {
        let mut spec = sample_spec();
        // Window ended before "now" — refuse to arm a stale blackout.
        let now = ts("2026-06-07T00:00:00Z");
        spec.start_time = ts("2026-06-06T12:30:00Z");
        spec.end_time = ts("2026-06-06T13:00:00Z");
        let err = build_pause_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("already in the past"), "{err}");
    }

    #[test]
    fn build_pause_rejects_bad_trade_id() {
        let mut spec = sample_spec();
        spec.trade_id = "Bad ID!".into();
        let now = ts("2026-06-06T00:00:00Z");
        let err = build_pause_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("trade_id"), "{err}");
    }

    #[test]
    fn build_pause_rejects_bad_blackout_id() {
        let mut spec = sample_spec();
        spec.blackout_id = Some("NFP!".into());
        let now = ts("2026-06-06T00:00:00Z");
        let err = build_pause_from_spec(spec, now).unwrap_err();
        assert!(err.to_string().contains("blackout_id"), "{err}");
    }

    #[test]
    fn signed_alerts_round_trip_through_parse_and_verify() {
        // The whole pipeline: build → sign → parse_and_verify with a
        // realistic shell substitution (drawings deliver close/high/
        // low/time only — no Pine placeholders).
        let now = ts("2026-06-06T00:00:00Z");
        let built = build_pause_from_spec(sample_spec(), now).unwrap();
        let key = [7u8; KEY_LEN];
        for alert in &built.alerts {
            let signed = wrap_signed_template_drawing(&alert.intent, &key).unwrap();
            // Simulate TradingView's substitution of the four universally-
            // delivered placeholders. The exact values don't matter for
            // pause/resume — the worker just needs a parseable shell.
            let on_wire = signed
                .replace("{{close}}", "1.16438")
                .replace("{{high}}", "1.16440")
                .replace("{{low}}", "1.16430")
                .replace("{{open}}", "1.16435")
                .replace("{{time}}", "2026-06-06T12:30:00Z");
            // `now` for verification is inside the alert's validity
            // window (well before `not_after`).
            let verify_now = ts("2026-06-06T12:30:30Z");
            let verified = parse_and_verify(&on_wire, &key, verify_now)
                .unwrap_or_else(|e| panic!("verify {}: {e}", alert.basename));
            assert_eq!(verified.intent.action, alert.intent.action);
            assert_eq!(verified.intent.trade_id.as_deref(), Some("eurusd-hs-1"));
            assert!(verified.intent.blackout_id.is_some());
        }
    }

    #[test]
    fn write_pause_emits_manifest_pause_yaml_and_alerts() {
        // Smoke test the disk side — the renderer + spec round-trip.
        let now = ts("2026-06-06T00:00:00Z");
        let built = build_pause_from_spec(sample_spec(), now).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let key = [3u8; KEY_LEN];
        write_pause(&built, &key, dir.path()).unwrap();
        for alert in &built.alerts {
            let p = dir.path().join(format!("{}.yaml", alert.basename));
            assert!(p.exists(), "missing alert file: {p:?}");
        }
        assert!(dir.path().join("manifest.yaml").exists());
        let pause_yaml = fs::read_to_string(dir.path().join("pause.yaml")).unwrap();
        // Round-trip the spec back through serde — proves the
        // emitted pause.yaml can drive a future rebuild.
        let parsed: PauseSpec = serde_yaml::from_str(&pause_yaml).unwrap();
        assert_eq!(parsed.trade_id, built.trade_id);
        assert_eq!(parsed.start_time, built.start_time);
    }
}
