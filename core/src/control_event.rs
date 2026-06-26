//! Durable audit record for control rows that carry a TTL.
//!
//! Rows like `cooldown:`, `veto:`, `prep:`, `pause:`, `news:`,
//! `spread-blackout:` and `blackout-hours:` are written with a window-anchored
//! TTL and then **silently deleted** by Cloudflare KV when that window closes —
//! no event, no log, no trace. So when journaling or debugging a past trade you
//! can see (from R2 `req/`) that a cooldown/veto was *set*, but nothing records
//! that it *expired*, nor when.
//!
//! A [`ControlEvent`] is the missing trail: a small, **no-TTL** record written
//! alongside each such `set_*`, capturing what was set, when, its TTL, and the
//! computed expiry. It lives until the trade is purged (`plan purge`), so the
//! full set→expire lifecycle is reconstructable long after the live row is gone.
//!
//! Pure data (no `worker`, no KV) so it lives in `core`; the [`StateStore`]
//! trait owns the read/write, the worker wires a thin helper at each `set_*`.
//!
//! [`StateStore`]: crate::state::StateStore

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which kind of control row this event records the setting of. The string form
/// is part of the KV key and the journaled record, so keep the `kebab-case`
/// rename stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ControlKind {
    Cooldown,
    Veto,
    Prep,
    Pause,
    News,
    SpreadBlackout,
    BlackoutHours,
}

impl ControlKind {
    /// Stable short tag used in the KV key (`control-event:{scope}:{trade_id}:
    /// {epoch}-{tag}-{name}`). Distinct from the `Display`/serde form only in
    /// that it's guaranteed key-safe (no spaces/colons).
    pub fn tag(&self) -> &'static str {
        match self {
            ControlKind::Cooldown => "cooldown",
            ControlKind::Veto => "veto",
            ControlKind::Prep => "prep",
            ControlKind::Pause => "pause",
            ControlKind::News => "news",
            ControlKind::SpreadBlackout => "spread-blackout",
            ControlKind::BlackoutHours => "blackout-hours",
        }
    }
}

/// One durable record that a TTL-carrying control row was set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlEvent {
    /// The kind of control row set.
    pub kind: ControlKind,
    /// The control's name where it has one (`too-low`, `reversal`, a blackout
    /// id, …). Empty for kinds keyed only by instrument (e.g. cooldown).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Instrument the control applies to (display / cross-reference).
    pub instrument: String,
    /// When the control was set (the tick/request time).
    pub set_at: DateTime<Utc>,
    /// The TTL applied to the live row, in seconds.
    pub ttl_seconds: u64,
    /// `set_at + ttl_seconds` — when the live row was computed to expire. The
    /// whole point of the record: KV doesn't log the actual passive delete, so
    /// this is the best available "and it lifted at…" for journaling.
    pub computed_expiry: DateTime<Utc>,
    /// The `request_id` of the alert that set it, when known — links the event
    /// back to its R2 `req/` bundle. `None` for engine-internal sets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

impl ControlEvent {
    /// Build a control event, computing `computed_expiry = set_at + ttl_seconds`.
    pub fn new(
        kind: ControlKind,
        name: impl Into<String>,
        instrument: impl Into<String>,
        set_at: DateTime<Utc>,
        ttl_seconds: u64,
        request_id: Option<String>,
    ) -> Self {
        let computed_expiry = set_at + chrono::Duration::seconds(ttl_seconds as i64);
        Self {
            kind,
            name: name.into(),
            instrument: instrument.into(),
            set_at,
            ttl_seconds,
            computed_expiry,
            request_id,
        }
    }

    /// The key suffix that makes this event unique + sortable within a trade:
    /// `{set_at_epoch}-{kind_tag}-{name}`. Append-only — a later set of the same
    /// control at a different time is a distinct event.
    pub fn key_suffix(&self) -> String {
        let epoch = self.set_at.timestamp();
        let tag = self.kind.tag();
        if self.name.is_empty() {
            format!("{epoch}-{tag}")
        } else {
            format!("{epoch}-{tag}-{}", self.name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn computed_expiry_is_set_at_plus_ttl() {
        let ev = ControlEvent::new(
            ControlKind::Cooldown,
            "",
            "EUR_USD",
            ts("2026-06-24T10:00:00Z"),
            8 * 3600,
            None,
        );
        assert_eq!(ev.computed_expiry, ts("2026-06-24T18:00:00Z"));
    }

    #[test]
    fn key_suffix_includes_name_when_present() {
        let ev = ControlEvent::new(
            ControlKind::Veto,
            "too-low",
            "GBP/USD",
            ts("2026-06-24T01:00:00Z"),
            3600,
            None,
        );
        // 2026-06-24T01:00:00Z = 1782262800
        assert_eq!(ev.key_suffix(), "1782262800-veto-too-low");
    }

    #[test]
    fn key_suffix_omits_empty_name() {
        let ev = ControlEvent::new(
            ControlKind::Cooldown,
            "",
            "EUR_USD",
            ts("2026-06-24T01:00:00Z"),
            3600,
            None,
        );
        assert_eq!(ev.key_suffix(), "1782262800-cooldown");
    }

    #[test]
    fn round_trips_through_json() {
        let ev = ControlEvent::new(
            ControlKind::Pause,
            "blackout-42",
            "USD_JPY",
            ts("2026-06-24T01:00:00Z"),
            7200,
            Some("abc123".into()),
        );
        let json = serde_json::to_string(&ev).unwrap();
        let back: ControlEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }
}
