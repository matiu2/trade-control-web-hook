//! System 1 of the DST-aware spread blackout: reject a brand-new entry
//! that fires during the post-NY-close liquidity trough when the live
//! spread on the incoming instrument is elevated. Pure decision lives
//! here; the KV-read + broker quote sample is a thin wrapper in
//! `run_enter` (src/lib.rs). Reject, not delay — the next signal bar
//! refires and re-checks (by then the spread may have recovered).

/// Decide whether to reject an entry on spread-blackout grounds.
///
/// `window_open`  — the global `spread-blackout:window` marker is present
///                  (Sub-plan 2). When `false` we never sample the spread.
/// `spread_pips`  — live `ask − bid` for the incoming instrument, in pips.
/// `threshold_pips` — the "elevated" cutoff (see OPEN QUESTION on
///                  [`elevated_threshold_pips`]).
///
/// Returns `true` ⇒ REJECT (`rejected: spread-blackout`).
/// `false` ⇒ fall through to the normal entry (window closed, OR window
/// open but the spread is fine — that instrument/day is not blacked out).
///
/// Strictly `>`: a spread exactly at the threshold is allowed (the
/// boundary is deliberately permissive — see the boundary unit test).
pub fn spread_blackout_decision(window_open: bool, spread_pips: f64, threshold_pips: f64) -> bool {
    window_open && spread_pips > threshold_pips
}

/// "Elevated" spread cutoff in pips for System 1's reject.
///
/// TODO(open-question, spread-blackout sub-plan 3): calibration + where
/// this lives. Start with a single conservative constant; promote to a
/// per-instrument table or a baked-on-intent value later. The entry path
/// *does* have the intent in hand (`verified.intent`, `pip_size`), so
/// baking the cutoff onto the intent is trivial here even if the cron
/// side (Sub-plan 2) needs the record approach.
///
/// Relationship to Sub-plan 2: `blackout_watch::recovered_cutoff` is the
/// matching cutoff for the cron-side recovery watcher. For hysteresis the
/// *elevated* cutoff (here) should sit a little **above** the *recovered*
/// cutoff so the window doesn't flap. Both are uncalibrated placeholders
/// today and MUST be tuned together. Note the units currently differ —
/// Sub-plan 2's placeholder is an absolute price (≈10 pips on a 5-dp FX
/// cross); this one is already in pips — reconcile units when calibrating.
pub fn elevated_threshold_pips(_instrument: &str) -> f64 {
    SPREAD_BLACKOUT_ELEVATED_PIPS
}

/// Placeholder cutoff. A thin FX cross normally spreads ~2p and blows to
/// ~20p+ in the trough; 8p sits clearly above normal and below the
/// blowout. Majors (EUR/USD ~1p) never trip it, so the window is
/// self-scoping. Calibrate on demo before relying on it.
///
/// Provisional — see [`elevated_threshold_pips`] for the open question and
/// the hysteresis relationship to [`SPREAD_BLACKOUT_RECOVERED_PIPS`].
pub const SPREAD_BLACKOUT_ELEVATED_PIPS: f64 = 8.0;

/// "Recovered" spread cutoff in pips for the Sub-plan-2 recovery watcher
/// (`src/cron/blackout_watch.rs`). The spread is considered back to normal
/// once the sampled `ask − bid` (in pips) drops to/under this.
///
/// **Hysteresis (single tuning point):** lives here, beside the *elevated*
/// cutoff, so the two are tuned together and the invariant
/// `RECOVERED < ELEVATED` is visible in one file. Recovered sits **below**
/// elevated so the window doesn't flap right at the boundary: an entry is
/// blacked out above 8p, and the watcher only declares recovery once the
/// spread has fallen all the way back to ≤4p. Both are provisional and
/// MUST be calibrated together on demo — see [`elevated_threshold_pips`].
pub const SPREAD_BLACKOUT_RECOVERED_PIPS: f64 = 4.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_closed_never_rejects() {
        // Window closed ⇒ pass even with an absurdly wide spread, and the
        // wrapper never calls `get_quote` on this branch.
        assert!(!spread_blackout_decision(false, 50.0, 8.0));
    }

    #[test]
    fn window_open_wide_spread_rejects() {
        assert!(spread_blackout_decision(true, 20.0, 8.0));
    }

    #[test]
    fn window_open_tight_spread_passes() {
        assert!(!spread_blackout_decision(true, 2.0, 8.0));
    }

    #[test]
    fn boundary_exactly_at_threshold_passes() {
        // Strictly `>`, so exactly-at-threshold falls through (allowed).
        assert!(!spread_blackout_decision(true, 8.0, 8.0));
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn recovered_cutoff_sits_below_elevated_for_hysteresis() {
        // The window must not flap at the boundary: recovery is only
        // declared once the spread has fallen below the (lower) recovered
        // cutoff, not the moment it dips under the elevated one. Constant
        // assertion on purpose — it guards the tuning invariant if a future
        // edit to either const inverts the pair.
        assert!(SPREAD_BLACKOUT_RECOVERED_PIPS < SPREAD_BLACKOUT_ELEVATED_PIPS);
    }

    #[test]
    fn threshold_lookup_returns_constant_for_any_instrument() {
        // Guards against a future per-instrument table regressing the
        // default for instruments it doesn't list.
        assert_eq!(
            elevated_threshold_pips("EUR_NZD"),
            SPREAD_BLACKOUT_ELEVATED_PIPS
        );
        assert_eq!(
            elevated_threshold_pips("EUR_USD"),
            SPREAD_BLACKOUT_ELEVATED_PIPS
        );
        assert_eq!(elevated_threshold_pips(""), SPREAD_BLACKOUT_ELEVATED_PIPS);
    }
}
