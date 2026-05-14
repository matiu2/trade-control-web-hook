//! Terminal I/O layer for the encrypt CLI. Wraps the pure helpers in
//! [`super::prompts`] with `dialoguer` prompts and a "fill the template"
//! driver loop.

use chrono::Utc;
use color_eyre::eyre::{Result, eyre};
use dialoguer::{Input, Select, theme::ColorfulTheme};
use serde_yaml::Value;

use super::prompts::{
    ALWAYS_REQUIRED, default_id, fresh_random_suffix, missing_fields, read_action,
    required_for_action, resolve_not_after, set_field,
};
#[cfg(test)]
use crate::intent::Action;
use crate::intent::Intent;

/// Drive the template to completion by prompting for missing fields. After
/// this returns, the template deserializes into a valid `Intent` for its
/// declared action.
///
/// `non_interactive`: when true, missing fields cause an error instead of a
/// prompt. Useful in scripts.
pub fn fill_missing_fields(template: &mut Value, non_interactive: bool) -> Result<()> {
    // Pass 1: always-required structural fields.
    let always = ALWAYS_REQUIRED.to_vec();
    fill_round(template, &always, non_interactive)?;

    // Pass 2: action-dependent fields. Need a valid action first.
    let action = read_action(template).ok_or_else(|| eyre!("action still missing after prompt"))?;
    let action_required: Vec<&'static str> = required_for_action(action).to_vec();
    fill_round(template, &action_required, non_interactive)?;

    // Final validation: deserialize fully into `Intent` to surface any
    // structural mistakes (bad enum variants, wrong value types) before we
    // encrypt and the worker rejects it.
    let intent: Intent = serde_yaml::from_value(template.clone())
        .map_err(|e| eyre!("template doesn't parse as a valid Intent: {e}"))?;

    // Fail fast on a below-floor `min_r`. The server also enforces this; we
    // duplicate it here so typos don't even get encrypted.
    if let Some(min_r) = intent.min_r
        && min_r < crate::intent::MIN_R_FLOOR
    {
        return Err(eyre!(
            "min_r={min_r} is below the hard floor of {} (server-enforced)",
            crate::intent::MIN_R_FLOOR
        ));
    }

    Ok(())
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
            let input: String = Input::with_theme(&theme)
                .with_prompt("not_after (duration like `8h` `2d`, or ISO-8601)")
                .default("8h".into())
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
        other => Err(eyre!("no prompt configured for field `{other}`")),
    }
}

fn prompt_action(theme: &ColorfulTheme) -> Result<Value> {
    let choices = ["enter", "close", "invalidate"];
    let idx = Select::with_theme(theme)
        .with_prompt("action")
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
}
