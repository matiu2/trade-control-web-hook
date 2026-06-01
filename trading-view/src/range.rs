//! Chart visible-range types.
//!
//! `node tv-mcp range` returns the chart's currently visible time
//! window plus the underlying bar range. We mirror that shape and
//! expose the visible window as a pair of `DateTime<Utc>` for the
//! consumers (tv-news will use it to scope its calendar query).

use chrono::{DateTime, TimeZone, Utc};
use color_eyre::eyre::{Result, eyre};
use serde::Deserialize;

/// A pair of unix-second timestamps that come back from tv-mcp.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct UnixRange {
    /// Start of the range, unix seconds.
    pub from: i64,
    /// End of the range, unix seconds.
    pub to: i64,
}

impl UnixRange {
    /// Convert both endpoints to `DateTime<Utc>`. Returns an error if
    /// either timestamp is outside chrono's representable range.
    pub fn to_utc(self) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        let from = Utc
            .timestamp_opt(self.from, 0)
            .single()
            .ok_or_else(|| eyre!("range.from {} is not a valid unix timestamp", self.from))?;
        let to = Utc
            .timestamp_opt(self.to, 0)
            .single()
            .ok_or_else(|| eyre!("range.to {} is not a valid unix timestamp", self.to))?;
        Ok((from, to))
    }
}

/// The full `node tv-mcp range` response.
///
/// `visible_range` is what the operator has scrolled/zoomed to —
/// the window tv-news uses when deciding which calendar events to
/// annotate. `bars_range` is the underlying loaded-bar coverage and
/// is usually a tighter subset; we expose it because some consumers
/// may want to clamp queries to actually-rendered data.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ChartRange {
    /// The visible time window the operator is currently looking at.
    pub visible_range: UnixRange,
    /// The bars actually loaded into the chart.
    pub bars_range: UnixRange,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_range_response() {
        let json = r#"{
            "success": true,
            "visible_range": { "from": 1779710400, "to": 1780484400 },
            "bars_range":    { "from": 1779710400, "to": 1780282800 }
        }"#;
        let r: ChartRange = serde_json::from_str(json).expect("parse");
        assert_eq!(r.visible_range.from, 1779710400);
        assert_eq!(r.bars_range.to, 1780282800);
    }

    #[test]
    fn to_utc_round_trips() {
        let r = UnixRange {
            from: 1_700_000_000,
            to: 1_700_086_400,
        };
        let (from, to) = r.to_utc().expect("ok");
        assert_eq!(from.timestamp(), 1_700_000_000);
        assert_eq!(to.timestamp(), 1_700_086_400);
        assert!(to > from);
    }
}
