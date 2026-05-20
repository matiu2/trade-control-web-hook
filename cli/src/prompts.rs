//! Interactive prompts for the `trade-control encrypt` flow.
//!
//! The flow is split into two layers:
//!   - `required_for_action` and `missing_fields` are pure functions over a
//!     `serde_yaml::Value`. They identify which top-level keys still need a
//!     value, and have no I/O. These are unit-tested.
//!   - `prompt_for_field` and `fill_missing_fields` do terminal I/O via
//!     `dialoguer`. They're driven by the pure layer.
//!
//! The set of required fields per action is derived from the `Intent` struct
//! shape: `Intent` makes most fields `Option<T>` because they vary by action,
//! so structural deserialization can't catch a missing `instrument` for
//! `action: enter`. The action-required set encodes which `Option<T>` fields
//! must actually be set per action.

use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Result, eyre};
use serde_yaml::Value;

use trade_control_core::intent::Action;

/// Fields every intent needs regardless of action. The interactive
/// driver splits these around an action-aware mid-pass; this constant
/// is the authoritative list, referenced by the structural test below
/// and by `missing_fields` callers.
#[allow(dead_code)]
pub const ALWAYS_REQUIRED: &[&str] = &["v", "action", "instrument", "id", "not_after"];

/// Optional fields the CLI offers to fill for the given action when not
/// already specified by the template. Unlike `required_for_action`, these
/// are not blocking — the user can leave them blank.
///
/// For `enter`, this is the prep/veto gate. Templates can already encode
/// these statically (and templates win — present-and-empty `[]` skips the
/// prompt), but ad-hoc trades typed at the CLI need a way in.
pub fn optional_for_action(action: Action) -> &'static [&'static str] {
    match action {
        // `account` routes an entry through a specific named account
        // from the worker's account index, not just the broker pool.
        // Absent means "use the legacy (pre-accounts) lookup".
        // `dry_run` short-circuits broker dispatch and logs the sizing
        // inputs instead of placing the order.
        Action::Enter => &["requires_preps", "vetos", "account", "dry_run"],
        // `clears` is optional on prep/veto — it lets an upstream prep
        // (like break-and-close) declare which downstream preps it
        // invalidates, fixing stale-prep ordering bugs. `account` is
        // also accepted on `veto` because escalated-level vetos
        // (cancel-pending / close-positions) hit the broker, and on
        // `close` / `invalidate` for the same reason.
        Action::Prep => &["clears"],
        Action::Veto => &["clears", "account"],
        Action::Close | Action::Invalidate => &["account"],
        _ => &[],
    }
}

/// Fields required *in addition* to `ALWAYS_REQUIRED` for the given action.
pub fn required_for_action(action: Action) -> &'static [&'static str] {
    match action {
        Action::Enter => &[
            "broker",
            "direction",
            "entry",
            "stop_loss",
            "take_profit",
            "risk_pct",
        ],
        Action::Close => &[],
        Action::Invalidate => &["cooldown_hours"],
        Action::Prep => &["step", "ttl_hours"],
        Action::Veto => &["name", "ttl_hours"],
        Action::ClearPrep => &["step"],
        Action::ClearVeto => &["name"],
        // `instrument` is already in `ALWAYS_REQUIRED`; nothing extra needed.
        Action::Status | Action::Unlock => &[],
    }
}

/// Read the `action` field from a partially-filled template. Returns `None`
/// if the field is absent or unparseable — the caller prompts for it.
pub fn read_action(template: &Value) -> Option<Action> {
    let action_str = template.get("action")?.as_str()?;
    serde_yaml::from_str::<Action>(action_str).ok()
}

/// Return the list of top-level keys (from `required`) that are absent or
/// `null` in `template`. Order matches `required`.
///
/// Special case: `risk_pct`, `risk_amount`, and `size_units` are
/// alternatives — if the template already carries either of the
/// alternates, we don't ask for `risk_pct`. The server-side resolver
/// rejects more-than-one-set so we don't double-prompt.
pub fn missing_fields<'a>(template: &Value, required: &[&'a str]) -> Vec<&'a str> {
    let has_alt_sizing = ["risk_amount", "size_units"]
        .iter()
        .any(|k| !matches!(template.get(*k), None | Some(Value::Null)));
    required
        .iter()
        .filter(|name| {
            if **name == "risk_pct" && has_alt_sizing {
                return false;
            }
            matches!(template.get(**name), None | Some(Value::Null))
        })
        .copied()
        .collect()
}

/// Splice `value` into `template` at top-level key `key`. Panics if the
/// template root isn't a mapping — that's a programmer error, not a user
/// error, so panicking is fine.
pub fn set_field(template: &mut Value, key: &str, value: Value) {
    let map = template
        .as_mapping_mut()
        .expect("template root must be a YAML mapping");
    map.insert(Value::String(key.to_string()), value);
}

/// Parse a relative duration like `8h`, `2d`, `45m` into a `chrono::Duration`.
/// Returns `None` for unrecognized formats; the caller falls back to absolute
/// ISO-8601 parsing.
pub fn parse_relative_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.len() < 2 {
        return None;
    }
    let (num_str, unit) = s.split_at(s.len() - 1);
    let n: i64 = num_str.parse().ok()?;
    match unit {
        "m" => Some(Duration::minutes(n)),
        "h" => Some(Duration::hours(n)),
        "d" => Some(Duration::days(n)),
        _ => None,
    }
}

/// Resolve a user-typed value for `not_after`: accept either a relative
/// duration (`8h`, `2d`) measured from `now`, or an absolute ISO-8601
/// timestamp.
pub fn resolve_not_after(input: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    if let Some(d) = parse_relative_duration(input) {
        return Ok(now + d);
    }
    input
        .parse::<DateTime<Utc>>()
        .map_err(|e| eyre!("not_after: expected duration like `8h` or ISO-8601 timestamp ({e})"))
}

/// Build the default `id` suggestion: `<instrument>-<YYYY-MM-DD>-<random>`.
/// The random suffix protects against accidental id reuse on the same day.
pub fn default_id(instrument: &str, now: DateTime<Utc>, random_suffix: &str) -> String {
    let date = now.format("%Y-%m-%d");
    format!("{instrument}-{date}-{random_suffix}")
}

/// 4-char hex suffix drawn from the OS RNG. Used as the random part of the
/// default `id` suggestion. Live behind a function so it can be replaced
/// in tests with a deterministic value.
pub fn fresh_random_suffix() -> String {
    let mut bytes = [0u8; 2];
    getrandom::fill(&mut bytes).expect("OS RNG");
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(s: &str) -> Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn required_for_enter_lists_trade_fields() {
        let fields = required_for_action(Action::Enter);
        for expected in [
            "broker",
            "direction",
            "entry",
            "stop_loss",
            "take_profit",
            "risk_pct",
        ] {
            assert!(fields.contains(&expected), "missing {expected}");
        }
    }

    #[test]
    fn required_for_invalidate_only_needs_cooldown() {
        assert_eq!(required_for_action(Action::Invalidate), &["cooldown_hours"]);
    }

    #[test]
    fn required_for_close_is_empty() {
        assert_eq!(required_for_action(Action::Close), &[] as &[&str]);
    }

    #[test]
    fn optional_for_enter_includes_prep_and_veto_lists() {
        let opts = optional_for_action(Action::Enter);
        assert!(opts.contains(&"requires_preps"));
        assert!(opts.contains(&"vetos"));
    }

    #[test]
    fn optional_for_close_and_invalidate_offers_account() {
        // Close + invalidate hit the broker, so the operator should
        // be able to direct them at a specific named account when the
        // worker has multiple TN accounts.
        assert_eq!(optional_for_action(Action::Close), &["account"]);
        assert_eq!(optional_for_action(Action::Invalidate), &["account"]);
    }

    #[test]
    fn optional_for_pure_state_actions_is_empty() {
        // Status / Unlock / Clear-* never touch the broker — no
        // account routing needed.
        for a in [
            Action::Status,
            Action::Unlock,
            Action::ClearPrep,
            Action::ClearVeto,
        ] {
            assert!(optional_for_action(a).is_empty(), "{a:?}");
        }
    }

    #[test]
    fn optional_for_enter_and_veto_offer_account() {
        // `account` joins the prep/veto/clears gates so the operator
        // can route an entry (or escalated veto) at a specific named
        // account.
        assert!(optional_for_action(Action::Enter).contains(&"account"));
        assert!(optional_for_action(Action::Veto).contains(&"account"));
    }

    #[test]
    fn optional_for_prep_and_veto_includes_clears() {
        // Prep/veto carry a `clears` list so an upstream step (e.g.
        // break-and-close) can drop downstream stale preps (e.g. an
        // earlier retest) at fire time.
        assert!(optional_for_action(Action::Prep).contains(&"clears"));
        assert!(optional_for_action(Action::Veto).contains(&"clears"));
    }

    #[test]
    fn missing_fields_detects_absent_keys() {
        let v = map("v: 1\naction: enter\n");
        let missing = missing_fields(&v, ALWAYS_REQUIRED);
        // v and action are present; the others are missing.
        assert_eq!(missing, vec!["instrument", "id", "not_after"]);
    }

    #[test]
    fn missing_fields_treats_null_as_missing() {
        let v = map("v: 1\nid: ~\naction: enter\n");
        let missing = missing_fields(&v, &["id"]);
        assert_eq!(missing, vec!["id"]);
    }

    #[test]
    fn missing_fields_skips_risk_pct_when_risk_amount_set() {
        // A template that pre-picks `risk_amount: 1.0` shouldn't be
        // re-prompted for `risk_pct` — they're alternatives.
        let v = map("risk_amount: 1.0\n");
        let missing = missing_fields(&v, &["risk_pct"]);
        assert!(
            missing.is_empty(),
            "risk_pct prompted despite risk_amount present"
        );
    }

    #[test]
    fn missing_fields_still_prompts_risk_pct_when_no_risk_amount() {
        let v = map("v: 1\n");
        let missing = missing_fields(&v, &["risk_pct"]);
        assert_eq!(missing, vec!["risk_pct"]);
    }

    #[test]
    fn missing_fields_skips_risk_pct_when_size_units_set() {
        // size_units is the third alternative to risk_pct/risk_amount.
        let v = map("size_units: 0.01\n");
        let missing = missing_fields(&v, &["risk_pct"]);
        assert!(
            missing.is_empty(),
            "risk_pct prompted despite size_units present"
        );
    }

    #[test]
    fn set_field_inserts_into_mapping() {
        let mut v = map("v: 1\n");
        set_field(&mut v, "instrument", Value::String("EUR_USD".into()));
        assert_eq!(v.get("instrument").unwrap().as_str(), Some("EUR_USD"));
    }

    #[test]
    fn read_action_parses_enter() {
        let v = map("action: enter\n");
        assert_eq!(read_action(&v), Some(Action::Enter));
    }

    #[test]
    fn read_action_returns_none_for_missing() {
        let v = map("v: 1\n");
        assert_eq!(read_action(&v), None);
    }

    #[test]
    fn parse_relative_duration_hours() {
        assert_eq!(parse_relative_duration("8h"), Some(Duration::hours(8)));
    }

    #[test]
    fn parse_relative_duration_days() {
        assert_eq!(parse_relative_duration("2d"), Some(Duration::days(2)));
    }

    #[test]
    fn parse_relative_duration_rejects_garbage() {
        assert!(parse_relative_duration("forever").is_none());
        assert!(parse_relative_duration("h").is_none());
    }

    #[test]
    fn resolve_not_after_handles_duration() {
        let now: DateTime<Utc> = "2026-05-13T12:00:00Z".parse().unwrap();
        let resolved = resolve_not_after("8h", now).unwrap();
        assert_eq!(resolved, now + Duration::hours(8));
    }

    #[test]
    fn resolve_not_after_handles_absolute() {
        let now: DateTime<Utc> = "2026-05-13T12:00:00Z".parse().unwrap();
        let resolved = resolve_not_after("2026-05-14T02:00:00Z", now).unwrap();
        assert_eq!(
            resolved,
            "2026-05-14T02:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn default_id_format() {
        let now: DateTime<Utc> = "2026-05-13T12:00:00Z".parse().unwrap();
        let id = default_id("EUR_USD", now, "ab12");
        assert_eq!(id, "EUR_USD-2026-05-13-ab12");
    }
}
