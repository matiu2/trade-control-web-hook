//! Resolve an FX rate that converts a price quoted in an instrument's
//! quote currency into the account's currency.
//!
//! Used by position sizing: a risk budget in account currency divided
//! by a stop distance in quote currency under-sizes when the two
//! currencies differ. Multiplying the budget through this rate first
//! aligns the units (`account_per_unit_of_quote`), so the floored
//! `units = budget * rate / stop_distance` matches the trader's
//! intended risk.
//!
//! ## Direction convention
//!
//! `resolve_quote_to_account_rate("CHF", "AUD", client, account_id)`
//! returns the AUD value of one CHF. Examples:
//!
//! - Account = AUD, instrument = NZD_CHF. Quote ccy = CHF. We need
//!   AUD per CHF. OANDA quotes `AUD_CHF` (CHF per AUD), so the rate
//!   is `1.0 / mid(AUD_CHF)`.
//! - Account = USD, instrument = EUR_USD. Quote ccy = USD. We need
//!   USD per USD = 1.0. Short-circuit, no OANDA call.
//! - Account = JPY, instrument = AUD_JPY. Quote ccy = JPY. We need
//!   JPY per JPY = 1.0. Short-circuit.
//! - Account = EUR, instrument = USD_JPY. Quote ccy = JPY. OANDA
//!   quotes `EUR_JPY` (JPY per EUR), so the rate is `1.0 / mid(EUR_JPY)`.
//!
//! ## Instrument selection
//!
//! OANDA's pricing endpoint takes a single instrument name; we try the
//! pair in both directions (`QUOTE_ACCOUNT`, then `ACCOUNT_QUOTE`) and
//! invert if necessary. If neither resolves, we return
//! [`FxRateError::Unresolved`] so the caller can reject the entry —
//! silently falling back to 1.0 would mis-size the trade.

use oanda_client::OandaClient;

/// Failure modes for [`resolve_quote_to_account_rate`].
#[derive(Debug)]
pub enum FxRateError {
    /// Neither `QUOTE_ACCOUNT` nor `ACCOUNT_QUOTE` could be priced. The
    /// inner string is what we tried, for logging.
    Unresolved(String),
    /// OANDA returned a price but the mid couldn't be parsed (empty
    /// ladders or non-numeric strings). Shouldn't happen for tradable
    /// instruments — defensive only.
    BadPrice(String),
    /// One of the supplied codes was empty or contained an underscore
    /// (which would collide with OANDA's `BASE_QUOTE` separator).
    BadCurrency(String),
}

impl core::fmt::Display for FxRateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unresolved(s) => write!(f, "could not resolve FX rate: tried {s}"),
            Self::BadPrice(s) => write!(f, "fx price unparseable: {s}"),
            Self::BadCurrency(s) => write!(f, "invalid currency code: {s}"),
        }
    }
}

impl std::error::Error for FxRateError {}

/// Resolve the rate that converts one unit of `quote_ccy` into
/// `account_ccy`. Returns `1.0` when the two match (case-insensitive,
/// no OANDA call).
///
/// Strategy: query OANDA pricing for `QUOTE_ACCOUNT` first; if that
/// works, return the mid directly. If OANDA rejects the symbol
/// (instrument not available on this account), try `ACCOUNT_QUOTE` and
/// invert. If neither resolves, return [`FxRateError::Unresolved`].
pub async fn resolve_quote_to_account_rate(
    client: &OandaClient,
    account_id: &str,
    quote_ccy: &str,
    account_ccy: &str,
) -> Result<f64, FxRateError> {
    let quote = quote_ccy.trim().to_uppercase();
    let account = account_ccy.trim().to_uppercase();
    if quote.is_empty() || account.is_empty() || quote.contains('_') || account.contains('_') {
        return Err(FxRateError::BadCurrency(format!(
            "{quote_ccy}/{account_ccy}"
        )));
    }
    if quote == account {
        return Ok(1.0);
    }

    let direct = format!("{quote}_{account}");
    if let Some(rate) = try_mid(client, account_id, &direct).await? {
        return Ok(rate);
    }

    let inverse = format!("{account}_{quote}");
    if let Some(rate) = try_mid(client, account_id, &inverse).await? {
        if rate <= 0.0 || !rate.is_finite() {
            return Err(FxRateError::BadPrice(format!("{inverse}={rate}")));
        }
        return Ok(1.0 / rate);
    }

    Err(FxRateError::Unresolved(format!("{direct}, {inverse}")))
}

/// Attempt to price `instrument` on `account_id` and return its mid.
///
/// Returns:
/// - `Ok(Some(mid))` — instrument priced fine.
/// - `Ok(None)` — OANDA rejected the symbol (treated as "this pair
///   doesn't exist for this account"; caller should try the inverse).
/// - `Err(BadPrice)` — instrument returned but ladders empty or
///   unparseable.
async fn try_mid(
    client: &OandaClient,
    account_id: &str,
    instrument: &str,
) -> Result<Option<f64>, FxRateError> {
    match client.get_pricing(account_id, &[instrument]).await {
        Ok(resp) => {
            let Some(tick) = resp.prices.into_iter().next() else {
                return Ok(None);
            };
            match tick.mid() {
                Some(m) if m > 0.0 && m.is_finite() => Ok(Some(m)),
                _ => Err(FxRateError::BadPrice(format!("{instrument} mid missing"))),
            }
        }
        Err(err) => {
            // OANDA returns 400 with "instrument not found" for symbols
            // the account can't price. We can't distinguish that from
            // transient errors without parsing the error body, so the
            // pragmatic thing is to log and treat any error as "try the
            // inverse" — if both fail we surface Unresolved.
            tracing::error!("oanda pricing {instrument}: {err:?}");
            Ok(None)
        }
    }
}

/// Extract the quote currency from an OANDA instrument name. OANDA uses
/// `BASE_QUOTE` (e.g. `EUR_USD` → `USD`). Returns the full string if no
/// underscore is present (defensive — shouldn't happen for FX).
pub fn quote_currency(instrument: &str) -> &str {
    instrument.split('_').nth(1).unwrap_or(instrument)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_currency_basic() {
        assert_eq!(quote_currency("EUR_USD"), "USD");
        assert_eq!(quote_currency("NZD_CHF"), "CHF");
        assert_eq!(quote_currency("AUD_JPY"), "JPY");
    }

    #[test]
    fn quote_currency_no_underscore_returns_input() {
        // Indices and other non-FX symbols don't follow BASE_QUOTE. We
        // don't expect to size them through FX, but the helper shouldn't
        // panic.
        assert_eq!(quote_currency("US30"), "US30");
    }
}
