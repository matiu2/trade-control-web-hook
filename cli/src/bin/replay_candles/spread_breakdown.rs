//! **Which spread did the SL floor size against?** — the operator-facing
//! breakdown of the three candidate readings and which one won.
//!
//! # Why
//!
//! The journal used to say only `[SL floored to 10× spread]`. That names the
//! multiple but not the input, so a stop 49 pips from entry looked arbitrary —
//! there was no way to tell from the output whether the floor had sized off the
//! bar in front of it or off a forecast for an hour that had not arrived. When a
//! floor overrides an operator's drawn stop, "which number did that" is the first
//! question, and the journal could not answer it.
//!
//! Three readings can contribute (see
//! [`SpreadInputs`](trade_control_core::order_control::SpreadInputs)):
//!
//! - **measured** — the bar (or trailing window) actually in front of us.
//!   Reactive: it only moves after the market has.
//! - **forecast, this hour** — the baked p90 for the schedule-local hour we are in.
//! - **forecast, next hour** — the baked p90 for the hour a resting order might
//!   *fill* in. A stop placed at 20:55 can fill at 21:05.
//!
//! # What this reports, and what it deliberately does not claim
//!
//! **The shipped entry floor sizes off the MEASURED reading alone.** The forecast
//! terms feed `sl_target` (the parked/stored-order re-check on the live cron), not
//! the entry floor — putting them into the entry `max` is the unshipped experiment
//! on `experiment/forecast-entry-floor`.
//!
//! So this renders the forecast columns as **context, explicitly marked as not
//! applied**, rather than implying they were part of the decision. Showing them as
//! contributors would misreport what the code does — the exact failure this module
//! exists to prevent. When the experiment ships, [`Breakdown::applied`] flips and
//! the same renderer starts marking a forecast term as the winner.

use trade_control_core::order_control::SpreadInputs;

/// Which of the three readings governed the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    Measured,
    ThisHour,
    NextHour,
}

impl Winner {
    fn label(self) -> &'static str {
        match self {
            Winner::Measured => "measured",
            Winner::ThisHour => "forecast(this hr)",
            Winner::NextHour => "forecast(next hr)",
        }
    }
}

/// The three candidate spreads for one entry, in **pips**, plus which one the
/// floor actually used.
#[derive(Debug, Clone, Copy)]
pub struct Breakdown {
    pub measured_pips: f64,
    pub this_hour_pips: f64,
    pub next_hour_pips: f64,
    /// The reading that governed. With `applied == false` this is always
    /// [`Winner::Measured`], because that is what the shipped floor uses.
    pub winner: Winner,
    /// Whether the forecast terms were part of the `max`. `false` on the shipped
    /// path — they are shown as context only.
    pub applied: bool,
}

impl Breakdown {
    /// Build the breakdown for an entry firing at `now`.
    ///
    /// `measured_pips` is what the floor actually sized off (the fire bar's close
    /// spread, or the trailing-window mean). `reference_price` converts the baked
    /// **fractions** to pips; a non-finite or non-positive one leaves the forecast
    /// columns at zero rather than inventing a number.
    pub fn for_entry(
        instrument: &str,
        measured_pips: f64,
        reference_price: f64,
        pip_size: f64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        let usable = reference_price.is_finite()
            && reference_price > 0.0
            && pip_size.is_finite()
            && pip_size > 0.0;
        let (this_frac, next_frac) = if usable {
            trade_control_core::spread_blackout::spread_forecast_frac(instrument, now)
        } else {
            (0.0, 0.0)
        };
        let to_pips = |frac: f64| {
            if usable {
                frac * reference_price / pip_size
            } else {
                0.0
            }
        };
        Self {
            measured_pips,
            this_hour_pips: to_pips(this_frac),
            next_hour_pips: to_pips(next_frac),
            // The shipped floor is measured-only; see the module docs.
            winner: Winner::Measured,
            applied: false,
        }
    }

    /// The spread the floor sized against, in pips.
    pub fn governing_pips(&self) -> f64 {
        match self.winner {
            Winner::Measured => self.measured_pips,
            Winner::ThisHour => self.this_hour_pips,
            Winner::NextHour => self.next_hour_pips,
        }
    }

    /// One-line rendering: every reading, with the governing one marked `◄`.
    ///
    /// Returns `None` when there is nothing informative to say — no forecast row
    /// for the instrument AND nothing measured — so a clean journal line isn't
    /// padded with zeros.
    pub fn render(&self) -> Option<String> {
        if !self.measured_pips.is_finite() {
            return None;
        }
        let no_forecast = self.this_hour_pips <= 0.0 && self.next_hour_pips <= 0.0;
        if no_forecast && self.measured_pips <= 0.0 {
            return None;
        }

        let mark = |w: Winner| if self.winner == w { " ◄" } else { "" };
        let mut parts = vec![format!(
            "measured {:.1}p{}",
            self.measured_pips,
            mark(Winner::Measured)
        )];
        if !no_forecast {
            parts.push(format!(
                "forecast this-hr {:.1}p{}",
                self.this_hour_pips,
                mark(Winner::ThisHour)
            ));
            parts.push(format!(
                "next-hr {:.1}p{}",
                self.next_hour_pips,
                mark(Winner::NextHour)
            ));
        }
        let suffix = if no_forecast {
            " (no baked forecast for this instrument)"
        } else if self.applied {
            ""
        } else {
            " (forecast shown for context; the entry floor sizes off measured)"
        };
        Some(format!(
            "spread: {} → floor uses {} {:.1}p{suffix}",
            parts.join(", "),
            self.winner.label(),
            self.governing_pips(),
        ))
    }
}

/// Build a [`Breakdown`] from an explicit [`SpreadInputs`] — the path a future
/// forecast-applying floor uses, where the `max` genuinely picks a winner.
///
/// Kept alongside [`Breakdown::for_entry`] so that when the experiment ships the
/// renderer needs no change: only the source of the winner does.
pub fn from_inputs(inputs: &SpreadInputs, pip_size: f64) -> Breakdown {
    let to_pips = |v: f64| {
        if pip_size.is_finite() && pip_size > 0.0 {
            v / pip_size
        } else {
            0.0
        }
    };
    let measured = to_pips(inputs.last_candle);
    let this_hour = to_pips(inputs.expected_this_hour);
    let next_hour = to_pips(inputs.expected_next_hour);
    // Highest wins, ties to the measured reading (it is the one we can verify).
    let mut winner = Winner::Measured;
    let mut best = measured;
    if this_hour > best {
        winner = Winner::ThisHour;
        best = this_hour;
    }
    if next_hour > best {
        winner = Winner::NextHour;
    }
    Breakdown {
        measured_pips: measured,
        this_hour_pips: this_hour,
        next_hour_pips: next_hour,
        winner,
        applied: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap_or_else(|e| panic!("bad test timestamp {s}: {e}"))
            .with_timezone(&chrono::Utc)
    }

    /// The headline: all three readings are named, and the governing one is
    /// marked, so the journal answers "which number produced this stop?".
    #[test]
    fn renders_all_three_readings_and_marks_the_winner() {
        let b = Breakdown::for_entry("AUD/NZD", 1.5, 1.209, 0.0001, t("2026-06-12T23:00:00Z"));
        let line = b.render().expect("renders");
        assert!(
            line.contains("measured 1.5p"),
            "measured must appear: {line}"
        );
        assert!(
            line.contains("forecast this-hr"),
            "this-hour forecast must appear: {line}"
        );
        assert!(
            line.contains("next-hr"),
            "next-hour forecast must appear: {line}"
        );
        assert!(
            line.contains('◄'),
            "the governing reading must be marked: {line}"
        );
    }

    /// The shipped floor is measured-only, so the rendering must SAY so rather
    /// than let a reader infer the forecast contributed. Misreporting which
    /// number drove a stop is the failure this module exists to prevent.
    #[test]
    fn shipped_path_marks_measured_and_says_forecast_is_context_only() {
        let b = Breakdown::for_entry("AUD/NZD", 1.5, 1.209, 0.0001, t("2026-06-12T23:00:00Z"));
        assert_eq!(b.winner, Winner::Measured);
        assert!(!b.applied);
        let line = b.render().expect("renders");
        assert!(
            line.contains("the entry floor sizes off measured"),
            "must not imply the forecast was applied: {line}"
        );
        // The mark sits on `measured`, not on a forecast column.
        let measured_at = line.find("measured 1.5p").expect("measured present");
        let mark_at = line.find('◄').expect("mark present");
        assert!(
            mark_at > measured_at && mark_at - measured_at < 20,
            "the ◄ must mark the measured reading: {line}"
        );
    }

    /// An instrument with no baked row shows the measured reading alone and says
    /// why the forecast columns are absent — rather than printing `0.0p` twice,
    /// which reads as "the forecast is zero" instead of "there isn't one".
    #[test]
    fn an_uncatalogued_instrument_says_there_is_no_forecast() {
        let b = Breakdown::for_entry("XYZ_ABC", 2.0, 1.0, 0.0001, t("2026-06-12T23:00:00Z"));
        let line = b.render().expect("renders");
        assert!(line.contains("measured 2.0p"));
        assert!(
            line.contains("no baked forecast"),
            "absence must be stated, not shown as a zero: {line}"
        );
        assert!(
            !line.contains("this-hr"),
            "no forecast columns when there is no row: {line}"
        );
    }

    /// The forecast-applying path (the experiment) picks the worst reading and
    /// marks THAT one — proving the renderer needs no change when it ships.
    #[test]
    fn the_forecast_path_marks_a_forecast_when_it_governs() {
        let inputs = SpreadInputs {
            last_candle: 1.5 * 0.0001,
            expected_this_hour: 3.0 * 0.0001,
            expected_next_hour: 18.0 * 0.0001, // the NY-close spike, still ahead
        };
        let b = from_inputs(&inputs, 0.0001);
        assert_eq!(b.winner, Winner::NextHour);
        assert!(b.applied);
        assert!((b.governing_pips() - 18.0).abs() < 1e-9);
        let line = b.render().expect("renders");
        assert!(
            line.contains("forecast(next hr) 18.0p"),
            "the governing forecast must be named: {line}"
        );
        assert!(
            !line.contains("context"),
            "an applied forecast must not be labelled context-only: {line}"
        );
    }

    /// A measured blowout worse than any forecast still governs — the rule is a
    /// `max`, so the reading we can actually verify is never averaged away.
    #[test]
    fn a_measured_blowout_beats_the_forecast() {
        let inputs = SpreadInputs {
            last_candle: 40.0 * 0.0001,
            expected_this_hour: 3.0 * 0.0001,
            expected_next_hour: 18.0 * 0.0001,
        };
        let b = from_inputs(&inputs, 0.0001);
        assert_eq!(b.winner, Winner::Measured);
        assert!((b.governing_pips() - 40.0).abs() < 1e-9);
    }

    /// Nothing measured and no forecast ⇒ no line at all, so a clean journal
    /// isn't padded with a row of zeros.
    #[test]
    fn nothing_to_say_renders_nothing() {
        let b = Breakdown::for_entry("XYZ_ABC", 0.0, 1.0, 0.0001, t("2026-06-12T23:00:00Z"));
        assert!(b.render().is_none());
    }

    /// A bad reference price must not invent forecast pips.
    #[test]
    fn a_bad_reference_price_suppresses_the_forecast_columns() {
        for bad in [0.0, -1.0, f64::NAN] {
            let b = Breakdown::for_entry("AUD/NZD", 1.5, bad, 0.0001, t("2026-06-12T23:00:00Z"));
            assert_eq!(b.this_hour_pips, 0.0, "reference {bad} must not scale");
            assert_eq!(b.next_hour_pips, 0.0);
        }
    }
}
