//! The session anchor an instrument's H4/D1 bars are aggregated on.
//!
//! One line of policy, kept out of the fetch paths so both of them share it:
//! resolve through `instrument-lookup` (re-exported by `tradenation-api` as
//! [`Anchor`]); if the catalog cannot be read, log and use the FX day rather
//! than failing a candle fetch. The FX day is the grid every instrument was on
//! before anchors existed, so the fallback never moves an instrument that was
//! not deliberately given an anchor.

use tradenation_api::Anchor;

pub(crate) fn session_anchor(instrument: &str) -> Anchor {
    Anchor::for_instrument(instrument).unwrap_or_else(|e| {
        tracing::warn!("session anchor unavailable for {instrument:?}, using the FX day: {e}");
        Anchor::FX
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spain_35_is_aggregated_from_its_cash_open_and_fx_from_the_new_york_close() {
        let spain = session_anchor("Spain 35");
        assert_eq!((spain.tz.name(), spain.hour), ("Europe/Madrid", 9));
        assert_eq!(session_anchor("EUR/USD"), Anchor::FX);
        assert_eq!(session_anchor("NOT AN INSTRUMENT"), Anchor::FX);
    }
}
