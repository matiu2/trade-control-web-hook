# BUG: System-2 spread-hour stop-widen never pre-arms — replay has no sub-hour ticks, and the 30-min lead is dead on H1

**Status:** ✅ FIXED (`08c6f27`, "report widen at its sub-candle instant"). Diagnosed
2026-07-14; header read "open, unfixed" until 2026-07-28.

The fix is `core::spread_blackout::spread_hour_widen_instant`, which resolves the
**precise sub-candle instant** the widen takes effect instead of snapping it to a
bar boundary — so the 30-min lead lands at `T - lead` even on an H1 stream, and the
"replay only evaluates at bar boundaries" premise below no longer costs the lead.
The replay consumes the same shared function (`cli/…/fill_sim.rs`), so the
replay↔live divergence is closed at the root: one implementation, both callers.
`19c7158` later made the mask DST-aware by indexing it on the schedule-local hour.
**Severity:** money-path. Causes a real −1R stop-out that the design intends to
avoid, and a replay↔live divergence (replay reports a loss the live worker may
not actually take).
**Owner of the sibling fix:** the OANDA-mask over-flag half is being handled
separately (see "Related, handled separately" below). **This report is only
about the sub-hour-tick / widen-lead defect.**

---

## One-paragraph summary

The spread-hour "System 2" pre-emptively widens an open position's stop-loss
~30 min **before** a learned spread-hour spike, so the stop is out of the way
before the liquidity-vacuum blowout wicks through it. That 30-min lead
(`SPREAD_HOUR_LEAD_MINUTES = 30`) can only fire if something evaluates the
position at :30–:59 past the hour. **The offline replay only evaluates at bar
boundaries (:00 on an H1 stream), so the lead never triggers in replay** — the
widen and the stop-out collide on the same spike bar and the stop-out wins. The
**live worker** ticks System 2 every 900 s (15 min), so live *does* have
sub-hour evaluation points and the lead *can* fire live — meaning **replay and
live disagree**. On top of that, even the live 15-min tick only helps if the
spike starts ≥30 min into the flagged hour; the real GBP/AUD spike starts at
**:04 past the hour**, so a 30-min lead measured from the spike hour's top is
too late regardless — the widen really needs to arm on the **prior bar**.

---

## Concrete failing case (the trade that surfaced it)

GBP/AUD H&S short, H1, week of 2026-07-13. Entry #1 filled ~02:00 Brisbane
(16:00 UTC), stop-loss set. The learned spread-hour is 21:00 UTC (07:00 Bris) —
a single isolated hour. Minute-level data (both OANDA and TN) shows the spread
spikes from ~1× normal to ~8–9× starting at **21:04 UTC** and subsiding by
22:00 UTC.

- **Replay (TN mask `[21]`):** widen only arms on the 21:00 bar itself; the
  same bar stops the position out. `widened_stop_at` checks stop-reached
  BEFORE applying the widen → returns `None` → **−1R stop-out.**
- **Replay (OANDA mask `[20,21]`):** the (incorrectly) flagged hour 20 makes
  the widen fire a full bar early on the 20:00 bar → stop out of the way →
  survives → **+2.18R TP.** (OANDA got the "right" answer for the wrong reason;
  its `[20]` flag is a mask artifact — see "Related".)

So with a *correct* single-hour mask, replay always loses this trade. The
question this bug asks: **would the live worker also have lost it, or would its
15-min tick have pre-widened in time?** Right now we can't trust the replay to
answer that, because replay can't reproduce the live sub-hour tick.

---

## Root cause, in two parts

### Part A — the 30-min lead is structurally dead on a bar-boundary clock

`core/src/spread_blackout.rs`, `spread_hour_widen_for`:

```rust
let minutes_into_hour = now.minute() as i64;
let lead_reaches_next = 60 - minutes_into_hour <= SPREAD_HOUR_LEAD_MINUTES; // 30
if lead_reaches_next {
    let next = (hour + 1) % 24;
    if mask & (1 << next) != 0 { return Some(widen[next]); }
}
```

At a bar boundary `now.minute() == 0`, so `60 - 0 = 60`, which is **not**
`<= 30`. The look-ahead branch is unreachable at :00. It can only ever fire when
`now` is at :30–:59. This function is shared by the live cron and the replay, so
the *logic* is identical — but the two callers feed it different `now` values:

- **Live** (`worker/src/scheduler.rs::blackout_apply_loop` → `trade-control-cron`
  `widen_open_stops_for_spread_hours`) ticks on wall-clock every
  `upkeep_secs = 900` (15 min). So live sees `now` at, e.g., 20:45, 20:50 — the
  lead branch fires and the widen pre-arms. **Live is (partly) fine.**
- **Replay** (`cli/src/bin/replay_candles/replay.rs`) sets
  `now = candles[i].time + bar` — one evaluation per bar, at the bar's close
  (:00 of the next hour on H1). `now.minute()` is always 0. The lead branch is
  never reached. **Replay is broken.**

### Part B — even a 15-min live tick is too late for a :04 onset

The GBP/AUD spike starts at :04 past 21:00 UTC. A 30-min lead measured from the
top of the *spike* hour means "widen once we're within 30 min of 21:00", i.e.
from 20:30 onward. The live 15-min tick at ~20:45 would catch that and widen
before 21:04 — so **for this instrument live probably is protected.** But the
design is fragile: any spike that starts at :00–:29 of its hour (no lead room
inside the same hour) would still race the widen even live. The robust behaviour
is to arm the widen a **full bar early** (on the bar *before* the flagged hour),
not to rely on a sub-hour lead that assumes the spike starts late in the hour.

---

## The two things to fix

### Fix 1 (required): give the replay sub-hour evaluation for System 2

The replay already has a **sub-bar tick** mechanism for a different subsystem —
pause/news-window edges that fall mid-bar are replayed at their wall-clock epoch
via `evaluate_controls_only` (see the comment block at
`cli/src/bin/replay_candles/replay.rs:245`). System 2's widen needs the same
treatment: between bar `i-1`'s close and bar `i`'s close, the replay should
evaluate the widen at the sub-hour epochs the live 900-s cron would have hit
(or, minimally, at the `SPREAD_HOUR_LEAD_MINUTES`-before-the-next-flagged-hour
instant), so `spread_hour_widen_for` sees a `now` in the :30–:59 window and the
lead fires exactly as live.

The consumer to make sub-hour-aware is `widened_stop_at` in
`engine/src/simulator.rs` (~line 818). Today it walks `fill.rest` bar-by-bar and
for each bar computes `spread_hour_widen_frac(instrument, c.time)` at the bar's
**open** time. It needs to also consider the pre-hour lead instant — i.e. a bar
that is *itself* not a spread hour but whose successor is, and whose close is
within the lead window, should widen. Equivalent framing: **widen on the bar
immediately before a flagged hour**, matching what the live cron achieves via
its mid-bar tick. Keep the ON/OFF asymmetry and the stop-reached-before-widen
ordering intact for the non-lead case; the fix is to make the lead reachable,
not to change what a widen does.

### Fix 2 (recommended): arm a full bar early, not a sub-hour lead

Because the spike can start anywhere in its hour (GBP/AUD: :04), the reliable
rule is "if the *next* bar is a flagged spread hour, widen the open stop **now**
(this bar)." That's a full-bar lead, granularity-aware, and doesn't depend on
the spike starting >30 min into the hour. This subsumes Fix 1's effect for the
replay and hardens the live path too. If you take Fix 2, apply it in the shared
`core` seam (`spread_hour_widen_for` or a new full-bar variant) so **both** the
live cron and the replay inherit it — do not fix it only in the replayer (see
`[[strategy_changes_in_both_replayer_and_worker]]`).

Whichever fix: **the replay and live must end up identical.** The acceptance
test is that replaying this GBP/AUD plan against a *correct* single-hour mask
produces the SAME outcome the live worker would (either both widen-and-survive,
or both stop-out) — no split.

---

## Reproduction

```sh
# Throwaway minute-level probe that proved the spike timing (both brokers):
cd trade-control-web-hook/spread-baseline-gen
OANDA_TOKEN=... OANDA_ACCOUNT_ID=... cargo run --example gbpaud_zoom
# -> OANDA onset median 21:04 UTC over 25 days; TN agrees (n=1 day, TN M1
#    history is capped ~1000 bars).

# The diverging replays (staging CLIs):
#   OANDA plan -> +2.18R (survives, via the bogus [20] flag)
#   TN plan    -> -2.00R (stops out at 21:00)
# (the exact plans are the ones the user replayed on 2026-07-14; regenerate via
#  tv-arm-staging on the GBP/AUD chart if needed.)
```

## Key files

- `core/src/spread_blackout.rs` — `spread_hour_widen_for` (the dead lead),
  `SPREAD_HOUR_LEAD_MINUTES`, `spread_hour_widen_frac`, `is_spread_hour`,
  `mask_active_with_lead`. **Shared seam — fix here for replay==live.**
- `engine/src/simulator.rs` — `widened_stop_at` (~818): the replay's System-2
  reconstruction; walks bars at open time, stop-reached checked at :849 before
  widen at :884. This is where the replay needs sub-hour / full-bar-lead
  awareness.
- `cli/src/bin/replay_candles/replay.rs` — the driver loop; `now = candles[i].time
  + bar` (:243), and the existing **sub-bar control-tick** precedent (:245+)
  that Fix 1 should mirror for System 2.
- `trade-control-cron/src/blackout_apply.rs` — the LIVE System-2 widen
  (`widen_open_stops_for_spread_hours` / `widen_open_stops`, :115/:132). Ticks
  via `worker/src/scheduler.rs::blackout_apply_loop` (:281) every
  `upkeep_secs = 900`.

## Related, handled separately (do NOT fix here)

- **OANDA GBP_AUD mask over-flags hour 20.** `core/src/spread_baseline_candle.rs`
  row `("oanda","GBP_AUD", …, 3145728 = [20,21], …)`. Minute data shows 6am
  (20:00 UTC) is calm; the `[20]` bit is a close-sampled-H1 boundary artifact
  (the 20:55–20:59 ramp bleeding into the hour-20 p90 bucket). TN's `[21]` is
  correct. This is being fixed in a separate change (regenerate/patch the mask).
  It is *why* OANDA incidentally survived — but it is not the widen-lead bug and
  fixing it will make OANDA *also* stop out, which is exactly what makes Fix 1/2
  necessary.

## Do-not-regress notes

- Keep the System-2 ON/OFF asymmetry (ON = baked clock only; OFF = live-spread
  recovered / baked-hour ended / backstop) — see `core/src/pending_lifecycle.rs`
  and `[[spread_blackout_backstop_split_ttl_vs_safety]]`.
- Any rule change lands in BOTH the replayer and the worker
  (`[[strategy_changes_in_both_replayer_and_worker]]`). The whole point of this
  bug is that they currently diverge.
- Don't reintroduce a per-record TTL; the backstop is a 12h safety ceiling only.
