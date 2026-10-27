use super::*;
use std::collections::BTreeMap;
pub(super) fn verify(events: &[adaptive_risk::ledger::Execution], snapshot: &Value) -> Result<()> {
    let mut quantities = BTreeMap::<String, Decimal>::new();
    for event in events {
        let previous = quantities
            .get(&event.position_id)
            .copied()
            .unwrap_or_default();
        quantities.insert(
            event.position_id.clone(),
            plus(previous, event.volume_delta)?,
        );
    }
    let broker = wire::array(snapshot, "position")
        .iter()
        .map(|position| {
            Ok((
                wire::integer(position, "positionId")?.to_string(),
                divide(
                    number(&position["tradeData"], "volume")?,
                    Decimal::from(100),
                )?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    ensure!(
        quantities.iter().all(|(id, qty)| *qty >= Decimal::ZERO
            && *qty == broker.get(id).copied().unwrap_or_default())
            && broker.keys().all(|id| quantities.contains_key(id)),
        "execution inventory disagrees with broker positions"
    );
    Ok(())
}
