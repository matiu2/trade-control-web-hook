//! Read the dispatcher's fixed recovery stages; broker diagnostics stay opaque.

pub(super) fn render(detail: &str, category: &str) -> String {
    let mut remaining = detail;
    std::iter::from_fn(|| {
        if remaining.is_empty() {
            return None;
        }
        let (step, rest) = take_step(remaining);
        remaining = rest;
        Some(step)
    })
    .enumerate()
    .map(|(index, step)| format!("{}. {}", index + 1, render_step(step, category)))
    .collect::<Vec<_>>()
    .join("\n\n")
}

fn take_step(detail: &str) -> (&str, &str) {
    // These terminal stages can carry arbitrary broker comments, semicolons,
    // or recovery-like words. Never split those diagnostics into fake steps.
    if detail.starts_with("market fallback: ")
        || detail.starts_with("limit failed: ")
        || detail.starts_with("market not attempted: ")
    {
        return (detail, "");
    }
    detail.split_once("; ").unwrap_or((detail, ""))
}

fn render_step(step: &str, category: &str) -> String {
    if let Some(detail) = step.strip_prefix("stop rejected: ") {
        return pending_block("Stop", detail);
    }
    if let Some(detail) = step.strip_prefix("limit rejected: ") {
        return pending_block("Limit", detail);
    }
    if let Some(detail) = step.strip_prefix("limit failed: ") {
        let (error, skipped) = detail
            .strip_suffix("; market not attempted: failure is not verified proximity")
            .map_or((detail, ""), |error| {
                (
                    error,
                    "\n   Market fallback was not attempted: failure is not verified proximity.",
                )
            });
        return format!("{category} — Limit entry failed.\n   {error}{skipped}");
    }
    if let Some(detail) = step.strip_prefix("market fallback: ") {
        let (quote, result) = detail
            .split_once("; market result: ")
            .unwrap_or((detail, ""));
        let mut message = format!("MARKET FALLBACK — Local proximity check passed.\n   {quote}");
        if !result.is_empty() {
            message.push_str(&format!("\n   {category} — {result}"));
        }
        return message;
    }
    if let Some(reason) = step.strip_prefix("market not attempted: ") {
        return format!("RECOVERY STOPPED — Market fallback was not attempted.\n   {reason}");
    }
    // Single-recovery and older distance-only records have no stage prefix.
    if step.contains(" blocked before submission:") {
        return pending_block("Pending", step);
    }
    format!("RECOVERY DETAIL — {step}")
}

fn pending_block(kind: &str, detail: &str) -> String {
    if detail.contains(" blocked before submission:") {
        format!(
            "LOCAL BLOCK — {kind} entry was not submitted: {}.\n   {detail}",
            distance_reason(detail)
        )
    } else {
        // Do not infer the source if a future producer changes the format.
        format!("ENTRY BLOCKED — {kind} entry.\n   {detail}")
    }
}

fn distance_reason(detail: &str) -> &'static str {
    let distance = detail
        .split_whitespace()
        .find_map(|field| field.strip_prefix("distance="))
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite());
    match distance {
        Some(value) if value <= 0.0 => "trigger is on the wrong side of the market",
        Some(_) => "trigger is closer to the market than the minimum allowed distance",
        None => "pending-entry distance validation failed",
    }
}
