//! Granularity parsing and the bridge between the two `Granularity` enums.
//!
//! Candle-cache speaks `candle_model::Granularity` (OANDA-style codes,
//! "H1"/"M5"/…); the engine speaks `trade_control_core::broker::Granularity`
//! (the closed `M1/M5/M15/H1/H4/D1` set). The CLI takes a friendly string like
//! `1h`. Neither enum has a `FromStr` for that friendly form, so we parse it
//! here once and expose both representations.

use candle_model::Granularity as CmGranularity;
use color_eyre::eyre::{Result, eyre};
use trade_control_core::broker::Granularity as EngineGranularity;

/// A granularity the replay harness supports — the intersection of what the
/// engine can evaluate and what a broker candle feed can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayGranularity {
    cm: CmGranularity,
    engine: EngineGranularity,
}

impl ReplayGranularity {
    /// The candle-cache / candle-model form, for pulling candles.
    pub fn candle_model(&self) -> CmGranularity {
        self.cm
    }

    /// The engine form, for `evaluate_plan` and matching `plan.granularity`.
    pub fn engine(&self) -> EngineGranularity {
        self.engine
    }
}

/// Parse a friendly granularity string (`1m`, `5m`, `15m`, `1h`, `4h`, `1d`)
/// into the supported pair. Case-insensitive; rejects anything the engine
/// can't evaluate.
pub fn parse(raw: &str) -> Result<ReplayGranularity> {
    let (cm, engine) = match raw.trim().to_ascii_lowercase().as_str() {
        "1m" | "m1" => (CmGranularity::OneMinute, EngineGranularity::M1),
        "5m" | "m5" => (CmGranularity::FiveMinutes, EngineGranularity::M5),
        "15m" | "m15" => (CmGranularity::FifteenMinutes, EngineGranularity::M15),
        "1h" | "h1" => (CmGranularity::OneHour, EngineGranularity::H1),
        "4h" | "h4" => (CmGranularity::FourHours, EngineGranularity::H4),
        "1d" | "d" | "d1" => (CmGranularity::OneDay, EngineGranularity::D1),
        other => {
            return Err(eyre!(
                "unsupported granularity {other:?}; the engine supports \
                 1m, 5m, 15m, 1h, 4h, 1d"
            ));
        }
    };
    Ok(ReplayGranularity { cm, engine })
}

/// The **finer** granularity the sub-bar zoom (PR-2) pulls to disambiguate an
/// exit bar that straddles both SL and TP. `None` when the plan is already at the
/// finest bar we pull (`M1`) — nothing finer to zoom into. Otherwise a coarser
/// plan maps to a granularity that gives a useful number of sub-bars WITHOUT an
/// enormous pull, and that every broker feed can actually serve:
///
/// - `M5`/`M15`/`H1` → `M1` (5/15/60 sub-bars; M1 is universally served).
/// - `H4` → `M15` (16 sub-bars; M1 over 4h × a long window is far too many bars,
///   and TradeNation serves 15m natively but not M5 via the raw endpoint).
/// - `D1` → `H1` (24 sub-bars; M1 over a day is 1440 bars/bar — impractical —
///   and H1 is universally served).
///
/// The zoom is fail-soft: if the finer pull errors or returns nothing, the broker
/// simply has no finer series and the sim keeps the pessimistic stop, so a broker
/// that can't serve the chosen finer granularity degrades cleanly.
pub fn finer(g: EngineGranularity) -> Option<ReplayGranularity> {
    let raw = match g {
        EngineGranularity::M1 => return None,
        EngineGranularity::M5 | EngineGranularity::M15 | EngineGranularity::H1 => "1m",
        EngineGranularity::H4 => "15m",
        EngineGranularity::D1 => "1h",
    };
    // `parse` only fails on an unsupported string; every arm above is supported,
    // so this never errors — but map to `None` rather than unwrap to stay total.
    parse(raw).ok()
}

/// Map an engine `Granularity` (e.g. from a loaded `plan.granularity`) to its
/// friendly string, for the mismatch error message.
pub fn engine_label(g: EngineGranularity) -> &'static str {
    match g {
        EngineGranularity::M1 => "1m",
        EngineGranularity::M5 => "5m",
        EngineGranularity::M15 => "15m",
        EngineGranularity::H1 => "1h",
        EngineGranularity::H4 => "4h",
        EngineGranularity::D1 => "1d",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_friendly_and_oanda_forms() {
        assert_eq!(parse("1h").unwrap().engine(), EngineGranularity::H1);
        assert_eq!(parse("H1").unwrap().engine(), EngineGranularity::H1);
        assert_eq!(parse(" 15m ").unwrap().engine(), EngineGranularity::M15);
        assert_eq!(parse("1d").unwrap().candle_model(), CmGranularity::OneDay);
    }

    #[test]
    fn rejects_unsupported() {
        assert!(parse("3m").is_err());
        assert!(parse("1w").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn engine_label_round_trips() {
        let g = parse("1h").unwrap();
        assert_eq!(engine_label(g.engine()), "1h");
    }

    #[test]
    fn finer_maps_each_granularity_to_a_practical_sub_grain() {
        assert_eq!(finer(EngineGranularity::M1), None); // already finest
        assert_eq!(
            finer(EngineGranularity::M5).map(|g| g.engine()),
            Some(EngineGranularity::M1)
        );
        assert_eq!(
            finer(EngineGranularity::H1).map(|g| g.engine()),
            Some(EngineGranularity::M1)
        );
        assert_eq!(
            finer(EngineGranularity::H4).map(|g| g.engine()),
            Some(EngineGranularity::M15)
        );
        assert_eq!(
            finer(EngineGranularity::D1).map(|g| g.engine()),
            Some(EngineGranularity::H1)
        );
    }
}
