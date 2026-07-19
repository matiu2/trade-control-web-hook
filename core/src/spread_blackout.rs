//! System 1 of the DST-aware spread blackout: reject a brand-new entry
//! that fires during the post-NY-close liquidity trough when the live
//! spread on the incoming instrument is elevated. Reject, not delay — the
//! next signal bar refires and re-checks (by then the spread may have
//! recovered).
//!
//! This is the **pure decision** (KV-free, broker-free, wasm-safe), so it
//! lives in `core` and is shared by BOTH consumers
//! (`[[strategy_changes_in_both_replayer_and_worker]]`):
//!
//! - the **live worker** wraps it with a KV window read + a live broker
//!   quote sample (`run_enter`, `src/lib.rs`);
//! - the **offline replay** wraps it with the fire-bar `ask_c − bid_c`
//!   spread from the recorded candle and an `is_ny_close_edge` stand-in for
//!   the window marker (`engine::simulate_fill`).
//!
//! Keeping the decision + the baked per-instrument baseline here is what
//! lets a replay 422 exactly the entries the live worker would 423.

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

/// The candle-derived, **per-broker** spread-hour table produced offline by the
/// `spread-baseline-gen` binary and committed as a source file (not build-time
/// generated — the fetch is a network op). Each row is
/// `(broker, symbol, schedule, reviewed, elevated_hours_mask, hour_widen_frac[24])`,
/// sorted by `(broker, symbol)`.
///
/// This is the source of truth for **which spread hours are elevated**
/// (`is_spread_hour`). It supersedes the sampler mask, which over-flagged tight
/// crosses' whole overnight block (the "12pm Brisbane rubbish" bug). The
/// per-broker keying means OANDA `EUR_USD` and TradeNation `EUR/USD` carry their
/// own masks — no canonical sharing. Symbol strings never collide across
/// brokers, so the lookup keys on the symbol alone.
///
/// **Mask bits are SCHEDULE-LOCAL hours (Stage 3, DST-aware).** Bit `h` of the
/// mask ⇒ *local wall-clock* hour `h` of the row's `schedule` tz is a spread
/// hour, NOT UTC hour `h`. The gate resolves the row's `schedule` FK to a
/// `chrono_tz::Tz` ([`schedule_tz`]) and converts the incoming UTC `now` to that
/// tz before indexing the mask — so a `ny` 17:00-local spike reads as a spread
/// hour at 21:00 UTC in summer (EDT) and 22:00 UTC in winter (EST) from the
/// *same* mask bit. See `SCOPING-spread-hour-dst-local-time.md`.
mod baseline_candle {
    include!("spread_baseline_candle.rs");
}

/// Resolve a spread-schedule FK **name** (as stored in the baked table's
/// `schedule` column) to its IANA timezone. The table stores the compact FK
/// name (`"ny"`), not the tz string, so this map is the single source of truth
/// that expands it — it **mirrors instrument-lookup's schedule table** and the
/// two MUST stay in sync (add a schedule there and here together).
///
/// `None` for `"none"` or any unknown name ⇒ the instrument has no DST-aware
/// spread schedule, and callers fall back to the NY-close-edge default. We
/// hand back the `Tz` (not a bare offset) so chrono-tz applies each zone's own
/// DST rule at read time; `ny_clock` stays as the hand-rolled fallback for
/// absent masks.
fn schedule_tz(schedule: &str) -> Option<chrono_tz::Tz> {
    match schedule {
        "ny" => Some(chrono_tz::America::New_York),
        "london" => Some(chrono_tz::Europe::London),
        "frankfurt" => Some(chrono_tz::Europe::Berlin),
        "zurich" => Some(chrono_tz::Europe::Zurich),
        "sydney" => Some(chrono_tz::Australia::Sydney),
        "johannesburg" => Some(chrono_tz::Africa::Johannesburg),
        "hongkong" => Some(chrono_tz::Asia::Hong_Kong),
        "singapore" => Some(chrono_tz::Asia::Singapore),
        "tokyo" => Some(chrono_tz::Asia::Tokyo),
        _ => None, // "none" or unknown → no tz, no spread hour
    }
}

/// The candle-derived row for `instrument` as `(schedule, mask, &widen)`, or
/// `None` when it isn't in the candle table. Keyed on the broker symbol string
/// (the same `resolved.instrument` the gate passes); symbols are unique across
/// brokers, so the `broker` column is not needed to disambiguate the lookup.
///
/// `schedule` is the spread-schedule FK name ([`schedule_tz`] resolves it to a
/// tz); bit `h` of the mask ⇒ *schedule-local* hour `h` is a spread hour;
/// `widen[h]` is that local hour's p90 `spread/mid` **fraction** (0.0 when not
/// elevated). The widen here is a scale-free fraction — the System-2 consumer
/// converts it to pips with a reference price + pip size via
/// [`widen_frac_to_pips`].
///
/// The table is sorted by `(broker, symbol)`, NOT by `symbol` alone, so a plain
/// `binary_search` on the symbol would be wrong — we scan for an exact symbol
/// match. The table is small (~200 rows) and this is called per-tick per-plan,
/// which is cheap; a symbol-sorted index can be added if it ever matters.
fn baked_candle_row(instrument: &str) -> Option<(&'static str, u32, &'static [f64; 24])> {
    baseline_candle::SPREAD_BASELINE_CANDLE
        .iter()
        .find(|(_broker, symbol, ..)| *symbol == instrument)
        .map(|(_broker, _symbol, schedule, _reviewed, mask, widen, ..)| (*schedule, *mask, widen))
}

/// The candle-derived `(mask, widen_frac[24], tz)` for `instrument`, or `None`
/// when it isn't in the candle table **or** its schedule has no resolvable tz
/// (`"none"`/unknown). The `tz` is the row's schedule zone, needed to convert a
/// UTC instant to the schedule-local hour the mask is indexed by. Callers that
/// get `None` here fall back to the NY-close-edge default.
fn baked_candle_spread_hours(instrument: &str) -> Option<(u32, [f64; 24], chrono_tz::Tz)> {
    let (schedule, mask, widen) = baked_candle_row(instrument)?;
    let tz = schedule_tz(schedule)?;
    Some((mask, *widen, tz))
}

/// Convert a candle-derived widen **fraction** (`spread/mid`) to pips, given a
/// reference price near the current market and the instrument's pip size:
/// `pips = frac × reference_price / pip_size`.
///
/// The reference price only needs to be within a fraction of a percent of the
/// live mid (the stop-loss price or the current close both qualify) — a small
/// error in it scales the widen by the same small fraction, negligible for a
/// protective stop nudge. Pure; a non-finite input yields a non-finite result
/// and the caller skips the widen upstream (as with a bad live spread).
pub fn widen_frac_to_pips(frac: f64, reference_price: f64, pip_size: f64) -> f64 {
    frac * reference_price / pip_size
}

/// The candle-derived pre-emptive widen **fraction** (`spread/mid`) for
/// `instrument` at `now`, or `None` when `now` is not in (or within
/// [`SPREAD_HOUR_LEAD_MINUTES`] of the start of) one of this instrument's
/// candle-learned spread hours.
///
/// Returns a scale-free fraction (the caller converts to pips via
/// [`widen_frac_to_pips`]), driven by the pure [`spread_hour_widen_for`] seam
/// over the candle `(mask, widen_frac)`.
///
/// Uses the default [`SPREAD_HOUR_LEAD_MINUTES`] (30-min) look-ahead — the value
/// the **live cron** wants, since it ticks every 15 min and so lands inside the
/// :30–:59 lead window before the top-of-hour spike. A caller that only
/// evaluates at bar boundaries (the offline replay) must instead use
/// [`spread_hour_widen_frac_with_lead`] with a lead ≥ its bar length, or the
/// look-ahead is unreachable at `minute == 0`. See
/// `BUG-spread-hour-widen-no-subhour-lead.md`.
pub fn spread_hour_widen_frac(instrument: &str, now: chrono::DateTime<chrono::Utc>) -> Option<f64> {
    spread_hour_widen_frac_with_lead(instrument, now, SPREAD_HOUR_LEAD_MINUTES)
}

/// [`spread_hour_widen_frac`] with an explicit look-ahead `lead_minutes` instead
/// of the fixed [`SPREAD_HOUR_LEAD_MINUTES`].
///
/// The lead is how far ahead of a flagged hour's top we pre-arm the widen. The
/// **live cron** ticks every 15 min so a 30-min lead is reached in time; the
/// **offline replay** only evaluates at bar closes (`minute == 0` on H1), where a
/// 30-min lead is structurally unreachable (`60 - 0 = 60 > 30`). So the replay
/// passes a lead ≥ its bar length (a full bar): on an H1 close a `lead = 60`
/// makes `60 - 0 <= 60` fire, widening on the bar **before** the flagged hour —
/// exactly what the live cron achieves via its mid-bar tick. Both paths thus
/// converge on "widen on the bar before the spike," curing the replay ↔ live
/// divergence (`BUG-spread-hour-widen-no-subhour-lead.md`).
pub fn spread_hour_widen_frac_with_lead(
    instrument: &str,
    now: chrono::DateTime<chrono::Utc>,
    lead_minutes: i64,
) -> Option<f64> {
    let (mask, widen, tz) = baked_candle_spread_hours(instrument)?;
    spread_hour_widen_for(mask, &widen, tz, now, lead_minutes)
}

/// The **exact sub-candle instant** at which the live cron would first widen an
/// open stop for `instrument`, for a bar spanning `[bar_open, bar_open +
/// bar_seconds)`, plus that widen's `spread/mid` fraction — or `None` when no
/// flagged spread hour is reached in (or led into by) this bar.
///
/// This is the faithful replay stand-in for the live 15-min cron's mid-bar
/// widen: it returns the precise wall-clock moment (e.g. 20:30Z = 30 min before
/// a 21:00Z spike) the widen would fire, NOT the bar-open time. So an H1 replay
/// reports "SL widened at 06:30" — the same sub-candle instant the live worker
/// hits — rather than snapping it to a bar boundary
/// (`BUG-spread-hour-widen-no-subhour-lead.md`).
///
/// Two cases, in priority order. **Already inside** wins: when `bar_open`'s own
/// hour is flagged the widen is already active, so it fires at `bar_open`.
/// Otherwise **lead-in**: when a flagged hour's top `T` is led into by this bar
/// (its `T - lead` instant falls inside `[bar_open, bar_end)`), the widen fires at
/// `T - lead` (clamped to `bar_open`, so it never predates the bar).
///
/// The lead is the fixed [`SPREAD_HOUR_LEAD_MINUTES`] (30) — the live value — so
/// the replay reproduces the live instant exactly; the earlier "full bar early"
/// lead is superseded by this precise sub-candle instant.
pub fn spread_hour_widen_instant(
    instrument: &str,
    bar_open: chrono::DateTime<chrono::Utc>,
    bar_seconds: i64,
) -> Option<(chrono::DateTime<chrono::Utc>, f64)> {
    use chrono::Duration;
    let (mask, widen, tz) = baked_candle_spread_hours(instrument)?;
    if mask == 0 {
        return None;
    }
    use chrono::Timelike;
    let lead = Duration::minutes(SPREAD_HOUR_LEAD_MINUTES);
    let bar_end = bar_open + Duration::seconds(bar_seconds.max(0));
    // Do all the hour/lead arithmetic in the schedule's LOCAL time, then convert
    // the chosen instant back to UTC for the returned wall-clock moment (callers
    // expect UTC). The mask is indexed by the local hour.
    let bar_open_local = bar_open.with_timezone(&tz);

    // Case 2 FIRST (priority) — bar_open's own LOCAL hour is ALREADY a flagged
    // spread hour (a bar deep inside a multi-hour block): the widen is live from
    // the bar's open, so it fires at `bar_open`. This must win over the lead-in
    // below, or an in-block bar would report the *next* hour's lead instant.
    let own_hour = bar_open_local.hour() as usize;
    if mask & (1 << own_hour) != 0 {
        return Some((bar_open, widen[own_hour]));
    }

    // Case 1 — a flagged LOCAL hour top T is LED INTO by this bar: `T - lead`
    // falls inside `[bar_open, bar_end)`. Walk local hour tops forward from the
    // first one strictly after bar_open; the first flagged one whose lead instant
    // lands in this bar is the sub-candle widen instant (e.g. 20:30Z for a
    // 20:00–21:00 bar leading into a 21:00Z=17:00-local spike). Bounded to 25
    // iterations (a day) so a pathological mask can't loop forever.
    let mut hour_top_local = top_of_local_hour(bar_open_local) + Duration::hours(1);
    for _ in 0..25 {
        // Convert the local hour top back to a UTC instant for the interval test.
        let hour_top_utc = hour_top_local.with_timezone(&chrono::Utc);
        let widen_at = (hour_top_utc - lead).max(bar_open);
        // Once even the lead instant of this hour is past the bar's end, no later
        // hour can land inside this bar either — stop.
        if widen_at >= bar_end {
            break;
        }
        let h = hour_top_local.hour() as usize;
        if mask & (1 << h) != 0 {
            return Some((widen_at, widen[h]));
        }
        hour_top_local += Duration::hours(1);
    }
    None
}

/// The top of the local hour containing `t` (minutes/seconds/nanos zeroed),
/// staying in the same `Tz`. Falls back to `t` if the truncation is not
/// representable (never happens on the hour boundary, but coded defensively —
/// no unwrap outside tests).
fn top_of_local_hour(t: chrono::DateTime<chrono_tz::Tz>) -> chrono::DateTime<chrono_tz::Tz> {
    use chrono::Timelike;
    t.with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(t)
}

/// The reject line, as a multiple of an instrument's *normal* (median)
/// spread. The 2026-06-23 spread-hour data showed the post-NY-close
/// blowout is an **FX** phenomenon — FX crosses spike 10–20× their normal
/// (AUD/USD 0.4p → 6p, EUR/GBP 0.5p → 10p), while commodities/indices
/// (Copper, Gold) stay flat. So a per-instrument multiple of normal is the
/// right shape: 5× sits clearly above resting jitter and busy-news
/// widening, yet well below a real spread-hour spike (which is ≥10×), so it
/// catches the blowout. For a flat-spread instrument like Copper (normal
/// ~150p) the line is ~750p — never a false block.
///
/// Chosen over "observed max" because with a short sample window a quiet
/// instrument's max can equal its normal, which would re-introduce the
/// over-blocking this whole change fixed.
pub const SPREAD_REJECT_MULTIPLE: f64 = 5.0;

/// "Elevated" spread cutoff in pips for System 1's reject, per instrument.
///
/// Looks the instrument up in the baked baseline (keyed by the
/// broker-canonical TradeNation name — the same `resolved.instrument` the
/// gate passes). When found, the threshold is
/// `median × SPREAD_REJECT_MULTIPLE` (5× the instrument's own normal
/// spread) — see [`SPREAD_REJECT_MULTIPLE`].
///
/// Falls back to the flat [`SPREAD_BLACKOUT_ELEVATED_PIPS`] for any
/// instrument not in the baseline (a fresh asset, or one whose samples
/// lacked a pip size). Re-baked whenever the committed samples grow.
pub fn elevated_threshold_pips(instrument: &str) -> f64 {
    threshold_from_baseline(baked_baseline(instrument))
}

/// The pure threshold math behind [`elevated_threshold_pips`]: `median ×
/// SPREAD_REJECT_MULTIPLE` when a baseline is present, else the flat
/// [`SPREAD_BLACKOUT_ELEVATED_PIPS`]. Split out (free of the table lookup) so the
/// decision is unit-testable independent of whatever the baked table currently
/// holds — the logic is byte-identical to before the candle-table swap; only the
/// data source of `baked_baseline` moved.
fn threshold_from_baseline(baseline: Option<(f64, f64, f64)>) -> f64 {
    match baseline {
        Some((_low, _high, median)) => median * SPREAD_REJECT_MULTIPLE,
        None => SPREAD_BLACKOUT_ELEVATED_PIPS,
    }
}

/// The baked `(low, high, median)` spread-magnitude pips for an instrument, or
/// `None` when it isn't in the candle table. Exposed so the reject path can name
/// the instrument's normal vs. current spread in the operator-facing message.
///
/// Reads the candle-derived [`baseline_candle`] table (the same source as the
/// spread-HOUR mask), keyed by the broker symbol — the source of the pips
/// baseline. The table's trailing columns
/// are `(median_pips, low_pips, high_pips)`; this returns them re-ordered to the
/// `(low, high, median)` contract the callers expect. A row whose `median_pips`
/// is `0.0` (no pip size at generation, or the pre-regen placeholder) yields
/// `None` so [`elevated_threshold_pips`] falls back to the flat cutoff exactly
/// as an absent instrument would — same threshold math, same fallback.
pub fn baked_baseline(instrument: &str) -> Option<(f64, f64, f64)> {
    baseline_candle::SPREAD_BASELINE_CANDLE
        .iter()
        .find(|(_broker, symbol, ..)| *symbol == instrument)
        .map(
            |(_broker, _symbol, _schedule, _reviewed, _mask, _widen, median, low, high)| {
                (*low, *high, *median)
            },
        )
        .filter(|(_low, _high, median)| *median > 0.0)
}

/// How many minutes ahead of an elevated hour's start we pre-emptively
/// widen an open stop. The spread spike is at the top of the hour; widening
/// ~30 min early means the stop is already out of the way before the spike
/// lands, rather than racing it. Shared constant so the worker cron and the
/// offline replay lead by the same amount.
pub const SPREAD_HOUR_LEAD_MINUTES: i64 = 30;

/// Pure spread-hour-widen decision over an explicit `(mask, widen)` — the
/// unit-testable seam behind [`spread_hour_widen_frac`], free of the baked
/// table so tests can drive it with synthetic instrument shapes.
///
/// Returns the widen pips iff `now`'s hour is elevated, or `now` is within
/// `lead_minutes` of the top of a next hour that is elevated (look-ahead, so the
/// stop is out of the way before the spike). `None` otherwise (including a mask
/// of 0).
///
/// `lead_minutes` is explicit so the two callers can differ: the live cron
/// passes [`SPREAD_HOUR_LEAD_MINUTES`] (30, reachable by its 15-min tick); the
/// offline replay passes ≥ its bar length so the look-ahead is reachable at a bar
/// boundary (`minute == 0`). See [`spread_hour_widen_frac_with_lead`].
///
/// The mask is indexed by the **schedule-local** hour: `now` (UTC) is converted
/// to `tz` first, and both the current-hour and lead-into-next-hour arithmetic
/// run in that local time. A `ny` 17:00-local bit therefore fires at 21:00 UTC
/// (EDT) or 22:00 UTC (EST) from the same bit.
fn spread_hour_widen_for(
    mask: u32,
    widen: &[f64; 24],
    tz: chrono_tz::Tz,
    now: chrono::DateTime<chrono::Utc>,
    lead_minutes: i64,
) -> Option<f64> {
    use chrono::Timelike;
    if mask == 0 {
        return None;
    }
    let local = now.with_timezone(&tz);
    let hour = local.hour() as usize;
    // Within the lead window of the next LOCAL hour's top? Then look ahead.
    let minutes_into_hour = local.minute() as i64;
    let lead_reaches_next = 60 - minutes_into_hour <= lead_minutes;
    if lead_reaches_next {
        let next = (hour + 1) % 24;
        if mask & (1 << next) != 0 {
            return Some(widen[next]);
        }
    }
    if mask & (1 << hour) != 0 {
        return Some(widen[hour]);
    }
    None
}

/// Is `now` a spread hour for `instrument` (per-instrument, learned)?
///
/// The hour membership comes from the **candle-derived** mask
/// ([`baked_candle_mask`]) — the source of truth that fixed the sampler's
/// whole-overnight over-flag. The same [`SPREAD_HOUR_LEAD_MINUTES`] look-ahead
/// as the System-2 widen applies (so System 3 cancels a resting order 30 min
/// before the spike, matching the widen lead).
///
/// Fallbacks preserve prior behaviour for anything the candle table doesn't
/// cover: an instrument absent from the candle table, or present with an empty
/// mask, drops to the legacy NY-close-edge gate. (An empty candle mask can mean
/// "reviewed, genuinely flat" — for those the NY-close-edge fallback is the
/// same conservative default the sampler gave an un-learned instrument; a later
/// stage can honour the explicit `reviewed` verdict to skip the fallback.)
pub fn is_spread_hour(instrument: &str, now: chrono::DateTime<chrono::Utc>) -> bool {
    match baked_candle_row(instrument) {
        // Present with a non-empty mask AND a resolvable schedule tz → index the
        // mask by the schedule-LOCAL hour (DST-aware).
        Some((schedule, mask, _widen)) if mask != 0 => match schedule_tz(schedule) {
            Some(tz) => mask_active_with_lead(mask, tz, now),
            // Mask present but schedule is `none`/unknown → no spread schedule,
            // fall back to the legacy NY-close-edge gate.
            None => crate::ny_clock::is_ny_close_edge(now),
        },
        // Absent, or reviewed-flat (mask 0) → legacy fallback.
        _ => crate::ny_clock::is_ny_close_edge(now),
    }
}

/// The longest bar on which a spread hour still *dominates* the candle, so the
/// rubbish-candle **suppression** (declining an entry/signal/cross that lands on
/// a spread-hour bar) should apply. A learned spread hour is a single UTC hour;
/// on a bar this length or shorter that one hour is most/all of the bar, so its
/// OHLC really is a liquidity-vacuum blowout. On a longer bar (H4, D) the spread
/// hour is a minority slice — the other 3 (or 23) hours of genuine trading
/// dilute it back to real data — so the suppression must NOT fire there.
///
/// We only trade 15m / 1h / 4h / D, so this is exactly "suppress on 15m + 1h,
/// allow on 4h + D". `3600` = one hour: `<=` keeps H1, drops H4.
const SPREAD_HOUR_SUPPRESSION_MAX_BAR_SECONDS: i64 = 60 * 60;

/// Whether spread-hour rubbish-candle **suppression** should apply for a bar of
/// `granularity` landing on a spread hour: `is_spread_hour` AND the bar is short
/// enough that the spread hour dominates it (see
/// [`SPREAD_HOUR_SUPPRESSION_MAX_BAR_SECONDS`]).
///
/// This is the single seam the engine's suppression sites (entry/signal, retest
/// stamp, intrabar veto/cross, reversal-close detection) call, so the
/// granularity policy lives in one place and replay == live. It deliberately
/// does **not** gate the *stop-widen* / pending-order-lifecycle consumers of
/// `is_spread_hour` — those protect an open position's stop through the actual
/// spike and are correct on any bar size.
pub fn suppress_on_spread_hour(
    instrument: &str,
    now: chrono::DateTime<chrono::Utc>,
    granularity: crate::broker::Granularity,
) -> bool {
    suppress_on_spread_hour_bar_seconds(instrument, now, granularity.seconds())
}

/// [`suppress_on_spread_hour`] keyed on a raw **bar length in seconds** instead
/// of a [`Granularity`] enum. For consumers that don't carry the plan's
/// granularity but do have the candle spacing to hand — the fill simulator
/// derives `bar_seconds` from consecutive candle times so its spread-hour fill
/// skip matches the engine's suppression decision (replay == live) without
/// threading `Granularity` through every simulator signature. A non-positive
/// `bar_seconds` (a degenerate/one-candle window) fails safe to "suppress"
/// (treated as a short bar) so we never *newly* allow a fill we can't size.
pub fn suppress_on_spread_hour_bar_seconds(
    instrument: &str,
    now: chrono::DateTime<chrono::Utc>,
    bar_seconds: i64,
) -> bool {
    let short_bar = bar_seconds <= 0 || bar_seconds <= SPREAD_HOUR_SUPPRESSION_MAX_BAR_SECONDS;
    short_bar && is_spread_hour(instrument, now)
}

/// Is `mask` active at `now` for schedule zone `tz`, honouring the
/// [`SPREAD_HOUR_LEAD_MINUTES`] look-ahead? The mask-only twin of
/// [`spread_hour_widen_for`] (which returns the widen size); this returns just
/// the boolean membership so `is_spread_hour` can key off the candle mask
/// without a widen array. Kept structurally identical to `spread_hour_widen_for`
/// so the two never drift.
///
/// The mask is indexed by the schedule-LOCAL hour: `now` (UTC) is converted to
/// `tz` before the hour/lead arithmetic, so a DST shift in `tz` moves the UTC
/// hour that matches a given local mask bit.
fn mask_active_with_lead(mask: u32, tz: chrono_tz::Tz, now: chrono::DateTime<chrono::Utc>) -> bool {
    use chrono::Timelike;
    if mask == 0 {
        return false;
    }
    let local = now.with_timezone(&tz);
    let hour = local.hour() as usize;
    let minutes_into_hour = local.minute() as i64;
    let lead_reaches_next = 60 - minutes_into_hour <= SPREAD_HOUR_LEAD_MINUTES;
    if lead_reaches_next {
        let next = (hour + 1) % 24;
        if mask & (1 << next) != 0 {
            return true;
        }
    }
    mask & (1 << hour) != 0
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

/// The spread-blackout backstop's two concerns, split (2026-07).
///
/// Historically a single `BLACKOUT_BACKSTOP_SECONDS = 3h` drove BOTH the
/// per-trade record's TTL and the "force-restore a stuck record" safety
/// ceiling. That conflation broke for **multi-hour** learned blocks: AUD/CHF's
/// baked block is 21:00–05:00Z (8h), but the record expired at 21:00 + 3h =
/// 00:00Z — mid-block, *before* the block-lift restore at 05:00Z could find it —
/// so the cancelled order was never restored (0R instead of the intended
/// deferred fill). And the 3h backstop *itself* would fire at 00:00Z, force-
/// restoring the order back INTO the still-active trough. The two roles want
/// opposite sizing, so they're now separate:
///
/// 1. **Record TTL = the block's own length + grace** ([`spread_block_ttl_seconds`]).
///    The cancel-record must always OUTLIVE its own spread-hour block so the
///    normal OFF-side restore (block-lifted / spread-recovered) can still find it.
///    Per-instrument, derived from the baked mask.
/// 2. **Safety force-restore ceiling** ([`SAFETY_FORCE_RESTORE_SECONDS`]) — a
///    tight, global last-resort for a record that is somehow still `applied` long
///    after it should have cleared. Gated so it NEVER fires while the instrument
///    is still in a spread hour (see `backstop_due` call sites), so it can't
///    restore into an active block.
///
/// The normal block-lift restore should almost always win first (TTL ≥ block);
/// the safety ceiling is belt-and-braces.
///
/// Sized as a genuine LAST-RESORT: **longer than any realistic learned block** so
/// it fires only when the normal `off_now` restore has somehow failed to clear a
/// record for far longer than a legitimate block (e.g. a persistent quote-error
/// storm, or a repeatedly-failing `clear`). At 12h it sits comfortably past the
/// widest overnight FX block (~8h) yet well under a full day; combined with the
/// `!is_spread_hour` gate on its call site, it can never restore into an active
/// block even for a mis-baked over-long mask. (The old 3h value was BOTH the
/// per-record TTL and this ceiling — a conflation that expired an 8h AUD/CHF
/// record at 3h, before its block-lift restore. TTL is now [`spread_block_ttl_seconds`].)
///
/// Lives in `core` (not the cron crate) so the offline replay computes the same
/// values as the live recovery watcher without depending on `trade-control-cron`.
/// The cron crate re-exports both.
pub const SAFETY_FORCE_RESTORE_SECONDS: u64 = 12 * 60 * 60;

/// The coarse legacy NY-close-edge **window marker** TTL (~3h). This is the
/// global "the NY-close spread window is open" flag the entry gate reads
/// (`dispatch::enter`'s spread-blackout gate), NOT a per-trade cancel-record — so
/// it is deliberately decoupled from both split backstop concerns (the per-record
/// block-length [`spread_block_ttl_seconds`] and the safety
/// [`SAFETY_FORCE_RESTORE_SECONDS`] ceiling). Kept at its historical 3h so the
/// legacy `is_ny_close_edge` entry-gating behaviour is unchanged.
///
/// Lives in `core` (same rationale as [`SAFETY_FORCE_RESTORE_SECONDS`]) so the
/// offline replay opens the window marker with the IDENTICAL TTL the live cron's
/// `apply_if_ny_close_edge` uses, without the replay depending on
/// `trade-control-cron`. The cron crate re-exports it.
pub const NY_CLOSE_WINDOW_MARKER_TTL_SECONDS: u64 = 3 * 60 * 60;

/// Grace tail added to a block's own length when sizing a cancel-record's TTL,
/// so the record comfortably outlives the block-lift restore (which fires the
/// tick the block ends). One extra hour: enough slack for the OFF-side pass to
/// run at/after the lift without the record having lapsed.
pub const SPREAD_BLOCK_TTL_GRACE_SECONDS: u64 = 60 * 60;

/// Hard ceiling on a walked block length (defensive): no learned block should
/// span the whole day, but if a (buggy) mask were all-ones we must not loop 24×
/// and hand back a 24h TTL. Caps the walk at 23 hours.
const MAX_BLOCK_HOURS: u32 = 23;

/// The TTL (seconds) a spread-hour cancel-record should carry so it OUTLIVES its
/// own block: the length of the contiguous spread-hour run that `opened_at` sits
/// in, plus [`SPREAD_BLOCK_TTL_GRACE_SECONDS`].
///
/// Walks forward hour-by-hour from `opened_at` through
/// [`is_spread_hour`] until the first non-spread hour (wrap-around midnight is
/// handled by [`is_spread_hour`]'s hour-of-day mask). A record opened at the very
/// top of the block gets `block_len + grace`; one opened partway through gets the
/// *remaining* block + grace, which is still ≥ the time to the lift (all we need:
/// the record must live until the lift). Falls back to `grace` alone when
/// `opened_at` is somehow not itself a spread hour (nothing to outlive), and is
/// capped at [`MAX_BLOCK_HOURS`] + grace for a pathological all-hours mask.
pub fn spread_block_ttl_seconds(instrument: &str, opened_at: chrono::DateTime<chrono::Utc>) -> u64 {
    use chrono::Timelike;
    // Walk on the pure per-instrument candle mask when the instrument has one
    // (deterministic block edges), indexed by the SCHEDULE-LOCAL hour so the walk
    // matches `is_spread_hour`; fall back to a real-clock probe through
    // `is_spread_hour` for legacy NY-close-edge instruments (no baked mask/tz).
    let hours = match baked_candle_spread_hours(instrument) {
        Some((mask, _widen, tz)) if mask != 0 => {
            let local_hour = opened_at.with_timezone(&tz).hour();
            block_hours_from_mask(mask, local_hour)
        }
        _ => block_hours_by_probe(instrument, opened_at),
    };
    hours as u64 * 3600 + SPREAD_BLOCK_TTL_GRACE_SECONDS
}

/// The contiguous spread-hour block that `now` sits in, as a `(start, end)` pair
/// of UTC instants — `start` is the top of the first elevated hour of the run,
/// `end` is the top of the first non-elevated hour after it (so the block is
/// `[start, end)`, half-open). `None` when `now` is not itself a spread hour.
///
/// The window is what the offline replay prints when a signal confirmed inside a
/// spread block but the entry was suppressed ("not entering — spread-hour from X
/// to Y"). It is derived the same way the TTL is: the pure per-instrument hour
/// mask when the instrument has one, a real-clock probe through
/// [`is_spread_hour`] otherwise (legacy NY-close-edge instruments). The bounds
/// snap to the top of the hour — the mask is hour-of-day granularity, so
/// sub-hour precision would be false detail. The 30-minute *lead* that
/// [`is_spread_hour`] adds before an elevated hour is intentionally **not**
/// folded into `start`: the reported window is the elevated block itself, not the
/// stop-widening lead-in.
pub fn spread_block_window(
    instrument: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    use chrono::{Duration, Timelike};
    // The hour containing `now` must itself be elevated (ignore the lead — we
    // report the block, not the lead-in). Use the pure candle mask when
    // available, indexed by the SCHEDULE-LOCAL hour. The reported `(start, end)`
    // are UTC instants: the block's hour-length is DST-invariant (a contiguous
    // run of local hours, each ≈ one real hour away from a DST transition), so
    // subtracting/adding those hour counts from the UTC hour-top is correct.
    let hour_top = now
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))?;
    let (back, fwd) = match baked_candle_spread_hours(instrument) {
        Some((mask, _widen, tz)) if mask != 0 => {
            let local_hour = now.with_timezone(&tz).hour();
            if mask & (1 << local_hour) == 0 {
                return None;
            }
            (
                block_start_hours_from_mask(mask, local_hour),
                block_hours_from_mask(mask, local_hour),
            )
        }
        // Legacy no-mask instrument: probe the real clock hour-by-hour. Require
        // `now`'s own hour to be a spread hour (probe at the hour top so the lead
        // of the *next* hour can't spoof membership).
        _ => {
            if !is_spread_hour(instrument, hour_top) {
                return None;
            }
            (
                block_start_hours_by_probe(instrument, hour_top),
                block_hours_by_probe(instrument, hour_top),
            )
        }
    };
    let start = hour_top - Duration::hours(back as i64);
    let end = hour_top + Duration::hours(fwd as i64);
    Some((start, end))
}

/// Consecutive elevated hours in `mask` ending at (and including) `hour`, walking
/// **backward** and wrapping midnight, capped at [`MAX_BLOCK_HOURS`]. The count
/// of hours strictly *before* `hour` that are still in the block — so
/// `hour - result` is the block's first elevated hour. `0` when the hour before
/// `hour` isn't elevated (this hour is the block start). Pure over the mask.
fn block_start_hours_from_mask(mask: u32, hour: u32) -> u32 {
    let mut back: u32 = 0;
    while back < MAX_BLOCK_HOURS {
        let h = (hour + 24 - ((back + 1) % 24)) % 24;
        if mask & (1 << h) == 0 {
            break;
        }
        back += 1;
    }
    back
}

/// Real-clock backward twin of [`block_hours_by_probe`] for legacy (no-mask)
/// instruments: probe [`is_spread_hour`] backward hour-by-hour from `now`.
fn block_start_hours_by_probe(instrument: &str, now: chrono::DateTime<chrono::Utc>) -> u32 {
    let mut back: u32 = 0;
    while back < MAX_BLOCK_HOURS {
        let probe = now - chrono::Duration::hours((back + 1) as i64);
        if !is_spread_hour(instrument, probe) {
            break;
        }
        back += 1;
    }
    back
}

/// The number of consecutive elevated hours in `mask` starting at (and
/// including) `opened_hour`, wrapping around midnight, capped at
/// [`MAX_BLOCK_HOURS`]. `0` when `opened_hour` itself isn't elevated (nothing to
/// outlive). Pure over the 24-bit mask so it's testable with synthetic shapes.
fn block_hours_from_mask(mask: u32, opened_hour: u32) -> u32 {
    let mut hours: u32 = 0;
    while hours < MAX_BLOCK_HOURS {
        let h = (opened_hour + hours) % 24;
        if mask & (1 << h) == 0 {
            break;
        }
        hours += 1;
    }
    hours
}

/// Real-clock fallback for legacy (no-mask) instruments: probe `is_spread_hour`
/// forward hour-by-hour from `opened_at`.
fn block_hours_by_probe(instrument: &str, opened_at: chrono::DateTime<chrono::Utc>) -> u32 {
    let mut hours: u32 = 0;
    while hours < MAX_BLOCK_HOURS {
        let probe = opened_at + chrono::Duration::hours(hours as i64);
        if !is_spread_hour(instrument, probe) {
            break;
        }
        hours += 1;
    }
    hours
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid rfc3339 fixture")
    }

    // --- spread-blackout reject decision (unchanged by Stage 3) ---

    #[test]
    fn window_closed_never_rejects() {
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
        assert!(!spread_blackout_decision(true, 8.0, 8.0));
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn recovered_cutoff_sits_below_elevated_for_hysteresis() {
        assert!(SPREAD_BLACKOUT_RECOVERED_PIPS < SPREAD_BLACKOUT_ELEVATED_PIPS);
    }

    #[test]
    fn unknown_instrument_falls_back_to_flat_constant() {
        assert_eq!(
            elevated_threshold_pips("DEFINITELY NOT A REAL MARKET ZZZ"),
            SPREAD_BLACKOUT_ELEVATED_PIPS
        );
        assert_eq!(elevated_threshold_pips(""), SPREAD_BLACKOUT_ELEVATED_PIPS);
    }

    #[test]
    fn threshold_from_present_baseline_is_five_times_median() {
        // A present candle-table baseline (low, high, median) → 5× median.
        let t = threshold_from_baseline(Some((1.0, 6.0, 2.0)));
        assert!(
            (t - 2.0 * SPREAD_REJECT_MULTIPLE).abs() < 1e-9,
            "threshold {t} != 5x median 2.0",
        );
    }

    #[test]
    fn threshold_from_absent_baseline_is_the_flat_constant() {
        assert_eq!(threshold_from_baseline(None), SPREAD_BLACKOUT_ELEVATED_PIPS);
    }

    #[test]
    fn elevated_threshold_reads_median_from_a_candle_row() {
        // Drive the full path over any candle-table row that carries a non-zero
        // median (post-regen). The committed table is currently all-placeholder
        // (0.0 pips), so scan for a real median; when found, assert the gate
        // returns 5× it. Until the regen lands this test is vacuously satisfied
        // by the fallback branch below — which is itself an assertion that the
        // placeholder era behaves exactly like the flat fallback.
        let with_median = baseline_candle::SPREAD_BASELINE_CANDLE
            .iter()
            .find(|(.., median, _low, _high)| *median > 0.0);
        match with_median {
            Some((_b, symbol, .., median, _low, _high)) => {
                assert!(
                    (elevated_threshold_pips(symbol) - median * SPREAD_REJECT_MULTIPLE).abs()
                        < 1e-9,
                    "{symbol}: threshold != 5x candle median {median}",
                );
            }
            None => {
                // Placeholder era: every real instrument falls back to flat.
                assert_eq!(
                    elevated_threshold_pips("oanda-non-existent-row"),
                    SPREAD_BLACKOUT_ELEVATED_PIPS
                );
            }
        }
    }

    // --- schedule tz resolver ---

    #[test]
    fn schedule_tz_maps_known_names_and_rejects_none() {
        assert_eq!(schedule_tz("ny"), Some(chrono_tz::America::New_York));
        assert_eq!(schedule_tz("tokyo"), Some(chrono_tz::Asia::Tokyo));
        assert_eq!(schedule_tz("london"), Some(chrono_tz::Europe::London));
        assert_eq!(schedule_tz("frankfurt"), Some(chrono_tz::Europe::Berlin));
        assert_eq!(schedule_tz("zurich"), Some(chrono_tz::Europe::Zurich));
        assert_eq!(schedule_tz("sydney"), Some(chrono_tz::Australia::Sydney));
        assert_eq!(
            schedule_tz("johannesburg"),
            Some(chrono_tz::Africa::Johannesburg)
        );
        assert_eq!(schedule_tz("hongkong"), Some(chrono_tz::Asia::Hong_Kong));
        assert_eq!(schedule_tz("singapore"), Some(chrono_tz::Asia::Singapore));
        assert_eq!(schedule_tz("none"), None);
        assert_eq!(schedule_tz("bananas"), None);
    }

    // --- spread-hour widen lookup (pure seam, synthetic tables) ---
    //
    // The pure seam `spread_hour_widen_for` now takes an explicit `tz` and
    // indexes the mask by the SCHEDULE-LOCAL hour. Passing `chrono_tz::UTC`
    // makes the mask bit == the UTC hour, so these synthetic-shape tests
    // (authored in UTC bits) stay meaningful and pin the local-time arithmetic.

    /// A single-spike mask (hour `h` UTC), widen 5p at that bit.
    fn spike_shape(h: usize) -> (u32, [f64; 24]) {
        let mut w = [0.0; 24];
        w[h] = 5.0;
        (1 << h, w)
    }

    /// A Gold-shaped mask: a structural overnight block (18:00–06:00 UTC),
    /// widen 75p across the block.
    fn gold_shape() -> (u32, [f64; 24]) {
        let mut mask = 0u32;
        let mut w = [0.0; 24];
        for h in (18..24).chain(0..7) {
            mask |= 1 << h;
            w[h] = 75.0;
        }
        (mask, w)
    }

    #[test]
    fn widen_fires_inside_the_elevated_hour() {
        let (m, w) = spike_shape(21);
        assert_eq!(
            spread_hour_widen_for(
                m,
                &w,
                chrono_tz::UTC,
                ts("2026-07-01T21:15:00Z"),
                SPREAD_HOUR_LEAD_MINUTES
            ),
            Some(5.0),
        );
    }

    #[test]
    fn widen_none_outside_any_elevated_hour() {
        let (m, w) = spike_shape(21);
        assert_eq!(
            spread_hour_widen_for(
                m,
                &w,
                chrono_tz::UTC,
                ts("2026-07-01T12:00:00Z"),
                SPREAD_HOUR_LEAD_MINUTES
            ),
            None
        );
    }

    #[test]
    fn widen_leads_into_the_next_elevated_hour() {
        let (m, w) = spike_shape(21);
        // 20:35 UTC — 25 min before 21:00, inside the 30-min lead → widen now.
        assert_eq!(
            spread_hour_widen_for(
                m,
                &w,
                chrono_tz::UTC,
                ts("2026-07-01T20:35:00Z"),
                SPREAD_HOUR_LEAD_MINUTES
            ),
            Some(5.0),
        );
        // 20:29 UTC — 31 min before, just outside the lead → not yet.
        assert_eq!(
            spread_hour_widen_for(
                m,
                &w,
                chrono_tz::UTC,
                ts("2026-07-01T20:29:00Z"),
                SPREAD_HOUR_LEAD_MINUTES
            ),
            None
        );
    }

    #[test]
    fn widen_lead_wraps_across_midnight() {
        let (m, w) = gold_shape();
        assert_eq!(
            spread_hour_widen_for(
                m,
                &w,
                chrono_tz::UTC,
                ts("2026-07-01T23:40:00Z"),
                SPREAD_HOUR_LEAD_MINUTES
            ),
            Some(75.0),
        );
    }

    #[test]
    fn widen_covers_a_structural_overnight_block() {
        let (m, w) = gold_shape();
        assert_eq!(
            spread_hour_widen_for(
                m,
                &w,
                chrono_tz::UTC,
                ts("2026-07-01T02:00:00Z"),
                SPREAD_HOUR_LEAD_MINUTES
            ),
            Some(75.0),
        );
        assert_eq!(
            spread_hour_widen_for(
                m,
                &w,
                chrono_tz::UTC,
                ts("2026-07-01T12:00:00Z"),
                SPREAD_HOUR_LEAD_MINUTES
            ),
            None
        );
    }

    #[test]
    fn widen_empty_mask_never_fires() {
        assert_eq!(
            spread_hour_widen_for(
                0,
                &[0.0; 24],
                chrono_tz::UTC,
                ts("2026-07-01T21:15:00Z"),
                SPREAD_HOUR_LEAD_MINUTES
            ),
            None
        );
    }

    // --- lead is parameterizable (BUG-spread-hour-widen-no-subhour-lead) ---

    #[test]
    fn widen_30min_lead_is_dead_at_a_bar_boundary() {
        let (m, w) = spike_shape(21);
        assert_eq!(
            spread_hour_widen_for(
                m,
                &w,
                chrono_tz::UTC,
                ts("2026-07-01T20:00:00Z"),
                SPREAD_HOUR_LEAD_MINUTES
            ),
            None,
            "the 30-min lead can't reach the next hour from a :00 bar boundary"
        );
    }

    #[test]
    fn widen_full_bar_lead_pre_arms_on_the_prior_bar() {
        let (m, w) = spike_shape(21);
        assert_eq!(
            spread_hour_widen_for(m, &w, chrono_tz::UTC, ts("2026-07-01T20:00:00Z"), 60),
            Some(5.0),
            "a 60-min (full H1 bar) lead widens on the 20:00 bar, before the 21:00 spike"
        );
        assert_eq!(
            spread_hour_widen_for(m, &w, chrono_tz::UTC, ts("2026-07-01T19:00:00Z"), 60),
            None,
            "only the immediately-prior bar pre-arms, not two bars early"
        );
    }

    // === DST-INVARIANCE — the payoff tests (Stage 3) ===

    /// THE test: a `ny` FX cross with a 17:00-LOCAL mask bit reads as a spread
    /// hour at BOTH a summer UTC instant (21:00 UTC = 17:00 EDT) and a winter
    /// UTC instant (22:00 UTC = 17:00 EST) — the same local mask bit, two
    /// different UTC hours. Proves the mask is indexed by schedule-local hour.
    #[test]
    fn ny_fx_local_1700_is_spread_hour_summer_and_winter() {
        // Fixture: oanda/AUD_CHF is ny with mask 1<<17.
        // Summer (EDT, UTC-4): 2026-07-09 17:00 New York == 21:00 UTC.
        let summer = ts("2026-07-09T21:00:00Z");
        assert!(
            is_spread_hour("AUD_CHF", summer),
            "17:00 EDT (21:00 UTC) must be a spread hour"
        );
        // Winter (EST, UTC-5): 2026-01-15 17:00 New York == 22:00 UTC.
        let winter = ts("2026-01-15T22:00:00Z");
        assert!(
            is_spread_hour("AUD_CHF", winter),
            "17:00 EST (22:00 UTC) must be a spread hour"
        );
        // And the OTHER UTC hour is NOT a spread hour in each season (the naive
        // fixed-UTC bug would flag the wrong one): 22:00 UTC in summer is 18:00
        // EDT (clean), 21:00 UTC in winter is 16:00 EST (clean).
        assert!(
            !is_spread_hour("AUD_CHF", ts("2026-07-09T22:00:00Z")),
            "22:00 UTC in summer is 18:00 EDT — NOT the 17:00 spike"
        );
        assert!(
            !is_spread_hour("AUD_CHF", ts("2026-01-15T21:00:00Z")),
            "21:00 UTC in winter is 16:00 EST — NOT the 17:00 spike"
        );
    }

    /// A fixed-UTC (no US-DST) schedule (`tokyo`, JST = UTC+9 all year) is a
    /// spread hour at the SAME UTC hour year-round — no seasonal shift. Tested on
    /// the pure `mask_active_with_lead` helper with a synthetic 15:00-JST mask so
    /// the assertion is independent of whatever the baked table currently holds
    /// for a Japanese instrument (real indices are flat, mask 0).
    #[test]
    fn tokyo_schedule_is_fixed_utc_year_round() {
        // Synthetic mask: bit 15 set (15:00 JST). 15:00 JST == 06:00 UTC all year.
        let mask = 1u32 << 15;
        let tz = chrono_tz::Asia::Tokyo;
        assert!(
            mask_active_with_lead(mask, tz, ts("2026-07-09T06:00:00Z")),
            "15:00 JST (06:00 UTC) is a spread hour in summer"
        );
        assert!(
            mask_active_with_lead(mask, tz, ts("2026-01-15T06:00:00Z")),
            "15:00 JST (06:00 UTC) is a spread hour in winter — no DST shift"
        );
        // A UTC hour that would be 15:00-local only if Japan observed DST is NOT
        // a spread hour — Japan has none, so 05:00 UTC stays clean. (05:00 UTC =
        // 14:00 JST; the 30-min lead reaches into 06:00 UTC's 15:00-JST hour only
        // from :30+, and this ts is at :00, so it must be clean.)
        assert!(
            !mask_active_with_lead(mask, tz, ts("2026-07-09T05:00:00Z")),
            "05:00 UTC (14:00 JST) is not the spike; Japan has no DST"
        );
    }

    /// An absent instrument and a `none`-schedule instrument both fall back to
    /// the legacy NY-close-edge gate (NOT the local-mask path).
    #[test]
    fn absent_and_none_schedule_fall_back_to_ny_close_edge() {
        // Absent from the candle table → fallback. 12-Mar-2026 is EDT, so the
        // NY-close edge is 21:00 UTC.
        let inst_absent = "DEFINITELY NOT A REAL MARKET ZZZ";
        assert_eq!(
            is_spread_hour(inst_absent, ts("2026-03-12T21:00:00Z")),
            crate::ny_clock::is_ny_close_edge(ts("2026-03-12T21:00:00Z")),
            "absent instrument tracks the NY-close-edge fallback exactly"
        );
        assert!(is_spread_hour(inst_absent, ts("2026-03-12T21:00:00Z")));
        assert!(!is_spread_hour(inst_absent, ts("2026-03-12T10:00:00Z")));

        // Fixture: oanda/BTC_USD has schedule "none" (mask 0) → same fallback.
        assert_eq!(
            is_spread_hour("BTC_USD", ts("2026-03-12T21:00:00Z")),
            crate::ny_clock::is_ny_close_edge(ts("2026-03-12T21:00:00Z")),
            "none-schedule instrument tracks the NY-close-edge fallback"
        );
        assert!(is_spread_hour("BTC_USD", ts("2026-03-12T21:00:00Z")));
        // reviewed=false (EUR_TRY, mask 0) → fallback too.
        assert!(is_spread_hour("EUR_TRY", ts("2026-03-12T21:00:00Z")));
        assert!(!is_spread_hour("EUR_TRY", ts("2026-03-12T10:00:00Z")));
    }

    /// The 30-min lead fires in LOCAL time: 30 min before a flagged LOCAL hour
    /// top pre-arms. For AUD_CHF (17:00-local spike), summer that top is
    /// 21:00 UTC, so 20:35 UTC (== 16:35 EDT, 25 min before) pre-arms; 20:25 UTC
    /// (35 min before) does not.
    #[test]
    fn lead_pre_arms_in_local_time_across_seasons() {
        // Summer.
        assert!(
            is_spread_hour("AUD_CHF", ts("2026-07-09T20:35:00Z")),
            "25 min before the 17:00-EDT spike (20:35 UTC) is inside the lead"
        );
        assert!(
            !is_spread_hour("AUD_CHF", ts("2026-07-09T20:25:00Z")),
            "35 min before (20:25 UTC) is outside the lead"
        );
        // Winter — the SAME local lead, now 30 min before 22:00 UTC.
        assert!(
            is_spread_hour("AUD_CHF", ts("2026-01-15T21:35:00Z")),
            "25 min before the 17:00-EST spike (21:35 UTC) is inside the lead"
        );
        assert!(
            !is_spread_hour("AUD_CHF", ts("2026-01-15T21:25:00Z")),
            "35 min before (21:25 UTC) is outside the lead"
        );
    }

    /// `mask_active_with_lead` pure form, driven directly with an explicit tz —
    /// the local-hour indexing is unit-visible without the table lookup.
    #[test]
    fn mask_active_with_lead_indexes_local_hour() {
        let mask = 1u32 << 17; // 17:00 local
        let ny = chrono_tz::America::New_York;
        // Summer: 21:00 UTC == 17:00 EDT.
        assert!(mask_active_with_lead(mask, ny, ts("2026-07-09T21:00:00Z")));
        // Winter: 22:00 UTC == 17:00 EST.
        assert!(mask_active_with_lead(mask, ny, ts("2026-01-15T22:00:00Z")));
        // Off-hour.
        assert!(!mask_active_with_lead(mask, ny, ts("2026-07-09T12:00:00Z")));
    }

    // --- sub-candle widen instant, DST-aware ---

    /// `spread_hour_widen_instant` returns a UTC instant that, converted to the
    /// row's local tz, sits at the right local pre-hour moment — ACROSS a DST
    /// boundary. AUD_CHF (ny, 17:00-local): the H1 bar leading into the spike
    /// widens 30 min before the LOCAL 17:00 top, i.e. at 16:30 local, which is
    /// 20:30 UTC in summer and 21:30 UTC in winter.
    #[test]
    fn widen_instant_is_local_1630_across_dst() {
        use chrono::Timelike;
        let ny = chrono_tz::America::New_York;

        // Summer: the 20:00–21:00 UTC H1 bar (16:00–17:00 EDT) leads into the
        // 17:00-EDT spike → widen at 20:30 UTC (16:30 EDT).
        let (at_s, frac_s) = spread_hour_widen_instant("AUD_CHF", ts("2026-07-09T20:00:00Z"), 3600)
            .expect("summer bar leads into the 17:00-EDT spike");
        assert_eq!(at_s, ts("2026-07-09T20:30:00Z"));
        assert!(frac_s > 0.0);
        let local_s = at_s.with_timezone(&ny);
        assert_eq!((local_s.hour(), local_s.minute()), (16, 30));

        // Winter: the 21:00–22:00 UTC H1 bar (16:00–17:00 EST) leads into the
        // 17:00-EST spike → widen at 21:30 UTC (16:30 EST).
        let (at_w, _frac_w) =
            spread_hour_widen_instant("AUD_CHF", ts("2026-01-15T21:00:00Z"), 3600)
                .expect("winter bar leads into the 17:00-EST spike");
        assert_eq!(at_w, ts("2026-01-15T21:30:00Z"));
        let local_w = at_w.with_timezone(&ny);
        assert_eq!(
            (local_w.hour(), local_w.minute()),
            (16, 30),
            "same LOCAL 16:30 pre-hour moment, different UTC hour"
        );
    }

    /// The "own hour" case: a bar OPENING at the flagged LOCAL hour widens at the
    /// bar open. Summer: 21:00 UTC == 17:00 EDT (flagged) → widen at bar open.
    #[test]
    fn widen_instant_fires_at_bar_open_when_local_hour_flagged() {
        let (at, _frac) = spread_hour_widen_instant("AUD_CHF", ts("2026-07-09T21:00:00Z"), 3600)
            .expect("21:00 UTC == 17:00 EDT IS the flagged local hour");
        assert_eq!(
            at,
            ts("2026-07-09T21:00:00Z"),
            "own local hour flagged → widen at the bar open"
        );
    }

    /// Two bars before the spike → no widen (only the immediately-leading bar).
    #[test]
    fn widen_instant_none_two_bars_before_the_spike() {
        assert_eq!(
            spread_hour_widen_instant("AUD_CHF", ts("2026-07-09T19:00:00Z"), 3600),
            None,
            "the 19:00–20:00 UTC bar (15:00 EDT) is two bars before the 17:00 spike"
        );
    }

    #[test]
    fn widen_instant_none_for_a_clean_daytime_bar() {
        assert_eq!(
            spread_hour_widen_instant("AUD_CHF", ts("2026-07-09T12:00:00Z"), 3600),
            None
        );
    }

    #[test]
    fn widen_instant_none_for_none_schedule() {
        // BTC_USD is schedule "none" (no tz) → the candle path yields None.
        assert_eq!(
            spread_hour_widen_instant("BTC_USD", ts("2026-07-09T21:00:00Z"), 3600),
            None
        );
    }

    // --- widen_frac delegation + tz path ---

    #[test]
    fn widen_frac_with_lead_matches_the_default_helper() {
        // AUD_CHF (ny, 17:00-local). Summer inside-the-hour instant.
        let inside = ts("2026-07-09T21:15:00Z");
        assert_eq!(
            spread_hour_widen_frac("AUD_CHF", inside),
            spread_hour_widen_frac_with_lead("AUD_CHF", inside, SPREAD_HOUR_LEAD_MINUTES),
        );
        // The full-bar lead reaches the local spike from the prior H1 close where
        // the 30-min default cannot. Prior H1 close = 20:00 UTC (16:00 EDT).
        let prior_close = ts("2026-07-09T20:00:00Z");
        assert!(
            spread_hour_widen_frac("AUD_CHF", prior_close).is_none(),
            "30-min default: dead at the 20:00 UTC boundary"
        );
        assert!(
            spread_hour_widen_frac_with_lead("AUD_CHF", prior_close, 60).is_some(),
            "60-min lead: pre-arms on the 20:00 UTC bar"
        );
    }

    // --- widen_frac_to_pips (unchanged) ---

    #[test]
    fn widen_frac_to_pips_converts_at_reference_price() {
        let pips = widen_frac_to_pips(0.0004, 1.10, 0.0001);
        assert!(
            (pips - 4.4).abs() < 1e-9,
            "0.04% of 1.10 / 0.0001 = 4.4p, got {pips}"
        );
        let gold = widen_frac_to_pips(0.001, 2400.0, 0.01);
        assert!(
            (gold - 240.0).abs() < 1e-6,
            "0.1% of 2400 / 0.01 = 240p, got {gold}"
        );
    }

    #[test]
    fn widen_frac_scales_with_price() {
        let frac = 0.0005;
        let cheap = widen_frac_to_pips(frac, 1.0, 0.0001);
        let dear = widen_frac_to_pips(frac, 2.0, 0.0001);
        assert!(
            (dear - 2.0 * cheap).abs() < 1e-9,
            "twice the price → twice the pips"
        );
    }

    // --- suppression seam (short-bar policy unchanged; DST-aware via is_spread_hour) ---

    #[test]
    fn suppression_only_applies_to_short_bars() {
        use crate::broker::Granularity;
        // AUD_CHF summer spike: 21:00 UTC == 17:00 EDT is a spread hour.
        let inst = "AUD_CHF";
        let t = ts("2026-07-09T21:00:00Z");
        assert!(
            is_spread_hour(inst, t),
            "premise: 17:00 EDT is a spread hour"
        );
        assert!(suppress_on_spread_hour(inst, t, Granularity::M15));
        assert!(suppress_on_spread_hour(inst, t, Granularity::H1));
        assert!(!suppress_on_spread_hour(inst, t, Granularity::H4));
        assert!(!suppress_on_spread_hour(inst, t, Granularity::D1));
        let clean = ts("2026-07-09T12:00:00Z");
        assert!(!suppress_on_spread_hour(inst, clean, Granularity::M15));
    }

    #[test]
    fn suppression_bar_seconds_matches_the_granularity_seam() {
        let inst = "AUD_CHF";
        let t = ts("2026-07-09T21:00:00Z");
        assert!(suppress_on_spread_hour_bar_seconds(inst, t, 900)); // 15m
        assert!(suppress_on_spread_hour_bar_seconds(inst, t, 3600)); // 1h
        assert!(!suppress_on_spread_hour_bar_seconds(inst, t, 14400)); // 4h
        assert!(!suppress_on_spread_hour_bar_seconds(inst, t, 86400)); // 1d
        assert!(suppress_on_spread_hour_bar_seconds(inst, t, 0)); // degenerate → suppress
        assert!(!suppress_on_spread_hour_bar_seconds(
            inst,
            ts("2026-07-09T12:00:00Z"),
            0
        ));
    }

    // --- block-length TTL (concern 1 of the backstop split), pure-mask helpers ---

    #[test]
    fn block_hours_multi_hour_wrap_from_top() {
        let mask: u32 = (1 << 21)
            | (1 << 22)
            | (1 << 23)
            | (1 << 0)
            | (1 << 1)
            | (1 << 2)
            | (1 << 3)
            | (1 << 4);
        assert_eq!(block_hours_from_mask(mask, 21), 8);
    }

    #[test]
    fn block_hours_multi_hour_wrap_from_middle() {
        let mask: u32 = (1 << 21)
            | (1 << 22)
            | (1 << 23)
            | (1 << 0)
            | (1 << 1)
            | (1 << 2)
            | (1 << 3)
            | (1 << 4);
        assert_eq!(block_hours_from_mask(mask, 0), 5);
    }

    #[test]
    fn block_hours_single_hour() {
        assert_eq!(block_hours_from_mask(1 << 21, 21), 1);
    }

    #[test]
    fn block_hours_zero_off_block() {
        assert_eq!(block_hours_from_mask(1 << 21, 12), 0);
    }

    #[test]
    fn block_hours_all_ones_is_capped() {
        assert_eq!(block_hours_from_mask(u32::MAX, 0), MAX_BLOCK_HOURS);
    }

    #[test]
    fn ttl_seconds_adds_grace() {
        let hours = block_hours_from_mask(1 << 21, 21);
        let ttl = hours as u64 * 3600 + SPREAD_BLOCK_TTL_GRACE_SECONDS;
        assert_eq!(ttl, 2 * 3600);
    }

    #[test]
    fn block_start_hours_walks_back_across_midnight() {
        let mask = (1 << 21) | (1 << 22) | (1 << 23) | (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3);
        assert_eq!(block_start_hours_from_mask(mask, 2), 5);
        assert_eq!(block_start_hours_from_mask(mask, 21), 0);
    }

    // --- block TTL / window against the candle fixture, DST-aware ---

    /// EUR_HUF (baked ny mask bits 17+18) is a real two-hour LOCAL block
    /// (17:00 + 18:00 New York). A record opened at the block top (17:00 EDT =
    /// 21:00 UTC in summer) yields a 2h block + grace TTL; the SAME local block
    /// gives the same TTL in winter (17:00 EST = 22:00 UTC) — DST-invariant.
    #[test]
    fn block_ttl_from_candle_fixture_is_dst_invariant() {
        let summer_top = ts("2026-07-09T21:00:00Z"); // 17:00 EDT
        let winter_top = ts("2026-01-15T22:00:00Z"); // 17:00 EST
        let ttl_s = spread_block_ttl_seconds("EUR_HUF", summer_top);
        let ttl_w = spread_block_ttl_seconds("EUR_HUF", winter_top);
        // 2 local hours + 1h grace = 3h.
        assert_eq!(ttl_s, 2 * 3600 + SPREAD_BLOCK_TTL_GRACE_SECONDS);
        assert_eq!(
            ttl_w, ttl_s,
            "block TTL is the same local block in either season"
        );
    }

    /// The `(start, end)` window for the EUR_HUF 2-hour local block (bits 17+18),
    /// reported in UTC. Summer: 17:00–19:00 EDT == 21:00–23:00 UTC.
    #[test]
    fn block_window_from_candle_fixture_summer() {
        let (start, end) =
            spread_block_window("EUR_HUF", ts("2026-07-09T21:30:00Z")).expect("in block");
        assert_eq!(
            start,
            ts("2026-07-09T21:00:00Z"),
            "block start 17:00 EDT = 21:00 UTC"
        );
        assert_eq!(
            end,
            ts("2026-07-09T23:00:00Z"),
            "block end 19:00 EDT = 23:00 UTC"
        );
        // A clean midday bar returns None.
        assert!(spread_block_window("EUR_HUF", ts("2026-07-09T12:00:00Z")).is_none());
    }

    /// Absent instrument block window falls through the probe path (NY-close
    /// edge). 12-Mar EDT close edge is 21:00 UTC.
    #[test]
    fn block_window_absent_uses_probe_fallback() {
        let inst = "DEFINITELY NOT A REAL MARKET ZZZ";
        // At the NY-close edge the probe reports a 1h block.
        let w = spread_block_window(inst, ts("2026-03-12T21:00:00Z"));
        assert!(
            w.is_some(),
            "NY-close edge is a spread hour via the probe fallback"
        );
        assert!(
            spread_block_window(inst, ts("2026-03-12T10:00:00Z")).is_none(),
            "a clean hour is not a spread block"
        );
    }
}
