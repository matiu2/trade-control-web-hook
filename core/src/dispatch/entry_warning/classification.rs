//! Legacy failure tokens describe error families, not proof of transmission.

pub(super) fn category(failure: &str) -> &'static str {
    let token = failure
        .strip_prefix("entry-failed: ")
        .and_then(|s| s.split_whitespace().next());
    match token {
        Some("broker-rejected") => rejection_source(reason(failure)),
        Some("ambiguous-success") => "OUTCOME UNKNOWN",
        Some(
            "risk-cap-exceeded"
            | "open-positions-cap"
            | "units-below-minimum"
            | "contract-size-unavailable"
            | "pending-entry-too-close"
            | "market-recovery-declined",
        ) => "LOCAL BLOCK",
        Some("account-fetch" | "currency-conversion" | "equity-parse") => "LOCAL ERROR",
        // A legacy too-close error does not say whether validation was local.
        _ => "ENTRY ERROR",
    }
}

pub(super) fn reason(failure: &str) -> &str {
    failure
        .strip_prefix("entry-failed: ")
        .and_then(|s| s.split_once(" ("))
        .and_then(|(_, reason)| reason.strip_suffix(')'))
        .unwrap_or(failure)
}

fn rejection_source(reason: &str) -> &'static str {
    let detail = reason
        .strip_prefix("broker rejected the order: ")
        .unwrap_or(reason);
    if [
        "MT5 entry validation before submission:",
        "cTrader market entries require ",
        "cTrader FIX cannot open strategy entries:",
        "market audit ID missing; no order submitted",
    ]
    .iter()
    .any(|prefix| detail.starts_with(prefix))
    {
        return "LOCAL BLOCK";
    }
    if [
        "MT5 entry rejected before transmission:",
        "Open API entry not sent:",
        "not submitted:",
        "market entry not submitted:",
    ]
    .iter()
    .any(|prefix| detail.starts_with(prefix))
    {
        return "LOCAL ERROR";
    }
    if mt5_rejection_evidence(detail) {
        "BROKER REJECTION"
    } else {
        "ENTRY ERROR"
    }
}

fn mt5_rejection_evidence(detail: &str) -> bool {
    let Some(response) = detail.strip_prefix("MT5 request_id=") else {
        return false;
    };
    // Only the response header is evidence. A broker comment is arbitrary text.
    let header = response.split(" comment=").next().unwrap_or(response);
    header.split_whitespace().any(|field| {
        field
            .strip_prefix("result_code=")
            .or_else(|| field.strip_prefix("submission_code="))
            .and_then(|value| value.trim_end_matches(';').parse::<u32>().ok())
            .is_some_and(|code| code != 0)
    })
}
