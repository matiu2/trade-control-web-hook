use super::{Adoption, FillProof};
use chrono::{DateTime, Duration, Utc};
use color_eyre::eyre::{Result, ensure, eyre};
use trade_control_core::{
    incoming::{parse_and_verify, replay_ttl_seconds},
    intent::{Action, BrokerKind, EntrySpec, Resolved, RiskBudget},
    recording::RequestRecord,
    state::EntryAttempt,
};

/// Recover exactly one signed market request associated with this audit ID.
/// Historical freshness is verified at receipt, never bypassed or re-signed.
pub fn recover(
    records: &[RequestRecord],
    key: &[u8; 32],
    trade_id: &str,
    proof: FillProof,
    now: DateTime<Utc>,
) -> Result<Adoption> {
    validate_proof(&proof)?;
    let matching = records
        .iter()
        .filter(|record| {
            record.trade_id.as_deref() == Some(trade_id)
                && audit_token(&record.outcome, &proof.audit_id)
        })
        .collect::<Vec<_>>();
    ensure!(
        !matching.is_empty(),
        "no recorded request bound to this trade and broker audit ID"
    );
    let first = matching[0];
    ensure!(
        matching.iter().all(|record| record.body == first.body),
        "multiple different signed requests are associated with this broker audit ID"
    );
    let received = DateTime::parse_from_rfc3339(&first.ts)?.with_timezone(&Utc);
    let verified = parse_and_verify(&first.body, key, received)
        .map_err(|error| eyre!("original request authentication failed: {error:?}"))?;
    let intent = &verified.intent;
    ensure!(
        intent.action == Action::Enter && intent.broker == BrokerKind::Ctrader,
        "original request is not a cTrader entry"
    );
    ensure!(
        intent.trade_id.as_deref() == Some(trade_id)
            && intent.account.as_deref() == Some(proof.account.as_str()),
        "signed account or trade ID does not match broker proof"
    );
    ensure!(
        intent.instrument == proof.instrument && intent.direction == Some(proof.direction),
        "signed instrument or direction does not match broker proof"
    );
    ensure!(
        first.intent_id.as_deref() == Some(intent.id.as_str()),
        "recorded intent ID does not match signed request"
    );
    ensure!(
        matches!(intent.entry, Some(EntrySpec::Market)) && intent.mw.is_none(),
        "only ordinary filled market requests can be attached"
    );
    ensure!(
        intent.breakeven.is_none(),
        "request contains break-even policy requiring an explicit verified timeframe; attachment refused"
    );
    let pip = intent
        .pip_size
        .ok_or_else(|| eyre!("signed request has no native pip size"))?;
    ensure!(
        same(pip, proof.pip_size),
        "signed pip size does not match instrument metadata"
    );
    // Native dispatch has no edge tick override: use the signed grid, falling
    // back to the pip grid for legacy requests. Identity rounding would compare
    // unrounded drawings with the already-snapped broker audit.
    let tick = intent.tick_size.unwrap_or(pip);
    let resolved = Resolved::from_intent(intent, &verified.shell, pip, tick)
        .map_err(|error| eyre!("original request resolution failed: {error:?}"))?;
    ensure!(
        !resolved.dry_run && resolved.contract_multiplier.is_none(),
        "dry-run or futures request cannot own this fill"
    );
    match_audit(&resolved, &proof)?;
    let ttl = i64::try_from(replay_ttl_seconds(intent.not_after, now))?;
    let expires_at = now
        .checked_add_signed(Duration::seconds(ttl))
        .ok_or_else(|| eyre!("replay expiry overflow"))?;
    let attempt = EntryAttempt {
        trade_id: trade_id.into(),
        account: Some(proof.account.clone()),
        instrument: proof.instrument.clone(),
        attempt_no: 1,
        broker_order_id: proof.order_id.clone(),
        broker_trade_id: Some(proof.position_id.clone()),
        direction: proof.direction,
        placed_at: proof.opened_at,
        shell_time: verified.shell.time,
        expires_at,
        stop_loss_price: Some(proof.stop_loss),
        adverse_extreme: None,
        cancel_at: None,
        pip_size: Some(pip),
        blackout_close: intent.blackout_close,
        breakeven: None,
        order_control: None,
        superseded: false,
    };
    Ok(Adoption {
        proof,
        request_id: first.request_id.clone(),
        intent_id: intent.id.clone(),
        attempt,
    })
}

fn validate_proof(proof: &FillProof) -> Result<()> {
    ensure!(
        !proof.account.is_empty()
            && !proof.audit_id.is_empty()
            && !proof.order_id.is_empty()
            && !proof.position_id.is_empty(),
        "broker proof has missing identity"
    );
    ensure!(
        proof.account_id > 0
            && [
                proof.units,
                proof.entry_price,
                proof.stop_loss,
                proof.take_profit,
                proof.pip_size
            ]
            .into_iter()
            .all(|number| number.is_finite() && number > 0.0),
        "broker proof has invalid quantities or protection"
    );
    if let Some(exit) = &proof.closed_exit {
        ensure!(
            exit.broker_trade_id == proof.position_id,
            "closed fill refers to a different position"
        );
    }
    Ok(())
}

fn match_audit(resolved: &Resolved, proof: &FillProof) -> Result<()> {
    let audit = &proof.audit_intent;
    ensure!(
        audit["instrument"].as_str() == Some(proof.instrument.as_str())
            && audit["direction"] == serde_json::to_value(proof.direction)?
            && audit["entry_kind"].as_str() == Some("market"),
        "broker audit identity does not match signed request"
    );
    let (risk_kind, risk_value) = match resolved.risk {
        RiskBudget::Percent(value) => ("percent", value),
        RiskBudget::Amount(value) => ("amount", value),
        RiskBudget::Units(value) => ("units", value),
    };
    ensure!(
        audit["risk_kind"].as_str() == Some(risk_kind),
        "broker audit risk mode differs from signed request"
    );
    for (name, expected) in [
        ("entry_price", resolved.entry.reference_price()),
        ("stop_loss", resolved.stop_loss),
        ("take_profit", resolved.take_profit),
        ("risk_value", risk_value),
    ] {
        ensure!(
            audit[name]
                .as_f64()
                .is_some_and(|actual| same(actual, expected)),
            "broker audit {name} differs from signed request"
        );
    }
    ensure!(
        same(resolved.stop_loss, proof.requested_stop_loss)
            && same(resolved.take_profit, proof.requested_take_profit),
        "broker requested protection differs from signed request"
    );
    Ok(())
}

// Prevent a shorter audit ID accidentally matching another request's opaque ID.
fn audit_token(outcome: &str, audit_id: &str) -> bool {
    outcome.match_indices(audit_id).any(|(offset, _)| {
        let token = |ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':');
        outcome[..offset]
            .chars()
            .next_back()
            .is_none_or(|ch| !token(ch))
            && outcome[offset + audit_id.len()..]
                .chars()
                .next()
                .is_none_or(|ch| !token(ch))
    })
}

fn same(left: f64, right: f64) -> bool {
    left.is_finite()
        && right.is_finite()
        && (left - right).abs() <= 1e-10 * left.abs().max(right.abs()).max(1.0)
}
