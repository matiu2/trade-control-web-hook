//! [`LineName`] — a geometry line's **compile-time name**.
//!
//! The line half of a fact key `(line, kind)`. This mirrors
//! [`FactKind`](crate::facts::FactKind) exactly: line names were string literals
//! (`"neckline"`), collision-prone across the rule that writes a fact and the
//! rule that reads it. Here they become zero-size marker types, so a fact key is
//! type-checked on **both** axes — a rule can no longer target the wrong line by
//! mistyping a string.
//!
//! # An OPEN set (trait, not enum) — same reason as `FactKind`
//!
//! `LineName` is a **trait**, not a closed enum, so a setup crate can name its
//! own lines (a future trend-follow crate's `"channel_top"`) without editing a
//! central enum here. H&S and M/W share exactly one real line — `Neckline` — plus
//! two horizontal caps, `TooHigh`/`TooLow` (these are really price *levels*, not
//! sloped lines; 4c splits them into a `PriceLevel` geometry with a
//! no-projection cross path — see `SCOPING-engine-v2-typed-geometry.md`. Until
//! then they ride the same `Line{a,b}` as a degenerate `a.price == b.price`).
//!
//! # Wire format UNCHANGED
//!
//! The store keys on [`NAME`](LineName::NAME) (a `&'static str`) and serializes
//! as the same strings a pre-typing plan used. The type layer is a compile-time
//! convenience over a string-keyed store — **not** a new wire format. A rename of
//! a `NAME` is a persisted-state migration; pin them with a test.

/// A geometry line's compile-time name. Implemented by a zero-size marker per
/// line; [`NAME`](Self::NAME) is the stable string the fact store keys on and
/// serializes as.
pub trait LineName {
    /// The stable serialized name for this line, e.g. `"neckline"`. Persisted —
    /// a change here is a migration.
    const NAME: &'static str;
}

/// The head-and-shoulders / M-W **neckline** — the only genuinely sloped line in
/// the current setup vocabulary. Break-and-close, retest, and the enter's prep
/// chain all reference it.
pub struct Neckline;
impl LineName for Neckline {
    const NAME: &'static str = "neckline";
}

/// The upper **invalidation cap** (a short's "too high" / an iH&S long's
/// ceiling). A horizontal price level today expressed as a degenerate `Line`;
/// becomes a `PriceLevel` in 4c.
pub struct TooHigh;
impl LineName for TooHigh {
    const NAME: &'static str = "too_high";
}

/// The lower **invalidation / pcl-exhausted cap** (a short's "pcl exhausted" ~80%
/// to TP, or an iH&S long's floor). Horizontal price level, degenerate `Line`
/// today; `PriceLevel` in 4c.
pub struct TooLow;
impl LineName for TooLow {
    const NAME: &'static str = "too_low";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stable serialized line NAMEs — persisted state (a fact keys on
    /// `(NAME, kind)`), so a change here is a migration. Pin them, exactly as
    /// [`kind_names_are_stable`](crate::facts) pins the kind names.
    #[test]
    fn line_names_are_stable() {
        assert_eq!(Neckline::NAME, "neckline");
        assert_eq!(TooHigh::NAME, "too_high");
        assert_eq!(TooLow::NAME, "too_low");
    }
}
