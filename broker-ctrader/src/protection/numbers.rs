use super::*;
use std::str::FromStr;
pub(super) fn decimal(value: f64) -> Result<Decimal> {
    ensure!(value.is_finite(), "nonfinite broker number");
    Ok(Decimal::from_str(&value.to_string())?)
}
pub(super) fn number(v: &Value, key: &str) -> Result<Decimal> {
    let raw = v
        .get(key)
        .ok_or_else(|| eyre!("missing broker field {key}"))?;
    match raw {
        Value::String(s) => Ok(Decimal::from_str(s)?),
        Value::Number(n) => Ok(Decimal::from_str(&n.to_string())?),
        _ => Err(eyre!("invalid decimal broker field {key}")),
    }
}
pub(super) fn money(v: &Value, key: &str) -> Result<Decimal> {
    let digits = wire::integer(v, "moneyDigits")?;
    ensure!((0..=12).contains(&digits), "invalid money digits");
    divide(number(v, key)?, Decimal::from(10u64.pow(digits as u32)))
}
pub(super) fn plus(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_add(b).ok_or_else(|| eyre!("risk overflow"))
}
pub(super) fn minus(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_sub(b).ok_or_else(|| eyre!("risk overflow"))
}
pub(super) fn times(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_mul(b).ok_or_else(|| eyre!("risk overflow"))
}
pub(super) fn divide(a: Decimal, b: Decimal) -> Result<Decimal> {
    a.checked_div(b)
        .ok_or_else(|| eyre!("risk overflow/invalid divisor"))
}
pub(super) fn float(a: Decimal) -> Result<f64> {
    Ok(a.to_string().parse()?)
}
