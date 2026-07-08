//! Round order prices to an instrument's tick grid before they reach the
//! broker.
//!
//! OANDA (and other venues) reject an order whose price carries more decimal
//! precision than the instrument allows (`PRICE_PRECISION_EXCEEDED`). A price
//! computed from shell-anchored geometry — a fib reflection, an ATR offset —
//! almost never lands on the tick grid, so it must be snapped before placement.
//!
//! The rounding is **directional** for the risk-bearing levels so snapping can
//! never silently change the trade's risk profile:
//! - the **entry** trigger is a level, not risk-bearing on its own → nearest.
//! - the **stop-loss** rounds *away from entry* → never tightens the stop
//!   (a tighter stop would silently inflate position size).
//! - the **take-profit** rounds *toward entry* → never inflates advertised R.
//!
//! A non-positive or non-finite `tick` is treated as **identity** (return the
//! price unchanged). That single guard makes every fallback in the caller safe:
//! a legacy intent with no baked tick, or a test that passes `0.0`, rounds to
//! nothing rather than dividing by zero.

/// Which way to snap a price onto the tick grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundDir {
    /// Nearest tick (ties toward +∞, i.e. `(price / tick).round()`).
    Nearest,
    /// Toward −∞ (`floor`). Used for a level that must not move *up*.
    Down,
    /// Toward +∞ (`ceil`). Used for a level that must not move *down*.
    Up,
}

/// Snap `price` onto the `tick` grid in direction `dir`.
///
/// Returns `price` unchanged when `tick` is non-finite or `<= 0.0` (the
/// identity guard — see the module docs).
pub fn round_to_tick(price: f64, tick: f64, dir: RoundDir) -> f64 {
    if !tick.is_finite() || tick <= 0.0 || !price.is_finite() {
        return price;
    }
    let steps = price / tick;
    let snapped = match dir {
        RoundDir::Nearest => steps.round(),
        RoundDir::Down => steps.floor(),
        RoundDir::Up => steps.ceil(),
    };
    // Re-round the product to the tick's own decimal grid so float dust
    // (e.g. 88067.0 * 0.1 = 8806.700000000001) doesn't reintroduce the very
    // over-precision we're removing.
    let raw = snapped * tick;
    let places = tick_decimal_places(tick);
    round_half_away(raw, places)
}

/// Convenience: nearest-tick snap for a plain level (entry trigger).
pub fn round_price(price: f64, tick: f64) -> f64 {
    round_to_tick(price, tick, RoundDir::Nearest)
}

/// Snap a stop-loss *away from* `entry` so rounding never tightens risk.
/// Long stop (below entry) rounds down; short stop (above entry) rounds up.
pub fn round_stop_loss(stop_loss: f64, entry: f64, tick: f64) -> f64 {
    let dir = if stop_loss <= entry {
        RoundDir::Down
    } else {
        RoundDir::Up
    };
    round_to_tick(stop_loss, tick, dir)
}

/// Snap a take-profit *toward* `entry` so rounding never inflates advertised R.
/// Long TP (above entry) rounds down; short TP (below entry) rounds up.
pub fn round_take_profit(take_profit: f64, entry: f64, tick: f64) -> f64 {
    let dir = if take_profit >= entry {
        RoundDir::Down
    } else {
        RoundDir::Up
    };
    round_to_tick(take_profit, tick, dir)
}

/// Number of decimal places implied by a decimal tick (`0.1 → 1`, `0.001 → 3`,
/// `1.0 → 0`). Clamped to a sane 0..=12 range; a non-decimal tick (rare) just
/// gets a generous precision that still strips float dust.
fn tick_decimal_places(tick: f64) -> u32 {
    // log10 of the tick's reciprocal, rounded, gives the decimal count for a
    // power-of-ten tick. For non-power-of-ten ticks it's still a good bound.
    let places = (-(tick.log10())).round();
    if places.is_finite() {
        places.clamp(0.0, 12.0) as u32
    } else {
        6
    }
}

/// Round `value` to `places` decimal places, ties away from zero. Used only to
/// scrub float dust off an already-snapped product.
fn round_half_away(value: f64, places: u32) -> f64 {
    let factor = 10f64.powi(places as i32);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_on_zero_or_nonfinite_tick() {
        assert_eq!(
            round_to_tick(8806.70784, 0.0, RoundDir::Nearest),
            8806.70784
        );
        assert_eq!(
            round_to_tick(8806.70784, -0.1, RoundDir::Nearest),
            8806.70784
        );
        assert_eq!(
            round_to_tick(8806.70784, f64::NAN, RoundDir::Nearest),
            8806.70784
        );
        assert_eq!(
            round_to_tick(8806.70784, f64::INFINITY, RoundDir::Nearest),
            8806.70784
        );
    }

    #[test]
    fn nonfinite_price_passes_through() {
        assert!(round_to_tick(f64::NAN, 0.1, RoundDir::Nearest).is_nan());
    }

    #[test]
    fn au200_entry_rounds_to_one_decimal() {
        // The exact incident: AU200_AUD ticks 0.1, entry came out 8806.70784.
        let r = round_price(8806.70784, 0.1);
        assert_eq!(r, 8806.7);
        // No float dust: the string form must be a clean 1-dp value.
        assert_eq!(format!("{r}"), "8806.7");
    }

    #[test]
    fn nearest_rounds_both_ways() {
        assert_eq!(round_price(8806.74, 0.1), 8806.7);
        assert_eq!(round_price(8806.76, 0.1), 8806.8);
        // Exact half rounds toward +inf (Rust f64::round ties away from zero).
        assert_eq!(round_price(8806.75, 0.1), 8806.8);
    }

    #[test]
    fn long_stop_rounds_away_down_tp_toward_down() {
        let entry = 8806.7;
        // Long: SL below entry rounds DOWN (away → wider), TP above rounds DOWN (toward → nearer).
        assert_eq!(round_stop_loss(8801.36, entry, 0.1), 8801.3);
        assert_eq!(round_take_profit(8830.58, entry, 0.1), 8830.5);
    }

    #[test]
    fn short_stop_rounds_away_up_tp_toward_up() {
        let entry = 8806.7;
        // Short (this is the real trade): SL above entry rounds UP (away → wider),
        // TP below entry rounds UP (toward → nearer).
        assert_eq!(round_stop_loss(8841.392, entry, 0.1), 8841.4);
        assert_eq!(round_take_profit(8730.561, entry, 0.1), 8730.6);
    }

    #[test]
    fn directional_rounding_never_worsens_r() {
        // Short geometry from the incident, un-snapped.
        let entry = 8806.70784;
        let sl = 8841.39216;
        let tp = 8730.56867;
        let re = round_price(entry, 0.1);
        let rsl = round_stop_loss(sl, entry, 0.1);
        let rtp = round_take_profit(tp, entry, 0.1);
        // Rounded stop is no closer to entry than the raw stop (risk not tightened).
        assert!((rsl - re).abs() >= (sl - entry).abs() - 0.1);
        // Rounded TP is no further from entry than the raw TP (R not inflated).
        assert!((re - rtp).abs() <= (entry - tp).abs() + 0.1);
    }

    #[test]
    fn fx_five_dp_grid() {
        // EUR/USD ticks 0.00001.
        assert_eq!(round_price(1.103453, 0.00001), 1.10345);
        assert_eq!(round_price(1.103457, 0.00001), 1.10346);
    }

    #[test]
    fn whole_point_grid() {
        // A tick of 1.0 snaps to integers.
        assert_eq!(round_price(8806.7, 1.0), 8807.0);
        assert_eq!(round_stop_loss(8801.4, 8807.0, 1.0), 8801.0);
    }

    #[test]
    fn tick_decimal_places_maps_powers_of_ten() {
        assert_eq!(tick_decimal_places(1.0), 0);
        assert_eq!(tick_decimal_places(0.1), 1);
        assert_eq!(tick_decimal_places(0.001), 3);
        assert_eq!(tick_decimal_places(0.00001), 5);
    }
}
