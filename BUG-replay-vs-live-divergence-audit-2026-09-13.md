# Replay ↔ live divergence audit — 2026-09-13

Three parallel read-only audits over (a) the dispatch loop, (b) engine
timing/clocks/windows, (c) resting-order & position lifecycle, looking for
places replay mode behaves differently from the real worker.

**The recurring root cause:** replay reuses the shared *decision* predicates
but skips or reimplements the *state effects* — store writes, broker cancels,
stop amends. The "strategy changes in both replayer and worker" convention is
honoured for decisions and quietly dropped for effects.

Already-documented gaps (corpus blind to the breakeven cron, timing-sensitive
gates, equity/FX sizing, the `--start` golden-count bug) are excluded unless a
new angle was found.

---

## Tier 1 — change fixture outcomes at default flags

### 1. Replay's veto/invalidate arms are hand-rolled and drop state effects

`cli/src/bin/replay_candles/replay.rs:565-583` vs live
`trade-control-cron/src/engine.rs:948-1007` → `core/src/dispatch/veto.rs` /
`invalidate.rs`.

- **No store write.** Live routes every fired `Action::Veto` through
  `handle_veto` / `run_veto_with_broker`, both calling `store.set_veto`.
  Replay only touches the broker. So `run_enter`'s `is_vetoed` gate
  (`core/src/dispatch/enter.rs:235-259`) is **permanently blind offline** —
  same class as the already-fixed missing-prep replay hole, still open for
  vetos. Partially masked by the engine's `entries_blocked`/`Phase::Done`
  latches, which is why it hasn't surfaced; any veto whose enter-blocking
  effect isn't mirrored by a latch diverges.
- **`CancelPending`-level vetos do nothing.** Replay's arm is gated on
  `level == Some(VetoLevel::ClosePositions)`; everything else falls to
  `_ => {}`. Live cancels pending orders for **every** veto level and only
  level-gates the flatten. A `CancelPending` veto (trade-expiry / M/W-abort
  class) leaves a resting order alive offline that can fill later and book R
  the live account never took.
- **`Action::Invalidate` is a total no-op offline.** Invalidate intents
  generally carry no level, so they drop through `_ => {}`: no
  `store.set_cooldown`, no cancel, no control event. Post-invalidation
  entries that live rejects (cooldown gate) are taken in replay.
- Also dropped: `clears:` handling (`clear_named_vetos`) and control-event
  records, so journal/timeline comparisons for vetos are structurally
  incomparable.

**Fix direction:** route replay fires through the shared
`run_veto_with_broker` / `run_invalidate` instead of hand-rolling.

### 2. `sweep_pending_orders` has no side-effecting replay counterpart

Live: `trade-control-cron/src/sweep.rs:62-175` on the upkeep loop —
**actually cancels and deletes** resting attempts on expiry, bar-expiry, and
SL-breach (market-hours is now a `HoldReason::MarketHours` hold).

Replay: `fill_sim.rs::sweep_reason` (~:828) is explicitly pure and is called
only from `report.rs:986` to label a `NEVER FILLED` line. The fill path
enforces `expiry_bars` independently (`fill_sim.rs:672-677`) but has **no
SL-breach cancel** — inconsistent within replay's own sweep emulation.

Consequences, in both directions:
- An order whose SL is overtaken pre-fill is cancelled live and can never
  fill; replay leaves it resting — if price returns through the trigger it
  fills and books a trade production structurally could not take.
- The order stays visible to `list_pending_orders` and the retry gate's
  prior-attempt resolution for the rest of the fixture, so a multi-shot
  re-entry live permits (slot freed by the sweep) can be rejected offline.
- The report prints "swept" over an order that nonetheless booked P&L —
  state-vs-cosmetics split that silently biases the golden corpus.

### 3. NY-close-edge spread-blackout marker sampled once per bar

Live: `worker/src/scheduler.rs:294-299` runs `apply_if_ny_close_edge` on the
900 s upkeep loop; `is_ny_close_edge` (`core/src/ny_clock.rs:40-47`) matches
the whole close hour, so live gets ~4 chances per hour.

Replay: `replay.rs:339-348` evaluates it exactly once per bar at
`now = bar close`. On **H4/D1** that instant can step straight over the close
hour, so `set_spread_blackout_window` is never called and `run_enter`'s
spread-blackout gate **fails open offline** — replay takes entries live would
reject. On **M15** the opposite: the 3 h TTL is refreshed up to 4×/hour,
extending it past where live would let it lapse. Granularity-dependent,
present at default settings; not the documented `--start` bug.

### 4. The re-price pass is absent from replay

Live: `order_control_tick.rs:156-166` `run_both` = `promote_due_orders`
**then** `reprice_pass` (:228-263), which cancel-and-replaces resting orders
at a new stake/stop from `SpreadInputs { measured, expected_this_hour,
expected_next_hour }`.

Replay: `replay.rs:699-707` runs only `promote_due_orders`; no
`reprice_pending_order` call exists under `cli/`.

Why it's the most consequential lifecycle gap: slice 7
(`core/src/pending_lifecycle.rs:958-970`) deliberately retired the
`SpreadHour` ON-side hold in favour of the forward-looking SL floor —
delivered *exclusively* by the re-price pass. Replay therefore has **neither**
the old hold **nor** its replacement: a resting order sits offline at its
original stop through a forecast spread spike while live re-prices it wider
or parks it. Entry price, stake, and R all diverge. The comment at
`replay.rs:684-688` claims parity with `order_control_tick` that is only half
true.

### 5. Replay widens break-even stops that live refuses to widen

Live: `blackout_apply.rs:338` guards System-2 widening with
`!may_widen(direction, original_sl, be.entry_price)` — never widen a stop
already at break-even (would re-expose banked risk).

Replay: `fill_sim.rs::widen_episodes_at_resolved` (~1150-1247) has **no
`may_widen` call**; episodes gate only on the spread mask + reading. Via
`stop_on_bar` at `fill_sim.rs:445` the widened stop **overrides
`active_stop`** — which is the BE stop once `close_arms` latches
(`fill_sim.rs:493-496`). So a trade that armed BE and then crossed a spread
hour is scored offline against the widened stop while live holds at entry.
This is a sign-flip on the exit, not rounding.

---

## Tier 2 — real divergences, rarer or blocked on Tier-1 plumbing

### 6. Replay's Close arm discards `CloseOutcome` — **CLOSED, won't-fix (2026-09-14)**

⚠️ **Two of this finding's three claims were WRONG.** Corrected on re-reading
the engine, before any code was written:

- ❌ **"Band-anchor fail-closed path not reproduced."** False. The engine
  applies `close_windows_pass` (`engine/src/evaluate.rs:1286,1336`) **before it
  ever emits a Close fire**, and that function is a deliberate mirror of
  `run_close`'s contextual gate — same news test, same band test, same OR
  composition, reading the same detector-computed `band_anchor`. A Close that
  reaches replay's arm has already passed the band test. `run_close`'s copy is
  the *second* of two guards; replay has the first.
- ❌ **"Coupled gap: news windows must be seeded first."** False, and this was
  repeated several times as a blocker. The engine gates on its in-memory
  `state.open_news_windows` (which replay *does* maintain, `replay.rs:1207`),
  not on the store. The store copy `run_close` reads is a second path to the
  same answer, not the only one. No seeding work is required.
- ✅ **`CloseOutcome` is thrown away.** Real: `replay.rs:559-561` ignores the
  return value.

**Won't-fix, by operator decision.** What remains is not a rule difference —
it is *"live's broker can fail, replay's cannot."* The only way to model it is
to inject random failures into the replay broker, which would make every
fixture non-deterministic and destroy the corpus's value as a regression net.
This is the audit's one genuine **resolution** difference (per the rule-vs-
resolution distinction): document it, don't fix it.

Consequence to remember: **the corpus has no opinion about broker failure.**
A fixture proves nothing about retry behaviour, and the live-side re-fire
(engine tick, `engine_secs = 15` on both workers — a wall-clock timer, NOT a
candle, so an H4 close retries in 15s not 4h) is covered by unit tests only.

⚠️ The live re-fire is a **re-evaluation, not a retry**: the tick re-runs
`evaluate_plan` from scratch with no memory of the failed attempt, so the close
only fires again if the whole gate chain still passes on that tick (signal still
latched, news window still open, price still in band). Usually true seconds
later on a long bar; not guaranteed near a bar boundary. There is no bounded
retry anywhere — **no broker call in this repo retries today.** Follow-up
(separate from this audit): phone alerts on sustained broker errors — see the
`[[want-phone-alerts-on-broker-trouble]]` memory. Sizing it wants a
`request_records` query for the real `Errored`-vs-`Closed` rate, never run.

### 7. `AttemptState::Unknown` is unreachable offline — **CLOSED, won't-fix (2026-09-14)**

Live's retry gate (`retry_gate.rs:380-402`) rejects with 412 when a prior
attempt resolves `Unknown` — the fail-safe from the TradeNation stacking
incident, where a drifted order id resolved `Unknown` and the gate stacked a
duplicate entry. `replay_broker.rs`'s `lookup_attempt_state` is a total
function over in-memory state: it never returns `Unknown` and never errors.

**Same category as #6, same verdict.** `Unknown` means *"the broker could not
tell us what happened to this order"* — **broker ambiguity, which has no
offline analogue.** The replay broker holds every order in memory, so it always
knows; making it answer `Unknown` means fabricating confusion. That is a
**resolution** difference, not a rule difference.

⚠️ **Do not "fix" this by injecting `Unknown` into the replay broker.** Like
#6's random-failure injection, it buys nothing and costs fixture determinism.

Consequence to remember, and it is the same sentence as #6's:
**the corpus has no opinion about broker ambiguity.** The `Unknown` arm and the
`LookupError::Transient` arm beside it are covered by unit tests only — no
fixture will ever exercise either. The `Err(LookupError::Transient)` → 503 arm
is unreachable offline for exactly the same reason.

Note the audit's original wording ("phantom multi-shot re-entries in the corpus
that production would refuse") **overstated it**: production refuses those
re-entries only in the narrow case where the broker has gone ambiguous, not in
normal operation. Ordinary multi-shot re-entry decisions run through the *same*
shared `retry_gate::evaluate` on both sides and do agree.

### 8. Break-even noise floor (`InsideNoise`) not modelled

`breakeven_watch.rs:241-256` refuses the amend when the BE target is within
`BREAKEVEN_MIN_ATR_FRACTION` × ATR of the last close; replay arms purely on
`close_arms` (`fill_sim.rs:493-496`). Tight geometries: replay scratches 0R
where live runs to TP or takes the full −1R (both directions possible).

### 9. `amend_stop` silently no-ops; positions report `stop_loss: None`

`replay_broker.rs:1266-1273` returns `Ok(())` mutating nothing;
`replay_broker.rs:1250-1251` synthesizes every `OpenPosition` with
`stop_loss: None`, `take_profit: None`. So the live crons aren't just unwired
offline — if wired, `breakeven_watch.rs:197` and the widen would silently
early-return. Silent no-op is the dangerous failure mode; minimum fix:
record amends (as `place_entry` already does via `PlacedLevels`) so the fill
sim can read them back. Related: `get_candles` returns `Ok(vec![])`
unconditionally (`replay_broker.rs:1296-1305`) — another silent no-op.

### 10. In-spread-hour quote clamp defeats early release — **ACCEPTED + DOCUMENTED (2026-09-15)**

**Verdict: a real divergence, deliberately left in place.** Measured, scoped,
and NOT fixed — the obvious fix does not work (below) and the fix that would
work carries an unmeasured P&L change. Revisit only with the evidence named at
the end.

#### What it is

`ReplayBroker::get_quote` (`cli/src/bin/replay_candles/replay_broker.rs`)
discards the bar's real bid/ask whenever `is_spread_hour` is true and returns a
synthetic quote pinned at `elevated_threshold_pips(instrument)`
(= `baseline_median_pips × SPREAD_REJECT_MULTIPLE (5.0)`).

Live lifts a resting-order hold on **either** (a) the baked hour ending, or
(b) the real spread recovering to `<= SPREAD_BLACKOUT_RECOVERED_PIPS (4.0)`.
**Replay can only ever take (a)** — for EUR/USD the clamp reports 8.0p, exactly
2× the recovery threshold, so (b) is unreachable by construction.

⚠️ **`is_spread_hour` itself does NOT diverge.** It is a pure fn over a baked
table; both engines always agree on *when* a spread hour is on. The divergence
is ONLY the early release.

#### Scope — measured, not assumed (`EVIDENCE-spread-hour-trough-duration.md`)

14 real days per instrument, OANDA M1, Sept 2026:

| instrument | recovers before the hour ends | effect |
|---|---|---|
| **EUR/USD** | **9 of 14 days (64%)** | replay holds an order live would restore + fill |
| GBP/AUD | 0 of 14 | none — spike fills the hour |
| AUD/CHF | 0 of 13 | none |

So it bites **tight-spread instruments only**. Direction:
**replay is systematically MORE CONSERVATIVE than live inside spread hours** —
fixtures under-report trades production takes. Pessimistic, not optimistic.
⚠️ Confidence HIGH on those three, LOW on the other ~90 masked rows (unsampled).

#### Why the clamp exists (do not remove it)

Replay gets **one spread sample per bar**; live samples every 15s
(`engine_secs`). A bar that prints narrow by luck mid-trough would read as
recovery, restore the order, and the next in-block bar would re-cancel it —
**cancel/restore ping-pong**.

⚠️ **The anti-ping-pong property is CONSTANCY, not magnitude.** An earlier
reading of this (mine) claimed the clamp is safe because it always exceeds
4.0p. **False** — 9 of 109 masked hours already clamp below 4.0p (TradeNation
`AUD/USD` 2.00p, `EUR/USD` 2.50p, `USD/JPY` 3.00p). It cannot oscillate because
it is **constant within the hour**, whatever its value. Any replacement must
preserve constancy-within-the-hour; magnitude is not the safety property.

#### ❌ The obvious fix does NOT work — don't retry it

Using the baked table's 10th column `hour_p90_frac` (the "ungated per-hour
spread forecast") instead of `median × 5` was scoped, prototyped, and
**rejected**:

- `hour_p90_frac` is a **PEAK** statistic — the p90 of within-hour minute
  spreads, generated to size a stop widen ("the spike magnitude",
  `spread-baseline-gen/compute.rs`). Live's early release is driven by the
  **typical late-hour minute** (~2-4p on EUR/USD). It is the wrong statistic.
- It moves EUR/USD from 8.00p to **6.85p**. Recovery needs `<= 4.0p`. A 14%
  change to a number that had to halve. EUR/USD would need `mid <= 0.629` to
  read recovered — a price it has never traded at.
- **Worse, it silently flips the ENTRY GATE.** `spread_blackout_decision` is
  strictly `spread_pips > threshold_pips`, and the clamp reports *exactly*
  `elevated_threshold_pips` — so `8.0 > 8.0` is false and **the entry gate
  never rejects under the clamp today**, despite `get_quote`'s own comment
  claiming the opposite. Since p90 generally exceeds median×5, the swap flips
  **99 of 109 masked instrument-hours (91%) from ALLOW to REJECT** — a
  corpus-wide entry-suppression change riding in on a spread-reporting fix.

#### ✅ The SL-floor concern does NOT arise (checked)

Feared: the fabricated spread feeding `run_enter`'s SL-vs-spread floor would
make every in-spread-hour entry size its **stop** off a fiction. It does not.
The floor prefers `windowed_entry_spread` → `get_bidask_candles`, served in
replay from the **real unclamped** bid/ask series; `get_quote` is only the
fallback when `enter_granularity == None`, and replay always passes
`Some(granularity)` (`replay.rs:1076`). **No corpus entry sizes its stop off
the clamp.**

#### Blast radius — the three real consumers

1. **Hold release** (`pending_lifecycle`) — the divergence proper.
2. **Entry gate** (`spread_blackout_decision`) — inert today via the `>`
   boundary above. Any change to the clamp's VALUE wakes this up; treat that as
   a behaviour change needing its own measurement, not a side effect.
3. **Promote / re-price** (`order_control/tick.rs:138`,
   `order_control/reprice_pass.rs:93`) — `measured` feeds `sl_target`, so a
   parked order's stop target keys on a synthetic constant offline and a real
   tick live. The original audit called this out and it is **confirmed real**;
   note `sl_target` takes `max` over the measured *and* baked-forecast terms,
   so the clamp is not always the deciding input.

#### If revisited — the shape that would work

**Decouple the consumers**: report the **real bar close-spread** to the entry
gate / promote path, keep the **OFF-side (hold release) on the baked clock**.
Ping-pong becomes impossible *by construction* (the oscillating value no longer
decides anything) while the gate gets real data. Alternative: bake a
within-hour **decay/duration** column — needs a `spread-baseline-gen`
regenerate.

**Do not ship either without first measuring the entry-gate P&L effect**, since
consumer 2 goes from never-rejecting to really-rejecting. That measurement is
the missing evidence, and it is the reason this is accepted rather than fixed.

### 11. Spread-hour gates disagree on H4+ — **REFUTED (2026-09-15)**

**The premise is true; the conclusion is false. There is no bug.** Settled by
a test, not by argument — `a_cancelled_order_never_fills_even_on_an_unsuppressed_h4_spread_hour_bar`
(`cli/src/bin/replay_candles/replay_broker.rs`).

**The premise, measured** at the exact instant the test uses (EUR/USD,
2026-06-15T21:00Z, `bar_seconds = 14400`):

| gate | value | question it answers |
|---|---|---|
| `suppress_on_spread_hour_bar_seconds` | **false** | "is this candle's OHLC rubbish?" |
| `is_spread_hour` | **true** | "should a resting order be pulled?" |

**Why that is CORRECT, not a disagreement.** They answer different questions
and both answers are right:

- Suppression **must** be bar-gated (`SPREAD_HOUR_SUPPRESSION_MAX_BAR_SECONDS
  = 3600`): one bad hour inside a four-hour bar is diluted by three hours of
  genuine trading, so discarding the bar would throw away real data.
- The hold **must not** be bar-gated: a four-hour bar does not stop a fill at
  21:15, so the order comes off the broker regardless of the chart timeframe.

**Why there is no race.** The original finding said "which wins depends on the
dispatch/lifecycle interleave". It does not: `ReplayBroker::advance` skips
`order.cancelled` with a `continue` **before** it ever consults the fill logic,
so a cancelled order fills nothing whatever the simulator thinks of the bar.
No interleave opens a position from a pulled order. (The finding — and a later
retelling of it — inferred a race from two functions returning different
booleans. That is not the same as finding one.)

⚠️ **Related question, already answered:** *"can we drop suppression for H4?"*
— it is **already off there.** The constant's own doc says "suppress on 15m +
1h, allow on 4h + D". On H4 the system does not discard the bar; **System 2
widens the open position's stop** through the spike and restores it after.

**What the test pins is one line.** Delete the `continue` in `advance()` and an
order the lifecycle deliberately pulled fills anyway — a position the live
worker never takes, booked into the corpus as real R. Mutation-verified RED,
along with a premise assertion that fails loudly if a mask regen ever moves the
spread hour out from under the test (rather than passing vacuously against an
ordinary bar), and a control arm proving the *uncancelled* order does fill on
that same bar.

---

## Tier 3 — noted, lower priority

- **Detector window right edge:** live fetches `since → wall-clock now`
  (mid-bar, `engine.rs:779-808`); replay slices `..=i` (`replay.rs:287`).
  Off-by-one in the as-of bar can shift which bar the confirmed-signal scan
  latches. `confirmed_scan_floor`'s mitigation is depth-based and doesn't
  cover a differing right edge.
- **Replay `DispatchConfig` hardcoded** (`replay.rs:922-931`): risk 1.0%,
  cap 3, `caps: Default::default()`, `tick_size: None`. Per-account
  `meta.caps` narrowing invisible offline — a cap-driven live rejection
  replays as a fill.
- **No seen-index writes in replay** (`is_seen`/`mark_seen`/
  `record_dispatcher_outcome` absent). Mostly masked by the engine's
  `state.fired` latch, but bugs in the seen-index layer are unreproducible
  offline.
- **Sub-bar control ticks route only Pause/Resume** (`replay.rs:1060-1078`,
  `gate_outcome` hardcoded `NotAnEnter`); live's `tick_controls_only`
  dispatches through the real handlers.
- **Lifecycle scoping:** replay passes `account: None` +
  `PromoteScope::Every`; live scopes per-account. Benign single-broker,
  but account-scoping bugs are structurally invisible offline. Also the
  `ClearPolicy::ClearRecord` (replay) vs `LeaveForCaller` (live) split means
  the "restore both, clear once" contract is only half-exercised offline.
- **`expires_at` clocks:** live derives from `Utc::now()+1d`, replay from the
  candle series; the `MemStateStore` clock pin covers the store but not any
  expiry logic outside it.

## Verified clean (don't re-audit)

- Spread-hour **suppression** itself is fully shared and clock-free (all
  engine sites pass bar open; `fill_sim` uses the bar-seconds twin).
- `set_as_of(bar_open)` discipline is correct and documented.
- `filter_new_candles` exclusivity matches replay's `lo = i + 1` advance —
  no double/skipped bar at the watermark.
- Live `now` threading is disciplined — no raw `Utc::now()` on the live
  engine path; only the scheduler loop heads read wall-clock.
- The hold/release refcount (`Holders`, one-derivation `hold_reasons`) is
  genuinely shared and correct in replay.
- `BUG-spread-hour-widen-no-subhour-lead.md` is genuinely fixed
  (`spread_hour_widen_instant` is consumed by `fill_sim.rs:1198`).

## Suggested order of attack

1. Empirically confirm #2 (SL-breach order later filling) and #3 (H4/D1
   fixture spanning the NY close hour) — both were traced statically.
2. Fix #1 by delegating to the shared handlers (also fixes the store-write
   half for free). Small, high value.
3. Fix #2 with a real sweep driver sharing `core::sweep_gate` and actually
   cancelling in `ReplayBroker`; keep `sweep_reason` for the report.
4. Fix #3 by evaluating `is_ny_close_edge` over each bar's span, not its
   close instant.
5. Fix #5 and #8 (shared-predicate imports into `fill_sim`).
6. #4, #6 (+news seeding), #9 are bigger plumbing — replay broker must track
   stops / run reprice; scope separately.

After each fix: full corpus run with `--fixtures-dir`, explain every moved
cell before re-blessing (split tests where coverage would retire).
