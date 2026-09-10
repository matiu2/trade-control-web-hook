# BUG: break-even watcher arms off pre-fill history and targets the trigger, not the fill

**Status:** FIXED on `fix/breakeven-arms-off-pre-fill-history` (v143). Both defects
closed; see "What was actually changed" at the foot of this file.
**Severity:** HIGH — silently converts winners into ~0R scratches, live only
**First confirmed incident:** NZD_CAD H4 short, 2026-08-10 (OANDA main account, tickets 2320→2326)
**File:** `trade-control-cron/src/breakeven_watch.rs` (both defects)

---

## Summary

An automated NZD_CAD H4 short filled at **0.82043** and was stopped out **6 minutes 2 seconds
later** at 0.82060 for −152.97 AUD. The designed stop was 0.82527 — **456 ticks** away. The
position had been amended to a stop at **0.82046**, i.e. **3 ticks** from the fill, or **1.6% of
the bar's ATR** (~0.00188).

Left alone, the trade hit its take-profit at 0.81531 three days later for **+1.18R**. All eight
entry-rule replay variants agree on +1.18R; the fixture corpus never reproduces this because
**replay does not share either defect** (see "Why fixtures never caught it").

Two independent defects combine. Either alone is a bug; together they guarantee an immediate
scratch on any trade whose instrument has traded below the arming level at any point in the
previous ~83 days.

---

## Defect 1 — `best_close_toward_tp` never filters to post-fill candles

`trade-control-cron/src/breakeven_watch.rs:211-230`

```rust
let since = now - Duration::seconds(snap.granularity.seconds() * BREAKEVEN_LOOKBACK_BARS);
let candles = fetch_candles(broker, instrument, snap.granularity, since, now).await?;
let bar = Duration::seconds(snap.granularity.seconds());
candles
    .into_iter()
    .filter(|c| c.time + bar <= now)      // <-- ONLY "is it closed". No `>= fill time`.
    .map(|c| c.c)
    .reduce(...)
```

`BREAKEVEN_LOOKBACK_BARS = 500` (`:58`). For H4 that is **500 × 4h ≈ 83 days** of candles.

The only filter is "has this bar closed". There is **no lower bound at the fill time**, despite
the function's own doc comment at `:207-210` stating *"Fetch the closed candles **since the
fill**"* — and the same claim repeated at `:23-25` and `:151`. `EntryAttempt` carries `placed_at`
(`core/src/state.rs:201-230`) and it is never consulted here.

Consequence: **any** bar in the preceding ~83 days that closed past the 50%-to-TP level arms
break-even on the **first cron tick after the fill**, before the trade has moved at all.

### Measured on the actual incident

Arming level for this trade (short): `entry + 0.5 × (tp − entry)` = **0.81788**.

Pulled the 500-bar H4 lookback that the watcher would have fetched
(2026-05-19 → 2026-08-10, 359 bars returned):

| | |
|---|---|
| Bars in window | 359 |
| Bars closing ≤ 0.81788 (i.e. arming BE) | **255 (71%)** |
| Most recent arming bar | **2026-07-30 11:00** — eleven days *before* the fill |
| Lowest close in window | 0.79976 (2026-06-29) |

Break-even was armed by July price action. The trade filled on 2026-08-10 and was scratched by
history it had no relationship to.

---

## Defect 2 — the break-even target is the resolved trigger, not the broker fill

`core/src/state.rs:190-192`:

```rust
/// Resolved entry/reference price at placement — the break-even target stop
/// (a 0R scratch) and the lower bound of the 50%-to-TP window.
pub entry_price: f64,
```

`Breakeven::target_stop(entry) -> entry` (`core/src/intent/breakeven.rs:100`) returns that value
verbatim as the new stop.

`entry_price` is snapshotted **once at placement** from `resolved.entry.reference_price()`
(`core/src/dispatch/enter.rs:951`) — the stop/limit trigger, or for a market entry the signal
bar's close (`core/src/intent/resolution.rs:287-291`). It is **never reconciled to the actual
fill**; `EntryAttempt` has no fill-price field.

### Measured on the actual incident

| | Price |
|---|---|
| Stop-order trigger (snapshot `entry_price`) | 0.82046 |
| **Actual broker fill** (tx 2321) | **0.82043** |
| **Stop the watcher amended to** (tx 2325) | **0.82046** |

The amended stop equals the snapshot trigger **exactly**. For a short, a stop 3 ticks *above* the
fill is not a scratch — it is a **guaranteed 3-tick loss plus slippage**, and it sits far inside
normal noise. OANDA's own trade analysis reports `risk_reward_ratio: 170.67` and
`stop_loss_distance: 0.00003` for the resulting position.

Exit was a further **1.4 pips unfavourable slippage** on the stop (0.82060 vs 0.82046).

Note this defect is *direction-dependent*: for a short filled better than trigger (fill below
trigger) the BE stop lands above the fill → loss. Long trades filled worse than trigger get the
mirror-image problem. Only an exact-trigger fill is harmless.

---

## Broker evidence (OANDA v20, account `m-and-w` main)

```
08-10 23:00:03 Bne  2320  STOP_ORDER      NZD_CAD  -876,934   CLIENT_ORDER
08-10 23:46:00 Bne  2321  ORDER_FILL      @ 0.82043            STOP_ORDER
08-10 23:46:00 Bne  2322  TAKE_PROFIT_ORDER  @ 0.81531         ON_FILL
08-10 23:46:00 Bne  2323  STOP_LOSS_ORDER    @ 0.82527         ON_FILL
08-10 23:52:02 Bne  2324  ORDER_CANCEL    orderID 2323   CLIENT_REQUEST_REPLACED
08-10 23:52:02 Bne  2325  STOP_LOSS_ORDER    @ 0.82046         REPLACEMENT      <-- 3 ticks
08-10 23:52:02 Bne  2326  ORDER_FILL      @ 0.82060  −152.97   STOP_LOSS_ORDER
08-10 23:52:02 Bne  2327  ORDER_CANCEL    orderID 2322   LINKED_TRADE_CLOSED
```

`2324`, `2325`, `2326` share `batchID 2324` and an identical timestamp — the replacement stop was
filled in the same batch that created it. Duration 6m 2s, `realized_pl` −152.972 AUD.

---

## Why fixtures never caught it

Replay implements break-even **correctly** and therefore diverges from live on both counts —
`cli/src/bin/replay_candles/fill_sim.rs:786`:

- it walks only `fill.rest`, i.e. strictly **post-fill** bars (no Defect 1), and
- it arms and targets off `fill.entry_price`, the **actual fill** (no Defect 2).

So the entire fixture corpus is blind to this class of bug. The 8-variant matrix for this very
setup returns +1.18R unanimously, including the `normal` variant that was traded live.

**This means replay-vs-live parity on break-even is untested.** Any live trade whose journalled R
disagrees with its replay R should be checked against this signature before the setup is blamed.

---

## Suggested fixes

1. **Bound the candle window at the fill.** In `best_close_toward_tp`, add
   `.filter(|c| c.time >= fill_time)` using `EntryAttempt.placed_at` (or better, a real fill
   timestamp — see 3). The 500-bar lookback then only bounds the *fetch*, not the *logic*.
   This alone stops the incident.
2. **Reject a BE amend that lands inside noise.** Guard the amend: refuse when
   `|target − current_price|` is below some floor (e.g. `max(spread × k, ATR × 0.1)`), and log
   loudly rather than silently amending. This is defence-in-depth — a correct BE stop should
   never be 1.6% of ATR from market.
3. **Carry the broker fill price back onto the snapshot.** `de91749a` already carries size and
   rate back onto the trade log; extend it to write the fill price into `BreakevenSnapshot
   .entry_price` (or add a `fill_price` field and prefer it). Fixes Defect 2 and makes the 0R
   scratch an actual 0R.
4. **Add a replay-vs-live parity fixture for break-even** so this class can't recur silently.

## Suggested confirmation from logs

`breakeven_watch.rs:175-185` prints an `INTENT amend_stop` line carrying `entry=`, `tp=` and
`best_close=`. For this trade, expect `entry=0.82046` (proving Defect 2) and a `best_close` dated
on or before **2026-07-30** (proving Defect 1). If both hold, this report is fully confirmed from
the system's own logs.

## Related

- `books/demo-journal/src/trade-155-nzdcad-h4-hs-live-scratch-asdesigned-win-1p18r.md` — the journal page
- `books/demo-journal/src/oanda-main-account-aug-2026.md` — Findings 3 / 3b, two further legs
  (AU200 −1,890 closed 82 min after fill; NZD/JPY −490 closed 72 s after fill) with the same
  "correct entry, premature exit" shape but no transaction trail pulled yet. **Both are worth
  re-checking against this signature.**
- Trade 135 (CAD/SGD) — SL→BE 142 s after fill with the threshold unmet; likely the same root cause.


---

## What was actually changed (2026-09-10, v143)

Both defects are closed, plus the noise-floor guard. The one thing the report
suggested that was **not** done as written is fix 3 — see below, it turned out
to need no new persistence at all.

### The premise that turned out to be wrong (and made the fix simpler)

The report frames fix 1 as bounding on `EntryAttempt.placed_at`, with fix 3
(carrying a fill price back onto the snapshot) as the better-but-harder
alternative. Both were unnecessary: **the brokers already report the fill on the
very call the watcher was already making.**

`list_open_positions` reads OANDA `Trade` (which carries `price` — the execution
price — and `openTime`) and TradeNation `Position` (`opening_price`,
`creation_time`), and `core::broker::OpenPosition` was simply dropping both on
the floor. So the true fill price *and* the true fill time were one struct field
away, needing no round-trip, no migration, and no reconciliation pass over
`EntryAttempt`.

That matters for the `placed_at` trap the report flags: `placed_at` is when the
*order* was placed and would still have admitted the 46 minutes of pre-fill bars
on this incident. **The window is bounded on `OpenPosition.opened_at` — the
broker's own record of when the position filled** — so the trap is avoided
rather than mitigated.

### Changes

1. **`OpenPosition` gained `entry_price: Option<f64>` and `opened_at:
   Option<DateTime<Utc>>`** (`core/src/broker.rs`), populated by both live
   adapters from the broker's own execution record and by the replay broker from
   the simulator's known fill. `None` means "the broker did not report one" —
   never a fabricated value; an unparseable OANDA `openTime` warns and yields
   `None`.

2. **The decision moved out of the wiring** into a new pure module
   `trade-control-cron/src/breakeven_decision.rs`. `best_close_toward_tp` and the
   inline `decide_move` call in `watch_one` are gone; `watch_one` is now a thin
   broker wrapper around `decide()`, which returns `Hold` / `Amend` /
   `Blocked(..)`. Both defects lived in logic that no test could reach, which is
   the reason for the split.

3. **Defect 1 — the window is bounded at the fill.** `armable_candles` keeps only
   bars satisfying `c.time + bar > fill_at` as well as the pre-existing
   "has it closed" bound. `BREAKEVEN_LOOKBACK_BARS` now bounds the *fetch* only,
   and its doc says so.

   The bound is on the bar's **close**, not its open, deliberately: `fill_sim`'s
   post-fill window is `&candles[i + 1..]` where `i + 1` is the fill bar, and its
   own comment says it "**includes** the fill bar itself". Bounding on the open
   would have made live one bar stricter than replay on every trade — a fresh
   divergence introduced by the fix for a divergence.

4. **Defect 2 — the target is the broker fill.** `target_entry` prefers
   `position.entry_price` and falls back to the placement snapshot only when the
   broker reports none. Note the asymmetry, which is intentional: a missing fill
   *time* fails **closed** (block, don't arm), a missing fill *price* fails
   **open** (use the snapshot). An unbounded window is unboundedly wrong; a
   snapshot price is off by the slippage on one fill, and refusing break-even
   over it would cost more than it saves.

5. **Fix 2 (noise floor) — implemented**, as `BREAKEVEN_MIN_ATR_FRACTION = 0.1`.
   It refuses an amend landing within `0.1 × ATR` of the last close and logs at
   `error`. It **fails open** when the ATR is unjudgeable (window shorter than
   `atr_length_for`), matching `sl_spread_floor_violation`'s "a degenerate spread
   is unjudgeable" discipline — a mutation that made it fail *closed* broke five
   legitimate break-evens, so the fail-open direction is load-bearing.

   The repo already holds this principle for *entries* (`intent::sl_spread_floor`
   rejects an SL within `10 ×` the live spread, because "the spread alone can
   stop the trade out"). A break-even amend is the same act and had no such
   check. It is expressed in ATR rather than spread only because this cron sees
   mid candles and cannot read a book. `0.1 × ATR` is a tripwire for absurdity,
   not a tuning knob: a legitimate break-even sits ~50%-to-TP from market, orders
   of magnitude clear of it.

### What was deliberately NOT done

- **Fix 3 as written** — no `fill_price` field was added to `EntryAttempt` and
  no reconciliation pass was written. The broker reports the fill on the open
  position itself, so persisting a second copy would add a migration and a
  staleness window to duplicate data already in hand. The *outcome* fix 3 asked
  for (a 0R scratch that is actually 0R) is delivered.

- **Fix 4 as a fixture** — no replay fixture was added, because the corpus
  **structurally cannot** exercise this path: fixtures drive `simulate_fill`,
  which has its own break-even implementation and never calls the cron. A fixture
  would have passed before the fix and after it, proving nothing. Parity is
  instead pinned where it can actually fail: `the_bar_the_fill_landed_inside_arms_matching_replay`
  asserts the live window boundary equals `fill_sim`'s, and the replay broker now
  reports the same two fill fields the live adapters do, so the two sides answer
  from the same shape.

### The related incidents, against this fix

Not verifiable from here (no broker access), so this is reasoning from the
shapes described, not confirmation:

- **AU200 −1,890, closed 82 min after fill** — *probably prevented, less
  certain.* 82 minutes is long enough that a genuine post-fill H1 bar could have
  closed and armed break-even legitimately. If the exit price sat at the entry it
  is this bug and the fix prevents it; if the loss is large relative to the
  designed stop it is more likely a real adverse move. The `entry_source=` field
  now in the log line distinguishes the two on any future occurrence.
- **NZD/JPY −490, closed 72 s after fill** — *prevented.* 72 seconds cannot
  contain a closed bar on any timeframe this system trades, so no post-fill bar
  could have armed break-even; the arm must have come from pre-fill history.
  Defect 1 alone.
- **Trade 135 (CAD/SGD), SL→BE 142 s after fill with the threshold unmet** —
  *prevented.* "Threshold unmet" plus 142 seconds is the exact signature: the
  arming evidence cannot have come from the position's own life.

### Tests

24 new tests. Every production change was mutation-verified — reverted
deliberately, with the corresponding test confirmed RED — including the wiring
between `decide` and the broker, which a first pass left uncovered: a mutation
making `watch_one` amend on a `Blocked` decision **survived** every test of the
pure decision. That is what the `PositionBroker` seam and the `SpyBroker` tests
exist for; they assert what reached the broker, not what the decision returned.

The full 2695-cell fixture corpus is unchanged (`--check`, exit 0), as expected —
and the gate was itself proven able to fail by poisoning one golden (exit 5).
