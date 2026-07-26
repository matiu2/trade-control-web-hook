//! Build a v1 [`Shell`] from the v2 firing bar — the one adapter entry
//! *resolution* needs.
//!
//! v1's [`Resolved::from_intent`](trade_control_core::intent::Resolved::from_intent)
//! resolves every anchored price (entry trigger, SL, TP) against a
//! [`Shell`](trade_control_core::intent::Shell): the firing bar's OHLC plus the
//! **latched signal** levels (`signal_high`/`signal_low`/`atr`/`golden`/…) that
//! `PriceAnchor::Signal*` reads. engine-v2 already carries both on the enter's
//! [`FiredIntent`] — the firing [`Candle`] and an `Option<LatchedSignal>` — so this
//! module is a pure, mechanical projection of those onto the `Shell` the resolver
//! wants. No new state, no re-derivation.
//!
//! # Why this is the whole adapter
//!
//! The v2 executor deliberately reuses v1's proven resolver rather than porting the
//! anchor/offset/R-multiple/sizing math (hundreds of lines, well-tested). The only
//! impedance is the *input shape*: v1 speaks `Shell`, v2 speaks `Candle` +
//! `LatchedSignal`. Bridge the shape and the entire resolver is available unchanged.
//!
//! # Absent signal ⇒ absent signal fields (deliberate)
//!
//! When the enter fired with **no** latched signal (`signal: None` — a break-only /
//! market-structure entry with no pinbar/engulfer), the signal fields are left
//! `None`. An intent that anchors entry/SL/TP to `signal_high`/`signal_low` then
//! `Err`s in the resolver (`MissingField` / an offset error) — which the executor
//! logs loudly and treats as *decline-this-bar-stay-armed*, exactly as v1 does. So a
//! genuinely signal-anchored setup that fires before its signal is present recovers
//! on a later bar; it is never silently mis-resolved against a zero.

use trade_control_core::intent::Shell;
use trade_control_core::signals::LatchedSignal;

use crate::Candle;

/// Project a v2 firing [`Candle`] (+ its optional [`LatchedSignal`]) onto a v1
/// [`Shell`] the resolver consumes. Pure and total: OHLC/time map directly; the
/// signal block is filled from `signal` when present, left `None` otherwise.
///
/// The `open` field is always `Some` — a v2 [`Candle`] always carries its open,
/// unlike the wire `Shell` where pre-2026 templates omitted it.
pub fn shell_from_candle(candle: &Candle, signal: Option<&LatchedSignal>) -> Shell {
    Shell {
        close: candle.c,
        high: candle.h,
        low: candle.l,
        open: Some(candle.o),
        time: candle.time,
        // Signal block — present iff the enter fired off a latched signal.
        signal_high: signal.map(|s| s.signal_high),
        signal_low: signal.map(|s| s.signal_low),
        signal_range: signal.map(|s| s.signal_range),
        signal_start_time: signal.map(|s| s.signal_start_time),
        signal_kind: signal.map(|s| s.kind),
        golden: signal.map(|s| s.golden),
        atr: signal.and_then(|s| s.atr),
        signal_confirmed: signal.map(|s| s.signal_confirmed),
        recent_high: signal.and_then(|s| s.recent_high),
        recent_low: signal.and_then(|s| s.recent_low),
        // The five `next_candle_timestamp_*` slots are a live-worker
        // retest-scheduling concern the entry resolver never reads — always `None`.
        next_candle_timestamp_1: None,
        next_candle_timestamp_2: None,
        next_candle_timestamp_3: None,
        next_candle_timestamp_4: None,
        next_candle_timestamp_5: None,
    }
}
