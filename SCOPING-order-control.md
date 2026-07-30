# SCOPING — `order_control`: one home for stored / pending / live order state

**Status:** design, awaiting review. No code written.
**Date:** 2026-07-31

---

## 1. The problem, in one fixture

`replay-fixtures/sgdjpy-spread-floor-min-r-block` is the whole motivation:

> three `05-enter` fires (13:30, 14:30, next-day 06:15), **each independently
> rejected** by `sl-widen-below-min-r`, nothing remembered between them, plan dead
> at trade-expiry. `net_r: 0.0`, `legs: []`.

The spread was wide at 13:30. The setup was **thrown away**, not parked. And that
isn't a considered policy — it's that a rejection leaves *no trace*:
`EntryAttempt.broker_order_id` is non-`Option`, written only inside the
`Ok(order_id)` arm (`core/src/dispatch/enter.rs:878`). **There is no schema slot for
an intended-but-unplaced order.** The only "retry" is that the seen-id isn't
poisoned, so an identical signal on a later candle re-runs the whole chain from
scratch.

Meanwhile a second class of loss: an order placed *during* volatility carries a
widened stop (bigger risk, smaller R) and **keeps it forever**, even after the
market calms. We already pre-widen before a known spread hour; we never give it back.

---

## 2. Vocabulary (the operator's, adopted)

An order is in exactly one of three states:

| state | where it lives | at risk? |
|---|---|---|
| **Stored** | our DB only. Never sent to the broker. | no |
| **Pending** | placed with the broker, not yet triggered. | no |
| **Live** | filled; it is a position. | **yes** |

This vocabulary is load-bearing for the module tree (§6) — the three states have
different mutation surfaces and different safety rules, and that is the primary
axis everything else hangs off.

---

## 3. What exists today (and what's wrong with it)

Nine systems touch stops, orders, or entry gating. Full inventory in §10; the four
that matter most here:

| | system | mutates | trigger |
|---|---|---|---|
| **A** | `intent/sl_spread_floor.rs` — 10× spread entry floor | stored geometry, pre-place | at placement |
| **B** | `blackout_widen.rs` + `cron/blackout_apply.rs` — spread-hour widen | **live** position SL | 900s loop, gated on baked hour |
| **C** | `pending_lifecycle.rs` + `hold.rs` — hold refcount | **pending** orders | 900s loop |
| **F** | `cron/breakeven_watch.rs` — break-even | **live** position SL | 900s loop |

Confirmed defects this design resolves:

1. **B can silently undo F.** Both amend the same broker handle on the same 900s
   cadence with zero coordination. B remembers `original_stops` and restores it
   **verbatim**; if F moved the stop to break-even in between, B's restore reverts
   it. B's idempotency check is on `record.applied`, not on whether the stop moved
   underneath it.
2. **`join_position_to_attempt` is duplicated byte-for-byte** —
   `blackout_apply.rs:444` and `breakeven_watch.rs:279` — including the same
   documented aliasing bug in its coarse fallback.
3. **Three independent resting-order cancel paths** — C's `cancel_pass`, the sweep's
   four reasons, and the retry gate's cancel-and-replace — despite
   `pending_lifecycle.rs:41-46` asserting it is the only one.
4. **A and B are two unrelated "widen" implementations**: A in price units off a
   5-bar windowed mean, B in pips off a baked p90 with a 22–40 clamp. Different
   constants, different sampling, no shared notion of "what should this stop be".
5. **Four 900s loops each re-fetch the same accounts / positions / orders** — ~4×
   the broker round-trips of one consolidated pass.

---

## 4. The rules (operator's, with the amendments agreed in discussion)

1. **Store before widening.** When an order is placed, record it with its
   **original desired SL** alongside the widened one.
2. **Sub-1R ⇒ stay stored, don't drop.** If spread widening would push the trade
   under `min_r`, keep it **Stored** and log — instead of today's terminal 422.
3. **A new order supersedes all Stored *and* Pending orders** for that trade. A
   fresher signal is more current, therefore superior — better *or* worse R is an
   empirical question, not a reason to prefer the stale one.
   ⚠️ Must not burn a `max_retries` slot: a supersede is a *replacement*, not a new
   attempt. Reuse the retry gate's existing cancel-and-replace semantics
   (`retry_gate.rs:229`).
4. **Stop↔limit flip on re-place.** If price has moved past a stop's trigger,
   re-place as a limit (and vice versa); market stays market.
   *Correction to the original framing:* this is **not** only a spread/news
   concern. `place_entry_too_close_fallback` (`enter.rs:1085`) fires on the
   broker's "#19-10 too close to market" reject, which happens any time price
   drifts between signal and placement. So the flip belongs to the **shared
   re-place path**, not to the hold machinery. Both callers then get it.
   *Open:* the flip is keyed off `recover_entry`, which is opt-in per intent. A
   stored order re-placing with no `recover_entry` set won't flip. Decide whether
   stored re-placement forces a default.
5. **Stored orders live until 3 bars before expiry**, then are dropped with a log
   line. (45m on 15m, 3h on H1.) Uses the existing `cancel_at` / `not_after` clocks.
6. **Shrink a widened stop back — but only when in profit.** Applies to Stored,
   Pending and Live.
   **"In profit" = price beyond entry by more than the current spread**, so the
   position is green net of the round-trip, not merely green on mid. This filter is
   what makes shrinking a *live* stop safe: without it, tightening converts an
   unrealised loss into a realised one at a level you never chose.
7. **Re-evaluate every candle**: expected-spread floor and the ≥1R test.
   - `sl <= floor` ⇒ widen
   - `sl > floor` but `> original` ⇒ shrink toward original (never past it)
   - result <1R and not yet placed ⇒ **demote to Stored** (cancel at broker, keep in DB)
   - not yet placed and ≥1R ⇒ read price, flip stop/limit as needed, place
   - **Stored/Pending re-size on the new SL; Live never re-sizes** (that would mean
     a partial close — a real fill with real cost).

### 4b. Pre-widen at **every** candle, not just known spike hours

The data already exists and is thrown away. `spread-baseline-gen` computes
`hour_p90_frac[24]` for **every** hour of every instrument
(`compute.rs:363-372`), then `compute.rs:441-451` writes `widen[h]` **only inside
the elevated-hour gate**, so the committed table is zero at 23 of 24 positions.

Un-gating is ~3 lines in `render.rs:78-84` plus a re-bake.

⚠️ **The trap:** `spread_hour_widen_frac()` returns `Option<f64>` where `None`
*means* "not a spread hour" — the presence of the value **is** the gate. Un-gating
conflates two questions that must stay separate:

- *Is this an elevated hour?* → the **mask** (drives suppression + the 30-min lead)
- *What spread do I expect here?* → the **array** (drives pre-widening)

Split them, or every `is_spread_hour` consumer starts firing 24/7.

Note the flag statistic is p75 (`FLAG_PERCENTILE`) and the widen is p90
(`WIDEN_PERCENTILE`) — different percentiles of the same buckets, so un-gating p90
changes no flagging behaviour. Per-broker-symbol keying stays (LOCKED: OANDA
EUR/USD 21:00 peak 1.81× vs TN 5.58×).

### 4c. What happens to `is_spread_hour`

**It leaves order control entirely, and survives only as a signal-quality gate.**

Two distinct jobs are hiding behind one predicate:

- **Order control** (hold / cancel / widen). Here `is_spread_hour` is a *baked-clock
  proxy* for "the spread is bad now". The ≥1R test measures the actual thing.
  A calm 21:00Z bar is currently held for nothing; a blown 09:00Z spread currently
  sails through. **Delete the proxy, keep the measurement.** `HoldReason::SpreadHour`
  goes away.
- **Rubbish-candle suppression** — `suppress_on_spread_hour` at
  `engine/src/evaluate.rs:836,619,1589,1867`, engine-v2, replay. This says the bar's
  **OHLC is untrustworthy as a signal** ("dominated by the spread, not a real market
  move"), so a break-and-close or retest read off it is a lie. That is a different
  stage from sizing — it stops the signal being *read*, where 1R runs after a signal
  has already fired. **The 1R filter cannot replace this. Keep it.**

⚠️ **Deliberate trade-off to accept explicitly:** dropping the `SpreadHour` hold
also drops the **30-min pre-emptive lead** (`SPREAD_HOUR_LEAD_MINUTES`), which
exists so we are flat *before* the spread blows out — reacting after means reacting
at a bad price. §4b's pre-widening covers this (the stop is already sized for the
expected spread when the hour arrives), but it is a real behaviour change, not a
free simplification.

Also retired by this: the `mask_active_with_lead` / `spread_hour_widen_for`
structural twins (`spread_blackout.rs:466` / `:327`), kept in sync by hand today.

---

## 5. Placement: `core`, not `engine`, not a new crate

`order_control` is **shared code hitting the `Broker` + `StateStore` traits**, so
replay and live get it from one implementation — the same mechanism that made v120
and v121 replay-correct with zero replay-specific code (two golden cells moved on
their own).

- **Not `engine`** — the engine is pure (`[[engine_is_pure_broker_trait_only]]`):
  plans + candles in, fires out. No `StateStore`, and it shouldn't get one. Order
  control is inherently effectful.
- **Not a new crate** — it needs `Broker`, `StateStore`, `Intent`, `Resolved`,
  `Holders`, all in `core`; and `core::dispatch::enter` must call *into* it for the
  stored path. That's a dependency cycle, resolvable only by splitting `core` or
  threading generics. Not worth it for ~6 files.
- **`core`** already is exactly this shape — `pending_lifecycle`, `retry_gate`,
  `dispatch` all live there, all generic over the two traits, all backend-free.

---

## 6. Module tree

Split by **what is being mutated** (which determines the plumbing), with widen and
shrink as *directions within* each — not as the top-level split.

```
core/src/order_control.rs          # mod decls + re-exports (no mod.rs, per convention)
core/src/order_control/
  hold.rs        # MOVED from core/src/hold.rs. HoldReason loses SpreadHour,
                 # gains MarketHours. Refcount semantics unchanged.
  sl_target.rs   # PURE. (expected_spread, original_sl, entry, tp, price, min_r)
                 #   -> SlTarget { desired_sl, r, action: Widen|Shrink|Hold|BelowMinR }
                 # No I/O. The ONE place widen-vs-shrink is decided.
  join.rs        # the de-duplicated position <-> EntryAttempt join
  stored.rs      # NEW state: intended-but-unplaced. Promote to Pending when
                 # sl_target says >=1R; drop 3 bars before expiry.
  pending.rs     # cancel / restore / re-place, stop<->limit flip, SL adjust + resize
  live.rs        # open-position SL only: widen, shrink-when-in-profit, break-even.
                 # Absorbs System B + System F so they can no longer fight.
```

**Why not the originally-proposed `widen/{stored,pending,live}` +
`shrink/{stored,pending,live}`:** it duplicates the *plumbing* (position joining,
broker amend, record I/O) across six leaves, when the thing that actually differs
between widen and shrink is **a sign**. Widen and shrink are the same question —
*"what should this SL be right now?"* — so they belong in one pure function with
three call sites. Splitting them first is precisely how A and B drifted into two
incompatible widen implementations (§3.4), and how B ended up able to clobber F
(§3.1).

`sl_target.rs` being **pure** is the load-bearing part: it makes the decision
unit-testable without a broker, and mutation-testable (a flipped sign must turn a
test red — green refcount/SL tests prove very little on their own).

---

## 7. Schema

`HeldTradeRecord` (renamed from `SpreadBlackoutRecord` in v122) already carries
`holders`, `original_stops`, `cancelled_orders`. Two additions:

1. **`StoredOrder`** — the net-new state. Needs the signed body (to re-drive
   through `run_enter`), the **original desired SL**, the resolved geometry, and
   the expiry clock. Naturally a `Vec<StoredOrder>` on `HeldTradeRecord`, or its own
   `stored_order` table if we want to query it independently.
   *Recommendation:* on the record — it's per-trade, TTL'd the same way, and
   `jsonb` means **no SQL migration** (`#[serde(default)]`, as with `holders`).
2. **`EntryAttempt.original_stop_loss`** — so Pending/Live know what to shrink
   *toward*. Today only System B remembers an original, and only in
   `original_stops` on the record, only while a spread-hour hold is active.

`applied` stays distinct from `holders` (v120 note): it means "this record mutated
something at the broker" and System B's idempotency guard reads it.

---

## 8. Slices

Each ships green, with its own tests and fixture check.

| # | slice | resolves | risk |
|---|---|---|---|
| **1** | **Stored orders** — rules 1, 2, 4, 5, 7-partial, + supersede | the sgdjpy 0R loss | low: net-new; existing paths untouched |
| **2** | **Un-gate hourly spread** — split mask from forecast, re-bake | rule 4b | low: data + one `Option` split |
| **3** | **`sl_target` + `live.rs`** — merge B and F | §3.1 B-undoes-F, §3.2 dup join, §3.4 two widens | **high: live money path** |
| **4** | **Pending SL adjust + resize** — rules 6, 7 for Pending | — | medium |
| **5** | **`HoldReason::MarketHours`** — fold the sweep's cancel in | §3.3 third cancel path | medium |
| **6** | **Retire `is_spread_hour` from order control** | §4c | medium: behaviour change (lead) |

Slice 3 is where the existing bug lives, so there's an argument for doing it first.
Recommendation is still 1-first: it's additive, it pays for itself immediately, and
it builds the vocabulary the later slices are expressed in.

---

## 9. Testing

- `sl_target.rs` pure-unit + **mutation-verified** (flip the widen/shrink sign, drop
  the in-profit filter, drop the never-past-original clamp — each must turn a test
  red). Per `[[verify_new_analysis_code_by_mutation]]`.
- **`sgdjpy-spread-floor-min-r-block` is the acceptance test for slice 1.** Today it
  asserts `net_r: 0.0, legs: []`. After slice 1 the first fire should park as Stored
  and place on a later candle when the spread calms — a **deliberate golden change**,
  re-blessed with the reason in `meta.json`. If that fixture does *not* move, slice 1
  didn't work.
- Every slice: full replay-fixture sweep. Watch for the known trap that the fixture
  test **panics on the first mismatch** and hides the rest
  (`[[fixture_check_hides_nine_of_ten_failures]]`) — collect all divergences before
  re-blessing.
- Slice 3 needs an explicit **B-then-F interleaving** test: widen, break-even, then
  restore — the break-even must survive.

---

## 10. Full system inventory (for the consolidation)

| # | system | module | mutates | scope |
|---|---|---|---|---|
| A | entry SL 10× floor | `core/src/intent/sl_spread_floor.rs` | stored geometry | per-entry |
| B | spread-hour widen | `core/src/blackout_widen.rs`, `cron/blackout_apply.rs` | **live** SL | per-trade |
| C | hold / cancel / restore | `core/src/pending_lifecycle.rs`, `hold.rs` | **pending** | per-trade |
| D | spread clock + mask | `core/src/spread_blackout.rs` | (pure) + global marker | global / per-instrument |
| E | market-hours blackout | `core/src/intent/blackout.rs`, `blackout/baked.rs` | pending (via sweep) | per-instrument |
| F | break-even | `cron/breakeven_watch.rs`, `core/src/intent/breakeven.rs` | **live** SL | per-trade |
| G | pending sweep | `cron/sweep.rs` | pending + opt-in position | per-attempt |
| H | news pause | `core/src/pause_gate.rs`, `dispatch/control.rs` | pause rows; gates entry | per-trade |
| I | retry cancel-and-replace | `core/src/retry_gate.rs` | pending | per-trade |
| — | trailing stop | **does not exist** | — | — |

**Scheduler loops** (`worker/src/scheduler.rs`) — four share the 900s `upkeep`
cadence and each independently re-fetch accounts/positions/orders:
`breakeven_loop` (F), `blackout_watch_loop` (C + B-restore), `blackout_apply_loop`
(B + the NY marker), `sweep_loop` (G). Plus `engine_tick_loop` (60s) and
`expiry_gc_loop` (3600s). **Only `engine_tick_loop` is panic-isolated** — a panic in
any of the other five kills all six (the exact 2026-07-14 failure mode
`run_isolated` was written for). Worth fixing while consolidating.

### Incidental findings worth separate cleanup

- **`daily_tick_interval()` is defined and documented but never called**
  (`worker/src/config.rs:144`), so nothing writes `blackout_windows` on the native
  runtime — the market-hours refresh cron doesn't exist. `sweep_gate` still ORs
  those (permanently empty) rows with the baked table. Dead half-system.
- **`apply_if_ny_close_edge`'s module docstring is stale** — it claims to widen
  stops; it now only sets the global marker, with an unused `_cron` param.
- **`is_spread_hour` fires for reviewed-flat instruments**: `mask == 0` and
  absent-from-table both fall back to `is_ny_close_edge`, so BTC/Gold/indices get
  spread-hour treatment at 21:00Z despite the generator explicitly reviewing them
  as flat (`spread_blackout.rs:387-393` flags this as a deferral). §4c retires this.
- **The news-pause entry gate is hand-inlined** at `enter.rs:77-113` rather than
  calling `pause_gate::entry_blocked`, which exists for exactly this and is what the
  replay uses. Duplicated decision.
