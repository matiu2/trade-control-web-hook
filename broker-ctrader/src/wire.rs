use color_eyre::{
    Result,
    eyre::{ensure, eyre},
};
use serde_json::Value;

pub(crate) fn integer(v: &Value, key: &str) -> Result<i64> {
    let n = &v[key];
    n.as_i64()
        .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| eyre!("cTrader response missing integer {key}"))
}
pub(crate) fn number(v: &Value, key: &str) -> Result<f64> {
    let n = &v[key];
    let value = n
        .as_f64()
        .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| eyre!("cTrader response missing number {key}"))?;
    ensure!(value.is_finite(), "nonfinite cTrader {key}");
    Ok(value)
}
pub(crate) fn optional_number(v: &Value, key: &str) -> Option<f64> {
    number(v, key).ok()
}
pub(crate) fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v[key].as_array().map(Vec::as_slice).unwrap_or_default()
}
pub(crate) fn money(v: &Value, key: &str) -> Result<f64> {
    let digits = integer(v, "moneyDigits")?;
    ensure!(
        (0..=12).contains(&digits),
        "invalid cTrader moneyDigits {digits}"
    );
    Ok(number(v, key)? / 10f64.powi(digits as i32))
}
pub(crate) fn is_enum(v: &Value, key: &str, n: i64, name: &str) -> bool {
    integer(v, key).ok() == Some(n) || v[key].as_str() == Some(name)
}
pub(crate) fn lookup(error: color_eyre::Report) -> trade_control_core::broker::LookupError {
    tracing::warn!(%error,"cTrader read failed");
    trade_control_core::broker::LookupError::Transient
}
pub(crate) fn direction(v: &Value) -> Result<trade_control_core::intent::Direction> {
    if is_enum(v, "tradeSide", 1, "BUY") {
        Ok(trade_control_core::intent::Direction::Long)
    } else if is_enum(v, "tradeSide", 2, "SELL") {
        Ok(trade_control_core::intent::Direction::Short)
    } else {
        Err(eyre!("unknown cTrader trade side"))
    }
}

pub(crate) fn array_shape(v: &Value, key: &str) -> Result<()> {
    ensure!(
        v.get(key).is_none_or(Value::is_array),
        "cTrader {key} must be an array"
    );
    Ok(())
}
pub(crate) fn validate_snapshot(v: &Value) -> Result<()> {
    for key in ["position", "order"] {
        array_shape(v, key)?;
        for row in array(v, key) {
            let td = &row["tradeData"];
            ensure!(
                integer(td, "symbolId")? > 0 && integer(td, "volume")? >= 0,
                "invalid cTrader snapshot trade data"
            );
            direction(td)?;
            if key == "position" {
                ensure!(
                    integer(row, "positionId")? > 0,
                    "invalid cTrader position ID"
                );
                ensure!(
                    [
                        (1, "POSITION_STATUS_OPEN"),
                        (2, "POSITION_STATUS_CLOSED"),
                        (3, "POSITION_STATUS_CREATED"),
                        (4, "POSITION_STATUS_ERROR")
                    ]
                    .iter()
                    .any(|(n, name)| is_enum(row, "positionStatus", *n, name)),
                    "unknown cTrader snapshot position status"
                );
            } else {
                ensure!(integer(row, "orderId")? > 0, "invalid cTrader order ID");
                ensure!(
                    [
                        (1, "ORDER_STATUS_ACCEPTED"),
                        (2, "ORDER_STATUS_FILLED"),
                        (3, "ORDER_STATUS_REJECTED"),
                        (4, "ORDER_STATUS_EXPIRED"),
                        (5, "ORDER_STATUS_CANCELLED")
                    ]
                    .iter()
                    .any(|(n, name)| is_enum(row, "orderStatus", *n, name)),
                    "unknown cTrader snapshot order status"
                );
            }
        }
    }
    Ok(())
}
