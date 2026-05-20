//! Terminal I/O layer for the encrypt CLI. Wraps the pure helpers in
//! [`super::prompts`] with `dialoguer` prompts and a "fill the template"
//! driver loop.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use color_eyre::eyre::{Result, eyre};
use dialoguer::{FuzzySelect, Input, Select, theme::ColorfulTheme};
use serde_yaml::Value;

use super::expiry;
use super::history;
use super::prompts::{
    default_id, fresh_random_suffix, missing_fields, optional_for_action, read_action,
    required_for_action, resolve_not_after, set_field,
};
use super::templates::templates_root;
use trade_control_core::intent::Action;
use trade_control_core::intent::Intent;

/// Name reserved for the per-instrument trade-expiry anchor veto.
/// When this veto fires (action=veto, name=trade-expiry), the CLI
/// persists the operator-supplied expiry timestamp so subsequent
/// prep/veto/enter prompts default `ttl_hours` and `not_after` to it.
const TRADE_EXPIRY_NAME: &str = "trade-expiry";

/// Drive the template to completion by prompting for missing fields. After
/// this returns, the template deserializes into a valid `Intent` for its
/// declared action.
///
/// `non_interactive`: when true, missing fields cause an error instead of a
/// prompt. Useful in scripts.
pub fn fill_missing_fields(template: &mut Value, non_interactive: bool) -> Result<()> {
    // Pass 1a: ask v / action / instrument up front. We need the action
    // (and for veto, the name) before we can offer a sensible
    // `not_after` default from the per-instrument trade-expiry anchor.
    let pre = ["v", "action", "instrument"];
    fill_round(template, &pre, non_interactive)?;
    let action = read_action(template).ok_or_else(|| eyre!("action still missing after prompt"))?;

    // For veto: ask the `name` early. If the user picks
    // `trade-expiry`, follow up with a prompt for the anchor timestamp
    // and persist it. This way the `not_after` / `ttl_hours` prompts
    // that come next can pre-fill against it.
    if action == Action::Veto && template.get("name").is_none() && !non_interactive {
        let value = prompt_for_field("name", template)?;
        set_field(template, "name", value);
        if name_is_trade_expiry(template) {
            capture_trade_expiry_anchor(template)?;
        }
    }

    // Pass 1b: rest of the always-required structural fields
    // (id + not_after). `not_after` is action-aware and consults the
    // expiry anchor for prep/veto/enter defaults.
    let post = ["id", "not_after"];
    fill_round(template, &post, non_interactive)?;

    // Pass 1c (Enter only): ask the sizing mode up front so the
    // value-prompt can target the right key (risk_pct / risk_amount /
    // size_units). Skipped if the template already picks one — the
    // existing `has_alt_sizing` check in `missing_fields` then skips
    // the risk_pct prompt in Pass 2.
    if action == Action::Enter && !non_interactive && !sizing_field_already_set(template) {
        let (key, value) = prompt_sizing_mode()?;
        set_field(template, key, value);
    }

    // Pass 2: remaining action-dependent fields.
    let action_required: Vec<&'static str> = required_for_action(action).to_vec();
    fill_round(template, &action_required, non_interactive)?;

    // Pass 3: optional action-dependent fields (e.g. `requires_preps` /
    // `vetos` for `enter`, `clears` for `prep` / `veto`). Skipped
    // entirely in non-interactive mode — an absent optional field is
    // fine there.
    if !non_interactive {
        let optional = optional_for_action(action);
        for field in optional {
            // Only prompt if the field is absent. A template that sets
            // the field (even to an empty list) wins.
            if template.get(*field).is_some() {
                continue;
            }
            let value = match *field {
                "account" => prompt_optional_account()?,
                "dry_run" => prompt_optional_dry_run()?,
                _ => prompt_optional_name_list(field, action)?,
            };
            // Blank `account` is encoded as null — skip it so the wire
            // form stays minimal and `skip_serializing_if = None` kicks
            // in.
            if !matches!(value, Value::Null) {
                set_field(template, field, value);
            }
        }
    }

    // Final validation: deserialize fully into `Intent` to surface any
    // structural mistakes (bad enum variants, wrong value types) before we
    // encrypt and the worker rejects it.
    let intent: Intent = serde_yaml::from_value(template.clone())
        .map_err(|e| eyre!("template doesn't parse as a valid Intent: {e}"))?;

    // Fail fast on a below-floor `min_r`. The server also enforces this; we
    // duplicate it here so typos don't even get encrypted.
    if let Some(min_r) = intent.min_r
        && min_r < trade_control_core::intent::MIN_R_FLOOR
    {
        return Err(eyre!(
            "min_r={min_r} is below the hard floor of {} (server-enforced)",
            trade_control_core::intent::MIN_R_FLOOR
        ));
    }

    Ok(())
}

/// True when the current template names the trade-expiry veto.
fn name_is_trade_expiry(template: &Value) -> bool {
    template
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s == TRADE_EXPIRY_NAME)
        .unwrap_or(false)
}

/// Prompt for the trade-expiry timestamp and persist it for the
/// template's instrument. Idempotent on cancel — the operator can
/// blank the prompt to skip persistence.
fn capture_trade_expiry_anchor(template: &Value) -> Result<()> {
    let Some(instrument) = template.get("instrument").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let theme = ColorfulTheme::default();
    let now = Utc::now();
    let existing = expiry::load(instrument, now);
    let default = existing.unwrap_or(now + expiry::DEFAULT_HORIZON);
    let raw: String = Input::with_theme(&theme)
        .with_prompt(format!(
            "trade-expiry timestamp for {instrument} \
             (duration like `2d` `4h`, or ISO-8601; blank to skip persistence)"
        ))
        .default(default.to_rfc3339())
        .allow_empty(true)
        .interact_text()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let resolved = resolve_not_after(trimmed, now)?;
    if resolved <= now {
        return Err(eyre!("trade-expiry must be in the future"));
    }
    expiry::save(instrument, resolved)?;
    eprintln!(
        "stored trade-expiry anchor for {instrument} at {}",
        resolved.to_rfc3339()
    );
    Ok(())
}

/// Default `not_after` suggestion. For prep/veto/enter on an
/// instrument with a live trade-expiry anchor, suggest the anchor
/// itself. Otherwise fall back to the legacy `8h` relative default.
fn not_after_default(template: &Value, now: DateTime<Utc>) -> String {
    let action = read_action(template);
    let uses_anchor = matches!(
        action,
        Some(Action::Prep) | Some(Action::Veto) | Some(Action::Enter)
    );
    if uses_anchor
        && let Some(instrument) = template.get("instrument").and_then(|v| v.as_str())
        && let Some(anchor) = expiry::load(instrument, now)
    {
        return anchor.to_rfc3339();
    }
    "8h".into()
}

/// Default `ttl_hours` suggestion. For prep/veto on an instrument
/// with a live trade-expiry anchor, suggest the hour-count between
/// now and the anchor (rounded up). Otherwise fall back to `4`.
fn ttl_hours_default(template: &Value, now: DateTime<Utc>) -> u32 {
    let action = read_action(template);
    let uses_anchor = matches!(action, Some(Action::Prep) | Some(Action::Veto));
    if uses_anchor
        && let Some(instrument) = template.get("instrument").and_then(|v| v.as_str())
        && let Some(anchor) = expiry::load(instrument, now)
    {
        let mins = (anchor - now).num_minutes().max(0);
        // Round up to the next whole hour so the TTL never expires
        // before the anchor itself.
        let hours = (mins + 59) / 60;
        return hours.try_into().unwrap_or(u32::MAX);
    }
    4
}

fn fill_round(template: &mut Value, required: &[&str], non_interactive: bool) -> Result<()> {
    let missing = missing_fields(template, required);
    if missing.is_empty() {
        return Ok(());
    }
    if non_interactive {
        return Err(eyre!(
            "missing required fields: {} (use without --non-interactive to prompt)",
            missing.join(", ")
        ));
    }
    for field in missing {
        let value = prompt_for_field(field, template)?;
        set_field(template, field, value);
    }
    Ok(())
}

/// Prompt for a single named field, returning the YAML value to splice in.
/// The strategy varies per field — enums get a `Select`, scalars get an
/// `Input` with validation, structural fields (entry, stop_loss, take_profit)
/// get a guided multi-prompt builder.
fn prompt_for_field(field: &str, template: &Value) -> Result<Value> {
    let theme = ColorfulTheme::default();
    match field {
        "v" => Ok(Value::Number(1.into())),
        "id" => {
            let now = Utc::now();
            let instrument = template
                .get("instrument")
                .and_then(|v| v.as_str())
                .unwrap_or("UNKNOWN");
            let suffix = fresh_random_suffix();
            let default = default_id(instrument, now, &suffix);
            let id: String = Input::with_theme(&theme)
                .with_prompt("id (unique per intended trade)")
                .default(default)
                .interact_text()?;
            Ok(Value::String(id))
        }
        "not_after" => {
            let now = Utc::now();
            let default = not_after_default(template, now);
            let input: String = Input::with_theme(&theme)
                .with_prompt("not_after (duration like `8h` `2d`, or ISO-8601)")
                .default(default)
                .interact_text()?;
            let resolved = resolve_not_after(&input, now)?;
            Ok(Value::String(resolved.to_rfc3339()))
        }
        "action" => prompt_action(&theme),
        "instrument" => {
            let s: String = Input::with_theme(&theme)
                .with_prompt("instrument (e.g. EUR_USD)")
                .interact_text()?;
            Ok(Value::String(s))
        }
        "broker" => prompt_broker(&theme),
        "direction" => prompt_direction(&theme),
        "entry" => prompt_entry(&theme),
        "stop_loss" => prompt_price_ref(&theme, "stop_loss"),
        "take_profit" => prompt_take_profit(&theme),
        "risk_pct" => prompt_float(&theme, "risk_pct (% of equity)", Some(0.5)),
        "cooldown_hours" => {
            let n: u32 = Input::with_theme(&theme)
                .with_prompt("cooldown_hours")
                .default(12)
                .interact_text()?;
            Ok(Value::Number(n.into()))
        }
        "step" => {
            let s = prompt_history_backed_name(
                &theme,
                NameKind::Prep,
                "step (named prep)",
                "e.g. break-and-close",
            )?;
            Ok(Value::String(s))
        }
        "name" => {
            let s = prompt_history_backed_name(
                &theme,
                NameKind::Veto,
                "name (named veto)",
                "e.g. news-window",
            )?;
            Ok(Value::String(s))
        }
        "ttl_hours" => {
            let now = Utc::now();
            let default = ttl_hours_default(template, now);
            let n: u32 = Input::with_theme(&theme)
                .with_prompt("ttl_hours")
                .default(default)
                .interact_text()?;
            Ok(Value::Number(n.into()))
        }
        other => Err(eyre!("no prompt configured for field `{other}`")),
    }
}

/// Default destination shown by `prompt_save_as_template`. Picked to put
/// the user inside their templates dir so the fuzzy picker sees it next
/// time without extra config.
fn default_save_path() -> Result<PathBuf> {
    Ok(templates_root()?.join("new.yaml"))
}

/// Prompt the operator to save the completed template YAML to disk.
/// Empty input means "skip". A non-empty path writes `completed_yaml`
/// to that path (creating the parent dir if needed) and prints a
/// one-line confirmation to stderr.
///
/// Returns the saved path (if any) so callers can log / test.
pub fn prompt_save_as_template(completed_yaml: &str) -> Result<Option<PathBuf>> {
    let theme = ColorfulTheme::default();
    let hint = default_save_path()?;
    let raw: String = Input::with_theme(&theme)
        .with_prompt(format!(
            "save as template? (path, blank to skip — e.g. {})",
            hint.display()
        ))
        .default(String::new())
        .allow_empty(true)
        .interact_text()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(trimmed);
    write_template(&path, completed_yaml)?;
    eprintln!("saved template to {}", path.display());
    Ok(Some(path))
}

/// Write `yaml` to `path`, creating the parent directory if needed.
/// Errors include the path for context.
fn write_template(path: &Path, yaml: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| eyre!("creating {}: {e}", parent.display()))?;
    }
    fs::write(path, yaml).map_err(|e| eyre!("writing {}: {e}", path.display()))?;
    Ok(())
}

/// Which history list a list-of-names field should pull suggestions from
/// and write back to. `clears` is contextual: on a `prep` action it
/// names other preps; on a `veto` action it names other vetos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NameKind {
    Prep,
    Veto,
    /// No history association — prompt without suggestions and don't
    /// record entries. Reserved for unknown fields; not currently used.
    None,
}

fn name_kind_for(field: &str, action: trade_control_core::intent::Action) -> NameKind {
    use trade_control_core::intent::Action;
    match (field, action) {
        ("requires_preps", _) => NameKind::Prep,
        ("vetos", _) => NameKind::Veto,
        ("clears", Action::Prep) => NameKind::Prep,
        ("clears", Action::Veto) => NameKind::Veto,
        _ => NameKind::None,
    }
}

/// True iff the template already picks one of the three sizing modes
/// (`risk_pct`, `risk_amount`, `size_units`). Used to skip the
/// sizing-mode picker when a template pre-fills the choice.
fn sizing_field_already_set(template: &Value) -> bool {
    ["risk_pct", "risk_amount", "size_units"]
        .iter()
        .any(|k| !matches!(template.get(*k), None | Some(Value::Null)))
}

/// Ask the operator which sizing mode to use and prompt for its value.
/// Returns `(field_name, yaml_value)` so the caller splices the value
/// under the right key. The three modes are mutually exclusive at the
/// resolver, so the picker enforces a single choice.
///
/// - Percent: `risk_pct` (default), % of equity. Backwards compatible.
/// - Amount: `risk_amount`, money sum in account currency. "Bet $1".
/// - Units: `size_units`, literal broker units / TN stake. Bypasses
///   sizing math; cap-checked via implied money risk on both brokers.
fn prompt_sizing_mode() -> Result<(&'static str, Value)> {
    let theme = ColorfulTheme::default();
    let options = [
        "risk_pct — % of account equity (default)",
        "risk_amount — fixed money sum in account currency",
        "size_units — literal units / TN stake (bypasses sizing math)",
    ];
    let idx = Select::with_theme(&theme)
        .with_prompt("sizing mode")
        .items(options.as_slice())
        .default(0)
        .interact()?;
    match idx {
        0 => {
            let value = prompt_float(&theme, "risk_pct (% of equity)", Some(0.5))?;
            Ok(("risk_pct", value))
        }
        1 => {
            let value = prompt_float(&theme, "risk_amount (account currency)", Some(1.0))?;
            Ok(("risk_amount", value))
        }
        2 => {
            let value = prompt_float(&theme, "size_units (broker units / TN stake)", None)?;
            Ok(("size_units", value))
        }
        _ => unreachable!("Select returned out-of-range index"),
    }
}

/// Prompt for the optional `dry_run` flag on `enter` intents. Defaults
/// to `false` (place the order). When `true`, the worker logs the
/// sizing inputs and skips broker dispatch entirely. Returns
/// `Value::Null` for the default so the caller's `skip_serializing_if`
/// logic keeps the wire form minimal.
fn prompt_optional_dry_run() -> Result<Value> {
    let theme = ColorfulTheme::default();
    let options = ["false — place the order (default)", "true — log only"];
    let idx = Select::with_theme(&theme)
        .with_prompt("dry_run")
        .items(options.as_slice())
        .default(0)
        .interact()?;
    match idx {
        0 => Ok(Value::Null),
        1 => Ok(Value::Bool(true)),
        _ => unreachable!("Select returned out-of-range index"),
    }
}

/// Prompt for an optional `account` name. Blank input is treated as
/// "no account — use the legacy lookup" and encoded as `Value::Null`;
/// the caller then skips setting the field so the wire form omits it.
///
/// Doesn't probe the worker for the live account list — that would
/// require the admin key in the encrypt path, which is the wrong trust
/// boundary. Operators who want auto-complete should run
/// `trade-control account list` first.
fn prompt_optional_account() -> Result<Value> {
    let theme = ColorfulTheme::default();
    let raw: String = Input::with_theme(&theme)
        .with_prompt("account (named account from the worker index; blank to skip)")
        .default(String::new())
        .allow_empty(true)
        .interact_text()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Null);
    }
    Ok(Value::String(trimmed.to_string()))
}

/// Prompt for a comma-separated list of names (used for `requires_preps`,
/// `vetos`, and `clears`). Empty input is fine and yields an empty
/// sequence — the worker treats empty as "no gate" / "no clears".
///
/// Shows recent names from history as a hint so the operator doesn't have
/// to remember exact spellings (typos silently break the gate).
fn prompt_optional_name_list(
    field: &str,
    action: trade_control_core::intent::Action,
) -> Result<Value> {
    let theme = ColorfulTheme::default();
    let history = history::load();
    let kind = name_kind_for(field, action);
    let suggestions = match kind {
        NameKind::Prep => history.prep_names(),
        NameKind::Veto => history.veto_names(),
        NameKind::None => Vec::new(),
    };
    let hint = if suggestions.is_empty() {
        String::new()
    } else {
        let preview: Vec<&str> = suggestions.iter().take(8).map(String::as_str).collect();
        format!(" [recent: {}]", preview.join(", "))
    };

    let raw: String = Input::with_theme(&theme)
        .with_prompt(format!("{field} (comma-separated, blank for none){hint}"))
        .default(String::new())
        .allow_empty(true)
        .interact_text()?;

    let names: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // Persist newly-used names so they suggest next time.
    let now = Utc::now();
    let mut h = history;
    for name in &names {
        match kind {
            NameKind::Prep => h.record_prep(name, now),
            NameKind::Veto => h.record_veto(name, now),
            NameKind::None => {}
        }
    }
    let _ = history::save(&h);

    let seq: Vec<Value> = names.into_iter().map(Value::String).collect();
    Ok(Value::Sequence(seq))
}

/// Prompt for a single name, offering recent entries from history as
/// a fuzzy-selectable list plus a "(type new...)" sentinel that drops
/// into freeform text entry. Used for `step` (prep) and `name` (veto).
///
/// `kind` decides which history list to draw from and write to.
/// `prompt_label` is shown both on the picker and on the freeform input.
/// `example` is appended to the freeform prompt for hint text
/// (e.g. `"e.g. break-and-close"`).
fn prompt_history_backed_name(
    theme: &ColorfulTheme,
    kind: NameKind,
    prompt_label: &str,
    example: &str,
) -> Result<String> {
    let history = history::load();
    let suggestions: Vec<String> = match kind {
        NameKind::Prep => history.prep_names(),
        NameKind::Veto => history.veto_names(),
        NameKind::None => Vec::new(),
    };

    let chosen = if suggestions.is_empty() {
        // No history — go straight to freeform.
        Input::<String>::with_theme(theme)
            .with_prompt(format!("{prompt_label} ({example})"))
            .interact_text()?
    } else {
        const TYPE_NEW: &str = "(type new...)";
        let mut items: Vec<&str> = suggestions.iter().map(String::as_str).collect();
        items.push(TYPE_NEW);
        let idx = FuzzySelect::with_theme(theme)
            .with_prompt(format!("{prompt_label} (recent — type to filter)"))
            .items(&items)
            .default(0)
            .interact()?;
        if items[idx] == TYPE_NEW {
            Input::<String>::with_theme(theme)
                .with_prompt(format!("{prompt_label} ({example})"))
                .interact_text()?
        } else {
            items[idx].to_string()
        }
    };

    let trimmed = chosen.trim().to_string();
    if trimmed.is_empty() {
        return Err(eyre!("{prompt_label}: empty value not allowed"));
    }

    // Promote the chosen name to the top of its history list so the
    // next prompt offers it first.
    let mut h = history::load();
    let now = Utc::now();
    match kind {
        NameKind::Prep => h.record_prep(&trimmed, now),
        NameKind::Veto => h.record_veto(&trimmed, now),
        NameKind::None => {}
    }
    let _ = history::save(&h);

    Ok(trimmed)
}

fn prompt_action(theme: &ColorfulTheme) -> Result<Value> {
    // Order is roughly "frequency of use" — trade entry first, then the
    // common control actions, then the recovery / cleanup actions.
    let choices = [
        "enter",
        "close",
        "invalidate",
        "prep",
        "veto",
        "clear-prep",
        "clear-veto",
        "status",
        "unlock",
    ];
    let idx = Select::with_theme(theme)
        .with_prompt("action")
        .items(choices)
        .default(0)
        .interact()?;
    Ok(Value::String(choices[idx].into()))
}

fn prompt_broker(theme: &ColorfulTheme) -> Result<Value> {
    let choices = ["oanda", "tradenation"];
    let idx = Select::with_theme(theme)
        .with_prompt("broker")
        .items(choices)
        .default(0)
        .interact()?;
    Ok(Value::String(choices[idx].into()))
}

fn prompt_direction(theme: &ColorfulTheme) -> Result<Value> {
    let choices = ["long", "short"];
    let idx = Select::with_theme(theme)
        .with_prompt("direction")
        .items(choices)
        .default(0)
        .interact()?;
    Ok(Value::String(choices[idx].into()))
}

fn prompt_entry(theme: &ColorfulTheme) -> Result<Value> {
    let kinds = ["market", "stop", "limit"];
    let idx = Select::with_theme(theme)
        .with_prompt("entry type")
        .items(kinds)
        .default(0)
        .interact()?;
    let mut map = serde_yaml::Mapping::new();
    map.insert(
        Value::String("type".into()),
        Value::String(kinds[idx].into()),
    );
    if kinds[idx] != "market" {
        let anchor = prompt_anchor(theme, "entry trigger anchor")?;
        let offset = prompt_float(theme, "entry offset_pips (signed)", Some(0.0))?;
        map.insert(Value::String("from".into()), anchor);
        map.insert(Value::String("offset_pips".into()), offset);
    }
    Ok(Value::Mapping(map))
}

fn prompt_price_ref(theme: &ColorfulTheme, name: &str) -> Result<Value> {
    let kinds = [
        "anchored (from candle's high/low/close + pip offset)",
        "absolute price",
    ];
    let idx = Select::with_theme(theme)
        .with_prompt(format!("{name} type"))
        .items(kinds)
        .default(0)
        .interact()?;
    let mut map = serde_yaml::Mapping::new();
    if idx == 0 {
        let anchor = prompt_anchor(theme, &format!("{name} anchor"))?;
        let offset = prompt_float(theme, &format!("{name} offset_pips (signed)"), Some(0.0))?;
        map.insert(Value::String("from".into()), anchor);
        map.insert(Value::String("offset_pips".into()), offset);
    } else {
        let price = prompt_float(theme, &format!("{name} absolute price"), None)?;
        map.insert(Value::String("absolute".into()), price);
    }
    Ok(Value::Mapping(map))
}

fn prompt_take_profit(theme: &ColorfulTheme) -> Result<Value> {
    let kinds = [
        "R-multiple of stop distance",
        "Anchored price (candle anchor + pip offset)",
        "Absolute price",
    ];
    let idx = Select::with_theme(theme)
        .with_prompt("take_profit type")
        .items(kinds)
        .default(0)
        .interact()?;
    let mut map = serde_yaml::Mapping::new();
    match idx {
        0 => {
            let anchor = prompt_anchor(theme, "take_profit reference anchor")?;
            let r = prompt_float(theme, "take_profit offset_r (R-multiple)", Some(2.0))?;
            map.insert(Value::String("from".into()), anchor);
            map.insert(Value::String("offset_r".into()), r);
        }
        1 => {
            let anchor = prompt_anchor(theme, "take_profit anchor")?;
            let offset = prompt_float(theme, "take_profit offset_pips (signed)", Some(0.0))?;
            map.insert(Value::String("from".into()), anchor);
            map.insert(Value::String("offset_pips".into()), offset);
        }
        _ => {
            let price = prompt_float(theme, "take_profit absolute price", None)?;
            map.insert(Value::String("absolute".into()), price);
        }
    }
    Ok(Value::Mapping(map))
}

fn prompt_anchor(theme: &ColorfulTheme, prompt: &str) -> Result<Value> {
    let choices = ["close", "high", "low"];
    let idx = Select::with_theme(theme)
        .with_prompt(prompt)
        .items(choices)
        .default(0)
        .interact()?;
    Ok(Value::String(choices[idx].into()))
}

fn prompt_float(theme: &ColorfulTheme, prompt: &str, default: Option<f64>) -> Result<Value> {
    let mut builder = Input::<String>::with_theme(theme).with_prompt(prompt);
    if let Some(d) = default {
        builder = builder.default(format!("{d}"));
    }
    let raw: String = builder.interact_text()?;
    let parsed: f64 = raw
        .trim()
        .parse()
        .map_err(|e| eyre!("{prompt}: not a number ({e})"))?;
    let num = serde_yaml::Number::from(parsed);
    Ok(Value::Number(num))
}

/// Pure-function unit test of `fill_missing_fields`'s non-interactive path.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_interactive_errors_on_missing_required() {
        let mut template: Value = serde_yaml::from_str("v: 1\naction: enter\n").unwrap();
        let err = fill_missing_fields(&mut template, true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing required fields"), "got: {msg}");
    }

    #[test]
    fn non_interactive_passes_when_fully_specified() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        fill_missing_fields(&mut template, true).unwrap();
        // Round-trips into a real Intent.
        let intent: Intent = serde_yaml::from_value(template).unwrap();
        assert_eq!(intent.action, Action::Enter);
    }

    #[test]
    fn non_interactive_rejects_min_r_below_floor() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            min_r: 0.5
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        let err = fill_missing_fields(&mut template, true).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("min_r"), "got: {msg}");
        assert!(msg.contains("floor"), "got: {msg}");
    }

    #[test]
    fn non_interactive_rejects_min_r_zero() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            min_r: 0
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        assert!(fill_missing_fields(&mut template, true).is_err());
    }

    #[test]
    fn non_interactive_accepts_min_r_one() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            min_r: 1.0
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        assert!(fill_missing_fields(&mut template, true).is_ok());
    }

    #[test]
    fn non_interactive_accepts_min_r_above_floor() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            min_r: 1.5
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        assert!(fill_missing_fields(&mut template, true).is_ok());
    }

    #[test]
    fn write_template_creates_parent_dir() {
        let root = std::env::temp_dir().join(format!(
            "trade-control-save-test-{}-create",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("a/b/c/new.yaml");
        write_template(&path, "v: 1\naction: enter\n").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "v: 1\naction: enter\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn write_template_overwrites_existing() {
        let root = std::env::temp_dir().join(format!(
            "trade-control-save-test-{}-overwrite",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join("foo.yaml");
        write_template(&path, "first\n").unwrap();
        write_template(&path, "second\n").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "second\n");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn non_interactive_accepts_template_supplied_preps_and_vetos() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: short
            entry: { type: market }
            stop_loss: { from: high, offset_pips: 2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            requires_preps: [break-and-close, retest]
            vetos: [news-window]
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        fill_missing_fields(&mut template, true).unwrap();
        let intent: Intent = serde_yaml::from_value(template).unwrap();
        assert_eq!(
            intent.requires_preps,
            vec!["break-and-close".to_string(), "retest".to_string()]
        );
        assert_eq!(intent.vetos, vec!["news-window".to_string()]);
    }

    #[test]
    fn non_interactive_leaves_optional_fields_empty_when_absent() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        // Non-interactive mode skips the optional prompt entirely.
        fill_missing_fields(&mut template, true).unwrap();
        let intent: Intent = serde_yaml::from_value(template).unwrap();
        assert!(intent.requires_preps.is_empty());
        assert!(intent.vetos.is_empty());
    }

    #[test]
    fn non_interactive_accepts_template_supplied_prep_clears() {
        // A prep template can pre-declare its `clears:` list — the
        // non-interactive path must honour it without prompting.
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
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        fill_missing_fields(&mut template, true).unwrap();
        let intent: Intent = serde_yaml::from_value(template).unwrap();
        assert_eq!(intent.action, Action::Prep);
        assert_eq!(intent.clears, vec!["retest".to_string()]);
    }

    #[test]
    fn non_interactive_leaves_clears_empty_when_absent_on_prep() {
        // Templates that don't mention `clears` round-trip cleanly with
        // an empty list. The optional prompt is skipped in
        // non-interactive mode.
        let yaml = "
            v: 1
            id: prep-1
            not_after: \"2026-05-14T03:30:00Z\"
            action: prep
            instrument: EUR_USD
            step: break-and-close
            ttl_hours: 4
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        fill_missing_fields(&mut template, true).unwrap();
        let intent: Intent = serde_yaml::from_value(template).unwrap();
        assert!(intent.clears.is_empty());
    }

    #[test]
    fn non_interactive_passes_for_invalidate_with_all_fields() {
        let yaml = "
            v: 1
            id: kill-eurusd
            not_after: \"2026-05-13T20:00:00Z\"
            action: invalidate
            instrument: EUR_USD
            cooldown_hours: 12
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        fill_missing_fields(&mut template, true).unwrap();
        let intent: Intent = serde_yaml::from_value(template).unwrap();
        assert_eq!(intent.action, Action::Invalidate);
        assert_eq!(intent.cooldown_hours, Some(12));
    }

    /// Mutex shared with `expiry` tests: any test that pokes
    /// `$XDG_CONFIG_HOME` has to serialize. Distinct from the `expiry`
    /// module's own guard because that one is private; the duplication
    /// is fine — both write to the same env var.
    static EXPIRY_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn isolated_xdg(tag: &str) -> (std::path::PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = EXPIRY_TEST_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!(
            "trade-control-interactive-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: serialized via EXPIRY_TEST_GUARD.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        (dir, guard)
    }

    #[test]
    fn not_after_default_falls_back_to_8h_without_anchor() {
        let (_root, _g) = isolated_xdg("not-after-fallback");
        let yaml = "
            v: 1
            action: prep
            instrument: EURUSD
        ";
        let template: Value = serde_yaml::from_str(yaml).unwrap();
        let now: chrono::DateTime<chrono::Utc> = "2026-05-19T10:00:00Z".parse().unwrap();
        assert_eq!(not_after_default(&template, now), "8h");
    }

    #[test]
    fn not_after_default_uses_anchor_for_prep_veto_enter() {
        let (_root, _g) = isolated_xdg("not-after-anchor");
        let now: chrono::DateTime<chrono::Utc> = "2026-05-19T10:00:00Z".parse().unwrap();
        let anchor: chrono::DateTime<chrono::Utc> = "2026-05-22T14:00:00Z".parse().unwrap();
        expiry::save("GBPJPY", anchor).unwrap();

        for action in ["prep", "veto", "enter"] {
            let yaml = format!("v: 1\naction: {action}\ninstrument: GBPJPY\n");
            let template: Value = serde_yaml::from_str(&yaml).unwrap();
            let suggested = not_after_default(&template, now);
            assert_eq!(suggested, anchor.to_rfc3339(), "action={action}");
        }
    }

    #[test]
    fn not_after_default_ignores_anchor_for_other_actions() {
        let (_root, _g) = isolated_xdg("not-after-other-actions");
        let now: chrono::DateTime<chrono::Utc> = "2026-05-19T10:00:00Z".parse().unwrap();
        let anchor: chrono::DateTime<chrono::Utc> = "2026-05-22T14:00:00Z".parse().unwrap();
        expiry::save("EURUSD", anchor).unwrap();

        // `invalidate`, `close`, `status`, `unlock`, `clear-prep`,
        // `clear-veto` shouldn't inherit the anchor — they're not part
        // of the setup-build cycle.
        for action in [
            "invalidate",
            "close",
            "status",
            "unlock",
            "clear-prep",
            "clear-veto",
        ] {
            let yaml = format!("v: 1\naction: {action}\ninstrument: EURUSD\n");
            let template: Value = serde_yaml::from_str(&yaml).unwrap();
            assert_eq!(not_after_default(&template, now), "8h", "action={action}");
        }
    }

    #[test]
    fn ttl_hours_default_falls_back_to_4() {
        let (_root, _g) = isolated_xdg("ttl-fallback");
        let yaml = "v: 1\naction: prep\ninstrument: EURUSD\n";
        let template: Value = serde_yaml::from_str(yaml).unwrap();
        let now: chrono::DateTime<chrono::Utc> = "2026-05-19T10:00:00Z".parse().unwrap();
        assert_eq!(ttl_hours_default(&template, now), 4);
    }

    #[test]
    fn ttl_hours_default_rounds_up_to_anchor() {
        let (_root, _g) = isolated_xdg("ttl-anchor");
        let now: chrono::DateTime<chrono::Utc> = "2026-05-19T10:00:00Z".parse().unwrap();
        // Anchor is exactly 3h30m away — should round up to 4.
        let anchor = now + chrono::Duration::minutes(210);
        expiry::save("GBPJPY", anchor).unwrap();
        let yaml = "v: 1\naction: prep\ninstrument: GBPJPY\n";
        let template: Value = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(ttl_hours_default(&template, now), 4);
    }

    #[test]
    fn ttl_hours_default_handles_full_day_anchor() {
        let (_root, _g) = isolated_xdg("ttl-day");
        let now: chrono::DateTime<chrono::Utc> = "2026-05-19T10:00:00Z".parse().unwrap();
        let anchor = now + chrono::Duration::days(2);
        expiry::save("USDJPY", anchor).unwrap();
        let yaml = "v: 1\naction: veto\ninstrument: USDJPY\n";
        let template: Value = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(ttl_hours_default(&template, now), 48);
    }

    #[test]
    fn sizing_field_already_set_detects_each_mode() {
        // Templates that pre-pick any one of the three sizing fields
        // suppress the interactive mode picker.
        for yaml in [
            "risk_pct: 0.5\n",
            "risk_amount: 1.0\n",
            "size_units: 0.01\n",
        ] {
            let v: Value = serde_yaml::from_str(yaml).unwrap();
            assert!(sizing_field_already_set(&v), "should detect: {yaml}");
        }
    }

    #[test]
    fn sizing_field_already_set_returns_false_when_all_absent() {
        let v: Value = serde_yaml::from_str("v: 1\naction: enter\n").unwrap();
        assert!(!sizing_field_already_set(&v));
    }

    #[test]
    fn sizing_field_already_set_treats_null_as_absent() {
        // YAML `risk_pct: ~` (explicit null) is "not picked" — same
        // semantics as `missing_fields` treating null as missing.
        let v: Value = serde_yaml::from_str("risk_pct: ~\n").unwrap();
        assert!(!sizing_field_already_set(&v));
    }

    #[test]
    fn non_interactive_accepts_template_with_risk_amount() {
        // risk_amount picks fixed-money sizing; non-interactive should
        // pass without prompting for risk_pct.
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_amount: 1.0
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        fill_missing_fields(&mut template, true).unwrap();
        let intent: Intent = serde_yaml::from_value(template).unwrap();
        assert_eq!(intent.risk_amount, Some(1.0));
        assert_eq!(intent.risk_pct, None);
    }

    #[test]
    fn non_interactive_accepts_template_with_size_units() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            size_units: 0.01
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        fill_missing_fields(&mut template, true).unwrap();
        let intent: Intent = serde_yaml::from_value(template).unwrap();
        assert_eq!(intent.size_units, Some(0.01));
        assert_eq!(intent.risk_pct, None);
    }

    #[test]
    fn non_interactive_accepts_template_with_dry_run() {
        let yaml = "
            v: 1
            id: abc
            not_after: \"2026-05-13T20:00:00Z\"
            action: enter
            instrument: EUR_USD
            broker: oanda
            direction: long
            entry: { type: market }
            stop_loss: { from: low, offset_pips: -2 }
            take_profit: { from: close, offset_r: 2.0 }
            risk_pct: 0.5
            dry_run: true
        ";
        let mut template: Value = serde_yaml::from_str(yaml).unwrap();
        fill_missing_fields(&mut template, true).unwrap();
        let intent: Intent = serde_yaml::from_value(template).unwrap();
        assert_eq!(intent.dry_run, Some(true));
    }

    #[test]
    fn name_is_trade_expiry_recognises_the_reserved_name() {
        let yaml = "name: trade-expiry\n";
        let v: Value = serde_yaml::from_str(yaml).unwrap();
        assert!(name_is_trade_expiry(&v));

        let yaml2 = "name: news-window\n";
        let v2: Value = serde_yaml::from_str(yaml2).unwrap();
        assert!(!name_is_trade_expiry(&v2));

        let v3: Value = serde_yaml::from_str("v: 1\n").unwrap();
        assert!(!name_is_trade_expiry(&v3));
    }
}
