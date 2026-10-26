//! Verify the newly filled position and both absolute protection levels.
use super::*;
use proof::{account, trade_data};

pub(super) fn position<'a>(
    snapshot: &'a Value,
    terms: &Terms,
    order: i64,
    id: i64,
    filled: i64,
) -> Result<&'a Value> {
    account(snapshot, terms)?;
    wire::validate_snapshot(snapshot)?;
    ensure!(
        !array(snapshot, "order")
            .iter()
            .any(|row| integer(row, "orderId").ok() == Some(order)),
        "market order remains pending after terminal fill"
    );
    let rows: Vec<_> = array(snapshot, "position")
        .iter()
        .filter(|row| integer(row, "positionId").ok() == Some(id))
        .collect();
    ensure!(
        rows.len() == 1 && id > 0 && !terms.before.contains(&id),
        "market fill has no unique new position"
    );
    identity(rows[0], terms, id, filled)?;
    Ok(rows[0])
}

fn identity(position: &Value, terms: &Terms, id: i64, filled: i64) -> Result<()> {
    ensure!(
        integer(position, "positionId")? == id
            && is_enum(position, "positionStatus", 1, "POSITION_STATUS_OPEN"),
        "market position identity/status mismatch"
    );
    trade_data(&position["tradeData"], terms, filled)?;
    ensure!(
        wire::number(position, "price")? > 0.0,
        "market position lacks actual fill price"
    );
    Ok(())
}

pub(super) fn fill_geometry(position: &Value, terms: &Terms) -> Result<()> {
    let price = wire::number(position, "price")?;
    let sign = if terms.direction == Direction::Long {
        1.0
    } else {
        -1.0
    };
    ensure!(
        (price - terms.stop) * sign > 0.0 && (terms.target - price) * sign > 0.0,
        "actual market fill is outside the requested absolute protection"
    );
    let distance = (price - terms.stop) * sign;
    ensure!(
        terms.max_stop_distance.is_finite()
            && terms.max_stop_distance > 0.0
            && distance <= terms.max_stop_distance * (1.0 + 1e-10),
        "market fill would exceed the authorized risk budget when restoring absolute SL"
    );
    let stop = wire::number(position, "stopLoss")?;
    let target = wire::number(position, "takeProfit")?;
    ensure!(
        stop > 0.0 && target > 0.0 && (price - stop) * sign > 0.0 && (target - price) * sign > 0.0,
        "market position lacks valid initial relative protection"
    );
    Ok(())
}

pub(super) fn protected(position: &Value, terms: &Terms, id: i64, filled: i64) -> Result<()> {
    identity(position, terms, id, filled)?;
    fill_geometry(position, terms)?;
    ensure!(
        (wire::number(position, "stopLoss")? - terms.stop).abs() < 1e-9
            && (wire::number(position, "takeProfit")? - terms.target).abs() < 1e-9,
        "market absolute SL/TP normalization was not confirmed"
    );
    Ok(())
}
